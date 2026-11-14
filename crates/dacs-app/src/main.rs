use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use dacs::{local, server};
use dacs_protocol::{Command, CommandEnvelope, PROTOCOL_VERSION};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "dacs",
    version,
    about = "Private distributed-agent coordinator and local setup client"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Run the coordinator. No worker or network membership starts automatically.
    Serve(Serve),
    /// Manually run or locally control the enrolled worker.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// Serve the private persistent task-board preview and compiled Rust frontend.
    TaskBoard {
        #[arg(long, default_value = "127.0.0.1:7448")]
        listen: SocketAddr,
        #[arg(long, default_value = ".dacs/task-board.redb")]
        data: PathBuf,
        #[arg(long)]
        assets: PathBuf,
    },
    /// Hash the installed Rust sysroot for an immutable distributed-build profile.
    BuildSysrootDigest {
        #[arg(long)]
        sysroot: PathBuf,
    },
    /// Send a typed Command or CommandEnvelope JSON document.
    Request {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        command_id: Option<String>,
    },
    /// Read authenticated coordinator state.
    Snapshot {
        #[command(flatten)]
        connection: Connection,
    },
    /// Open the shared authenticated UI in the native webview.
    Desktop {
        #[arg(long, default_value = "http://127.0.0.1:7447")]
        url: String,
    },
    /// Detect and install explicitly approved local development dependencies.
    Setup {
        #[command(subcommand)]
        command: Setup,
    },
    /// Verify, stage and apply signed application updates.
    Update {
        #[command(subcommand)]
        command: Update,
    },
    /// Transfer source through authenticated Git bundles.
    Source {
        #[command(subcommand)]
        command: Source,
    },
    /// Upload or download immutable project artifacts.
    Artifact {
        #[command(subcommand)]
        command: Artifact,
    },
    /// Generate a private development certificate; clients must explicitly trust it.
    Tls {
        #[arg(long, required = true)]
        hostname: Vec<String>,
        #[arg(long)]
        output_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorkerCommand {
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    Status {
        #[arg(long)]
        data_dir: PathBuf,
    },
    Control {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long, value_enum)]
        action: WorkerControl,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum WorkerControl {
    Drain,
    EmergencyCancel,
    PauseDesktop,
    ResumeDesktop,
}

#[derive(Args)]
struct Serve {
    #[arg(long, default_value = "127.0.0.1:7447")]
    listen: SocketAddr,
    /// Optional TLS-only enrollment listener, normally 0.0.0.0:7444.
    #[arg(long)]
    bootstrap_listen: Option<SocketAddr>,
    /// JSON NetworkConfig for the coordinator-to-netadmin bridge.
    #[arg(long)]
    network_config: Option<PathBuf>,
    #[arg(long)]
    public_url: Option<String>,
    #[arg(long, default_value = "DACS coordinator")]
    host_name: String,
    #[arg(long, default_value = ".dacs")]
    data_dir: PathBuf,
    /// Enable owner-authenticated setup controls only on a loopback listener.
    #[arg(long)]
    local_setup: bool,
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    #[arg(long)]
    update_key_file: Option<PathBuf>,
    /// Check this HTTPS release manifest before accepting requests.
    #[arg(long, requires = "update_key_file")]
    update_manifest_url: Option<String>,
}

#[derive(Args)]
struct Connection {
    #[arg(long, default_value = "http://127.0.0.1:7447", env = "DACS_URL")]
    url: String,
    #[arg(long, default_value = ".dacs/admin.token", env = "DACS_TOKEN_FILE")]
    token_file: PathBuf,
    /// Additional PEM trust anchor. Certificate verification is never disabled.
    #[arg(long)]
    ca: Option<PathBuf>,
}

impl Connection {
    fn client(&self) -> Result<reqwest::Client> {
        validate_url(&self.url)?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(180));
        if let Some(path) = &self.ca {
            builder =
                builder.add_root_certificate(reqwest::Certificate::from_pem(&fs::read(path)?)?);
        }
        Ok(builder.build()?)
    }
    fn token(&self) -> Result<String> {
        anyhow::ensure!(
            fs::metadata(&self.token_file)?.len() <= 4096,
            "Token file is too large"
        );
        let token = fs::read_to_string(&self.token_file)?.trim().to_owned();
        anyhow::ensure!(!token.is_empty(), "Token file is empty");
        Ok(token)
    }
    fn endpoint(&self, segments: &[&str]) -> Result<url::Url> {
        let mut url = validate_url(&self.url)?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid API URL"))?
            .clear()
            .extend(segments);
        Ok(url)
    }
    async fn json(
        &self,
        method: reqwest::Method,
        segments: &[&str],
        value: Option<&Value>,
    ) -> Result<Value> {
        let mut request = self
            .client()?
            .request(method, self.endpoint(segments)?)
            .bearer_auth(self.token()?);
        if let Some(value) = value {
            request = request.json(value);
        }
        response_json(request.send().await?).await
    }
}

