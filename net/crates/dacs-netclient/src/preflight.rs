//! Explicit-target diagnostics; never enroll, switch profiles, or use admin credentials.
use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE_REVISION, CONTRACT_REVISION, TailnetAssessment, is_overlay_ip,
};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};
use url::Url;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightConfig {
    pub login_server: String,
    pub bootstrap_url: String,
    /// Omit private endpoints for an unenrolled invitee's public bootstrap check.
    pub control_url: Option<String>,
    pub netadmin_url: Option<String>,
    /// Public CA certificates only; never private keys or administrative tokens.
    #[serde(default)]
    pub ca_files: Vec<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}
fn default_timeout() -> u64 {
    5000
}

#[derive(Debug, Serialize)]
pub struct PreflightReport {
    pub ok: bool,
    pub assessment: TailnetAssessment,
    pub checks: Vec<EndpointCheck>,
    pub private_endpoints_checked: bool,
}

#[derive(Debug, Serialize)]
pub struct EndpointCheck {
    pub service: &'static str,
    pub url: String,
    pub resolved: Vec<SocketAddr>,
    pub http_status: Option<u16>,
    pub elapsed_ms: u128,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
enum Service {
    Headscale,
    Bootstrap,
    Control,
    Netadmin,
    Bridge,
}
impl Service {
    fn name(self) -> &'static str {
        match self {
            Self::Headscale => "headscale",
            Self::Bootstrap => "bootstrap",
            Self::Control => "control",
            Self::Netadmin => "netadmin",
            Self::Bridge => "bootstrap_network_bridge",
        }
    }
    fn route(self) -> &'static str {
        match self {
            Self::Netadmin => "/v1/health",
            Self::Bridge => "/api/network/contract",
            _ => "/health",
        }
    }
    fn private(self) -> bool {
        matches!(self, Self::Control | Self::Netadmin)
    }
}

fn base_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|_| "endpoint is not a valid URL".to_owned())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(
            "endpoints must be HTTPS origins without credentials, paths, queries or fragments"
                .into(),
        );
    }
    let host = url.host_str().unwrap();
    if host.contains('_')
        || host.ends_with(".invalid")
        || host == "example.org"
        || host.ends_with(".example.org")
    {
        return Err(
            "replace example endpoint placeholders with owner-supplied deployment addresses".into(),
        );
    }
    Ok(url)
}

pub async fn run(config: &PreflightConfig) -> Result<PreflightReport, String> {
    if !(1..=60_000).contains(&config.timeout_ms) {
        return Err("timeout_ms must be 1..60000".into());
    }
    // Validate every target before making any requests; never print malformed secret-bearing URLs.
    let login = base_url(&config.login_server)?;
    let bootstrap = base_url(&config.bootstrap_url)?;
    let control = config.control_url.as_deref().map(base_url).transpose()?;
    let netadmin = config.netadmin_url.as_deref().map(base_url).transpose()?;
    let mut certificates = Vec::new();
    for path in &config.ca_files {
        let pem = tokio::fs::read(path)
            .await
            .map_err(|e| format!("CA file {}: {e}", path.display()))?;
        certificates.extend(
            reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|_| "invalid CA certificate PEM")?,
        );
    }
    let timeout = Duration::from_millis(config.timeout_ms);
    let mut checks = Vec::new();
    for (service, url) in [
        (Service::Headscale, Some(login)),
        (Service::Bootstrap, Some(bootstrap.clone())),
        (Service::Bridge, Some(bootstrap)),
        (Service::Control, control),
        (Service::Netadmin, netadmin),
    ] {
        if let Some(url) = url {
            checks.push(check(service, url, &certificates, timeout).await);
        }
    }
    // Assessment is read-only and distinguishes installed/logged-out/displaced tailnets.
    let assessment = crate::assess(&config.login_server).await;
    Ok(PreflightReport {
        ok: checks.iter().all(|c| c.ok),
        assessment,
        checks,
        private_endpoints_checked: config.control_url.is_some() && config.netadmin_url.is_some(),
    })
}

