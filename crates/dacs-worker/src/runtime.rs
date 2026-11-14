use crate::{
    Journal, ManagedChild, OmpEvent, OmpRpc, PendingCommand, PendingResult, Supervisor,
    local_control::{self, LocalStatus, OwnerControls},
    workspace,
};
use anyhow::{Context, Result, bail};
use dacs_protocol::*;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, watch},
};

fn poll_default() -> u64 {
    2000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub coordinator_url: String,
    pub credential_file: PathBuf,
    pub worker_id: String,
    pub registration: WorkerRegistration,
    pub omp_executable: PathBuf,
    pub omp_version: String,
    #[serde(default)]
    pub omp_profile: Option<String>,
    pub data_dir: PathBuf,
    pub workspaces_dir: PathBuf,
    #[serde(default)]
    pub ca_certificate: Option<PathBuf>,
    #[serde(default = "poll_default")]
    pub poll_interval_ms: u64,
}

#[derive(Clone)]
struct Coordinator {
    http: reqwest::Client,
    base: String,
    token: Arc<String>,
    journal: Arc<Journal>,
    transport: Arc<Mutex<()>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Upload {
    path: PathBuf,
    endpoint: String,
    digest: String,
    artifact: bool,
}

impl Coordinator {
    fn new(config: &WorkerConfig, journal: Arc<Journal>) -> Result<Self> {
        let url = reqwest::Url::parse(&config.coordinator_url)?;
        let loopback = url.host_str().is_some_and(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        anyhow::ensure!(
            url.scheme() == "https" || url.scheme() == "http" && loopback,
            "coordinator must use HTTPS (HTTP is only allowed on loopback)"
        );
        anyhow::ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "coordinator URL cannot embed credentials, query, or fragment"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                std::fs::metadata(&config.credential_file)?
                    .permissions()
                    .mode()
                    & 0o077
                    == 0,
                "device credential file must be owner-only (0600)"
            );
        }
        let token = std::fs::read_to_string(&config.credential_file)?
            .trim()
            .to_owned();
        anyhow::ensure!(
            !token.is_empty() && !token.contains(['\r', '\n']),
            "invalid device credential file"
        );
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20));
        if let Some(path) = &config.ca_certificate {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
        }
        Ok(Self {
            http: builder.build()?,
            base: config.coordinator_url.trim_end_matches('/').into(),
            token: Arc::new(token),
            journal,
            transport: Arc::new(Mutex::new(())),
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.token.as_str())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    fn enqueue(
        &self,
        id: &str,
        task: &str,
        attempt: &str,
        generation: u64,
        command: Command,
    ) -> Result<()> {
        self.journal.append_result(&PendingResult {
            result_id: id.into(),
            task_id: task.into(),
            attempt_id: attempt.into(),
            generation,
            sequence: 0,
            payload: serde_json::to_value(CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                command_id: id.into(),
                command,
            })?,
        })?;
        Ok(())
    }

    fn upload(
        &self,
        id: &str,
        task: &TaskRecord,
        attempt: &AttemptRecord,
        path: &Path,
        artifact: bool,
    ) -> Result<String> {
        let bytes = std::fs::read(path)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        let endpoint = if artifact {
            format!("/api/projects/{}/artifacts/{digest}", task.spec.project_id)
        } else {
            format!("/api/projects/{}/source", task.spec.project_id)
        };
        self.journal.append_result(&PendingResult { result_id:id.into(),task_id:task.id.clone(),attempt_id:attempt.id.clone(),generation:attempt.generation,sequence:0,
            payload: json!({"upload":Upload { path:path.to_owned(), endpoint, digest:digest.clone(), artifact }}) })?;
        Ok(digest)
    }

    async fn flush(&self) -> Result<()> {
        let _transport = self.transport.lock().await;
        for item in self.journal.pending_results(256)? {
            let mut envelope = if item.payload.get("upload").is_none() {
                Some(serde_json::from_value::<CommandEnvelope>(
                    item.payload.clone(),
                )?)
            } else {
                None
            };
            if let Some(envelope) = &mut envelope {
                match &mut envelope.command {
                    Command::SubmitCandidate { .. } => {
                        let receipt = self
                            .journal
                            .receipt(&format!("{}-source", item.result_id))?
                            .context("candidate source upload is still pending")?;
                        if receipt["ok"] != true {
                            self.journal.complete_result(&item.result_id,&json!({"ok":false,"error":"candidate source upload failed","upload":receipt}))?;
                            continue;
                        }
                    }
                    Command::ReportEvent {
                        event: event @ WorkerEvent::WorkflowCompleted { .. },
                        ..
                    } => {
                        let dependencies: Vec<String> = self
                            .journal
                            .get_value(&format!("requires-workflow:{}", item.attempt_id))?
                            .context("workflow upload dependencies missing")?;
                        let mut failed = Vec::new();
                        for dependency in dependencies {
                            let receipt = self
                                .journal
                                .receipt(&dependency)?
                                .context("workflow artifact upload pending")?;
                            if receipt["ok"] != true {
                                failed.push(dependency);
                            }
                        }
                        if !failed.is_empty() {
                            *event = WorkerEvent::Failed {
                                message: format!(
                                    "Workflow output delivery failed: {}",
                                    failed.join(", ")
                                ),
                            };
                        }
                    }
                    _ => (),
                }
            }
            let response = if let Some(upload) = item.payload.get("upload") {
                let upload: Upload = serde_json::from_value(upload.clone())?;
                let bytes = tokio::fs::read(&upload.path).await?;
                anyhow::ensure!(
                    hex::encode(Sha256::digest(&bytes)) == upload.digest,
                    "durable upload changed after enqueue"
                );
                self.http
                    .post(format!("{}{}", self.base, upload.endpoint))
                    .bearer_auth(self.token.as_str())
                    .header("Content-Type", "application/octet-stream")
                    .body(bytes)
                    .send()
                    .await?
            } else {
                self.http
                    .post(format!("{}/api/command", self.base))
                    .bearer_auth(self.token.as_str())
                    .json(envelope.as_ref().context("missing outbound envelope")?)
                    .send()
                    .await?
            };
            let status = response.status();
            let body: Value = response
                .json()
                .await
                .context("coordinator response is not JSON")?;
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                bail!("coordinator temporarily refused delivery ({status})");
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.journal.set_value("coordinator_revoked", &true)?;
            }
            if status.is_success() {
                self.journal.complete_result(
                    &item.result_id,
                    &json!({"ok":true,"result":body.get("result").unwrap_or(&body)}),
                )?;
            } else {
                if matches!(
                    envelope.as_ref().map(|e| &e.command),
                    Some(Command::ReportEvent {
                        event: WorkerEvent::Stopped,
                        ..
                    })
                ) {
                    // Contiguous event sequencing may already be fenced by an
                    // earlier refusal. Reconcile only because Stopped was queued
                    // after verified process-tree termination, never on RPC abort.
                    let reconciliation = CommandEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        command_id: format!("{}-reconcile", item.result_id),
                        command: Command::ReconcileWorker {
                            stopped_attempt_ids: vec![item.attempt_id.clone()],
                        },
                    };
                    let response = self
                        .http
                        .post(format!("{}/api/command", self.base))
                        .bearer_auth(self.token.as_str())
                        .json(&reconciliation)
                        .send()
                        .await?;
                    let status = response.status();
                    let outcome: Value = response.json().await?;
                    anyhow::ensure!(
                        status.is_success(),
                        "stop reconciliation remains pending: {outcome}"
                    );
                    self.journal.complete_result(
                        &item.result_id,
                        &json!({"ok":true,"reconciled":true,"result":outcome}),
                    )?;
                } else {
                    self.journal.complete_result(
                        &item.result_id,
                        &json!({"ok":false,"status":status.as_u16(),"error":body}),
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn command(&self, command: Command) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        self.enqueue(&id, "", "", 0, command)?;
        self.flush().await?;
        let receipt = self
            .journal
            .receipt(&id)?
            .context("command still pending")?;
        anyhow::ensure!(
            receipt["ok"] == true,
            "coordinator refused command: {}",
            receipt["error"]
        );
        Ok(receipt["result"].clone())
    }

    async fn source(&self, project: &str) -> Result<Vec<u8>> {
        let mut response = self
            .http
            .get(format!("{}/api/projects/{project}/source", self.base))
            .bearer_auth(self.token.as_str())
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 128 * 1024 * 1024,
                "source bundle exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

struct Running {
    stop: watch::Sender<bool>,
    join: tokio::task::JoinHandle<Result<()>>,
    desktop: bool,
    resources: Resources,
    capabilities: Vec<String>,
    /// The journaled assignment's spec: the attempt's bound source/contract/policy pins.
    pins: TaskSpec,
}

/// An attempt runs only under its assignment's pins; a coordinator record of the
/// same generation with other pins is treated as stale, never adopted.
fn same_pins(current: &TaskSpec, assigned: &TaskSpec) -> bool {
    current.project_id == assigned.project_id
        && current.source_commit == assigned.source_commit
        && current.contract_revision == assigned.contract_revision
        && current.policy_revision == assigned.policy_revision
}

/// Ordinary close requests: Ctrl-C, and on Unix also SIGTERM and SIGHUP (the
/// terminal window closing). Each requests a drain, never a kill.
async fn close_requests(journal: Arc<Journal>) {
    let drain = || {
        let mut controls: OwnerControls = journal
            .get_value("owner_controls")
            .ok()
            .flatten()
            .unwrap_or_default();
        controls.draining = true;
        let _ = journal.set_value("owner_controls", &controls);
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut interrupt), Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return;
        };
        loop {
            tokio::select! {
                Some(()) = interrupt.recv() => drain(),
                Some(()) = terminate.recv() => drain(),
                Some(()) = hangup.recv() => drain(),
                else => return,
            }
        }
    }
    #[cfg(not(unix))]
    while tokio::signal::ctrl_c().await.is_ok() {
        drain();
    }
}

/// Manually invoked; neither public installation nor invitation starts this loop.
/// Ctrl-C requests a drain. A second signal does not silently become a kill.
pub async fn run(mut config: WorkerConfig) -> Result<()> {
    workspace::safe_id(&config.worker_id)?;
    anyhow::ensure!(
        config.poll_interval_ms >= 100 && config.poll_interval_ms <= 30_000,
        "poll interval must be 100..30000 ms"
    );
    anyhow::ensure!(
        config.registration.owner_consented,
        "named-host owner consent is required"
    );
    anyhow::ensure!(
        config.registration.omp_version == config.omp_version,
        "registration and runtime OMP pins differ"
    );
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(&config.workspaces_dir)?;
    config.data_dir = std::fs::canonicalize(&config.data_dir)?;
    config.workspaces_dir = std::fs::canonicalize(&config.workspaces_dir)?;
    config.omp_executable =
        std::fs::canonicalize(&config.omp_executable).context("pinned OMP executable missing")?;
    // Never advertise capacity the physical host cannot back; refuse before
    // any lock, process launch or coordinator contact.
    crate::capacity::check_declared(
        &config.registration.capacity,
        &crate::capacity::observe(&config.workspaces_dir)?,
    )?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.data_dir.join("worker.lock"))?;
    lock.try_lock_exclusive()
        .context("another worker owns this data directory")?;
    let _host_lock = local_control::owner_lock(&format!(
        "worker-{}",
        hex::encode(Sha256::digest(
            config.registration.physical_host_id.as_bytes()
        ))
    ))?;
    let journal = Arc::new(Journal::open(config.data_dir.join("worker.sqlite"))?);
    let coordinator = Coordinator::new(&config, journal.clone())?;
    let supervisor = Arc::new(Supervisor::persistent(&config.data_dir.join("processes"))?);
    secure_directory(&supervisor, &config.data_dir).await?;
    supervisor.reconcile_previous().await?;
    let mut controls: OwnerControls = journal.get_value("owner_controls")?.unwrap_or_default();
    // Explicit manual start resets stop/drain, but never undoes a desktop pause.
    controls.draining = false;
    controls.emergency_cancel = false;
    journal.set_value("owner_controls", &controls)?;
    // Only locally requested desktop access can be enabled. Native observations
    // below may narrow it; a coordinator task cannot grant it.
    let desktop_requested = config
        .registration
        .capabilities
        .iter()
        .any(|cap| cap.starts_with("desktop"));
    let elevated = observe_elevation(&supervisor, &config.data_dir).await.ok();
    if elevated != Some(true) {
        config
            .registration
            .capabilities
            .retain(|cap| cap != "elevated" && cap != "administrator" && cap != "root");
    }
    let version = workspace::capture(
        &supervisor,
        None,
        &config.data_dir,
        config.omp_executable.clone(),
        vec!["--version".into()],
    )
    .await?;
    anyhow::ensure!(
        version.0 == 0,
        "`omp --version` failed with exit code {}",
        version.0
    );
    crate::omp::check_pinned_version(&String::from_utf8_lossy(&version.1), &config.omp_version)
        .map_err(anyhow::Error::msg)?;
    let mut status = LocalStatus { worker_id:config.worker_id.clone(), running:true, elevated,
        limitations:vec!["Native desktop capabilities are observed at launch, not cross-platform certification. Permission/lock/backend failures refuse access.".into(),
        "Tools inherit the owner token; DACS never silently elevates or replaces the owner profile. Restrictions are advisory, not containment.".into(),
        "An interrupted launch without a durable PID requires local reconciliation; external-effect attempts are never automatically replayed.".into()], ..LocalStatus::default() };
    let readiness = provider_probe(&config, &supervisor, desktop_requested, None).await;
    status.provider_authenticated = Some(readiness.is_ok());
    journal.set_value("provider_authenticated", &status.provider_authenticated)?;
    journal.set_value("coordinator_revoked", &false)?;
    match readiness {
        Ok(observation) => {
            status.desktop_supported = observation
                .as_ref()
                .is_some_and(local_control::desktop_available);
            journal.set_value("desktop_observation", &observation)?;
        }
        Err(error) => {
            status.last_error = Some(format!(
                "OMP provider readiness failed; complete manual login in the selected profile: {error:#}"
            ))
        }
    }
    if !status.desktop_supported {
        config
            .registration
            .capabilities
            .retain(|cap| !cap.starts_with("desktop"));
    }
    journal.set_value("local_status", &status)?;
    let mut running = BTreeMap::<String, Running>::new();
    let mut interval = tokio::time::interval(Duration::from_millis(config.poll_interval_ms));
    let signal = tokio::spawn(close_requests(journal.clone()));
    let mut last_snapshot: Option<Snapshot> = None;
    let mut was_draining = true; // Manual startup explicitly resumes the device.
    loop {
        interval.tick().await;
        controls = journal.get_value("owner_controls")?.unwrap_or_default();
        if controls.emergency_cancel
            || journal
                .get_value::<bool>("coordinator_revoked")?
                .unwrap_or(false)
        {
            for (id, runtime) in &running {
                let reason = if controls.emergency_cancel {
                    "emergency_cancel"
                } else {
                    "worker_revoked"
                };
                local_control::record_stop(&journal, id, reason)?;
                let _ = runtime.stop.send(true);
            }
        }
        if controls.desktop_paused {
            for (id, runtime) in running.iter().filter(|(_, r)| r.desktop) {
                local_control::record_stop(&journal, id, "desktop_paused")?;
                let _ = runtime.stop.send(true);
            }
        }
        let finished: Vec<_> = running
            .iter()
            .filter(|(_, r)| r.join.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            let runtime = running.remove(&id).expect("known running attempt");
            match runtime.join.await {
                Ok(Ok(())) => (),
                Ok(Err(error)) => {
                    status.last_error = Some(format!("attempt {id}: {error:#}"));
                }
                Err(error) => {
                    status.last_error = Some(format!("attempt {id} panicked: {error}"));
                    controls.draining = true;
                    journal.set_value("owner_controls", &controls)?;
                }
            }
        }
        status.observed_at = local_control::now();
        status.requested_controls = controls.clone();
        status.draining = controls.draining;
        let live_attempts = supervisor.active_attempts().await;
        status.provider_authenticated = journal
            .get_value::<Option<bool>>("provider_authenticated")?
            .flatten();
        status.emergency_cancel =
            controls.emergency_cancel && running.is_empty() && live_attempts.is_empty();
        let desktop_owner: Option<String> = journal.get_value("desktop_owner")?;
        status.desktop_active = running.values().any(|r| r.desktop)
            || desktop_owner.is_some_and(|id| live_attempts.contains(&id));
        status.desktop_paused = controls.desktop_paused && !status.desktop_active;
        // Permission reduction drains: report what is still exercised.
        let mut registration = config.registration.clone();
        registration
            .capabilities
            .retain(|cap| !local_control::withdrawn(&controls.withdrawn_capabilities, cap));
        status.effective_capabilities = registration.capabilities.clone();
        let exercised: Vec<_> = running
            .iter()
            .map(|(id, r)| (id.clone(), r.capabilities.clone()))
            .collect();
        (
            status.pending_permission_changes,
            status.active_broader_permissions,
        ) = local_control::permission_drain(&controls.withdrawn_capabilities, &exercised);
        let stop_requests: Vec<(String, String)> =
            journal.get_value("stop_requests")?.unwrap_or_default();
        let unreported = journal.pending_results(4096)?;
        status.stops = stop_requests
            .into_iter()
            .map(|(id, reason)| {
                let attempt = last_snapshot
                    .as_ref()
                    .and_then(|s| s.attempts.iter().find(|a| a.id == id));
                let task = attempt.and_then(|a| {
                    last_snapshot
                        .as_ref()
                        .and_then(|s| s.tasks.iter().find(|t| t.id == a.task_id))
                });
                let phase = local_control::stop_phase(
                    running.contains_key(&id) || live_attempts.contains(&id),
                    unreported.iter().any(|r| r.attempt_id == id),
                    attempt,
                    task,
                );
                local_control::StopStatus {
                    attempt_id: id,
                    reason,
                    phase,
                }
            })
            .collect();
        status.active_attempt_ids = running.keys().cloned().chain(live_attempts).collect();
        status.active_attempt_ids.sort();
        status.active_attempt_ids.dedup();
        journal.set_value("local_status", &status)?;
        let mut refused = Vec::new();
        let cycle: Result<()> = async {
            coordinator.flush().await?;
            // Narrowed by local withdrawals; a heartbeat never widens consent.
            coordinator.command(Command::Heartbeat { registration:registration.clone() }).await?;
            coordinator.command(Command::ReportRuntimeStatus { status:WorkerRuntimeStatus {
                worker_id:config.worker_id.clone(),provider_authenticated:status.provider_authenticated,network_ready:Some(true),
                desktop_session:status.desktop_supported.then(|| format!("owner:{}",config.registration.physical_host_id)),
                desktop_active:status.desktop_active,desktop_paused:controls.desktop_paused,observed_at:local_control::now(),
            }}).await?;
            if controls.draining != was_draining {
                coordinator.command(Command::SetDraining { worker_id:config.worker_id.clone(),draining:controls.draining }).await?;
                was_draining = controls.draining;
            }
            let after: i64 = journal.get_value("message_cursor")?.unwrap_or(0);
            let messages: Vec<ControlMessage> = coordinator.get(&format!("/api/commands?after={after}")).await?;
            let mut through = after;
            for message in messages {
                anyhow::ensure!(message.worker_id == config.worker_id, "message belongs to another device");
                let command = PendingCommand { command_id:message.id.clone(),task_id:message.payload.get("task").and_then(|t| t.get("id")).and_then(Value::as_str).unwrap_or("").into(),attempt_id:message.attempt_id.clone().unwrap_or_default(),generation:message.generation.unwrap_or(0),payload:serde_json::to_value(&message)? };
                match journal.receive(&command) {
                    Ok(_) => (),
                    Err(crate::JournalError::StaleGeneration) => { through = through.max(message.sequence); continue; },
                    // Same id, different body: never applied (the original stays bound),
                    // recorded, and it must not wedge later stops/fencing in this queue.
                    Err(crate::JournalError::DuplicateMismatch) => {
                        journal.set_value(&format!("refused-message:{}", message.id), &json!({"sequence":message.sequence,"reason":"message id already received with a different body"}))?;
                        refused.push(message.id.clone());
                        through = through.max(message.sequence);
                        continue;
                    },
                    Err(error) => return Err(error.into()),
                }
                if message.kind == "stop" {
                    if let Some(id) = &message.attempt_id {
                        journal.set_value(&format!("stop:{id}"), &true)?;
                        local_control::record_stop(&journal, id, "coordinator_stop")?;
                        if let Some(runtime) = running.get(id) { let _ = runtime.stop.send(true); }
                    }
                    journal.mark_command(&message.id, "done")?;
                } else if matches!(message.kind.as_str(), "pause_desktop" | "desktop_pause") {
                    // The coordinator may only narrow: pause. Nothing it sends
                    // resumes the desktop, clears a drain, or widens consent.
                    local_control::request(&config.data_dir, local_control::ControlAction::PauseDesktop)?;
                    journal.mark_command(&message.id, "done")?;
                }
                through = through.max(message.sequence);
            }
            if through > after {
                // All messages are durable before advancing the cursor/remote ACK.
                journal.set_value("message_cursor", &through)?;
            }
            if through > 0 { coordinator.command(Command::AcknowledgeMessages { through_sequence:through }).await?; }
            let snapshot: Snapshot = coordinator.get("/api/state").await?;
            last_snapshot = Some(snapshot.clone());
            let worker = snapshot.workers.iter().find(|w| w.id == config.worker_id).context("worker missing from coordinator snapshot")?;
            if worker.revoked { for runtime in running.values() { let _ = runtime.stop.send(true); } bail!("worker was revoked"); }
            for (id, runtime) in &running {
                let current = snapshot.attempts.iter().find(|a| a.id == *id);
                let stale = current.is_none_or(|a| {
                    a.worker_id != config.worker_id || matches!(a.state.as_str(), "fenced" | "abandoned" | "stopping" | "stopped") || snapshot.tasks.iter().find(|t| t.id == a.task_id).is_none_or(|t| t.generation != a.generation || t.attempt_id.as_deref() != Some(id) || t.cancel_requested || matches!(t.state,TaskState::Accepted|TaskState::Cancelled|TaskState::Failed) || !same_pins(&t.spec, &runtime.pins))
                });
                if stale {
                    // The attempt watchdog acts on local controls without waiting
                    // for this loop, so the snapshot may already show our own stop.
                    let local: OwnerControls = journal.get_value("owner_controls")?.unwrap_or_default();
                    let reason = if local.emergency_cancel { "emergency_cancel" } else if runtime.desktop && local.desktop_paused { "desktop_paused" } else if current.and_then(|a| snapshot.tasks.iter().find(|t| t.id == a.task_id)).is_some_and(|t| t.cancel_requested) { "coordinator_cancel" } else { "coordinator_fenced" };
                    local_control::record_stop(&journal, id, reason)?;
                    let _ = runtime.stop.send(true);
                }
            }
            for command in journal.pending_commands()? {
                let message: ControlMessage = serde_json::from_value(command.payload.clone())?;
                if message.kind != "assignment" || running.contains_key(&command.attempt_id) { continue; }
                let task: TaskRecord = serde_json::from_value(message.payload["task"].clone())?;
                let original: AttemptRecord = serde_json::from_value(message.payload["attempt"].clone())?;
                let Some(attempt) = snapshot.attempts.iter().find(|a| a.id == original.id) else { continue };
                let latest = snapshot.tasks.iter().find(|t| t.id == task.id);
                let stopped = journal.get_value::<bool>(&format!("stop:{}", attempt.id))?.unwrap_or(false);
                let valid = attempt.worker_id == config.worker_id && attempt.generation == original.generation && !matches!(attempt.state.as_str(),"fenced"|"abandoned"|"stopped"|"stopping") && latest.is_some_and(|t| t.generation == attempt.generation && t.attempt_id.as_deref() == Some(&attempt.id) && !t.cancel_requested && !matches!(t.state,TaskState::Accepted|TaskState::Failed|TaskState::Cancelled) && same_pins(&t.spec, &task.spec));
                if !valid || stopped || controls.emergency_cancel {
                    let reason = if controls.emergency_cancel { "emergency_cancel" } else if latest.is_some_and(|t| t.cancel_requested) { "coordinator_cancel" } else { "coordinator_stop" };
                    local_control::record_stop(&journal, &attempt.id, reason)?;
                    anyhow::ensure!(supervisor.terminate_attempt(&attempt.id, Duration::from_secs(10)).await?, "attempt process tree could not be stopped");
                    coordinator.command(Command::ReconcileWorker { stopped_attempt_ids:vec![attempt.id.clone()] }).await?;
                    journal.mark_command(&command.command_id,"done")?;
                    continue;
                }
                let phase: Option<String> = journal.get_value(&format!("phase:{}",attempt.id))?;
                if phase.as_deref() == Some("finished") { continue; }
                if phase.is_some() && task.spec.effect_class == EffectClass::External {
                    relay_compiler_telemetry(&coordinator,&task,attempt,&config.data_dir.join("attempts").join(&attempt.id))?;
                    // Interrupted, not Failed: a terminal failure would let the coordinator
                    // settle uncertain external effects. The attempt stays current and
                    // unfinished (stopped, reservation released) until explicitly abandoned.
                    journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Progress { message:"Interrupted: worker restarted during external-effect execution; effects are unreconciled and automatic replay is forbidden. Inspect effects, then explicitly abandon/reassign.".into() })?;
                    journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Stopped)?;
                    journal.set_value(&format!("phase:{}",attempt.id),&"finished")?;
                    journal.mark_command(&command.command_id,"done")?;
                    continue;
                }
                if let Some(capability) = task.spec.required_capabilities.iter().find(|cap| local_control::withdrawn(&controls.withdrawn_capabilities, cap)) {
                    // Permission reduction drains running work but never starts
                    // new use of the withdrawn grant. Refuse explicitly so the
                    // coordinator can release the reservation and reschedule.
                    journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Failed { message:format!("Owner withdrew local capability {capability} before this attempt started; not executed") })?;
                    journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Stopped)?;
                    journal.set_value(&format!("phase:{}",attempt.id),&"finished")?;
                    journal.mark_command(&command.command_id,"done")?;
                    continue;
                }
                // Defense in depth against over-admission: OMP, its tools and
                // build/compiler processes all run under this reservation.
                if !crate::capacity::fits(running.values().map(|r| &r.resources), &attempt.resources, &config.registration.capacity) {
                    tracing::warn!(attempt = %attempt.id, "assignment exceeds remaining declared capacity; held unstarted");
                    continue;
                }
                // Previously admitted work resumes even while draining. No local
                // task creation or reservation growth occurs during outages.
                let (stop, receiver) = watch::channel(false);
                let config = config.clone();
                let coordinator = coordinator.clone();
                let supervisor = supervisor.clone();
                let attempt = attempt.clone();
                let id = attempt.id.clone();
                let desktop = task.spec.required_capabilities.iter().any(|c| c.starts_with("desktop"));
                let resources = attempt.resources.clone();
                let capabilities = task.spec.required_capabilities.clone();
                let pins = task.spec.clone();
                let join = tokio::spawn(async move { execute_attempt(config,coordinator,supervisor,task,attempt,command.command_id,receiver).await });
                running.insert(id, Running { stop, join, desktop, resources, capabilities, pins });
            }
            Ok(())
        }.await;
        if !refused.is_empty() {
            status.last_error = Some(format!(
                "Refused control message(s) {} redelivered with a different body; the original receipt stays bound",
                refused.join(", ")
            ));
            journal.set_value("local_status", &status)?;
        }
        if let Err(error) = cycle {
            status.last_error = Some(format!(
                "Coordinator unavailable/refused reconciliation: {error:#}"
            ));
            journal.set_value("local_status", &status)?;
        }
        if controls.draining && running.is_empty() && supervisor.active_ids().await.is_empty() {
            // Results remain durable if the coordinator is unreachable on close.
            let _ = coordinator.flush().await;
            break;
        }
    }
    signal.abort();
    status.running = false;
    status.observed_at = local_control::now();
    journal.set_value("local_status", &status)?;
    drop(lock);
    Ok(())
}

