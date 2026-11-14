//! Netadmin configuration (JSON). Secrets live in separate files referenced by
//! path; the config file itself holds none.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use dacs_net_contract::{CoordinatorEndpoints, Secret};
use serde::Deserialize;

use crate::service::{CompilerPorts, Settings};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the operation API listens on.
    pub listen: SocketAddr,
    /// TLS certificate/key (PEM). Required unless `listen` is loopback and
    /// `allow_plaintext_loopback` is set (local testing only).
    pub tls: Option<TlsFiles>,
    #[serde(default)]
    pub allow_plaintext_loopback: bool,
    /// Headscale listen URL on this host, e.g. `http://127.0.0.1:8080`.
    pub headscale_url: String,
    /// File holding the Headscale API key (`headscale apikeys create`).
    pub headscale_api_key_path: PathBuf,
    /// File with one hex SHA-256 hash per line of accepted operation tokens.
    pub operation_token_hashes_path: PathBuf,
    /// Public Headscale `server_url` given to enrolling devices.
    pub login_server: String,
    pub coordinator: CoordinatorEndpoints,
    #[serde(default = "default_ttl")]
    pub key_ttl_minutes: i64,
    pub audit_log_path: Option<PathBuf>,
    #[serde(default)]
    pub compiler_ports: Ports,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    pub scheduler: u16,
    pub server: u16,
}

impl Default for Ports {
    // sccache-dist defaults.
    fn default() -> Self {
        Self {
            scheduler: 10600,
            server: 10501,
        }
    }
}

fn default_ttl() -> i64 {
    15
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })
}

fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Refuse secret files readable by group/other on Unix.
fn check_private(path: &Path) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::Invalid(format!(
                "{} must not be group/world accessible (chmod 600)",
                path.display()
            )));
        }
    }
    let _ = path;
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let cfg: Config = serde_json::from_str(&read(path)?)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.tls.is_none() && !(self.allow_plaintext_loopback && self.listen.ip().is_loopback())
        {
            return Err(ConfigError::Invalid(
                "tls is required; plaintext is only allowed on a loopback listen address with allow_plaintext_loopback".into(),
            ));
        }
        if !(self.headscale_url.starts_with("https://") || is_loopback_url(&self.headscale_url)) {
            return Err(ConfigError::Invalid(
                "headscale_url must be https:// or a loopback http:// address".into(),
            ));
        }
        if !self.login_server.starts_with("https://") {
            return Err(ConfigError::Invalid(
                "login_server must be an https:// URL".into(),
            ));
        }
        if !self.coordinator.bootstrap_url.starts_with("https://")
            || !self.coordinator.overlay_url.starts_with("https://")
        {
            return Err(ConfigError::Invalid(
                "coordinator URLs must be https:// (application TLS stays on)".into(),
            ));
        }
        if !(1..=24 * 60).contains(&self.key_ttl_minutes) {
            return Err(ConfigError::Invalid(
                "key_ttl_minutes must be 1..=1440".into(),
            ));
        }
        Ok(())
    }

    pub fn settings(&self) -> Settings {
        Settings {
            login_server: self.login_server.trim_end_matches('/').to_owned(),
            coordinator: self.coordinator.clone(),
            key_ttl: time::Duration::minutes(self.key_ttl_minutes),
            compiler_ports: CompilerPorts {
                scheduler: self.compiler_ports.scheduler,
                server: self.compiler_ports.server,
            },
        }
    }

    pub fn headscale_api_key(&self) -> Result<Secret, ConfigError> {
        check_private(&self.headscale_api_key_path)?;
        let key = read(&self.headscale_api_key_path)?.trim().to_owned();
        if key.is_empty() {
            return Err(ConfigError::Invalid(
                "headscale API key file is empty".into(),
            ));
        }
        Ok(Secret::new(key))
    }

    pub fn token_hashes(&self) -> Result<Vec<[u8; 32]>, ConfigError> {
        check_private(&self.operation_token_hashes_path)?;
        let mut out = Vec::new();
        for line in read(&self.operation_token_hashes_path)?.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut h = [0u8; 32];
            hex::decode_to_slice(line, &mut h).map_err(|_| {
                ConfigError::Invalid("operation token hash must be 64 hex characters".into())
            })?;
            out.push(h);
        }
        if out.is_empty() {
            return Err(ConfigError::Invalid(
                "no operation token hashes configured".into(),
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "listen": "0.0.0.0:9443",
            "tls": {"cert_path": "/etc/x/cert.pem", "key_path": "/etc/x/key.pem"},
            "headscale_url": "http://127.0.0.1:8080",
            "headscale_api_key_path": "/etc/x/key",
            "operation_token_hashes_path": "/etc/x/tokens",
            "login_server": "https://hs.example.org",
            "coordinator": {"bootstrap_url": "https://bootstrap.example.org", "overlay_url": "https://coord.dacs.internal:7443"}
        })
    }

    fn parse(v: serde_json::Value) -> Result<Config, ConfigError> {
        let c: Config = serde_json::from_value(v)?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn accepts_valid_config() {
        let c = parse(base()).unwrap();
        assert_eq!(c.key_ttl_minutes, 15);
        assert_eq!(c.compiler_ports.scheduler, 10600);
    }

    #[test]
    fn refuses_public_plaintext() {
        let mut v = base();
        v["tls"] = serde_json::Value::Null;
        v["allow_plaintext_loopback"] = true.into();
        assert!(parse(v.clone()).is_err());
        v["listen"] = "127.0.0.1:9443".into();
        assert!(parse(v).is_ok());
    }

    #[test]
    fn refuses_remote_plaintext_headscale() {
        let mut v = base();
        v["headscale_url"] = "http://10.0.0.5:8080".into();
        assert!(parse(v).is_err());
        let mut v = base();
        v["headscale_url"] = "http://localhost:8080".into();
        assert!(parse(v).is_ok());
    }
}