#[derive(Subcommand)]
enum Setup {
    Catalog,
    Detect,
    Plan {
        #[arg(long)]
        request: PathBuf,
    },
    Apply {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        consent: PathBuf,
        #[arg(long, default_value = ".dacs/setup")]
        state_dir: PathBuf,
    },
    Scan {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        ai: bool,
    },
}
#[derive(Subcommand)]
enum Update {
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        public_key_file: PathBuf,
    },
    Stage {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        public_key_file: PathBuf,
        #[arg(long, default_value = ".dacs/updates")]
        state_dir: PathBuf,
    },
    Apply {
        #[arg(long, default_value = ".dacs/updates")]
        state_dir: PathBuf,
        #[arg(long)]
        target: PathBuf,
        #[arg(long, hide = true)]
        restart_args: Option<PathBuf>,
    },
    Rollback {
        #[arg(long, default_value = ".dacs/updates")]
        state_dir: PathBuf,
        #[arg(long)]
        target: PathBuf,
    },
}
#[derive(Subcommand)]
enum Source {
    Import {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        project: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        seed: bool,
    },
    Export {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        project: String,
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Subcommand)]
enum Artifact {
    Upload {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        project: String,
        #[arg(long)]
        file: PathBuf,
    },
    Download {
        #[command(flatten)]
        connection: Connection,
        #[arg(long)]
        project: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        output: PathBuf,
    },
}