/// The `--config` overlay of every managed OMP session: native async/task/
/// launch spawning paths off so scheduled children are admitted by DACS.
pub fn managed_overlay(desktop: bool) -> String {
    // Never override session.maxCost or owner provider/model configuration.
    format!(
        "async:\n  enabled: false\nlaunch:\n  enabled: false\n  persist: false\nbash:\n  autoBackground:\n    enabled: false\ntask:\n  maxRecursionDepth: 0\neval:\n  py: false\n  js: {desktop}\ncomputer:\n  enabled: {desktop}\ntools:\n  xdev: false\n  approval:\n    computer: allow\n    computer.control: allow\nupdate:\n  auto: false\n"
    )
}

/// Arguments of every managed OMP launch: headless JSONL RPC and an explicit
/// tool allowlist without OMP's native `task`/`wait` spawning tools.
pub fn managed_omp_args(
    overlay: &Path,
    session_dir: &Path,
    profile: Option<&str>,
    probe: bool,
    desktop: bool,
) -> Vec<String> {
    let mut args = vec![
        "--mode".into(),
        "rpc".into(),
        "--no-ui".into(),
        "--no-extensions".into(),
        "--no-pty".into(),
        "--config".into(),
        overlay.to_string_lossy().into_owned(),
        "--session-dir".into(),
        session_dir.to_string_lossy().into_owned(),
    ];
    if probe && !desktop {
        args.push("--no-tools".into());
    } else if probe {
        args.extend(["--tools".into(), "eval".into()]);
    } else {
        args.extend([
            "--tools".into(),
            if desktop {
                "read,write,edit,bash,grep,glob,web_search,eval"
            } else {
                "read,write,edit,bash,grep,glob,web_search"
            }
            .into(),
        ]);
    }
    if let Some(profile) = profile {
        args.extend(["--profile".into(), profile.to_owned()]);
    }
    args
}

