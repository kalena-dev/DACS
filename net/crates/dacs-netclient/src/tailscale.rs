//! Thin wrapper over the local `tailscale` CLI and parsers for its output.
//! Verified against Tailscale 1.104 (`status --json`, `debug prefs`, `ping`).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dacs_net_contract::PeerPath;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("tailscale CLI not found (set DACS_TAILSCALE_BIN or install the Tailscale client)")]
    NotInstalled,
    #[error("running tailscale {args}: {source}")]
    Spawn {
        args: String,
        source: std::io::Error,
    },
    #[error("tailscale {args} timed out after {secs}s")]
    Timeout { args: String, secs: u64 },
    #[error("tailscale {args} failed (exit {code:?}): {stderr}")]
    Failed {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("unexpected tailscale output for {args}: {detail}")]
    Parse { args: String, detail: String },
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug)]
pub struct Tailscale {
    bin: PathBuf,
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

impl Tailscale {
    /// Find the CLI: `DACS_TAILSCALE_BIN`, then `PATH`, then platform defaults.
    pub fn locate() -> Result<Self, CliError> {
        if let Some(p) = std::env::var_os("DACS_TAILSCALE_BIN") {
            let p = PathBuf::from(p);
            return if p.is_file() {
                Ok(Self { bin: p })
            } else {
                Err(CliError::NotInstalled)
            };
        }
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join(exe("tailscale"));
                if candidate.is_file() {
                    return Ok(Self { bin: candidate });
                }
            }
        }
        let defaults: &[&str] = if cfg!(windows) {
            &[r"C:\Program Files\Tailscale\tailscale.exe"]
        } else if cfg!(target_os = "macos") {
            &[
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                "/usr/local/bin/tailscale",
                "/opt/homebrew/bin/tailscale",
            ]
        } else {
            &["/usr/bin/tailscale", "/usr/local/bin/tailscale"]
        };
        defaults
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .map(|p| Self { bin: p.to_owned() })
            .ok_or(CliError::NotInstalled)
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub async fn run(&self, args: &[&str], timeout: Duration) -> Result<Output, CliError> {
        let label = args.join(" ");
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let fut = cmd.output();
        let out = match tokio::time::timeout(timeout, fut).await {
            Err(_) => {
                return Err(CliError::Timeout {
                    args: label,
                    secs: timeout.as_secs(),
                });
            }
            Ok(Err(source)) => {
                return Err(CliError::Spawn {
                    args: label,
                    source,
                });
            }
            Ok(Ok(o)) => o,
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if !out.status.success() {
            return Err(CliError::Failed {
                args: label,
                code: out.status.code(),
                stderr: first_lines(&stderr, &stdout),
            });
        }
        Ok(Output { stdout, stderr })
    }
}

fn first_lines(stderr: &str, stdout: &str) -> String {
    let src = if stderr.is_empty() { stdout } else { stderr };
    src.lines().take(6).collect::<Vec<_>>().join(" | ")
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Status {
    #[serde(default)]
    pub backend_state: String,
    #[serde(rename = "Self")]
    pub self_: Option<PeerStatus>,
    #[serde(default)]
    pub current_tailnet: Option<Tailnet>,
    #[serde(default)]
    pub peer: Option<BTreeMap<String, PeerStatus>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Tailnet {
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PeerStatus {
    #[serde(default)]
    pub host_name: String,
    #[serde(rename = "DNSName", default)]
    pub dns_name: String,
    #[serde(default)]
    pub tailscale_ips: Option<Vec<IpAddr>>,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub cur_addr: String,
    #[serde(default)]
    pub relay: String,
    #[serde(default)]
    pub peer_relay: String,
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
}

impl PeerStatus {
    pub fn ips(&self) -> Vec<IpAddr> {
        self.tailscale_ips.clone().unwrap_or_default()
    }

    /// Current path from passive status. Idle peers may report only their
    /// home DERP region until traffic flows; use `ping` for an active check.
    pub fn path(&self) -> PeerPath {
        if !self.cur_addr.is_empty() {
            PeerPath::Direct {
                endpoint: self.cur_addr.clone(),
            }
        } else if !self.peer_relay.is_empty() {
            PeerPath::PeerRelay {
                relay: self.peer_relay.clone(),
            }
        } else if !self.relay.is_empty() {
            PeerPath::Derp {
                region: self.relay.clone(),
            }
        } else {
            PeerPath::Unreachable {
                detail: "no direct address or relay reported".into(),
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Prefs {
    #[serde(rename = "ControlURL", default)]
    pub control_url: String,
    #[serde(default)]
    pub logged_out: bool,
}

pub fn parse_status(json: &str) -> Result<Status, CliError> {
    serde_json::from_str(json).map_err(|e| CliError::Parse {
        args: "status --json".into(),
        detail: e.to_string(),
    })
}

pub fn parse_prefs(json: &str) -> Result<Prefs, CliError> {
    serde_json::from_str(json).map_err(|e| CliError::Parse {
        args: "debug prefs".into(),
        detail: e.to_string(),
    })
}

/// One `pong from NAME (IP) via VIA in LATENCY` line.
#[derive(Clone, Debug, PartialEq)]
pub struct Pong {
    pub node: String,
    pub path: PeerPath,
    pub rtt_ms: f64,
}

/// Parse `tailscale ping` output. Format (cmd/tailscale/cli/ping.go):
/// `pong from <name> (<ip>[, extra]) via <endpoint|DERP(code)|peer-relay(x)> in <dur>`.
pub fn parse_pongs(output: &str) -> Vec<Pong> {
    output.lines().filter_map(parse_pong).collect()
}

fn parse_pong(line: &str) -> Option<Pong> {
    let rest = line.trim().strip_prefix("pong from ")?;
    let (node, rest) = rest.split_once(" (")?;
    let (_, rest) = rest.split_once(") via ")?;
    let (via, dur) = rest.rsplit_once(" in ")?;
    let path = if let Some(region) = via.strip_prefix("DERP(").and_then(|v| v.strip_suffix(')')) {
        PeerPath::Derp {
            region: region.to_owned(),
        }
    } else if let Some(relay) = via
        .strip_prefix("peer-relay(")
        .and_then(|v| v.strip_suffix(')'))
    {
        PeerPath::PeerRelay {
            relay: relay.to_owned(),
        }
    } else {
        PeerPath::Direct {
            endpoint: via.to_owned(),
        }
    };
    Some(Pong {
        node: node.to_owned(),
        path,
        rtt_ms: parse_go_duration_ms(dur.trim())?,
    })
}

/// Parse Go `time.Duration` strings as printed for ping latency
/// (`850µs`, `12ms`, `1.2s`, `1m2.5s`).
fn parse_go_duration_ms(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = s;
    while !rest.is_empty() {
        let num_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let value: f64 = rest[..num_end].parse().ok()?;
        rest = &rest[num_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_end] {
            "ns" => 1e-6,
            "us" | "µs" | "μs" => 1e-3,
            "ms" => 1.0,
            "s" => 1e3,
            "m" => 60e3,
            "h" => 3600e3,
            _ => return None,
        };
        total += value * factor;
        rest = &rest[unit_end..];
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS_RUNNING: &str = r#"{
      "Version": "1.104.7",
      "BackendState": "Running",
      "Self": {"HostName": "alice-pc", "DNSName": "alice-pc.dacs.internal.", "TailscaleIPs": ["100.64.0.5", "fd7a:115c:a1e0::5"], "Online": true},
      "CurrentTailnet": {"Name": "dacs", "MagicDNSSuffix": "dacs.internal", "MagicDNSEnabled": true},
      "Peer": {
        "nodekey:aa": {"HostName": "bob-pc", "DNSName": "bob-pc.dacs.internal.", "TailscaleIPs": ["100.64.0.6"], "Online": true, "CurAddr": "192.168.1.20:41641", "Relay": "fra", "RxBytes": 1000, "TxBytes": 2000},
        "nodekey:bb": {"HostName": "carol-vm", "TailscaleIPs": ["100.64.0.7"], "Online": true, "CurAddr": "", "Relay": "dacs", "RxBytes": 5, "TxBytes": 6}
      }
    }"#;

    const STATUS_FRESH: &str = r#"{"Version":"1.104.7","BackendState":"NeedsLogin","Self":{"HostName":"pc","TailscaleIPs":null,"Online":false},"CurrentTailnet":null,"Peer":null}"#;

    #[test]
    fn parses_running_status_and_paths() {
        let s = parse_status(STATUS_RUNNING).unwrap();
        assert_eq!(s.backend_state, "Running");
        assert_eq!(s.current_tailnet.unwrap().name, "dacs");
        let peers = s.peer.unwrap();
        assert_eq!(
            peers["nodekey:aa"].path(),
            PeerPath::Direct {
                endpoint: "192.168.1.20:41641".into()
            }
        );
        assert_eq!(
            peers["nodekey:bb"].path(),
            PeerPath::Derp {
                region: "dacs".into()
            }
        );
    }

    #[test]
    fn parses_fresh_install_status() {
        let s = parse_status(STATUS_FRESH).unwrap();
        assert_eq!(s.backend_state, "NeedsLogin");
        assert!(s.self_.unwrap().ips().is_empty());
        assert!(s.current_tailnet.is_none());
    }

    #[test]
    fn parses_prefs() {
        let p = parse_prefs(r#"{"ControlURL":"https://hs.example.org","RouteAll":false,"LoggedOut":false,"Hostname":""}"#).unwrap();
        assert_eq!(p.control_url, "https://hs.example.org");
        assert!(!p.logged_out);
    }

    #[test]
    fn parses_ping_lines() {
        let out = "pong from bob-pc (100.64.0.6) via DERP(dacs) in 48ms\n\
                   pong from bob-pc (100.64.0.6) via 192.168.1.20:41641 in 850µs\n\
                   pong from carol-vm (100.64.0.7, 1234) via peer-relay(100.64.0.9:7777:vni:3) in 1.5s\n\
                   ping \"100.64.0.8\" timed out\n";
        let p = parse_pongs(out);
        assert_eq!(p.len(), 3);
        assert_eq!(
            p[0].path,
            PeerPath::Derp {
                region: "dacs".into()
            }
        );
        assert!((p[0].rtt_ms - 48.0).abs() < 1e-9);
        assert_eq!(
            p[1].path,
            PeerPath::Direct {
                endpoint: "192.168.1.20:41641".into()
            }
        );
        assert!((p[1].rtt_ms - 0.85).abs() < 1e-9);
        assert_eq!(
            p[2].path,
            PeerPath::PeerRelay {
                relay: "100.64.0.9:7777:vni:3".into()
            }
        );
        assert!((p[2].rtt_ms - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration_ms("1m2.5s"), Some(62_500.0));
        assert_eq!(parse_go_duration_ms("bogus"), None);
    }
}