async fn check(
    service: Service,
    mut url: Url,
    certificates: &[reqwest::Certificate],
    timeout: Duration,
) -> EndpointCheck {
    url.set_path(service.route());
    let started = Instant::now();
    let mut result = EndpointCheck {
        service: service.name(),
        url: url.to_string(),
        resolved: vec![],
        http_status: None,
        elapsed_ms: 0,
        ok: false,
        error: None,
    };
    let operation = async {
        let host = url.host_str().unwrap().trim_matches(['[', ']']);
        let addresses: Vec<_> =
            tokio::net::lookup_host((host, url.port_or_known_default().unwrap()))
                .await
                .map_err(|_| "DNS lookup failed".to_owned())?
                .collect();
        result.resolved = addresses.clone();
        if addresses.is_empty() {
            return Err("DNS returned no addresses".into());
        }
        if service.private() && addresses.iter().any(|a| !is_overlay_ip(a.ip())) {
            return Err(
                "private endpoint resolved outside the overlay; no connection attempted".into(),
            );
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .resolve_to_addrs(host, &addresses);
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate.clone());
        }
        let client = builder
            .build()
            .map_err(|_| "TLS client initialization failed".to_owned())?;
        let mut response = client.get(url.clone()).send().await.map_err(|e| {
            if e.is_timeout() {
                "HTTPS connection timed out".to_owned()
            } else {
                "HTTPS connection failed (check route, listener, certificate name and trusted CA)"
                    .to_owned()
            }
        })?;
        result.http_status = Some(response.status().as_u16());
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "response read failed".to_owned())?
        {
            if body.len() + chunk.len() > 64 * 1024 {
                return Err("health response exceeds 64 KiB".into());
            }
            body.extend_from_slice(&chunk);
        }
        validate_health(service, &body)
    };
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(())) => result.ok = true,
        Ok(Err(error)) => result.error = Some(error),
        Err(_) => result.error = Some("DNS/TLS/HTTP check exceeded timeout".into()),
    }
    result.elapsed_ms = started.elapsed().as_millis();
    result
}

fn validate_health(service: Service, body: &[u8]) -> Result<(), String> {
    // Headscale's documented /health is status-only; DACS health and bridge have schemas.
    if matches!(service, Service::Headscale) {
        return Ok(());
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "expected a JSON health/contract response")?;
    let valid = match service {
        Service::Bootstrap | Service::Control => {
            value["status"] == "ready" && value["protocol_version"] == 1
        }
        Service::Netadmin => value["ok"] == true && value["contract_revision"] == CONTRACT_REVISION,
        Service::Bridge => {
            value["contract_revision"] == CONTRACT_REVISION
                && value["admin_disclosure_revision"] == ADMIN_ACCESS_DISCLOSURE_REVISION
        }
        Service::Headscale => unreachable!(),
    };
    if valid {
        Ok(())
    } else {
        Err("service is unhealthy or protocol/contract revision is incompatible".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_validation_rejects_secrets_paths_and_plaintext() {
        for url in [
            "http://host.test",
            "https://user:secret@host.test",
            "https://host.test/?token=secret",
            "https://host.test/path",
            "https://HS_DOMAIN",
            "https://hs.example.org",
        ] {
            assert!(base_url(url).is_err(), "{url}");
        }
        assert!(base_url("https://100.64.0.5:9443").is_ok());
        assert!(base_url("https://[fd7a:115c:a1e0::5]:9443").is_ok());
    }
    #[test]
    fn successful_http_is_not_sufficient_application_health() {
        assert!(
            validate_health(
                Service::Control,
                br#"{"status":"ready","protocol_version":1}"#
            )
            .is_ok()
        );
        assert!(validate_health(Service::Control, b"<html>proxy page</html>").is_err());
        assert!(
            validate_health(
                Service::Control,
                br#"{"status":"ready","protocol_version":2}"#
            )
            .is_err()
        );
        assert!(validate_health(Service::Netadmin, br#"{"ok":false}"#).is_err());
        assert!(validate_health(Service::Bridge, br#"{"contract_revision":"old"}"#).is_err());
    }
    #[tokio::test]
    async fn private_endpoint_is_rejected_before_contacting_public_or_loopback_addresses() {
        let report = check(
            Service::Netadmin,
            base_url("https://127.0.0.1:1").unwrap(),
            &[],
            Duration::from_secs(1),
        )
        .await;
        assert!(!report.ok);
        assert_eq!(report.http_status, None);
        assert!(report.error.unwrap().contains("outside the overlay"));
    }
}