async fn launch_omp(
    config: &WorkerConfig,
    supervisor: &Supervisor,
    attempt: Option<&str>,
    workspace: &Path,
    state: &Path,
    probe: bool,
    desktop: bool,
) -> Result<(
    String,
    OmpRpc,
    tokio::sync::mpsc::Receiver<OmpEvent>,
    tokio::task::JoinHandle<Result<()>>,
)> {
    std::fs::create_dir_all(state)?;
    let overlay = state.join("managed.yml");
    std::fs::write(&overlay, managed_overlay(desktop))?;
    let args = managed_omp_args(
        &overlay,
        &state.join("session"),
        config.omp_profile.as_deref(),
        probe,
        desktop,
    );
    let id = uuid::Uuid::new_v4().to_string();
    supervisor
        .launch(
            &id,
            &ManagedChild {
                name: "managed OMP".into(),
                program: config.omp_executable.clone(),
                args,
                cwd: workspace.into(),
                env: workspace::owner_environment(),
                effect_class: "external".into(),
                reservation_id: attempt.map(str::to_owned),
            },
        )
        .await?;
    let io = supervisor.take_io(&id).await?;
    let log = state.join(format!("omp-{id}.stderr"));
    let stderr = tokio::spawn(async move {
        let mut input = io.stderr;
        let mut file = tokio::fs::File::create(log).await?;
        let mut remaining = 4 * 1024 * 1024;
        let mut bytes = [0; 16384];
        loop {
            let size = input.read(&mut bytes).await?;
            if size == 0 {
                break;
            }
            let keep = remaining.min(size);
            if keep > 0 {
                file.write_all(&bytes[..keep]).await?;
                remaining -= keep;
            }
        }
        file.sync_all().await?;
        Ok(())
    });
    let (rpc, events) = OmpRpc::from_io(io.stdin, io.stdout, 64);
    let startup = async {
        rpc.initialize().await?;
        let filter = if probe && desktop {
            json!([
                "agent_end",
                "notice",
                "tool_execution_start",
                "tool_execution_end"
            ])
        } else {
            json!(["agent_end", "notice"])
        };
        rpc.request(
            "set_event_filter",
            json!({"events":filter,"messageUpdates":"delta"}),
            Duration::from_secs(30),
        )
        .await?;
        Ok::<_, crate::OmpRpcError>(())
    }
    .await;
    if let Err(error) = startup {
        let _ = rpc.close().await;
        anyhow::ensure!(
            supervisor.terminate(&id, Duration::from_secs(10)).await?,
            "failed OMP startup remains unreconciled"
        );
        tokio::time::timeout(Duration::from_secs(10), stderr).await???;
        return Err(error.into());
    }
    Ok((id, rpc, events, stderr))
}

