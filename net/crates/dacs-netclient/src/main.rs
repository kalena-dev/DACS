//! `dacs-net`: device-side network operations and evidence capture.
//! Every command prints JSON on stdout. No command prints the auth key.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dacs_net_contract::{CompilerEndpoint, EnrollmentGrant, HostConsent};
use dacs_netclient::compiler::{self, CompilerSide};
use dacs_netclient::traffic::{self, SnapshotFile};
use dacs_netclient::{JoinOptions, assess, detect, join, leave, ping, probe_tcp};
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
    /// Read-only DNS/TLS/application/bridge checks. No enrollment or tailnet changes.
    Preflight {
        #[arg(long)]
        config: PathBuf,
    },
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
    /// Passive per-peer path and transfer counters with the client's backend
    /// state. Exit 2 when the client is not Running (values are last-known).
    Peers,
    /// Active path probe to a peer (name or overlay IP).
    Ping {
        peer: String,
        #[arg(long, default_value_t = 5)]
        count: u32,
    },
    /// TCP reachability of one or more ADDR:PORT targets.
    Probe {
        #[arg(required = true, num_args = 1..)]
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
        /// This host's sccache role (client, scheduler or server): probe only
        /// the roles it must reach and require each over the overlay.
        #[arg(long)]
        from: Option<CompilerSide>,
    },
    /// Per-peer transfer between two `peers` snapshots (e.g. around a build).
    /// Flags counter resets, vanished peers and non-running snapshots (exit 2).
    Traffic {
        #[arg(long)]
        before: PathBuf,
        #[arg(long)]
        after: PathBuf,
    },
    /// Measured compiler-path availability and overlay transfer evidence.
    #[command(subcommand)]
    Measure(dacs_netclient::measure::MeasureCommand),
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
        Command::Measure(cmd) => return dacs_netclient::measure::run_cli(cmd).await,
        Command::Status => print(&detect().await),
        Command::Assess { login_server } => print(&assess(&login_server).await),
        Command::Preflight { config } => {
            let config: dacs_netclient::preflight::PreflightConfig = read_json(&config)?;
            let report = dacs_netclient::preflight::run(&config).await?;
            let ok = report.ok;
            print(&report);
            return Ok(ok);
        }
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
        Command::Peers => {
            let report = traffic::capture().await.map_err(|e| e.to_string())?;
            print(&report);
            return Ok(report.running);
        }
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
            from,
        } => {
            let endpoints: Vec<CompilerEndpoint> = read_json(&endpoints)?;
            let local_ips = match detect().await {
                dacs_net_contract::TailnetClientState::LoggedIn { overlay_ips, .. } => overlay_ips,
                _ => Vec::new(),
            };
            let timeout = Duration::from_millis(timeout_ms);
            let probe = &compiler::Live;
            if let Some(side) = from {
                let report =
                    compiler::check_side(side, &endpoints, &local_ips, probe, timeout).await?;
                print(&report);
                return Ok(report.ok);
            }
            let checks = compiler::check_endpoints(&endpoints, &local_ips, probe, timeout).await?;
            print(&checks);
            return Ok(!checks.is_empty() && checks.iter().all(|c| c.ok));
        }
        Command::Traffic { before, after } => {
            let before: SnapshotFile = read_json(&before)?;
            let after: SnapshotFile = read_json(&after)?;
            let report = traffic::diff(&before, &after);
            print(&report);
            return Ok(report.ok);
        }
    }
    Ok(true)
}
