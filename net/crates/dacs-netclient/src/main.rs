//! `dacs-net`: device-side network operations and evidence capture.
//! Every command prints JSON on stdout. No command prints the auth key.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dacs_net_contract::{CompilerEndpoint, EnrollmentGrant, HostConsent, PeerSnapshot};
use dacs_netclient::{JoinOptions, assess, detect, join, leave, peers, ping, probe_tcp};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "dacs-net",
    about = "DACS network client: consented Headscale join, paths and private reachability"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Local Tailscale client state.
    Status,
    /// Check whether joining LOGIN_SERVER needs tailnet-switch consent. No changes.
    Assess { login_server: String },
    /// Join with a device grant after owner consent (both JSON files).
    Join {
        #[arg(long)]
        grant: PathBuf,
        #[arg(long)]
        consent: PathBuf,
        /// Windows: keep the tunnel up after the GUI user logs out.
        #[arg(long)]
        unattended: bool,
        /// Do not accept MagicDNS configuration.
        #[arg(long)]
        no_dns: bool,
    },
    /// Log out of the DACS profile only.
    Leave { login_server: String },
    /// Passive per-peer path and transfer counters.
    Peers,
    /// Active path probe to a peer (name or overlay IP).
    Ping {
        peer: String,
        #[arg(long, default_value_t = 5)]
        count: u32,
    },
    /// TCP reachability of one or more ADDR:PORT targets.
    Probe {
        targets: Vec<SocketAddr>,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
    /// Verify compiler endpoints (JSON list from netadmin /v1/compiler-endpoints):
    /// overlay-only addresses, TCP reachability and the path in use.
    CompilerCheck {
        #[arg(long)]
        endpoints: PathBuf,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
    /// Per-peer transfer between two `peers` snapshots (e.g. around a build).
    Traffic {
        #[arg(long)]
        before: PathBuf,
        #[arg(long)]
        after: PathBuf,
    },
}

fn read_json<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn print<T: Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable")
    );
}

#[derive(Serialize)]
struct CompilerCheck {
    endpoint: CompilerEndpoint,
    tcp: dacs_net_contract::TcpProbe,
    path: Option<dacs_net_contract::PathProbe>,
    ok: bool,
}

#[derive(Serialize)]
struct Traffic {
    host_name: String,
    path_after: dacs_net_contract::PeerPath,
    rx_bytes: u64,
    tx_bytes: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Returns Ok(false) when the command ran but a check failed.
async fn run(cli: Cli) -> Result<bool, String> {
    match cli.command {
        Command::Status => print(&detect().await),
        Command::Assess { login_server } => print(&assess(&login_server).await),
        Command::Join {
            grant,
            consent,
            unattended,
            no_dns,
        } => {
            let grant: EnrollmentGrant = read_json(&grant)?;
            let consent: HostConsent = read_json(&consent)?;
            let opts = JoinOptions {
                unattended,
                accept_dns: !no_dns,
                timeout: None,
            };
            print(
                &join(&grant, &consent, &opts)
                    .await
                    .map_err(|e| e.to_string())?,
            );
        }
        Command::Leave { login_server } => {
            leave(&login_server).await.map_err(|e| e.to_string())?;
            print(&serde_json::json!({ "left": login_server }));
        }
        Command::Peers => print(&peers().await.map_err(|e| e.to_string())?),
        Command::Ping { peer, count } => {
            let probe = ping(&peer, count).await.map_err(|e| e.to_string())?;
            let ok = !matches!(probe.path, dacs_net_contract::PeerPath::Unreachable { .. });
            print(&probe);
            return Ok(ok);
        }
        Command::Probe {
            targets,
            timeout_ms,
        } => {
            let mut all = true;
            let mut out = Vec::new();
            for t in targets {
                let p = probe_tcp(t, Duration::from_millis(timeout_ms)).await;
                all &= p.reachable;
                out.push(p);
            }
            print(&out);
            return Ok(all);
        }
        Command::CompilerCheck {
            endpoints,
            timeout_ms,
        } => {
            let endpoints: Vec<CompilerEndpoint> = read_json(&endpoints)?;
            let mut all = !endpoints.is_empty();
            let mut out = Vec::new();
            for ep in endpoints {
                let tcp = probe_tcp(ep.socket_addr(), Duration::from_millis(timeout_ms)).await;
                let path = ping(&ep.overlay_ip.to_string(), 3).await.ok();
                let ok = tcp.overlay_private && tcp.reachable;
                all &= ok;
                out.push(CompilerCheck {
                    endpoint: ep,
                    tcp,
                    path,
                    ok,
                });
            }
            print(&out);
            return Ok(all);
        }
        Command::Traffic { before, after } => {
            let before: Vec<PeerSnapshot> = read_json(&before)?;
            let after: Vec<PeerSnapshot> = read_json(&after)?;
            let out: Vec<Traffic> = after
                .into_iter()
                .map(|a| {
                    let b = before.iter().find(|b| b.host_name == a.host_name);
                    Traffic {
                        rx_bytes: a.rx_bytes.saturating_sub(b.map_or(0, |b| b.rx_bytes)),
                        tx_bytes: a.tx_bytes.saturating_sub(b.map_or(0, |b| b.tx_bytes)),
                        host_name: a.host_name,
                        path_after: a.path,
                    }
                })
                .collect();
            print(&out);
        }
    }
    Ok(true)
}