const DESKTOP_PROBE: &str =
    "console.log('DACS_DESKTOP_CAPS='+JSON.stringify(await computer.capabilities()));";

fn observed_desktop(raw: &Value, calls: &mut BTreeMap<String, bool>) -> Option<Value> {
    let id = raw["toolCallId"].as_str()?;
    if raw["type"] == "tool_execution_start" {
        calls.insert(
            id.into(),
            raw["toolName"] == "eval"
                && raw["args"]["language"] == "js"
                && raw["args"]["code"] == DESKTOP_PROBE,
        );
        return None;
    }
    if raw["type"] != "tool_execution_end"
        || !calls.remove(id).unwrap_or(false)
        || raw["isError"] == true
    {
        return None;
    }
    for content in raw["result"]["content"].as_array()? {
        let Some(text) = content["text"].as_str() else {
            continue;
        };
        for line in text.lines() {
            if let Some((_, json)) = line.split_once("DACS_DESKTOP_CAPS=") {
                if let Ok(value) = serde_json::from_str::<Value>(json.trim()) {
                    if value.is_object() {
                        return Some(value);
                    }
                }
            }
        }
    }
    None
}

async fn provider_probe(
    config: &WorkerConfig,
    supervisor: &Supervisor,
    desktop: bool,
    attempt: Option<&str>,
) -> Result<Option<Value>> {
    let state = config
        .data_dir
        .join("provider-probe")
        .join(uuid::Uuid::new_v4().to_string());
    let (id, rpc, mut events, stderr) = launch_omp(
        config,
        supervisor,
        attempt,
        &config.data_dir,
        &state,
        true,
        desktop,
    )
    .await?;
    let probe: Result<Option<Value>> = async {
        let prompt = if desktop {
            format!("Readiness check only. Call eval once with language js and this exact code (no wrappers or modifications): {DESKTOP_PROBE}\nThen reply READY. Do not request permissions or send input.")
        } else { "Provider readiness check only. Reply exactly READY. Do not call tools.".into() };
        rpc.request("prompt",json!({"message":prompt}),Duration::from_secs(30)).await?;
        tokio::time::timeout(Duration::from_secs(90),async {
            let mut observation = None;
            let mut calls = BTreeMap::new();
            while let Some(event) = events.recv().await {
                match event {
                    OmpEvent::Unknown { raw } => { if let Some(value) = observed_desktop(&raw,&mut calls) { observation = Some(value); } },
                    OmpEvent::PromptResult { status,error,.. } => {
                        anyhow::ensure!(status == "completed", "provider probe did not complete: {error:?}");
                        return Ok::<_,anyhow::Error>(observation);
                    }
                    _ => (),
                }
            }
            bail!("OMP closed before provider readiness result")
        }).await?
    }.await;
    let _ = rpc.close().await;
    anyhow::ensure!(
        supervisor.terminate(&id, Duration::from_secs(10)).await?,
        "provider probe process remains active"
    );
    tokio::time::timeout(Duration::from_secs(10), stderr).await???;
    probe
}

