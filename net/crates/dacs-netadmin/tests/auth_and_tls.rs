//! Item 1: TLS is required and the coordinator operation token is required
//! and checked before anything reaches Headscale.

mod common;

use axum::http::StatusCode;
use common::*;
use dacs_netadmin::config::Config;
use dacs_netadmin::http::hash_token;
use serde_json::{Value, json};

const PROTECTED: &[(&str, &str)] = &[
    ("GET", "/v1/contract"),
    ("POST", "/v1/enrollments"),
    ("POST", "/v1/memberships/query"),
    ("POST", "/v1/revocations"),
    ("GET", "/v1/nodes"),
    ("GET", "/v1/compiler-endpoints"),
];

fn valid_body(path: &str) -> Option<Value> {
    match path {
        "/v1/enrollments" => Some(enrollment_request("alice-pc", &["worker"])),
        "/v1/memberships/query" => Some(membership_query("dev-alice-pc", "alice-pc", Some(1), None)),
        "/v1/revocations" => Some(revocation("dev-alice-pc", Some(1), None)),
        _ => None,
    }
}

#[tokio::test]
async fn every_operation_route_rejects_missing_or_wrong_tokens_without_touching_headscale() {
    let h = Harness::start().await;
    let token_hash_hex = hex::encode(hash_token(TOKEN));
    let bad_headers: Vec<Option<String>> = vec![
        None,
        Some(String::new()),
        Some("Bearer".into()),
        Some("Bearer ".into()),
        Some("Bearer    ".into()),
        Some(TOKEN.into()),                           // no scheme
        Some(format!("Basic {TOKEN}")),               // wrong scheme
        Some(format!("Token {TOKEN}")),               // wrong scheme
        Some("Bearer wrong-token".into()),
        Some(format!("Bearer {TOKEN}x")),             // suffix
        Some(format!("Bearer {}", &TOKEN[..TOKEN.len() - 1])), // prefix
        Some(format!("Bearer {token_hash_hex}")),     // the stored hash is not a token
        Some(format!("Bearer {}", h.hs.api_key)),     // Headscale's key is not an operation token
        Some(format!("Bearer {TOKEN},Bearer {TOKEN}")),
    ];
    for (method, path) in PROTECTED {
        for auth in &bad_headers {
            let (status, body) = h.call(method, path, auth.as_deref(), valid_body(path)).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} with {auth:?} -> {body}"
            );
            let err = api_error(&body);
            assert_eq!(err.code, "unauthorized");
            assert!(!body.contains(TOKEN), "401 body echoes the expected token");
        }
    }
    assert!(
        h.hs.requests().is_empty(),
        "rejected callers reached Headscale: {:?}",
        h.hs.requests()
    );
    assert!(h.audit().is_empty(), "rejected callers produced audit records");
}

