use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use dacs_netadmin::audit::Audit;
use dacs_netadmin::config::Config;
use dacs_netadmin::headscale::{HeadscaleApi, HeadscaleClient};
use dacs_netadmin::http::{AppState, hash_token, router};
use dacs_netadmin::service::Service;
use rand::RngCore;

#[derive(Parser)]
#[command(
    name = "dacs-netadmin",
    about = "DACS network enrollment adapter (runs beside Headscale)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the operation API.
    Serve {
        #[arg(long, default_value = "/etc/dacs-netadmin/config.json")]
        config: PathBuf,
    },
    /// Check configuration, secrets and Headscale connectivity, then exit.
    Check {
        #[arg(long, default_value = "/etc/dacs-netadmin/config.json")]
        config: PathBuf,
    },
    /// Issue a grant locally on the infra host from an EnrollmentRequest JSON
    /// file (same consent/approval checks as the API). Used for the first node
    /// (the coordinator), before the overlay-only API is reachable. Writes the
    /// grant, which contains the single-use key, to `--out` (mode 600).
    Issue {
        #[arg(long, default_value = "/etc/dacs-netadmin/config.json")]
        config: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Generate an operation token for Person 1's coordinator. Prints the token
    /// once on stdout and its SHA-256 hash (for `operation_token_hashes_path`)
    /// on stderr.
    GenToken,
    /// Print the SHA-256 hash of a token read from stdin.
    HashToken,
}

fn client(cfg: &Config) -> anyhow_lite::Result<HeadscaleClient> {
    Ok(HeadscaleClient::new(
        &cfg.headscale_url,
        cfg.headscale_api_key()?,
    )?)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow_lite::Result<()> {
    match cli.command {
        Command::Issue {
            config,
            request,
            out,
        } => {
            let cfg = Config::load(&config)?;
            let audit = match &cfg.audit_log_path {
                Some(p) => Audit::open(p)?,
                None => Audit::disabled(),
            };
            let svc = Service::new(client(&cfg)?, cfg.settings(), audit);
            let req: dacs_net_contract::EnrollmentRequest =
                serde_json::from_str(&std::fs::read_to_string(&request)?)?;
            let grant = svc.enroll(&req, time::OffsetDateTime::now_utc()).await?;
            write_private(&out, &serde_json::to_vec_pretty(&grant)?)?;
            println!(
                "{}",
                serde_json::json!({
                    "device_id": grant.device_id,
                    "host_name": grant.host_name,
                    "auth_key_id": grant.auth_key_id,
                    "expires_at": grant.expires_at.unix_timestamp(),
                    "grant_file": out,
                })
            );
            Ok(())
        }
        Command::GenToken => {
            let mut bytes = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            let token = format!("dacs-op-{}", hex::encode(bytes));
            println!("{token}");
            eprintln!("{}", hex::encode(hash_token(&token)));
            Ok(())
        }
        Command::HashToken => {
            let mut token = String::new();
            std::io::stdin().read_line(&mut token)?;
            println!("{}", hex::encode(hash_token(token.trim())));
            Ok(())
        }
        Command::Check { config } => {
            let cfg = Config::load(&config)?;
            let hashes = cfg.token_hashes()?;
            let hs = client(&cfg)?;
            hs.health().await?;
            let nodes = hs.list_nodes().await?;
            println!(
                "{}",
                serde_json::json!({
                    "config": "ok",
                    "operation_tokens": hashes.len(),
                    "headscale": "ok",
                    "headscale_nodes": nodes.len(),
                    "tls": cfg.tls.is_some(),
                })
            );
            Ok(())
        }
        Command::Serve { config } => {
            let cfg = Config::load(&config)?;
            let audit = match &cfg.audit_log_path {
                Some(p) => Audit::open(p)?,
                None => Audit::disabled(),
            };
            let state = Arc::new(AppState {
                service: Service::new(client(&cfg)?, cfg.settings(), audit),
                token_hashes: cfg.token_hashes()?,
            });
            if let Err(e) = state.service.health().await {
                tracing::warn!(error = %e, "headscale not healthy at startup; serving anyway");
            }
            let app = router(state);
            tracing::info!(listen = %cfg.listen, tls = cfg.tls.is_some(), "dacs-netadmin serving");
            match &cfg.tls {
                Some(tls) => {
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    let rustls_cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                        &tls.cert_path,
                        &tls.key_path,
                    )
                    .await?;
                    axum_server::bind_rustls(cfg.listen, rustls_cfg)
                        .serve(app.into_make_service())
                        .await?;
                }
                None => {
                    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
                    axum::serve(listener, app).await?;
                }
            }
            Ok(())
        }
    }
}

/// Create `path` readable only by the owner (Unix); fail if it already exists.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Boxed-error result without pulling in another crate.
mod anyhow_lite {
    pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
}