async fn execute_attempt(
    config: WorkerConfig,
    coordinator: Coordinator,
    supervisor: Arc<Supervisor>,
    task: TaskRecord,
    attempt: AttemptRecord,
    command_id: String,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let journal = &coordinator.journal;
    let key = format!("phase:{}", attempt.id);
    let resumed: bool = journal.get_value::<String>(&key)?.is_some();
    journal.set_value(&key, &"preparing")?;
    journal.mark_command(&command_id, "running")?;
    // Local stop does not wait for network requests, RPC callbacks, or a runner
    // future to yield. Fence launches and kill the attempt's actual process tree.
    let original_stop = stop.clone();
    let (stop_sender, local_stop) = watch::channel(*stop.borrow());
    stop = local_stop;
    let guard_supervisor = supervisor.clone();
    let guard_journal = coordinator.journal.clone();
    let guard_attempt = attempt.id.clone();
    let desktop = task
        .spec
        .required_capabilities
        .iter()
        .any(|c| c.starts_with("desktop"));
    let watchdog = tokio::spawn(async move {
        loop {
            let controls: OwnerControls = guard_journal
                .get_value("owner_controls")
                .ok()
                .flatten()
                .unwrap_or_default();
            let revoked = guard_journal
                .get_value::<bool>("coordinator_revoked")
                .ok()
                .flatten()
                .unwrap_or(false);
            if *original_stop.borrow()
                || controls.emergency_cancel
                || revoked
                || desktop && controls.desktop_paused
            {
                let _ = stop_sender.send(true);
                let _ = guard_supervisor
                    .terminate_attempt(&guard_attempt, Duration::from_secs(10))
                    .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let _cleanup = AttemptCleanup {
        watchdog,
        supervisor: supervisor.clone(),
        attempt: attempt.id.clone(),
    };
    let work: Result<()> = async {
        let source = coordinator.source(&task.spec.project_id).await?;
        if *stop.borrow() { return Ok(()); }
        let directory = workspace::prepare(&supervisor,&task,&attempt,&config.workspaces_dir,&source).await?;
        let state_dir = config.data_dir.join("attempts").join(&attempt.id);
        std::fs::create_dir_all(&state_dir)?;
        let started = if matches!(attempt.state.as_str(),"admitted"|"waiting") { WorkerEvent::Started }
            else { WorkerEvent::Progress { message:"Reattached durable OMP attempt after local process reconciliation".into() } };
        journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,started)?;
        // Started is legal only for admission/waiting resume; an executing
        // session restored from disk remains the same reservation/generation.
        journal.set_value(&key,&"running")?;
        match &task.spec.execution {
            TaskExecution::Agent => agent_attempt(&config,&coordinator,&supervisor,&task,&attempt,&directory,&state_dir,resumed,&mut stop).await,
            TaskExecution::Workflow { .. } => {
                let transport = state_dir.join("workflow-transport.json");
                write_private(&transport,&serde_json::to_vec(&json!({"coordinator_url":config.coordinator_url,"credential":coordinator.token.as_str(),"ca_certificate":config.ca_certificate}))?)?;
                let execution = crate::build_runner::execute_admitted(&supervisor,&task,&attempt,&directory,&state_dir);
                tokio::pin!(execution);
                let mut telemetry_tick = tokio::time::interval(Duration::from_millis(500));
                let result = loop {
                    tokio::select! {
                        result = &mut execution => break result?,
                        _ = telemetry_tick.tick() => {
                            relay_compiler_telemetry(&coordinator,&task,&attempt,&state_dir)?;
                        }
                        _ = stop.changed() => {
                            anyhow::ensure!(supervisor.terminate_attempt(&attempt.id,Duration::from_secs(10)).await?, "workflow cancellation not reconciled");
                            break tokio::time::timeout(Duration::from_secs(30),&mut execution).await??;
                        }
                    }
                };
                relay_compiler_telemetry(&coordinator,&task,&attempt,&state_dir)?;
                std::fs::remove_file(transport)?;
                anyhow::ensure!(result.processes_reconciled, "workflow reports unreconciled processes");
                anyhow::ensure!(supervisor.terminate_attempt(&attempt.id,Duration::from_secs(10)).await?, "workflow process tree remains");
                let mut dependencies = Vec::new();
                for (index, artifact) in result.artifacts.iter().enumerate() {
                    let digest = coordinator.upload(&format!("{}-artifact-{index}",attempt.id),&task,&attempt,Path::new(&artifact.path),true)?;
                    anyhow::ensure!(digest == artifact.sha256, "workflow artifact digest changed");
                    dependencies.push(format!("{}-artifact-{index}",attempt.id));
                }
                let output = state_dir.join("workflow-result.json");
                write_durable(&output,&serde_json::to_vec(&result)?)?;
                let digest = coordinator.upload(&format!("{}-workflow-result",attempt.id),&task,&attempt,&output,true)?;
                dependencies.push(format!("{}-workflow-result",attempt.id));
                journal.set_value(&format!("requires-workflow:{}",attempt.id),&dependencies)?;
                journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Settled)?;
                if !*stop.borrow() {
                    journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::WorkflowCompleted { success:result.success,artifact_digest:digest,evidence:result.checks })?;
                }
                Ok(())
            }
        }
    }.await;
    if let Err(error) = &work {
        if !*stop.borrow() {
            journal.event(
                &task.id,
                &attempt.id,
                attempt.generation,
                attempt.last_sequence,
                WorkerEvent::Failed {
                    message: format!("{error:#}"),
                },
            )?;
        }
    }
    anyhow::ensure!(
        supervisor
            .terminate_attempt(&attempt.id, Duration::from_secs(10))
            .await?,
        "attempt is not stopped; reservation remains held"
    );
    journal.event(
        &task.id,
        &attempt.id,
        attempt.generation,
        attempt.last_sequence,
        WorkerEvent::Stopped,
    )?;
    journal.set_value(&key, &"finished")?;
    journal.mark_command(&command_id, "done")?;
    supervisor.forget_attempt(&attempt.id).await?;
    work
}

fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// DACS host tools registered with `set_host_tools` in every managed session.
pub fn host_tools() -> Value {
    json!([
        {"name":"dacs_delegate","label":"Delegate DACS task","description":"Request admission of a child TaskSpec. Returns immediately with an admitted task ID or durable pending receipt. Completion arrives in the mailbox; parent reservation remains held.","loadMode":"essential","parameters":{"type":"object","properties":{"spec":{"type":"object"}},"required":["spec"],"additionalProperties":false}},
        {"name":"dacs_workflow","label":"Submit build/test workflow","description":"Admit a workflow child using TaskSpec execution.kind=workflow and effect_class=external. Never execute a distributed build outside admitted capacity.","loadMode":"essential","parameters":{"type":"object","properties":{"spec":{"type":"object"}},"required":["spec"],"additionalProperties":false}},
        {"name":"dacs_message","label":"Send durable message","description":"Send durable project-local message to an admitted task.","loadMode":"essential","parameters":{"type":"object","properties":{"recipient_task_id":{"type":"string"},"message":{"type":"string"}},"required":["recipient_task_id","message"],"additionalProperties":false}},
        {"name":"dacs_submit","label":"Submit committed candidate","description":"Submit current clean Git HEAD with actual check evidence. Commit your changes first. No terminal success until submission accepted; pending children must finish first.","loadMode":"essential","parameters":{"type":"object","properties":{"evidence":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"command":{"type":"string"},"source_sha":{"type":"string"},"passed":{"type":"boolean"},"artifact_digest":{"type":["string","null"]}},"required":["name","command","source_sha","passed","artifact_digest"],"additionalProperties":false}},"unresolved":{"type":"array","items":{"type":"string"}}},"required":["evidence"],"additionalProperties":false}},
        {"name":"dacs_pending","label":"Read durable obligations","description":"Read current admitted attempt and pending child/job obligations from coordinator.","loadMode":"essential","parameters":{"type":"object","properties":{},"additionalProperties":false}}
    ])
}

