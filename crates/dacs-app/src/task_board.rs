//! Private, loopback-only CSR preview. The database is separate from static assets.
use anyhow::{Context, bail};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use dacs_worker::task_board::{TaskBoard, router};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

pub async fn serve(listen: SocketAddr, data: PathBuf, assets: PathBuf) -> anyhow::Result<()> {
    if !listen.ip().is_loopback() {
        bail!("task-board preview must bind a loopback address");
    }
    let assets = asset_root(&assets).await?;
    if let Some(parent) = data.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let data_parent = tokio::fs::canonicalize(
        data.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
    .await?;
    let data_path = if tokio::fs::try_exists(&data).await? {
        tokio::fs::canonicalize(&data).await?
    } else {
        data_parent.join(data.file_name().context("database path needs a filename")?)
    };
    if data_path.starts_with(&assets) {
        bail!("task-board database must be outside the static assets directory");
    }
    // Bind before opening redb; a failed bind never leaves a second database owner.
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let address = listener.local_addr()?;
    let board = TaskBoard::open(&data).context("open task-board database")?;
    tracing::info!(%address, "private task-board preview listening");
    axum::serve(listener, app(board, assets, address).await?)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = terminate.recv() => {},
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn asset_root(assets: &Path) -> anyhow::Result<PathBuf> {
    let root = tokio::fs::canonicalize(assets)
        .await
        .context("task-board assets missing; build examples/task-board-web with Trunk first")?;
    let index = tokio::fs::canonicalize(root.join("index.html"))
        .await
        .context("task-board assets require compiled index.html")?;
    if !index.starts_with(&root) || !tokio::fs::metadata(&index).await?.is_file() {
        bail!("task-board index must be a file inside the assets directory");
    }
    let html = tokio::fs::read_to_string(&index).await?;
    if html.contains("data-trunk") {
        bail!("task-board assets are uncompiled; pass Trunk's dist directory");
    }
    let mut entries = tokio::fs::read_dir(&root).await?;
    let (mut javascript, mut wasm) = (false, false);
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let extension = path.extension().and_then(|e| e.to_str());
        if matches!(extension, Some("js" | "wasm")) {
            let resolved = tokio::fs::canonicalize(&path).await?;
            if !resolved.starts_with(&root) || !tokio::fs::metadata(&resolved).await?.is_file() {
                bail!("compiled task-board assets must remain inside the assets directory");
            }
            javascript |= extension == Some("js");
            wasm |= extension == Some("wasm");
        }
    }
    if !javascript || !wasm {
        bail!("task-board assets missing compiled JavaScript or WebAssembly; rebuild with Trunk");
    }
    Ok(root)
}

struct Assets {
    root: PathBuf,
    index: Bytes,
}

#[derive(Clone)]
struct Confinement {
    address: SocketAddr,
    csp: HeaderValue,
}

async fn app(board: TaskBoard, assets: PathBuf, address: SocketAddr) -> anyhow::Result<Router> {
    let index = tokio::fs::read_to_string(assets.join("index.html")).await?;
    let confinement = Confinement {
        address,
        csp: script_policy(&index)?.parse()?,
    };
    // The policy hashes this exact document, so the same bytes are served for the
    // lifetime of the process; a frontend rebuild takes effect on restart.
    let files = Router::new()
        .fallback(static_file)
        .with_state(Arc::new(Assets {
            root: assets,
            index: Bytes::from(index),
        }));
    Ok(router(board)
        .fallback_service(files)
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(confinement, confine)))
}

fn script_policy(index: &str) -> anyhow::Result<String> {
    // Trunk generates inline module initialization. Hash the exact built script,
    // not a wildcard unsafe-inline permission. HTML normalizes CRLF/CR to LF.
    let normalized = index.replace("\r\n", "\n").replace('\r', "\n");
    let mut rest = normalized.as_str();
    let mut scripts = String::from("'self' 'wasm-unsafe-eval'");
    while let Some(start) = rest.find("<script") {
        rest = &rest[start + "<script".len()..];
        let start = rest.find('>').context("invalid compiled script tag")?;
        rest = &rest[start + 1..];
        let end = rest
            .find("</script>")
            .context("unclosed compiled script tag")?;
        let hash = STANDARD.encode(Sha256::digest(rest[..end].as_bytes()));
        scripts.push_str(&format!(" 'sha256-{hash}'"));
        rest = &rest[end + "</script>".len()..];
    }
    Ok(format!(
        "default-src 'self'; script-src {scripts}; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
    ))
}

