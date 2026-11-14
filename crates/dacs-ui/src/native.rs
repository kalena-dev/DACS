//! Native window hosting the same SSR session, with no IPC or privileged bridge.
//! Call `launch` on the process main thread. Windows requires Edge WebView2;
//! Linux requires GTK3/WebKitGTK 4.1; macOS uses system WKWebView.
use anyhow::{Context, Result, bail};
use url::{Host, Url};

fn validated_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).context("invalid coordinator URL")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "native launch URL cannot contain credentials, query parameters or fragments; use the session login form"
        );
    }
    let loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain("localhost")) => true,
        _ => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("remote coordinator requires HTTPS; HTTP is allowed only on loopback");
    }
    if !matches!(
        url.path(),
        "/" | "/login" | "/setup" | "/onboarding" | "/network" | "/builds"
    ) {
        bail!(
            "launch a dashboard, login, compiler pools, network or local setup page; invitation and credential paths are not launch URLs"
        );
    }
    Ok(url)
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
pub fn launch(url: &str) -> Result<()> {
    use tao::{
        dpi::LogicalSize,
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoop},
        window::WindowBuilder,
    };
    use wry::{NewWindowResponse, WebViewBuilder};
    #[cfg(target_os = "linux")]
    use {tao::platform::unix::WindowExtUnix, wry::WebViewBuilderExtUnix};

    let url = validated_url(url)?;
    let allowed_origin = url.origin();
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(format!("DACS — {}", url.origin().ascii_serialization()))
        .with_inner_size(LogicalSize::new(1200.0, 820.0))
        .with_min_inner_size(LogicalSize::new(360.0, 480.0))
        .build(&event_loop)
        .context("create native DACS window")?;
    let builder = WebViewBuilder::new()
        .with_url(url.as_str())
        .with_incognito(true)
        .with_devtools(false)
        .with_javascript_disabled()
        .with_navigation_handler(move |raw| {
            Url::parse(&raw).is_ok_and(|next| {
                next.origin() == allowed_origin
                    && next.username().is_empty()
                    && next.password().is_none()
                    && !next.query_pairs().any(|(key, _)| {
                        matches!(
                            key.as_ref(),
                            "token" | "access_token" | "device_token" | "authorization"
                        )
                    })
            })
        })
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_download_started_handler(|_, _| false);
    #[cfg(target_os = "linux")]
    let webview = builder
        .build_gtk(window.gtk_window())
        .context("create GTK/WebKit native webview")?;
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let webview = builder
        .build(&window)
        .context("create native webview (Windows requires WebView2 runtime)")?;

    event_loop.run(move |event, _, control_flow| {
        // Both native resources must remain alive until the window closes.
        let _keep_alive = (&window, &webview);
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn launch(url: &str) -> Result<()> {
    let _ = validated_url(url)?;
    bail!(
        "native DACS supports Windows, macOS and Linux; this target has no native webview adapter"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_requires_secure_origin_without_url_secrets() {
        assert!(validated_url("https://coordinator.example/").is_ok());
        assert!(validated_url("http://127.0.0.1:8080/login").is_ok());
        assert!(validated_url("http://[::1]:8080/setup").is_ok());
        for url in [
            "http://coordinator.example/",
            "https://coordinator.example/?token=x",
            "https://u:p@coordinator.example/",
            "file:///tmp/index.html",
            "https://coordinator.example/#secret",
            "https://coordinator.example/invite/token",
        ] {
            assert!(validated_url(url).is_err(), "unexpectedly accepted {url}");
        }
    }
}