async fn agent_attempt(
    config: &WorkerConfig,
    coordinator: &Coordinator,
    supervisor: &Supervisor,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    directory: &Path,
    state_dir: &Path,
    resumed: bool,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let desktop = task
        .spec
        .required_capabilities
        .iter()
        .any(|c| c.starts_with("desktop"));
    let _desktop_lock = if desktop {
        anyhow::ensure!(
            config
                .registration
                .capabilities
                .iter()
                .any(|c| c.starts_with("desktop")),
            "owner did not enable native desktop"
        );
        let controls: OwnerControls = coordinator
            .journal
            .get_value("owner_controls")?
            .unwrap_or_default();
        anyhow::ensure!(!controls.desktop_paused, "owner paused desktop access");
        let lock = local_control::desktop_lock()?;
        let observation = provider_probe(config, supervisor, true, Some(&attempt.id))
            .await?
            .context("native desktop probe returned no tool observation")?;
        anyhow::ensure!(
            local_control::desktop_available(&observation),
            "desktop locked, unavailable, or native permission missing: {observation}"
        );
        coordinator
            .journal
            .set_value("desktop_observation", &observation)?;
        coordinator
            .journal
            .set_value("desktop_owner", &attempt.id)?;
        eprintln!(
            "DACS native desktop active for attempt {}. Use local PauseDesktop to stop access.",
            attempt.id
        );
        Some(lock)
    } else {
        None
    };
    let (process_id, rpc, mut events, stderr) = launch_omp(
        config,
        supervisor,
        Some(&attempt.id),
        directory,
        state_dir,
        false,
        desktop,
    )
    .await?;
    let work: Result<()> = async {
        rpc.request("set_host_tools",json!({"tools":host_tools()}),Duration::from_secs(30)).await?;
        if resumed {
            rpc.request("open_session",json!({"sessionDir":state_dir.join("session")}),Duration::from_secs(60)).await?;
        }
        let pending = attempt.pending_children.clone();
        let prompt = format!("DACS managed assignment {} generation {}. Source {}. Contract {} policy {}.\n{}\n\nUse dacs_delegate/dacs_workflow for admitted remote or long-running work. Never native task/eval delegation or detached services. Local checks are ordinary reserved work. Commit changes and call dacs_submit with actual evidence; yielding does not complete this attempt. Pending durable children/jobs: {:?}. {}",task.id,attempt.generation,task.spec.source_commit,task.spec.contract_revision,task.spec.policy_revision,task.spec.instructions,pending,if resumed { "This is a restart. Inspect durable messages and existing source before acting; do not duplicate external effects." } else { "" });
        let prompt = if desktop { format!("{prompt}\nNative desktop is exclusively reserved for this attempt. Use computer.run for each multi-step workflow, first verify desktop.capabilities() is unlocked, awake and not secureInput, then acquire computer.control for foreground work and release in finally. Never continue after partial input failure without observing effects. Local owner pause cancels this session.") } else { prompt };
        rpc.request("prompt",json!({"message":prompt}),Duration::from_secs(60)).await?;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let remaining = (attempt.started_at.saturating_add(task.spec.max_runtime_seconds as i64) - local_control::now()).max(0) as u64;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(remaining);
        let mut settled = false;
        let mut submit: Option<String> = coordinator.journal.get_value::<Option<String>>(&format!("submission:{}",attempt.id))?.flatten();
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        let _ = rpc.request("abort",json!({}),Duration::from_secs(5)).await;
                        break;
                    }
                }
                _ = tick.tick() => {
                    if tokio::time::Instant::now() >= deadline { bail!("assignment maximum runtime elapsed"); }
                    if let Some(id) = &submit {
                        if settled && coordinator.journal.receipt(id)?.is_none() {
                            if let Some(command) = coordinator.journal.get_value::<Command>(&format!("staged:{id}"))? {
                                coordinator.enqueue(id,&task.id,&attempt.id,attempt.generation,command)?;
                            }
                        }
                        if let Some(receipt) = coordinator.journal.receipt(id)? {
                            if receipt["ok"] == true { break; }
                            rpc.request("prompt",json!({"message":format!("DACS submission refused: {}. Resolve the reason and explicitly resubmit.",receipt["error"]),"streamingBehavior":"followUp"}),Duration::from_secs(30)).await?;
                            submit = None;
                            coordinator.journal.set_value(&format!("submission:{}",attempt.id),&submit)?;
                        }
                    }
                    for command in coordinator.journal.pending_commands()? {
                        if command.attempt_id != attempt.id || command.generation != attempt.generation { continue; }
                        let message: ControlMessage = serde_json::from_value(command.payload)?;
                        if !matches!(message.kind.as_str(),"child_result"|"user_message") { continue; }
                        rpc.request_id(&format!("mail-{}",message.id),"prompt",json!({"message":format!("Durable DACS mailbox message {} (may be replayed after a crash; do not repeat completed effects): {}",message.id,message.payload),"streamingBehavior":"followUp"}),Duration::from_secs(30)).await?;
                        coordinator.journal.mark_command(&message.id,"done")?;
                        settled = false;
                    }
                    // Deliver outcomes for admissions that outlived their tool call.
                    let pending_tools: Vec<String> = coordinator.journal.get_value(&format!("pending-tools:{}",attempt.id))?.unwrap_or_default();
                    let mut remaining = Vec::new();
                    for id in pending_tools {
                        if let Some(receipt) = coordinator.journal.receipt(&id)? {
                            rpc.request("prompt",json!({"message":format!("Durable DACS operation {id} completed: {receipt}"),"streamingBehavior":"followUp"}),Duration::from_secs(30)).await?;
                            settled = false;
                        } else { remaining.push(id); }
                    }
                    coordinator.journal.set_value(&format!("pending-tools:{}",attempt.id),&remaining)?;
                }
                event = events.recv() => {
                    let event = event.context("OMP transport closed before explicit submission")?;
                    match event {
                        OmpEvent::ToolCall { id,tool_call_id,name,arguments } => {
                            anyhow::ensure!(!id.is_empty() && !tool_call_id.is_empty(), "OMP host tool call omitted correlation identity");
                            settled = false;
                            let operation = format!("{}-{}",attempt.id,hex::encode(Sha256::digest(tool_call_id.as_bytes())));
                            let result = host_call(coordinator,supervisor,task,attempt,directory,state_dir,&operation,&name,arguments).await;
                            match result {
                                Ok(value) => {
                                    if name == "dacs_submit" { submit = Some(operation.clone()); coordinator.journal.set_value(&format!("submission:{}",attempt.id),&submit)?; }
                                    rpc.respond_tool(&id,value).await?;
                                }
                                Err(error) => rpc.reject_tool(&id,&format!("{error:#}")).await?,
                            }
                        }
                        OmpEvent::ToolCancel { .. } => {
                            // A durable admitted child is not cancelled by loss of
                            // its transient host call. Its coordinator ID survives.
                        }
                        event @ OmpEvent::PromptResult { .. } if event.cost_capped().is_some() => {
                            let error = event.cost_capped().cloned().unwrap_or(Value::Null);
                            coordinator.journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::CostCapped { message:error.to_string() })?;
                            return Ok(());
                        }
                        OmpEvent::PromptResult { status,error,session_settled,.. } => {
                            if status == "error" {
                                let error = error.unwrap_or(Value::Null);
                                coordinator.journal.set_value("provider_authenticated",&Some(false))?;
                                bail!("OMP provider turn failed: {error}");
                            }
                            if session_settled && !settled {
                                coordinator.journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Settled)?;
                                settled = true;
                            }
                        }
                        OmpEvent::Settled => {
                            if !settled { coordinator.journal.event(&task.id,&attempt.id,attempt.generation,attempt.last_sequence,WorkerEvent::Settled)?; settled = true; }
                        }
                        OmpEvent::Unknown { raw } if raw["type"] == "agent_end" => {
                            report_usage(coordinator,task,attempt,&rpc).await?;
                        }
                        _ => (),
                    }
                }
            }
        }
        Ok(())
    }.await;
    let _ = rpc.close().await;
    anyhow::ensure!(
        supervisor
            .terminate(&process_id, Duration::from_secs(10))
            .await?,
        "OMP tree remains active"
    );
    tokio::time::timeout(Duration::from_secs(10), stderr).await???;
    work
}