fn allowed_request(headers: &HeaderMap, method: &Method, address: SocketAddr) -> bool {
    let host = match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(host) if headers.get_all(header::HOST).iter().count() == 1 => host,
        _ => return false,
    };
    // Exact bound authority or localhost only: no wildcard Host/DNS rebinding.
    if host != address.to_string() && host != format!("localhost:{}", address.port()) {
        return false;
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v != "same-origin" && v != "none")
    {
        return false;
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        if headers.get_all(header::ORIGIN).iter().count() != 1
            || origin.to_str().ok() != Some(format!("http://{host}").as_str())
        {
            return false;
        }
    } else if !matches!(*method, Method::GET | Method::HEAD) {
        // Mutations require the browser's exact same-origin proof, including DELETE.
        return false;
    }
    true
}

async fn confine(State(confinement): State<Confinement>, request: Request, next: Next) -> Response {
    if !allowed_request(request.headers(), request.method(), confinement.address) {
        return (
            StatusCode::FORBIDDEN,
            "task-board requests must be same-origin loopback requests",
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "same-origin".parse().unwrap());
    headers.insert(
        "cross-origin-resource-policy",
        "same-origin".parse().unwrap(),
    );
    headers.insert("content-security-policy", confinement.csp);
    response
}

fn relative_asset(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/')?;
    if path.is_empty() {
        return Some("index.html");
    }
    // Trunk filenames are ASCII; refuse encoded separators, Windows ADS and dot paths.
    if path.contains(['%', '\\', ':'])
        || path.split('/').any(|p| p.is_empty() || p.starts_with('.'))
    {
        return None;
    }
    if path == "api" || path.starts_with("api/") {
        return None;
    }
    Some(path)
}

async fn static_file(State(assets): State<Arc<Assets>>, request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(relative) = relative_asset(request.uri().path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (mime, bytes) = if relative == "index.html" {
        ("text/html; charset=utf-8", assets.index.clone())
    } else {
        let root = &assets.root;
        let Ok(path) = tokio::fs::canonicalize(root.join(relative)).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        // Canonical containment also rejects symlinks and Windows junctions escaping root.
        if !path.starts_with(root)
            || !tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file())
        {
            return StatusCode::NOT_FOUND.into_response();
        }
        let mime = match path.extension().and_then(|e| e.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            Some("wasm") => "application/wasm",
            Some("css") => "text/css; charset=utf-8",
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("ico") => "image/x-icon",
            Some("json") => "application/json",
            Some("woff2") => "font/woff2",
            _ => "application/octet-stream",
        };
        let Ok(bytes) = tokio::fs::read(&path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (mime, Bytes::from(bytes))
    };
    let length = bytes.len();
    let body = if request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length)
        .body(body)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_boundaries_reject_foreign_null_absent_and_rebinding_origins() {
        let address: SocketAddr = "127.0.0.1:7311".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:7311".parse().unwrap());
        assert!(allowed_request(&headers, &Method::GET, address));
        assert!(!allowed_request(&headers, &Method::DELETE, address));
        for origin in [
            "null",
            "https://127.0.0.1:7311",
            "http://evil.example",
            "http://127.0.0.1:7312",
        ] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(!allowed_request(&headers, &Method::POST, address));
        }
        headers.insert(header::ORIGIN, "http://127.0.0.1:7311".parse().unwrap());
        assert!(allowed_request(&headers, &Method::POST, address));
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!allowed_request(&headers, &Method::GET, address));
        headers.remove("sec-fetch-site");
        headers.insert(header::HOST, "evil.example:7311".parse().unwrap());
        headers.insert(header::ORIGIN, "http://evil.example:7311".parse().unwrap());
        assert!(!allowed_request(&headers, &Method::POST, address));
    }

    #[test]
    fn static_paths_are_confined() {
        assert_eq!(relative_asset("/"), Some("index.html"));
        assert_eq!(
            relative_asset("/board-123_bg.wasm"),
            Some("board-123_bg.wasm")
        );
        for path in [
            "/../secret",
            "/%2e%2e/secret",
            "/file:secret",
            "/x\\secret",
            "/.env",
            "/api/missing",
            "//secret",
        ] {
            assert_eq!(relative_asset(path), None, "{path}");
        }
    }

    #[tokio::test]
    async fn assets_are_required_and_api_requests_cross_the_real_boundary() {
        let dir = tempfile::tempdir().unwrap();
        assert!(asset_root(dir.path()).await.is_err());
        let assets_dir = dir.path().join("assets");
        tokio::fs::create_dir(&assets_dir).await.unwrap();
        tokio::fs::write(
            assets_dir.join("index.html"),
            "<!doctype html><title>Board</title>",
        )
        .await
        .unwrap();
        assert!(asset_root(&assets_dir).await.is_err());
        // Transport tests use local asset fixtures; the integration owner verifies
        // actual Trunk output and browser execution separately.
        tokio::fs::write(assets_dir.join("board.js"), "export {};")
            .await
            .unwrap();
        tokio::fs::write(assets_dir.join("board.wasm"), b"\0asm\x01\0\0\0")
            .await
            .unwrap();
        let assets = asset_root(&assets_dir).await.unwrap();
        let board = TaskBoard::open(dir.path().join("board.redb")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app(board, assets, address).await.unwrap())
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://{address}");
        let response = client
            .get(format!("{base}/api/tasks"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tasks"))
                .json(&serde_json::json!({"title":"Denied"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tasks"))
                .header("Origin", &base)
                .json(&serde_json::json!({"title":"Created"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tasks"))
                .header("Origin", &base)
                .header("Content-Type", "text/plain")
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tasks"))
                .header("Origin", &base)
                .json(&serde_json::json!({"title":"  "}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tasks/missing/status"))
                .header("Origin", &base)
                .json(&serde_json::json!({"status":"unknown"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            client
                .delete(format!("{base}/api/tasks/missing"))
                .header("Origin", &base)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .post(format!("{base}/api/tasks"))
                .header("Origin", &base)
                .json(&serde_json::json!({"title":"Oversized","description":"x".repeat(65536)}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            client
                .get(format!("{base}/api/missing"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .get(format!("{base}/"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        // A rebuild while running must not serve a document the startup policy
        // does not cover: the index and its policy both come from startup.
        tokio::fs::write(
            assets_dir.join("index.html"),
            "<!doctype html><title>Rebuilt</title><script>rebuilt()</script>",
        )
        .await
        .unwrap();
        for path in ["/", "/index.html"] {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            let policy = response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .to_owned();
            assert!(!policy.contains("'sha256-"), "{policy}");
            assert_eq!(
                response.text().await.unwrap(),
                "<!doctype html><title>Board</title>"
            );
        }
        stop.send(()).unwrap();
        server.await.unwrap();
        let reopened = TaskBoard::open(dir.path().join("board.redb")).unwrap();
        assert_eq!(reopened.list(Default::default()).unwrap().len(), 1);
    }

    #[test]
    fn compiled_loader_hash_is_exact_and_does_not_allow_arbitrary_inline_scripts() {
        let policy = script_policy(
            "<script type=\"module\">import init from './board.js';\r\ninit();</script>",
        )
        .unwrap();
        let hash = STANDARD.encode(Sha256::digest(b"import init from './board.js';\ninit();"));
        assert!(policy.contains(&format!("'sha256-{hash}'")));
        assert!(!policy.contains("'unsafe-inline'"));
        assert!(script_policy("<script>missing closer").is_err());
    }

    #[tokio::test]
    async fn startup_rejects_public_bind_and_database_inside_static_directory() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("board.redb");
        assert!(
            serve(
                "0.0.0.0:0".parse().unwrap(),
                data.clone(),
                dir.path().to_owned()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("loopback")
        );
        tokio::fs::write(dir.path().join("index.html"), "<title>Board</title>")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("board.js"), "export {};")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("board.wasm"), b"\0asm\x01\0\0\0")
            .await
            .unwrap();
        assert!(
            serve("127.0.0.1:0".parse().unwrap(), data, dir.path().to_owned())
                .await
                .unwrap_err()
                .to_string()
                .contains("outside")
        );
    }
}