#[tokio::test]
async fn missing_and_invalid_tokens_have_distinct_documented_messages() {
    let h = Harness::start().await;
    let (s, body) = h.call("GET", "/v1/nodes", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(api_error(&body).message, "operation token required");
    let (s, body) = h.call("GET", "/v1/nodes", Some("Bearer nope"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(api_error(&body).message, "invalid operation token");
}

#[tokio::test]
async fn token_is_checked_before_the_body_is_parsed() {
    let h = Harness::start().await;
    for path in ["/v1/enrollments", "/v1/memberships/query", "/v1/revocations"] {
        // Malformed JSON, wrong schema and a missing content type: an
        // unauthenticated caller must get 401, not a schema-revealing 4xx.
        for (ct, body) in [
            (Some("application/json"), "{not json"),
            (Some("application/json"), "{\"device_id\": 7}"),
            (None, "{}"),
            (Some("text/plain"), "hello"),
        ] {
            let mut b = axum::http::Request::post(path);
            if let Some(ct) = ct {
                b = b.header("content-type", ct);
            }
            let req = b.body(axum::body::Body::from(body)).unwrap();
            let resp = tower::ServiceExt::oneshot(h.app.clone(), req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path} {ct:?} {body}");
        }
    }
    assert!(h.hs.requests().is_empty());
}

#[tokio::test]
async fn valid_token_is_accepted_on_every_route() {
    let h = Harness::start().await;
    let (s, body) = h.op("GET", "/v1/contract", None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let contract: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(contract["contract_revision"], dacs_net_contract::CONTRACT_REVISION);
    assert_eq!(contract["key_ttl_minutes"], 15);
    // The contract projection never carries Headscale credentials.
    assert_no_secrets(&body, &h.hs.secrets(), "contract");

    let (s, _) = h.op("GET", "/v1/nodes", None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.op("GET", "/v1/compiler-endpoints", None).await;
    assert_eq!(s, StatusCode::OK);
    // Surrounding whitespace in the token is tolerated.
    let (s, _) = h
        .call("GET", "/v1/nodes", Some(&format!("Bearer  {TOKEN} ")), None)
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn health_is_public_but_reveals_no_upstream_detail() {
    let h = Harness::start().await;
    let (s, body) = h.call("GET", "/v1/health", None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    h.hs.fault(Op::Health, Fault::EchoAuth);
    let (s, body) = h.call("GET", "/v1/health", None, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["headscale_error"], "headscale unavailable");
    assert_no_secrets(&body, &h.hs.secrets(), "public health body");
}

#[tokio::test]
async fn infra_role_is_refused_with_403_even_with_a_valid_token() {
    let h = Harness::start().await;
    for roles in [vec!["infra"], vec!["worker", "infra"], vec!["infra", "coordinator"]] {
        let (s, body) = h.enroll(&enrollment_request("edge-1", &roles)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{roles:?}: {body}");
        assert_eq!(api_error(&body).code, "forbidden");
    }
    assert!(
        h.hs.mutations().is_empty(),
        "an infra-role request reached Headscale: {:?}",
        h.hs.mutations()
    );
    assert!(h.hs.lock().keys.is_empty());
}

#[tokio::test]
async fn infra_and_personal_nodes_and_keys_cannot_be_queried_or_revoked() {
    let h = Harness::start().await;
    let infra = h.hs.add_foreign_node("headscale-infra", &["tag:dacs-infra"], Some(&["tag:dacs-infra"]));
    let mixed = h.hs.add_foreign_node(
        "mixed",
        &["tag:dacs-worker", "tag:dacs-infra"],
        Some(&["tag:dacs-worker", "tag:dacs-infra"]),
    );
    let personal = h.hs.add_foreign_node("laptop", &[], None);
    let foreign_tag = h.hs.add_foreign_node("printer", &["tag:office"], None);
    let infra_key = h.hs.add_foreign_key(&["tag:dacs-infra"]);
    let untagged_key = h.hs.add_foreign_key(&[]);
    let before_nodes = h.hs.lock().nodes.len();
    h.hs.clear_requests();

    for node in [infra, mixed, personal, foreign_tag] {
        let (s, body) = h.revoke(&revocation("dev-x", None, Some(node))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "revoke node {node}: {body}");
        assert_eq!(api_error(&body).code, "forbidden");
        let (s, body) = h
            .membership(&membership_query("dev-x", "renamed-x", None, Some(node)))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "query node {node}: {body}");
    }
    for key in [infra_key, untagged_key] {
        let (s, body) = h.revoke(&revocation("dev-x", Some(key), None)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "revoke key {key}: {body}");
        let (s, body) = h
            .membership(&membership_query("dev-x", "renamed-x", Some(key), None))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "query key {key}: {body}");
    }
    assert!(h.hs.mutations().is_empty(), "{:?}", h.hs.mutations());
    assert_eq!(h.hs.lock().nodes.len(), before_nodes);
    for key in [infra_key, untagged_key] {
        let k = h.hs.key(key).unwrap();
        assert!(k.expiration > Some(time::OffsetDateTime::now_utc()), "key {key} was expired");
    }
    // Status listings only show DACS-enrollable nodes.
    let (_, body) = h.op("GET", "/v1/nodes", None).await;
    assert_eq!(body, "[]");
}

// ---- TLS is required by configuration -------------------------------------

fn config_json(listen: &str, tls: bool, allow_plaintext: Option<bool>) -> Value {
    let mut v = json!({
        "listen": listen,
        "headscale_url": "http://127.0.0.1:8080",
        "headscale_api_key_path": "/nonexistent/key",
        "operation_token_hashes_path": "/nonexistent/tokens",
        "login_server": LOGIN_SERVER,
        "coordinator": {"bootstrap_url": BOOTSTRAP_URL, "overlay_url": OVERLAY_URL},
    });
    if tls {
        v["tls"] = json!({"cert_path": "/x/cert.pem", "key_path": "/x/key.pem"});
    }
    if let Some(a) = allow_plaintext {
        v["allow_plaintext_loopback"] = a.into();
    }
    v
}

fn load(v: &Value) -> Result<Config, String> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.json");
    std::fs::write(&p, v.to_string()).unwrap();
    Config::load(&p).map_err(|e| e.to_string())
}

#[test]
fn plaintext_listener_is_refused_unless_loopback_and_explicitly_allowed() {
    // TLS configured: any address is fine.
    for listen in ["100.64.0.1:9443", "0.0.0.0:9443", "127.0.0.1:9443", "[::1]:9443"] {
        assert!(load(&config_json(listen, true, None)).is_ok(), "{listen}");
    }
    // No TLS, no explicit opt-in: refused everywhere, loopback included.
    for listen in ["100.64.0.1:9443", "0.0.0.0:9443", "127.0.0.1:9443", "[::1]:9443"] {
        for allow in [None, Some(false)] {
            let err = load(&config_json(listen, false, allow)).unwrap_err();
            assert!(err.contains("tls is required"), "{listen} {allow:?}: {err}");
        }
    }
    // Opt-in only works on a loopback address, never a routable or wildcard one.
    for listen in ["100.64.0.1:9443", "0.0.0.0:9443", "[::]:9443", "192.168.1.5:9443"] {
        let err = load(&config_json(listen, false, Some(true))).unwrap_err();
        assert!(err.contains("tls is required"), "{listen}: {err}");
    }
    for listen in ["127.0.0.1:9443", "[::1]:9443"] {
        assert!(load(&config_json(listen, false, Some(true))).is_ok(), "{listen}");
    }
}

#[test]
fn headscale_api_key_is_only_sent_over_tls_or_loopback() {
    let mut ok = vec![
        "http://127.0.0.1:8080",
        "http://localhost:8080",
        "http://[::1]:8080",
        "https://hs.internal:443",
    ];
    for url in ok.drain(..) {
        let mut v = config_json("100.64.0.1:9443", true, None);
        v["headscale_url"] = url.into();
        assert!(load(&v).is_ok(), "{url}");
    }
    for url in [
        "http://10.0.0.5:8080",
        "http://hs.example.org",
        "http://localhost.evil.example:8080",
        "http://127.0.0.1.evil.example:8080",
        "http://127.0.0.1@evil.example/",
        "http://user@127.0.0.1:8080",
        "HTTP://10.0.0.5:8080",
        "ftp://127.0.0.1",
        "127.0.0.1:8080",
    ] {
        let mut v = config_json("100.64.0.1:9443", true, None);
        v["headscale_url"] = url.into();
        assert!(load(&v).is_err(), "{url} accepted");
    }
}

#[test]
fn login_server_and_coordinator_urls_must_be_https() {
    let mut v = config_json("100.64.0.1:9443", true, None);
    v["login_server"] = "http://hs.dacs.test".into();
    assert!(load(&v).is_err());
    let mut v = config_json("100.64.0.1:9443", true, None);
    v["coordinator"]["bootstrap_url"] = "http://bootstrap.dacs.test".into();
    assert!(load(&v).is_err());
    let mut v = config_json("100.64.0.1:9443", true, None);
    v["coordinator"]["overlay_url"] = "http://coordinator.dacs.internal:7443".into();
    assert!(load(&v).is_err());
    let mut v = config_json("100.64.0.1:9443", true, None);
    v["unexpected_field"] = true.into();
    assert!(load(&v).is_err(), "unknown config fields are refused");
}

#[cfg(unix)]
#[test]
fn secret_files_must_be_private_and_nonempty() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(dir.path(), "127.0.0.1:9443", "http://127.0.0.1:8080", "hskey-api-secret", json!({"allow_plaintext_loopback": true}));
    let cfg = Config::load(&cfg_path).unwrap();
    assert_eq!(cfg.headscale_api_key().unwrap().expose(), "hskey-api-secret");
    assert_eq!(cfg.token_hashes().unwrap(), vec![hash_token(TOKEN)]);

    for (file, mode) in [
        ("headscale-api-key", 0o640),
        ("headscale-api-key", 0o604),
        ("operation-tokens.sha256", 0o644),
    ] {
        let p = dir.path().join(file);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = if file == "headscale-api-key" {
            cfg.headscale_api_key().map(|_| ()).unwrap_err()
        } else {
            cfg.token_hashes().map(|_| ()).unwrap_err()
        };
        let msg = err.to_string();
        assert!(msg.contains("chmod 600"), "{file} {mode:o}: {msg}");
        assert!(!msg.contains("hskey-api-secret"), "error leaks the key");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    write_private(&dir.path().join("headscale-api-key"), "  \n");
    assert!(cfg.headscale_api_key().is_err(), "empty API key accepted");
    write_private(&dir.path().join("operation-tokens.sha256"), "# none\n\n");
    assert!(cfg.token_hashes().is_err(), "empty token list accepted");
    write_private(&dir.path().join("operation-tokens.sha256"), "not-hex\n");
    assert!(cfg.token_hashes().is_err(), "malformed token hash accepted");
    // A plaintext token in the hash file is refused (it is not a 64-hex hash).
    write_private(&dir.path().join("operation-tokens.sha256"), &format!("{TOKEN}\n"));
    assert!(cfg.token_hashes().is_err(), "plaintext token accepted as a hash");
}

#[tokio::test]
async fn any_configured_token_hash_is_accepted_for_rotation() {
    use dacs_netadmin::audit::Audit;
    use dacs_netadmin::headscale::HeadscaleClient;
    use dacs_netadmin::http::{AppState, router};
    use dacs_netadmin::service::Service;
    use std::sync::Arc;

    let hs = MockHeadscale::start().await;
    let client = HeadscaleClient::new(&hs.url(), dacs_net_contract::Secret::new(hs.api_key.clone())).unwrap();
    let app = router(Arc::new(AppState {
        service: Service::new(client, settings(), Audit::disabled()),
        token_hashes: vec![hash_token("old-token"), hash_token("new-token")],
    }));
    for t in ["old-token", "new-token"] {
        let (s, _) = call(&app, "GET", "/v1/nodes", Some(&format!("Bearer {t}")), None).await;
        assert_eq!(s, StatusCode::OK, "{t}");
    }
    let (s, _) = call(&app, "GET", "/v1/nodes", Some("Bearer older-token"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