async fn host_call(
    coordinator: &Coordinator,
    supervisor: &Supervisor,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    directory: &Path,
    state_dir: &Path,
    operation: &str,
    name: &str,
    arguments: Value,
) -> Result<Value> {
    let request_key = format!("host-request:{operation}");
    let request = json!({"name":name,"arguments":arguments});
    if let Some(prior) = coordinator.journal.get_value::<Value>(&request_key)? {
        anyhow::ensure!(
            prior == request,
            "host tool identity reused with different request"
        );
    } else {
        coordinator.journal.set_value(&request_key, &request)?;
    }
    if let Some(receipt) = coordinator.journal.receipt(operation)? {
        anyhow::ensure!(
            receipt["ok"] == true,
            "operation previously refused: {}",
            receipt["error"]
        );
        return Ok(receipt["result"].clone());
    }
    if name == "dacs_pending" {
        let snapshot: Snapshot = coordinator.get("/api/state").await.context(
            "coordinator unavailable: durable obligations cannot be read; continue only already admitted work",
        )?;
        return Ok(serde_json::to_value(
            snapshot
                .attempts
                .iter()
                .find(|a| a.id == attempt.id)
                .context("attempt no longer available")?,
        )?);
    }
    let command = match name {
        "dacs_delegate" | "dacs_workflow" => {
            let spec: TaskSpec = serde_json::from_value(arguments["spec"].clone())?;
            anyhow::ensure!(
                spec.project_id == task.spec.project_id,
                "child must remain in the assigned project"
            );
            if name == "dacs_workflow" {
                anyhow::ensure!(
                    matches!(spec.execution, TaskExecution::Workflow { .. })
                        && spec.effect_class == EffectClass::External,
                    "workflow requires workflow execution and external effects"
                );
            }
            Command::Delegate {
                parent_attempt_id: attempt.id.clone(),
                generation: attempt.generation,
                spec,
            }
        }
        "dacs_message" => Command::SendMessage {
            project_id: task.spec.project_id.clone(),
            recipient_task_id: arguments["recipient_task_id"]
                .as_str()
                .context("recipient_task_id required")?
                .into(),
            message_id: operation.into(),
            kind: "agent".into(),
            body: arguments["message"].clone(),
        },
        "dacs_submit" => {
            if let Some(pending) = coordinator
                .journal
                .get_value::<Option<String>>(&format!("submission:{}", attempt.id))?
                .flatten()
            {
                anyhow::ensure!(
                    pending == operation,
                    "submission {pending} is already pending; await its durable outcome"
                );
                if coordinator
                    .journal
                    .get_value::<Command>(&format!("staged:{operation}"))?
                    .is_some()
                {
                    return Ok(json!({"pending":true,"operation_id":operation}));
                }
            }
            // Submission intent and bytes can be durable while disconnected.
            // The coordinator authoritatively refuses unfinished child/jobs.
            let evidence: Vec<CheckEvidence> =
                serde_json::from_value(arguments["evidence"].clone())?;
            let unresolved: Vec<String> =
                serde_json::from_value(arguments.get("unresolved").cloned().unwrap_or(json!([])))?;
            let output = state_dir.join(format!("{operation}.bundle"));
            let sha = workspace::candidate(supervisor, task, attempt, directory, &output).await?;
            coordinator.upload(
                &format!("{operation}-source"),
                task,
                attempt,
                &output,
                false,
            )?;
            // The host submission is explicit intent; the OMP turn may still be
            // unwinding. The runtime emits Settled once the actual frame arrives.
            Command::SubmitCandidate {
                submission: CandidateSubmission {
                    task_id: task.id.clone(),
                    attempt_id: attempt.id.clone(),
                    generation: attempt.generation,
                    commit_sha: sha,
                    contract_revision: task.spec.contract_revision,
                    policy_revision: task.spec.policy_revision,
                    evidence,
                    unresolved,
                },
            }
        }
        _ => bail!("unsupported DACS host tool"),
    };
    if name == "dacs_submit" {
        coordinator.journal.set_values(&[
            (
                &format!("staged:{operation}"),
                serde_json::to_value(&command)?,
            ),
            (
                &format!("submission:{}", attempt.id),
                json!(Some(operation)),
            ),
        ])?;
    } else {
        coordinator.enqueue(
            operation,
            &task.id,
            &attempt.id,
            attempt.generation,
            command,
        )?;
        let delivery = tokio::time::timeout(Duration::from_millis(500), coordinator.flush()).await;
        if let Some(receipt) = coordinator.journal.receipt(operation)? {
            anyhow::ensure!(
                receipt["ok"] == true,
                "coordinator refused admission/operation: {}",
                receipt["error"]
            );
            return Ok(receipt["result"].clone());
        }
        let key = format!("pending-tools:{}", attempt.id);
        let mut pending: Vec<String> = coordinator.journal.get_value(&key)?.unwrap_or_default();
        if !pending.iter().any(|id| id == operation) {
            pending.push(operation.into());
            coordinator.journal.set_value(&key, &pending)?;
        }
        // Coordinator loss is reported explicitly: nothing was admitted and no
        // capacity is reserved until the coordinator answers after reconnection.
        let unavailable = match delivery {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("{error:#}")),
            Err(_) => Some("coordinator did not answer within 500 ms".to_owned()),
        };
        if let Some(reason) = unavailable {
            return Ok(
                json!({"pending":true,"operation_id":operation,"coordinator_unavailable":reason,"message":"Coordinator unreachable: this operation is persisted locally but NOT admitted and holds no capacity. Do not run it locally or elsewhere and do not repeat it; its durable outcome arrives after reconnection."}),
            );
        }
    }
    Ok(
        json!({"pending":true,"operation_id":operation,"message":"Persisted locally. Admission/submission is not yet confirmed. Do not assume capacity or repeat this operation."}),
    )
}