fn validate_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value)?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Use HTTPS for non-loopback endpoints"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "Expected an origin URL without credentials, path, query or fragment"
    );
    Ok(url)
}
fn print(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn key(path: &Path) -> Result<String> {
    anyhow::ensure!(
        fs::metadata(path)?.len() <= 4096,
        "Release public key file is too large"
    );
    Ok(fs::read_to_string(path)?.trim().to_owned())
}
async fn response_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let body = bounded_body(response, 8 * 1024 * 1024).await?;
    let value: Value = serde_json::from_slice(&body)
        .with_context(|| format!("Server returned non-JSON response ({status})"))?;
    anyhow::ensure!(
        status.is_success(),
        "API {status}: {}",
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("request rejected")
    );
    Ok(value)
}
async fn bounded_body(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if let Some(size) = response.content_length() {
        anyhow::ensure!(size <= max as u64, "Response exceeds size limit");
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(max as u64) as usize);
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= max,
            "Response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn read_binary(path: &Path, max: u64) -> Result<Vec<u8>> {
    anyhow::ensure!(fs::metadata(path)?.len() <= max, "Input exceeds size limit");
    Ok(fs::read(path)?)
}
fn write_download(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("Choose an output path that does not exist")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn restart_coordinator_after_update(value: &Value) -> Result<bool> {
    if value.get("exit_required").and_then(Value::as_bool) != Some(true) {
        return Ok(false);
    }
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    local::launch_update_helper(value, Some(&arguments))
}

async fn startup_update(url: &str, public_key: &str, data_dir: &Path) -> Result<bool> {
    let url = url::Url::parse(url)?;
    anyhow::ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "Release metadata must use credential-free HTTPS"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    let manifest: Value = serde_json::from_slice(&bounded_body(response, 1024 * 1024).await?)?;
    let key = public_key.to_owned();
    let state = data_dir.join("updates");
    let result = tokio::task::spawn_blocking(move || -> Result<Option<Value>> {
        let verified =
            dacs_setup::verify_update(manifest.clone(), &key, env!("CARGO_PKG_VERSION"))?;
        if verified.get("update_available").and_then(Value::as_bool) == Some(false) {
            return Ok(None);
        }
        local::private_directory(&state)?;
        dacs_setup::stage_update(manifest, &key, env!("CARGO_PKG_VERSION"), &state)?;
        Ok(Some(dacs_setup::apply_staged_update(
            &state,
            &std::env::current_exe()?,
        )?))
    })
    .await??;
    match result {
        Some(value) => restart_coordinator_after_update(&value),
        None => Ok(false),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dacs=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Action::Serve(args) => {
            let update_state = args.data_dir.join("updates");
            let target = std::env::current_exe()?;
            let recovery = tokio::task::spawn_blocking(move || {
                dacs_setup::reconcile_update(&update_state, &target)
            })
            .await??;
            if restart_coordinator_after_update(&recovery)? {
                return Ok(());
            }
            let public_key = args.update_key_file.as_deref().map(key).transpose()?;
            if let Some(url) = &args.update_manifest_url {
                if startup_update(
                    url,
                    public_key.as_deref().expect("required release key"),
                    &args.data_dir,
                )
                .await?
                {
                    return Ok(());
                }
            }
            let public_url = args.public_url.unwrap_or_else(|| {
                format!(
                    "{}://{}",
                    if args.tls_cert.is_some() {
                        "https"
                    } else {
                        "http"
                    },
                    args.listen
                )
            });
            let update = server::run(server::Config {
                listen: args.listen,
                bootstrap_listen: args.bootstrap_listen,
                network_config: args.network_config,
                public_url,
                host_name: args.host_name,
                data_dir: args.data_dir,
                local_setup: args.local_setup,
                tls_cert: args.tls_cert,
                tls_key: args.tls_key,
                update_public_key: public_key,
            })
            .await?;
            if let Some(value) = update {
                restart_coordinator_after_update(&value)?;
            }
        }
        Action::Worker { command } => match command {
            WorkerCommand::Run { config } => {
                let config = serde_json::from_value::<dacs_worker::runtime::WorkerConfig>(
                    local::read_json(&config)?,
                )?;
                dacs_worker::runtime::run(config).await?;
            }
            WorkerCommand::Status { data_dir } => {
                print(&serde_json::to_value(dacs_worker::local_control::status(
                    &data_dir,
                )?)?)?;
            }
            WorkerCommand::Control { data_dir, action } => {
                use dacs_worker::local_control::ControlAction;
                let action = match action {
                    WorkerControl::Drain => ControlAction::Drain,
                    WorkerControl::EmergencyCancel => ControlAction::EmergencyCancel,
                    WorkerControl::PauseDesktop => ControlAction::PauseDesktop,
                    WorkerControl::ResumeDesktop => ControlAction::ResumeDesktop,
                };
                dacs_worker::local_control::request(&data_dir, action)?;
                print(
                    &json!({"requested":true,"note":"Read worker status for observed application; a queued request is not completion."}),
                )?;
            }
        },
        Action::TaskBoard {
            listen,
            data,
            assets,
        } => {
            dacs::task_board::serve(listen, data, assets).await?;
        }
        Action::BuildSysrootDigest { sysroot } => {
            let digest = tokio::task::spawn_blocking(move || {
                dacs_worker::build_runner::sysroot_digest(&sysroot)
            })
            .await??;
            print(&json!({"sysroot_sha256":digest}))?;
        }
        Action::Request {
            connection,
            file,
            command_id,
        } => {
            let value = local::read_json(&file)?;
            let mut envelope = if value.get("protocol_version").is_some() {
                serde_json::from_value::<CommandEnvelope>(value)?
            } else {
                CommandEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    command_id: uuid::Uuid::new_v4().to_string(),
                    command: serde_json::from_value::<Command>(value)?,
                }
            };
            if let Some(id) = command_id {
                envelope.command_id = id;
            }
            print(
                &connection
                    .json(
                        reqwest::Method::POST,
                        &["api", "command"],
                        Some(&serde_json::to_value(envelope)?),
                    )
                    .await?,
            )?;
        }
        Action::Snapshot { connection } => print(
            &connection
                .json(reqwest::Method::GET, &["api", "state"], None)
                .await?,
        )?,
        Action::Desktop { url } => {
            validate_url(&url)?;
            #[cfg(feature = "native")]
            dacs_ui::native::launch(&url)?;
            #[cfg(not(feature = "native"))]
            anyhow::bail!(
                "This binary was built without the native shell; build dacs with --features native or open the dashboard URL in a browser"
            );
        }
        Action::Setup { command } => {
            let value = tokio::task::spawn_blocking(move || -> Result<Value> {
                Ok(match command {
                    Setup::Catalog => dacs_setup::catalog(),
                    Setup::Detect => dacs_setup::detect()?,
                    Setup::Plan { request } => dacs_setup::plan(local::read_json(&request)?)?,
                    Setup::Apply {
                        plan,
                        consent,
                        state_dir,
                    } => {
                        local::private_directory(&state_dir)?;
                        dacs_setup::apply(
                            local::read_json(&plan)?,
                            local::read_json(&consent)?,
                            &state_dir,
                        )?
                    }
                    Setup::Scan { repo, ai } => dacs_setup::scan_project(&repo, ai)?,
                })
            })
            .await??;
            print(&value)?;
        }
        Action::Update { command } => {
            let (value, restart) = tokio::task::spawn_blocking(move || -> Result<_> {
                let mut restart = None;
                let value = match command {
                    Update::Verify {
                        manifest,
                        public_key_file,
                    } => dacs_setup::verify_update(
                        local::read_json(&manifest)?,
                        &key(&public_key_file)?,
                        env!("CARGO_PKG_VERSION"),
                    )?,
                    Update::Stage {
                        manifest,
                        public_key_file,
                        state_dir,
                    } => {
                        local::private_directory(&state_dir)?;
                        dacs_setup::stage_update(
                            local::read_json(&manifest)?,
                            &key(&public_key_file)?,
                            env!("CARGO_PKG_VERSION"),
                            &state_dir,
                        )?
                    }
                    Update::Apply {
                        state_dir,
                        target,
                        restart_args,
                    } => {
                        let value = dacs_setup::apply_staged_update(&state_dir, &target)?;
                        if let Some(path) = restart_args {
                            let arguments: Vec<String> =
                                serde_json::from_value(local::read_json(&path)?)?;
                            fs::remove_file(path)?;
                            restart = Some((target, arguments));
                        }
                        value
                    }
                    Update::Rollback { state_dir, target } => {
                        dacs_setup::rollback_update(&state_dir, &target)?
                    }
                };
                Ok((value, restart))
            })
            .await??;
            print(&value)?;
            if !local::launch_update_helper(
                &value,
                restart.as_ref().map(|(_, args)| args.as_slice()),
            )? {
                if let Some((target, arguments)) = restart {
                    std::process::Command::new(target).args(arguments).spawn()?;
                }
            }
        }
        Action::Source { command } => match command {
            Source::Import {
                connection,
                project,
                file,
                seed,
            } => {
                let mut endpoint = connection.endpoint(&["api", "projects", &project, "source"])?;
                endpoint
                    .query_pairs_mut()
                    .append_pair("seed", if seed { "true" } else { "false" });
                let response = connection
                    .client()?
                    .post(endpoint)
                    .bearer_auth(connection.token()?)
                    .header("content-type", "application/octet-stream")
                    .body(read_binary(&file, 128 * 1024 * 1024)?)
                    .send()
                    .await?;
                print(&response_json(response).await?)?;
            }
            Source::Export {
                connection,
                project,
                output,
            } => {
                let response = connection
                    .client()?
                    .get(connection.endpoint(&["api", "projects", &project, "source"])?)
                    .bearer_auth(connection.token()?)
                    .send()
                    .await?
                    .error_for_status()?;
                let bytes = bounded_body(response, 128 * 1024 * 1024).await?;
                write_download(&output, &bytes)?;
                print(&json!({"path":output,"bytes":bytes.len()}))?;
            }
        },
        Action::Artifact { command } => match command {
            Artifact::Upload {
                connection,
                project,
                file,
            } => {
                let bytes = read_binary(&file, 64 * 1024 * 1024)?;
                let digest = hex::encode(Sha256::digest(&bytes));
                let response = connection
                    .client()?
                    .post(connection.endpoint(&[
                        "api",
                        "projects",
                        &project,
                        "artifacts",
                        &digest,
                    ])?)
                    .bearer_auth(connection.token()?)
                    .header("content-type", "application/octet-stream")
                    .body(bytes)
                    .send()
                    .await?;
                print(&response_json(response).await?)?;
            }
            Artifact::Download {
                connection,
                project,
                digest,
                output,
            } => {
                let response = connection
                    .client()?
                    .get(connection.endpoint(&[
                        "api",
                        "projects",
                        &project,
                        "artifacts",
                        &digest,
                    ])?)
                    .bearer_auth(connection.token()?)
                    .send()
                    .await?
                    .error_for_status()?;
                let bytes = bounded_body(response, 64 * 1024 * 1024).await?;
                anyhow::ensure!(
                    hex::encode(Sha256::digest(&bytes)) == digest.trim_start_matches("sha256:"),
                    "Artifact digest mismatch"
                );
                write_download(&output, &bytes)?;
                print(&json!({"path":output,"bytes":bytes.len(),"sha256":digest}))?;
            }
        },
        Action::Tls {
            hostname,
            output_dir,
        } => {
            local::private_directory(&output_dir)?;
            let certified = rcgen::generate_simple_self_signed(hostname)?;
            write_download(
                &output_dir.join("server.pem"),
                certified.cert.pem().as_bytes(),
            )?;
            write_download(
                &output_dir.join("server.key"),
                certified.key_pair.serialize_pem().as_bytes(),
            )?;
            print(
                &json!({"certificate":output_dir.join("server.pem"),"private_key":output_dir.join("server.key"),"certificate_sha256":hex::encode(Sha256::digest(certified.cert.der()))}),
            )?;
        }
    }
    Ok(())
}