async fn report_usage(
    coordinator: &Coordinator,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    rpc: &OmpRpc,
) -> Result<()> {
    let stats = rpc
        .request("get_session_stats", json!({}), Duration::from_secs(30))
        .await?;
    // Preserve the exact provider/session report; unknown fields stay unknown,
    // rather than invented zero-cost or credential-success claims.
    coordinator
        .journal
        .set_value(&format!("usage:{}", attempt.id), &stats)?;
    let input = stats.pointer("/tokens/input").and_then(Value::as_u64);
    let output = stats.pointer("/tokens/output").and_then(Value::as_u64);
    if let (Some(input), Some(output)) = (input, output) {
        let key = format!("usage-reported:{}", attempt.id);
        let previous: Option<(u64, u64, Option<f64>)> = coordinator.journal.get_value(&key)?;
        let cost = stats.get("cost").and_then(Value::as_f64);
        if previous != Some((input, output, cost)) {
            coordinator.journal.event_checkpoint(
                &task.id,
                &attempt.id,
                attempt.generation,
                attempt.last_sequence,
                WorkerEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cost_usd: cost,
                    estimated: true,
                },
                Some((&key, json!((input, output, cost)))),
            )?;
        }
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "private state path is not a regular file"
        );
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

async fn secure_directory(supervisor: &Supervisor, directory: &Path) -> Result<()> {
    anyhow::ensure!(
        !std::fs::symlink_metadata(directory)?
            .file_type()
            .is_symlink(),
        "private state cannot be a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        let _ = supervisor;
    }
    #[cfg(windows)]
    {
        let (code, out, _) = workspace::capture(
            supervisor,
            None,
            directory,
            PathBuf::from("powershell.exe"),
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "[Security.Principal.WindowsIdentity]::GetCurrent().User.Value".into(),
            ],
        )
        .await?;
        let sid = String::from_utf8(out)?.trim().to_owned();
        anyhow::ensure!(
            code == 0
                && sid.starts_with("S-")
                && sid
                    .bytes()
                    .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-'),
            "cannot determine owner SID"
        );
        let (code, _, _) = workspace::capture(
            supervisor,
            None,
            directory,
            PathBuf::from("icacls.exe"),
            vec![
                directory.to_string_lossy().into_owned(),
                "/inheritance:r".into(),
                "/grant:r".into(),
                format!("*{sid}:(OI)(CI)F"),
            ],
        )
        .await?;
        anyhow::ensure!(code == 0, "cannot restrict owner state ACL");
    }
    Ok(())
}

fn relay_compiler_telemetry(
    coordinator: &Coordinator,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    state_dir: &Path,
) -> Result<()> {
    use std::io::{BufRead, Read, Seek, SeekFrom};
    let path = state_dir
        .join(crate::build_runner::WORKFLOW_DIRECTORY)
        .join("engine.jsonl");
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let key = format!("compiler-offset:{}", attempt.id);
    let mut offset: u64 = coordinator.journal.get_value(&key)?.unwrap_or(0);
    anyhow::ensure!(
        file.metadata()?.len() >= offset,
        "compiler engine journal truncated before reconciliation"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut records = Vec::new();
    for _ in 0..128 {
        let mut line = Vec::new();
        Read::by_ref(&mut reader)
            .take(65537)
            .read_until(b'\n', &mut line)?;
        if line.is_empty() {
            break;
        }
        anyhow::ensure!(line.len() <= 65536, "compiler engine record exceeds 64KiB");
        if line.last() != Some(&b'\n') {
            break;
        }
        records.push(
            serde_json::from_slice::<Value>(&line).context("invalid compiler engine record")?,
        );
        offset += line.len() as u64;
    }
    if !records.is_empty() {
        coordinator.journal.event_checkpoint(
            &task.id,
            &attempt.id,
            attempt.generation,
            attempt.last_sequence,
            WorkerEvent::CompilerTelemetry { records },
            Some((&key, json!(offset))),
        )?;
    }
    Ok(())
}

struct AttemptCleanup {
    watchdog: tokio::task::JoinHandle<()>,
    supervisor: Arc<Supervisor>,
    attempt: String,
}

impl Drop for AttemptCleanup {
    fn drop(&mut self) {
        self.watchdog.abort();
        let supervisor = self.supervisor.clone();
        let attempt = self.attempt.clone();
        tokio::spawn(async move {
            if let Err(error) = supervisor
                .terminate_attempt(&attempt, Duration::from_secs(10))
                .await
            {
                tracing::error!(%attempt,%error,"attempt cleanup requires reconciliation");
            }
        });
    }
}

async fn observe_elevation(supervisor: &Supervisor, directory: &Path) -> Result<bool> {
    #[cfg(windows)]
    let (code,out,_) = workspace::capture(supervisor,None,directory,PathBuf::from("powershell.exe"),
        vec!["-NoProfile".into(),"-NonInteractive".into(),"-Command".into(),
        "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)".into()]).await?;
    #[cfg(not(windows))]
    let (code, out, _) = workspace::capture(
        supervisor,
        None,
        directory,
        PathBuf::from("id"),
        vec!["-u".into()],
    )
    .await?;
    anyhow::ensure!(code == 0, "owner elevation query failed");
    let output = String::from_utf8(out)?.trim().to_owned();
    #[cfg(windows)]
    {
        anyhow::ensure!(
            output == "True" || output == "False",
            "unexpected elevation result"
        );
        Ok(output == "True")
    }
    #[cfg(not(windows))]
    {
        Ok(output.parse::<u32>()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_schema_preserves_check_evidence_fields() {
        let tools = host_tools();
        let submit = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "dacs_submit")
            .unwrap();
        let item = &submit["parameters"]["properties"]["evidence"]["items"];
        let evidence = serde_json::to_value(CheckEvidence {
            name: "content check".into(),
            command: "check proof.txt".into(),
            source_sha: "0123456789abcdef".into(),
            passed: true,
            artifact_digest: None,
        })
        .unwrap();
        for field in evidence.as_object().unwrap().keys() {
            assert!(item["properties"].get(field).is_some(), "{field}");
            assert!(item["required"].as_array().unwrap().contains(&json!(field)));
        }
        assert_eq!(item["properties"]["passed"]["type"], "boolean");
        assert_eq!(
            item["properties"]["artifact_digest"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn native_capability_observation_requires_matching_actual_tool_execution() {
        let caps = json!({"capture":true,"input":true,"displayCount":1});
        let end = json!({"type":"tool_execution_end","toolCallId":"t","toolName":"eval","isError":false,"result":{"content":[{"type":"text","text":format!("DACS_DESKTOP_CAPS={caps}")}]}});
        let mut calls = BTreeMap::new();
        assert!(observed_desktop(&end, &mut calls).is_none());
        observed_desktop(
            &json!({"type":"tool_execution_start","toolCallId":"t","toolName":"eval","args":{"language":"js","code":"console.log('fake')"}}),
            &mut calls,
        );
        assert!(observed_desktop(&end, &mut calls).is_none());
        observed_desktop(
            &json!({"type":"tool_execution_start","toolCallId":"t","toolName":"eval","args":{"language":"js","code":DESKTOP_PROBE}}),
            &mut calls,
        );
        assert_eq!(observed_desktop(&end, &mut calls), Some(caps));
    }

    #[test]
    fn managed_overlay_never_overrides_owner_cost_cap() {
        for desktop in [false, true] {
            let overlay = managed_overlay(desktop);
            assert!(!overlay.contains("maxCost"));
            assert!(overlay.contains("maxRecursionDepth: 0"));
            assert!(overlay.contains("persist: false"));
        }
    }

    #[test]
    fn durable_private_files_refuse_non_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_private(dir.path(), b"secret").is_err());
        let file = dir.path().join("transport");
        write_private(&file, b"secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }
}
