//! Admitted build/test workflows. All child processes belong to the attempt's
//! supervisor reservation; evidence is generated from observed output, never supplied actions.
use crate::{
    ManagedChild,
    build_execution::{BuildProfile, CompilerPool},
    supervisor::{ProcessState, Supervisor},
};
use anyhow::{Context, Result, bail};
use dacs_protocol::workflow::PoolClaim;
pub use dacs_protocol::workflow::{WorkflowArtifact, WorkflowResult};
use dacs_protocol::{AttemptRecord, CheckEvidence, EffectClass, TaskExecution, TaskRecord};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt};

const LOG_LIMIT: u64 = 16 * 1024 * 1024;
pub(crate) const WORKFLOW_DIRECTORY: &str = "workflow";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequest {
    pub profile: BuildProfile,
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub git: PathBuf,
    pub operation: WorkflowOperation,
    #[serde(default)]
    pub distributed: Option<DistributedRun>,
    /// Relative files in the isolated workspace to collect after execution.
    #[serde(default)]
    pub artifacts: Vec<PathBuf>,
    #[serde(default)]
    pub exercise: Vec<BehaviorCheck>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowOperation {
    Build,
    Test {
        shards: Vec<TestShard>,
    },
    /// A long-lived service is its own externally-effectful admitted task. Its
    /// lifetime ends only by cancellation/deadline, not a successful launch receipt.
    CompilerService {
        pool: CompilerPool,
        role: ServiceRole,
        server_id: Option<String>,
        executable: PathBuf,
        executable_sha256: String,
        credential_file: PathBuf,
        scheduler_listen: Option<String>,
        scheduler_secret_file: Option<PathBuf>,
        bubblewrap: Option<PathBuf>,
        resource_scope_executable: PathBuf,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRole {
    Scheduler,
    Server,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestShard {
    pub name: String,
    pub filter: String,
    #[serde(default)]
    pub exact: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorCheck {
    pub executable_name: String,
    pub args: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributedRun {
    pub pool: CompilerPool,
    pub claim: PoolClaim,
    pub sccache: PathBuf,
    pub sccache_sha256: String,
    pub provenance_wrapper: PathBuf,
    pub provenance_wrapper_sha256: String,
    pub initiator_physical_host: String,
    /// Owner-provisioned local token, never a credential in task JSON/evidence.
    pub client_token_file: PathBuf,
    pub required: bool,
    pub required_remote_hosts: [String; 2],
}

fn label(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn owned_environment() -> BTreeMap<String, String> {
    [
        "PATH",
        "HOME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "SYSTEMROOT",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "TMP",
        "TEMP",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "SDKROOT",
        "INCLUDE",
        "LIB",
        "LIBPATH",
    ]
    .into_iter()
    .filter_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| (name.to_owned(), value))
    })
    .collect()
}
fn descriptor(
    program: &Path,
    args: Vec<String>,
    cwd: &Path,
    attempt: &AttemptRecord,
    env: &BTreeMap<String, String>,
) -> ManagedChild {
    ManagedChild {
        name: "workflow".into(),
        program: program.into(),
        args,
        cwd: cwd.into(),
        env: env.clone(),
        effect_class: "host_level".into(),
        reservation_id: Some(attempt.id.clone()),
    }
}
fn durable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
fn artifact(path: &Path, kind: &str) -> Result<WorkflowArtifact> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    anyhow::ensure!(before.is_file(), "artifact is not a regular file");
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut bytes = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        bytes += count as u64;
    }
    let after = file.metadata()?;
    anyhow::ensure!(
        bytes == before.len()
            && after.len() == before.len()
            && after.modified()? == before.modified()?,
        "artifact changed while hashing"
    );
    Ok(WorkflowArtifact {
        path: path.to_string_lossy().into_owned(),
        sha256: hex::encode(digest.finalize()),
        bytes,
        kind: kind.into(),
    })
}

/// Stable content pin for the compiler and sysroot; excludes documentation/cache.
/// Paths and content digests are length-framed, sorted and platform-independent.
pub fn sysroot_digest(root: &Path) -> Result<String> {
    fn visit(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let target = path.canonicalize()?;
                anyhow::ensure!(
                    target.starts_with(root.canonicalize()?) && target.is_file(),
                    "sysroot symlink escapes root or references a directory"
                );
                paths.push(path.strip_prefix(root)?.into());
            } else if kind.is_dir() {
                visit(root, &path, paths)?;
            } else if kind.is_file() {
                paths.push(path.strip_prefix(root)?.into());
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    visit(root, &root.join("bin"), &mut paths)?;
    visit(root, &root.join("lib"), &mut paths)?;
    paths.sort();
    anyhow::ensure!(!paths.is_empty(), "empty compiler sysroot");
    let mut hash = Sha256::new();
    for relative in paths {
        let name = relative
            .to_str()
            .context("non-UTF8 sysroot filename")?
            .replace('\\', "/");
        let observed = artifact(&root.join(&relative), "sysroot")?;
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(observed.bytes.to_le_bytes());
        hash.update(observed.sha256.as_bytes());
    }
    Ok(hex::encode(hash.finalize()))
}
async fn drain<R: AsyncRead + Unpin>(reader: R, path: PathBuf) -> Result<bool> {
    drain_stream(reader, path, None).await
}
async fn drain_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    path: PathBuf,
    telemetry: Option<PathBuf>,
) -> Result<bool> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut buffer = [0u8; 32768];
    let mut bytes = 0u64;
    let mut truncated = false;
    let mut telemetry = telemetry
        .map(|path| OpenOptions::new().write(true).create_new(true).open(path))
        .transpose()?;
    let mut telemetry_bytes = 0u64;
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = (LOG_LIMIT.saturating_sub(bytes)).min(count as u64) as usize;
        file.write_all(&buffer[..keep])?;
        bytes += keep as u64;
        truncated |= keep < count;
        if let Some(telemetry) = telemetry.as_mut() {
            for byte in &buffer[..count] {
                if *byte == b'\n' {
                    if !oversized {
                        if let Some(record) = line.strip_prefix(b"DACS_ENGINE_JSON:") {
                            let value: serde_json::Value = serde_json::from_slice(record)
                                .context("invalid engine telemetry")?;
                            anyhow::ensure!(value["schema"] == 1, "unsupported telemetry schema");
                            if telemetry_bytes + record.len() as u64 + 1 > LOG_LIMIT {
                                bail!("engine telemetry size bound exceeded");
                            }
                            telemetry.write_all(record)?;
                            telemetry.write_all(b"\n")?;
                            telemetry.sync_data()?;
                            telemetry_bytes += record.len() as u64 + 1;
                        }
                    }
                    line.clear();
                    oversized = false;
                } else if line.len() < 65536 {
                    line.push(*byte);
                } else {
                    oversized = true;
                    truncated = true;
                }
            }
        }
    }
    file.sync_all()?;
    Ok(truncated)
}
struct Observation {
    code: Option<i32>,
    stdout: PathBuf,
    stderr: PathBuf,
    truncated: bool,
}
async fn command(
    supervisor: &Supervisor,
    id: &str,
    child: &ManagedChild,
    root: &Path,
    deadline: Instant,
) -> Result<Observation> {
    let stdout = root.join(format!("{id}.stdout.log"));
    let stderr = root.join(format!("{id}.stderr.log"));
    supervisor.launch(id, child).await?;
    let io = supervisor.take_io(id).await?;
    drop(io.stdin);
    let out = tokio::spawn(drain(io.stdout, stdout.clone()));
    let telemetry = child
        .name
        .starts_with("sccache-server:")
        .then(|| root.join("engine.jsonl"));
    let err = tokio::spawn(drain_stream(io.stderr, stderr.clone(), telemetry));
    let code = loop {
        match supervisor.state(id).await? {
            ProcessState::Exited { code } => break code,
            ProcessState::Unreconciled => bail!("process {id} requires reconciliation"),
            _ => {}
        }
        if Instant::now() >= deadline {
            anyhow::ensure!(
                supervisor.terminate(id, Duration::from_secs(5)).await?,
                "deadline process could not be reaped"
            );
            break None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let truncated = tokio::time::timeout(
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(5)),
        async { Ok::<bool, anyhow::Error>(out.await?? | err.await??) },
    )
    .await
    .context("workflow output pipes did not close before reconciliation deadline")??;
    Ok(Observation {
        code,
        stdout,
        stderr,
        truncated,
    })
}
fn collect_observation(observed: &Observation, output: &mut Vec<WorkflowArtifact>) -> Result<()> {
    output.push(artifact(
        &observed.stdout,
        if observed.truncated {
            "stdout_truncated"
        } else {
            "stdout"
        },
    )?);
    output.push(artifact(
        &observed.stderr,
        if observed.truncated {
            "stderr_truncated"
        } else {
            "stderr"
        },
    )?);
    Ok(())
}
fn confined(root: &Path, relative: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "artifact path must be relative without parent components"
    );
    let path = root.join(relative).canonicalize()?;
    anyhow::ensure!(
        path.starts_with(root.canonicalize()?),
        "artifact escapes workspace"
    );
    Ok(path)
}
fn acquire_lock(root: &Path, name: &str) -> Result<File> {
    anyhow::ensure!(label(name), "invalid lock name");
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join(format!("{name}.lock")))?;
    lock.try_lock_exclusive()
        .context("local compiler pool/daemon already owned")?;
    Ok(lock)
}

/// `state_dir` is private to this attempt, including its compiler service logs.
pub async fn execute_admitted(
    supervisor: &Supervisor,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    workspace: &Path,
    state_dir: &Path,
) -> Result<WorkflowResult> {
    anyhow::ensure!(
        task.spec.effect_class == EffectClass::External
            && task.id == attempt.task_id
            && task.generation == attempt.generation
            && task.attempt_id.as_deref() == Some(attempt.id.as_str())
            && !task.cancel_requested,
        "workflow is not the current external-effect admission"
    );
    anyhow::ensure!(
        attempt.resources.cpu_millis > 0
            && attempt.resources.memory_mb > 0
            && task.spec.max_runtime_seconds > 0,
        "workflow requires admitted CPU, memory and deadline"
    );
    let TaskExecution::Workflow { request } = &task.spec.execution else {
        bail!("not a workflow task");
    };
    let request: WorkflowRequest =
        serde_json::from_value(request.clone()).context("invalid workflow request")?;
    anyhow::ensure!(
        request.profile.source_commit == task.spec.source_commit,
        "workflow source differs from assignment"
    );
    let validation_pool = request
        .distributed
        .as_ref()
        .map(|d| d.pool.clone())
        .unwrap_or_else(|| CompilerPool {
            pool_id: request.profile.pool_id.clone(),
            scheduler_url: "https://localhost".into(),
            servers: vec![],
        });
    request.profile.validate(&validation_pool)?;
    request
        .profile
        .verify_lockfile(&workspace.join("Cargo.lock"))?;
    anyhow::ensure!(label(&attempt.id), "unsafe attempt id");
    let root = state_dir.join(WORKFLOW_DIRECTORY);
    fs::create_dir_all(root.parent().context("workflow parent missing")?)?;
    fs::create_dir(&root)
        .context("workflow already has durable state; reconcile instead of replaying")?;
    durable_json(&root.join("admission.json"), attempt)?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(task.spec.max_runtime_seconds);
    let mut result = WorkflowResult {
        task_id: task.id.clone(),
        attempt_id: attempt.id.clone(),
        generation: attempt.generation,
        source_commit: task.spec.source_commit.clone(),
        success: false,
        exit_code: None,
        elapsed_ms: 0,
        checks: vec![],
        artifacts: vec![],
        distribution_failure: None,
        processes_reconciled: false,
    };
    let mut locks = Vec::new();
    let outcome = run(
        supervisor,
        task,
        attempt,
        workspace,
        state_dir,
        &root,
        &request,
        deadline,
        &mut result,
        &mut locks,
    )
    .await;
    // Even failures during preparation/launch must stop every daemon owned by this attempt.
    result.processes_reconciled = supervisor
        .terminate_attempt(&attempt.id, Duration::from_secs(10))
        .await?;
    drop(locks);
    if let Err(error) = outcome {
        let path = root.join("failure.txt");
        fs::write(&path, format!("{error:#}\n"))?;
        result.artifacts.push(artifact(&path, "workflow_error")?);
        result.success = false;
    }
    result.success &= result.processes_reconciled;
    result.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    durable_json(&root.join("result.json"), &result)?;
    Ok(result)
}

async fn run(
    supervisor: &Supervisor,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    workspace: &Path,
    state_dir: &Path,
    root: &Path,
    request: &WorkflowRequest,
    deadline: Instant,
    result: &mut WorkflowResult,
    locks: &mut Vec<File>,
) -> Result<()> {
    let lock_root = state_dir
        .parent()
        .context("attempt state parent missing")?
        .join("compiler-locks");
    let mut env = owned_environment();
    env.extend(request.profile.environment.clone());
    env.insert("RUSTC".into(), request.rustc.to_string_lossy().into_owned());
    env.insert("CARGO_INCREMENTAL".into(), "0".into());
    env.insert("RUSTC_WRAPPER".into(), String::new());
    env.insert("RUSTC_WORKSPACE_WRAPPER".into(), String::new());
    env.insert(
        "CARGO_BUILD_JOBS".into(),
        (attempt.resources.cpu_millis / 1000).max(1).to_string(),
    );
    let checks = [
        (
            "source",
            &request.git,
            crate::workspace::git_arguments(vec!["rev-parse".into(), "HEAD".into()]),
            task.spec.source_commit.as_str(),
        ),
        (
            "rustc",
            &request.rustc,
            vec!["--version".into()],
            request.profile.rustc_version.as_str(),
        ),
        (
            "rustc-verbose",
            &request.rustc,
            vec!["-vV".into()],
            request.profile.rustc_verbose.as_str(),
        ),
        (
            "cargo",
            &request.cargo,
            vec!["--version".into()],
            request.profile.cargo_version.as_str(),
        ),
        (
            "clean-source",
            &request.git,
            crate::workspace::git_arguments(vec![
                "status".into(),
                "--porcelain".into(),
                "--untracked-files=all".into(),
            ]),
            "",
        ),
    ];
    for (name, program, args, expected) in checks {
        let observed = command(
            supervisor,
            &format!("{}-{name}", attempt.id),
            &descriptor(program, args, workspace, attempt, &env),
            root,
            deadline,
        )
        .await?;
        collect_observation(&observed, &mut result.artifacts)?;
        let passed = observed.code == Some(0)
            && !observed.truncated
            && fs::read_to_string(&observed.stdout)?.trim() == expected;
        result.checks.push(CheckEvidence {
            name: name.into(),
            command: program.display().to_string(),
            source_sha: task.spec.source_commit.clone(),
            passed,
            artifact_digest: Some(artifact(&observed.stdout, "pin")?.sha256),
        });
        anyhow::ensure!(passed, "{name} pin verification failed");
    }
    let sysroot = command(
        supervisor,
        &format!("{}-sysroot", attempt.id),
        &descriptor(
            &request.rustc,
            vec!["--print".into(), "sysroot".into()],
            workspace,
            attempt,
            &env,
        ),
        root,
        deadline,
    )
    .await?;
    collect_observation(&sysroot, &mut result.artifacts)?;
    anyhow::ensure!(
        sysroot.code == Some(0) && !sysroot.truncated,
        "compiler sysroot query failed"
    );
    let sysroot_path = PathBuf::from(fs::read_to_string(&sysroot.stdout)?.trim());
    let observed_sysroot =
        tokio::task::spawn_blocking(move || sysroot_digest(&sysroot_path)).await??;
    let sysroot_passed = observed_sysroot.eq_ignore_ascii_case(&request.profile.sysroot_sha256);
    let sysroot_evidence = root.join("sysroot-digest.txt");
    fs::write(&sysroot_evidence, format!("{observed_sysroot}\n"))?;
    let sysroot_artifact = artifact(&sysroot_evidence, "toolchain_pin")?;
    result.checks.push(CheckEvidence {
        name: "sysroot-content".into(),
        command: "SHA-256 sorted sysroot bin/lib tree".into(),
        source_sha: task.spec.source_commit.clone(),
        passed: sysroot_passed,
        artifact_digest: Some(sysroot_artifact.sha256.clone()),
    });
    result.artifacts.push(sysroot_artifact);
    anyhow::ensure!(
        sysroot_passed,
        "compiler sysroot content differs from admitted pin"
    );
    if let WorkflowOperation::CompilerService {
        pool,
        role,
        server_id,
        executable,
        executable_sha256,
        credential_file,
        scheduler_listen,
        scheduler_secret_file,
        bubblewrap,
        resource_scope_executable,
    } = &request.operation
    {
        anyhow::ensure!(
            request.distributed.is_none(),
            "service cannot initiate a distributed build"
        );
        anyhow::ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64")),
            "pinned compiler engine requires the reviewed native x86_64 Linux environment"
        );
        pool.validate(&[])?;
        anyhow::ensure!(
            artifact(executable, "engine")?.sha256 == *executable_sha256,
            "compiler service executable digest differs"
        );
        anyhow::ensure!(
            credential_file.is_absolute(),
            "service credential must be an owner-provisioned absolute path"
        );
        let credential = fs::read_to_string(credential_file)?;
        let mut child = match role {
            ServiceRole::Scheduler => {
                let secret_file = scheduler_secret_file
                    .as_ref()
                    .context("scheduler auth secret file missing")?;
                anyhow::ensure!(
                    secret_file.is_absolute(),
                    "scheduler auth secret must be owner-provisioned"
                );
                let secret = fs::read_to_string(secret_file)?;
                let config = pool.write_scheduler_config(
                    root,
                    scheduler_listen
                        .as_deref()
                        .context("scheduler listen address missing")?,
                    credential.trim(),
                    secret.trim(),
                )?;
                pool.scheduler_descriptor(executable, config, workspace, &attempt.id)
            }
            ServiceRole::Server => {
                let server = pool
                    .servers
                    .iter()
                    .find(|s| Some(&s.device_id) == server_id.as_ref())
                    .context("compiler server not registered")?;
                anyhow::ensure!(
                    server.device_id == attempt.worker_id
                        && attempt.resources.cpu_millis
                            >= server
                                .capacity_units
                                .checked_mul(1000)
                                .context("compiler capacity overflow")?,
                    "server does not match admitted worker/capacity"
                );
                let bwrap = bubblewrap
                    .as_ref()
                    .context("reviewed bubblewrap path missing")?;
                anyhow::ensure!(
                    bwrap.is_absolute() && bwrap.is_file(),
                    "bubblewrap executable unavailable"
                );
                let config = pool.write_server_config(
                    server,
                    root,
                    &root.join("compiler-cache"),
                    &root.join("compiler-builds"),
                    bwrap,
                    credential.trim(),
                )?;
                pool.server_descriptor(server, executable, config, workspace, &attempt.id)?
            }
        };
        child.env.extend(env);
        anyhow::ensure!(
            resource_scope_executable.is_absolute() && resource_scope_executable.is_file(),
            "reviewed systemd-run resource scope executable unavailable"
        );
        child.env.insert(
            "DACS_COMPILER_SLOTS".into(),
            (attempt.resources.cpu_millis / 1000).max(1).to_string(),
        );
        let mut scoped_args = vec![
            "--scope".into(),
            "--quiet".into(),
            "--collect".into(),
            format!("--unit=dacs-{}", attempt.id),
            format!("--property=CPUQuota={}%", attempt.resources.cpu_millis / 10),
            format!("--property=MemoryMax={}M", attempt.resources.memory_mb),
            "--".into(),
            child.program.to_string_lossy().into_owned(),
        ];
        scoped_args.append(&mut child.args);
        child.program = resource_scope_executable.clone();
        child.args = scoped_args;
        locks.push(acquire_lock(
            &lock_root,
            &format!(
                "service-{}-{}",
                pool.pool_id,
                if matches!(role, ServiceRole::Server) {
                    "server"
                } else {
                    "scheduler"
                }
            ),
        )?);
        let observed = command(
            supervisor,
            &format!("{}-service", attempt.id),
            &child,
            root,
            deadline,
        )
        .await?;
        collect_observation(&observed, &mut result.artifacts)?;
        result.exit_code = observed.code;
        // An unexpected service exit is failure, including an unrequested exit(0).
        result.success = false;
        return Ok(());
    }
    let mut daemon_drains = None;
    let mut daemon_id = None;
    if let Some(distributed) = &request.distributed {
        anyhow::ensure!(
            matches!(request.operation, WorkflowOperation::Build),
            "distributed compilation is a build, not test/shard execution"
        );
        distributed.pool.validate(&[])?;
        anyhow::ensure!(
            distributed.claim.pool_id == distributed.pool.pool_id
                && !distributed.claim.service_attempt_ids.is_empty(),
            "pool admission claim missing"
        );
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "distributed profile requires Linux initiating environment"
        );
        let native_host = request
            .profile
            .rustc_verbose
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("rustc verbose host missing")?;
        anyhow::ensure!(
            native_host == request.profile.target && native_host.contains("-linux-"),
            "distribution proof requires a native Linux target"
        );
        locks.push(acquire_lock(&lock_root, &distributed.pool.pool_id)?);
        locks.push(acquire_lock(
            &lock_root,
            &format!("port-{}", request.profile.sccache_server_port),
        )?);
        anyhow::ensure!(
            artifact(&distributed.sccache, "engine")?.sha256 == distributed.sccache_sha256,
            "instrumented engine binary digest differs"
        );
        anyhow::ensure!(
            artifact(&distributed.provenance_wrapper, "wrapper")?.sha256
                == distributed.provenance_wrapper_sha256,
            "provenance wrapper binary digest differs"
        );
        // Never connect to, restart, or stop a daemon already listening on this port.
        let probe = std::net::TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            request.profile.sccache_server_port,
        ))
        .context("private sccache port is already occupied")?;
        let token = fs::read_to_string(&distributed.client_token_file)?;
        let config = request.profile.write_sccache_client_config(
            root,
            &distributed.pool.scheduler_url,
            token.trim(),
        )?;
        let cache_root = root.join(&request.profile.profile_id);
        fs::create_dir_all(&cache_root)?;
        let mut daemon = request.profile.sccache_daemon_descriptor(
            &distributed.sccache,
            &config,
            root,
            &attempt.id,
        );
        daemon.env.extend(env.clone());
        daemon.env.insert("SCCACHE_LOG".into(), "debug".into());
        let id = format!("{}-sccache", attempt.id);
        drop(probe);
        supervisor.launch(&id, &daemon).await?;
        let io = supervisor.take_io(&id).await?;
        drop(io.stdin);
        daemon_drains = Some((
            tokio::spawn(drain(io.stdout, root.join("daemon.stdout.log"))),
            tokio::spawn(drain_stream(
                io.stderr,
                root.join("daemon.stderr.log"),
                Some(root.join("client-engine.jsonl")),
            )),
        ));
        daemon_id = Some(id.clone());
        let ready_deadline = deadline.min(Instant::now() + Duration::from_secs(15));
        loop {
            if matches!(supervisor.state(&id).await?, ProcessState::Exited { .. }) {
                bail!("private sccache daemon exited during startup");
            }
            if tokio::net::TcpStream::connect((
                std::net::Ipv4Addr::LOCALHOST,
                request.profile.sccache_server_port,
            ))
            .await
            .is_ok()
            {
                break;
            }
            anyhow::ensure!(
                Instant::now() < ready_deadline,
                "private sccache daemon readiness timed out"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        env.insert(
            "RUSTC_WRAPPER".into(),
            distributed
                .provenance_wrapper
                .to_string_lossy()
                .into_owned(),
        );
        env.insert(
            "DACS_SCCACHE".into(),
            distributed.sccache.to_string_lossy().into_owned(),
        );
        env.insert(
            "DACS_CONSUMPTION_LOG".into(),
            root.join("consumption.jsonl")
                .to_string_lossy()
                .into_owned(),
        );
        env.insert("SCCACHE_CONF".into(), config.to_string_lossy().into_owned());
        env.insert(
            "SCCACHE_DIR".into(),
            cache_root.join("cache").to_string_lossy().into_owned(),
        );
        env.insert(
            "SCCACHE_SERVER_PORT".into(),
            request.profile.sccache_server_port.to_string(),
        );
    }
    let shards = match &request.operation {
        WorkflowOperation::Build => vec![TestShard {
            name: "build".into(),
            filter: String::new(),
            exact: false,
        }],
        WorkflowOperation::Test { shards } => {
            anyhow::ensure!(!shards.is_empty(), "test workflow must contain shards");
            shards.clone()
        }
        WorkflowOperation::CompilerService { .. } => unreachable!(),
    };
    let mut names = std::collections::BTreeSet::new();
    let mut passed = true;
    for shard in shards {
        anyhow::ensure!(
            label(&shard.name) && names.insert(shard.name.clone()),
            "shard names must be safe and unique"
        );
        let shard_root = root.join(&shard.name);
        fs::create_dir(&shard_root)?;
        let checkout = shard_root.join("workspace");
        for (phase, args) in [
            (
                "clone",
                vec![
                    "clone".into(),
                    "--local".into(),
                    "--no-hardlinks".into(),
                    "--no-checkout".into(),
                    workspace.to_string_lossy().into_owned(),
                    checkout.to_string_lossy().into_owned(),
                ],
            ),
            (
                "checkout",
                vec![
                    "-C".into(),
                    checkout.to_string_lossy().into_owned(),
                    "checkout".into(),
                    "--detach".into(),
                    task.spec.source_commit.clone(),
                ],
            ),
        ] {
            let observed = command(
                supervisor,
                &format!("{}-{}-{phase}", attempt.id, shard.name),
                &descriptor(
                    &request.git,
                    crate::workspace::git_arguments(args),
                    workspace,
                    attempt,
                    &env,
                ),
                root,
                deadline,
            )
            .await?;
            collect_observation(&observed, &mut result.artifacts)?;
            anyhow::ensure!(observed.code == Some(0), "isolated shard {phase} failed");
        }
        request
            .profile
            .verify_lockfile(&checkout.join("Cargo.lock"))?;
        let temp = shard_root.join("tmp");
        fs::create_dir(&temp)?;
        let mut child_env = env.clone();
        child_env.insert(
            "CARGO_TARGET_DIR".into(),
            shard_root.join("target").to_string_lossy().into_owned(),
        );
        child_env.insert(
            "DACS_SHARD_DIR".into(),
            shard_root.to_string_lossy().into_owned(),
        );
        child_env.insert(
            "DACS_SHARD_DATABASE".into(),
            shard_root.join("test.db").to_string_lossy().into_owned(),
        );
        child_env.insert("DACS_TEST_LISTEN".into(), "127.0.0.1:0".into());
        for name in ["TMPDIR", "TMP", "TEMP"] {
            child_env.insert(name.into(), temp.to_string_lossy().into_owned());
        }
        if !request.profile.rustflags.is_empty() {
            child_env.insert(
                "CARGO_ENCODED_RUSTFLAGS".into(),
                request.profile.rustflags.join("\u{1f}"),
            );
        }
        let is_test = matches!(request.operation, WorkflowOperation::Test { .. });
        let mut args: Vec<String> = vec![
            if is_test { "test" } else { "build" }.into(),
            "--locked".into(),
            "--message-format=json-render-diagnostics".into(),
            "--package".into(),
            request.profile.package.clone(),
            "--target".into(),
            request.profile.target.clone(),
            "--profile".into(),
            request.profile.cargo_profile.clone(),
        ];
        if !request.profile.default_features {
            args.push("--no-default-features".into());
        }
        if !request.profile.features.is_empty() {
            args.extend(["--features".into(), request.profile.features.join(",")]);
        }
        if is_test {
            args.push(shard.filter);
            args.push("--".into());
            if shard.exact {
                args.push("--exact".into());
            }
        }
        let invocation = args.join(" ");
        let observed = command(
            supervisor,
            &format!("{}-{}", attempt.id, shard.name),
            &descriptor(&request.cargo, args, &checkout, attempt, &child_env),
            root,
            deadline,
        )
        .await?;
        collect_observation(&observed, &mut result.artifacts)?;
        request
            .profile
            .verify_lockfile(&checkout.join("Cargo.lock"))?;
        result.exit_code = observed.code;
        let success = observed.code == Some(0);
        passed &= success;
        result.checks.push(CheckEvidence {
            name: shard.name,
            command: format!("{} {invocation}", request.cargo.display()),
            source_sha: task.spec.source_commit.clone(),
            passed: success,
            artifact_digest: Some(artifact(&observed.stdout, "cargo")?.sha256),
        });
        // Cargo supplies real executable artifacts. Only paths confined to this
        // fresh target tree qualify; truncated logs never become proof of absence.
        for line in fs::read_to_string(&observed.stdout)?.lines() {
            let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if message["reason"] == "compiler-artifact" {
                if let Some(executable) = message["executable"].as_str() {
                    let path = PathBuf::from(executable).canonicalize()?;
                    anyhow::ensure!(
                        path.starts_with(shard_root.canonicalize()?),
                        "Cargo artifact escaped private target tree"
                    );
                    result.artifacts.push(artifact(&path, "executable")?);
                }
            }
        }
        if !success {
            break;
        } // no retry that hides a failing shard
    }
    if let Some(id) = daemon_id {
        anyhow::ensure!(
            supervisor.terminate(&id, Duration::from_secs(10)).await?,
            "private daemon remains unreconciled"
        );
        if let Some((out, err)) = daemon_drains {
            out.await??;
            err.await??;
        }
        for name in ["daemon.stdout.log", "daemon.stderr.log"] {
            let path = root.join(name);
            if path.is_file() {
                result
                    .artifacts
                    .push(artifact(&path, "compiler_engine_log")?);
            }
        }
    }
    let mut exercised = Vec::new();
    if passed {
        for (index, check) in request.exercise.iter().enumerate() {
            let executable = result
                .artifacts
                .iter()
                .find(|artifact| {
                    artifact.kind == "executable"
                        && Path::new(&artifact.path)
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy() == check.executable_name)
                })
                .context("behavior check executable was not produced by this Cargo build")?
                .clone();
            let observed = command(
                supervisor,
                &format!("{}-behavior-{index}", attempt.id),
                &descriptor(
                    Path::new(&executable.path),
                    check.args.clone(),
                    root,
                    attempt,
                    &env,
                ),
                root,
                deadline,
            )
            .await?;
            collect_observation(&observed, &mut result.artifacts)?;
            let success = observed.code == Some(0);
            passed &= success;
            result.checks.push(CheckEvidence {
                name: format!("behavior-{index}"),
                command: format!("{} {}", executable.path, check.args.join(" ")),
                source_sha: task.spec.source_commit.clone(),
                passed: success,
                artifact_digest: Some(artifact(&observed.stdout, "behavior")?.sha256),
            });
            if success {
                exercised.push(executable.sha256);
            } else {
                break;
            }
        }
    }
    if let Some(distributed) = &request.distributed {
        let proof = reconcile_distribution(
            distributed,
            request,
            attempt,
            state_dir,
            root,
            &exercised,
            deadline,
            result,
        )
        .await;
        if let Err(error) = proof {
            let failure = format!("{error:#}");
            let evidence = root.join("distribution-failure.txt");
            fs::write(&evidence, &failure)?;
            result
                .artifacts
                .push(artifact(&evidence, "distribution_failure")?);
            if distributed.required {
                result.distribution_failure = Some(failure);
            }
        }
    }
    for relative in &request.artifacts {
        result
            .artifacts
            .push(artifact(&confined(workspace, relative)?, "requested")?);
    }
    request
        .profile
        .verify_lockfile(&workspace.join("Cargo.lock"))?;
    result.success = passed && result.distribution_failure.is_none();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_cannot_escape_state_directory() {
        for value in ["", "..", "x/y", "C:\\x"] {
            assert!(!label(value));
        }
        assert!(label("shard-1"));
    }
    #[test]
    fn artifact_parent_and_absolute_paths_are_rejected() {
        assert!(confined(Path::new("."), Path::new("../Cargo.lock")).is_err());
        assert!(confined(Path::new("."), &std::env::current_dir().unwrap()).is_err());
    }
    #[test]
    fn request_does_not_accept_fabricated_evidence() {
        let value = serde_json::json!({"pool":{},"claim":{},"sccache":"sccache","client_token_file":"token","required":true,"required_remote_hosts":["a","b"],"actions":[{"consumed_by_build":true}]});
        assert!(serde_json::from_value::<DistributedRun>(value).is_err());
    }
    #[test]
    fn local_daemon_lock_is_exclusive() {
        let root = tempfile::tempdir().unwrap();
        let held = acquire_lock(root.path(), "pool").unwrap();
        assert!(acquire_lock(root.path(), "pool").is_err());
        drop(held);
        assert!(acquire_lock(root.path(), "pool").is_ok());
    }
    #[tokio::test]
    async fn capture_drains_beyond_durable_log_limit() {
        use tokio::io::AsyncWriteExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bounded.log");
        let (mut writer, reader) = tokio::io::duplex(4096);
        let writing = tokio::spawn(async move {
            let block = [b'x'; 32768];
            for _ in 0..(LOG_LIMIT / block.len() as u64 + 2) {
                writer.write_all(&block).await.unwrap();
            }
        });
        assert!(drain(reader, path.clone()).await.unwrap());
        writing.await.unwrap();
        assert_eq!(fs::metadata(path).unwrap().len(), LOG_LIMIT);
    }

    #[tokio::test]
    async fn supervised_command_preserves_real_nonzero_exit() {
        let root = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        let (program, args) = if cfg!(windows) {
            (
                PathBuf::from("cmd.exe"),
                vec!["/d".into(), "/c".into(), "exit 7".into()],
            )
        } else {
            (PathBuf::from("/bin/sh"), vec!["-c".into(), "exit 7".into()])
        };
        let child = ManagedChild {
            name: "exit-test".into(),
            program,
            args,
            cwd: root.path().into(),
            env: owned_environment(),
            effect_class: "host_level".into(),
            reservation_id: Some("test-attempt".into()),
        };
        let observed = command(
            &supervisor,
            "nonzero",
            &child,
            root.path(),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(observed.code, Some(7));
        assert!(observed.stdout.is_file() && observed.stderr.is_file());
        assert!(
            supervisor
                .terminate_attempt("test-attempt", Duration::from_secs(1))
                .await
                .unwrap()
        );
    }

    #[test]
    fn consumption_requires_a_successful_path_to_exercised_binary() {
        let records = vec![
            serde_json::json!({"schema":1,"kind":"consume","exit_code":0,"inputs":[{"sha256":"remote-a"}],"outputs":[{"sha256":"middle"}]}),
            serde_json::json!({"schema":1,"kind":"consume","exit_code":0,"inputs":[{"sha256":"middle"},{"sha256":"remote-b"}],"outputs":[{"sha256":"binary"}]}),
            serde_json::json!({"schema":1,"kind":"consume","exit_code":1,"inputs":[{"sha256":"unused"}],"outputs":[{"sha256":"binary"}]}),
        ];
        let consumed = consumed_digests(&records, &["binary".into()]).unwrap();
        assert!(consumed.contains("remote-a") && consumed.contains("remote-b"));
        assert!(!consumed.contains("unused"));
        assert!(consumed_digests(&records, &[]).unwrap().is_empty());
    }

    fn proof_fixture() -> (
        DistributedRun,
        BuildProfile,
        serde_json::Value,
        dacs_protocol::workflow::CompilerTelemetryRecord,
    ) {
        let profile = BuildProfile {
            profile_id: "profile".into(),
            pool_id: "pool".into(),
            source_commit: "a".repeat(40),
            cargo_lock_sha256: "a".repeat(64),
            rustc_version: "rustc pinned".into(),
            cargo_version: "cargo pinned".into(),
            rustc_verbose: "host: x86_64-unknown-linux-gnu".into(),
            sysroot_sha256: "b".repeat(64),
            target: "x86_64-unknown-linux-gnu".into(),
            package: "example".into(),
            features: vec![],
            default_features: true,
            rustflags: vec![],
            environment: BTreeMap::new(),
            cargo_profile: "release".into(),
            sccache_server_port: 14226,
        };
        let distributed = DistributedRun {
            pool: CompilerPool {
                pool_id: "pool".into(),
                scheduler_url: "https://100.64.0.1".into(),
                servers: vec![crate::build_execution::CompilerServer {
                    device_id: "worker".into(),
                    physical_host_fingerprint: "remote".into(),
                    public_addr: "100.64.0.2:10501".into(),
                    capacity_units: 1,
                    credential_ref: "local-token".into(),
                }],
            },
            claim: PoolClaim {
                pool_id: "pool".into(),
                service_attempt_ids: vec!["service".into()],
            },
            sccache: "sccache".into(),
            sccache_sha256: "a".repeat(64),
            provenance_wrapper: "wrapper".into(),
            provenance_wrapper_sha256: "b".repeat(64),
            initiator_physical_host: "initiator".into(),
            client_token_file: "token".into(),
            required: true,
            required_remote_hosts: ["remote".into(), "other".into()],
        };
        let output = serde_json::json!({"path":"/target/libcrate.rlib","sha256":"a".repeat(64),"bytes":12,"transferred_bytes":8});
        let receipt = serde_json::json!({"schema":1,"engine":"sccache-0.10.0-dacs-1","kind":"received","job_id":"1","server_addr":"100.64.0.2:10501","exit_code":0,"outputs":[output.clone()]});
        let server = dacs_protocol::workflow::CompilerTelemetryRecord {
            worker_id: "worker".into(),
            service_attempt_id: "service".into(),
            record: serde_json::json!({"schema":1,"engine":"sccache-0.10.0-dacs-1","kind":"server_complete","job_id":"1","exit_code":0,"elapsed_ms":9,"outputs":[output]}),
        };
        (distributed, profile, receipt, server)
    }
    #[test]
    fn proof_requires_authenticated_matching_server_and_output() {
        let (distributed, profile, receipt, mut server) = proof_fixture();
        let consumed = std::collections::BTreeSet::from(["a".repeat(64)]);
        let actions = correlate_actions(
            &distributed,
            &profile,
            std::slice::from_ref(&receipt),
            std::slice::from_ref(&server),
            &consumed,
        )
        .unwrap();
        assert!(actions[0].consumed_by_build);
        assert_eq!(actions[0].transferred_bytes, 8);
        server.worker_id = "forged-worker".into();
        assert!(
            correlate_actions(
                &distributed,
                &profile,
                std::slice::from_ref(&receipt),
                std::slice::from_ref(&server),
                &consumed
            )
            .is_err()
        );
        server.worker_id = "worker".into();
        server.record["outputs"][0]["sha256"] = serde_json::json!("b".repeat(64));
        assert!(
            correlate_actions(&distributed, &profile, &[receipt], &[server], &consumed).is_err()
        );
    }
    #[test]
    fn engine_replay_and_unconsumed_output_never_pass() {
        let (distributed, profile, receipt, server) = proof_fixture();
        let actions = correlate_actions(
            &distributed,
            &profile,
            std::slice::from_ref(&receipt),
            std::slice::from_ref(&server),
            &Default::default(),
        )
        .unwrap();
        assert!(!actions[0].consumed_by_build);
        assert!(
            correlate_actions(
                &distributed,
                &profile,
                &[receipt.clone(), receipt],
                &[server],
                &Default::default()
            )
            .is_err()
        );
    }
}

#[derive(Deserialize)]
struct WorkflowTransport {
    coordinator_url: String,
    credential: String,
    ca_certificate: Option<PathBuf>,
}
fn json_lines(path: &Path) -> Result<Vec<serde_json::Value>> {
    anyhow::ensure!(
        fs::metadata(path)?.len() <= LOG_LIMIT,
        "evidence exceeds bounded journal size"
    );
    fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}
fn consumed_digests(
    records: &[serde_json::Value],
    exercised: &[String],
) -> Result<std::collections::BTreeSet<String>> {
    let mut graph = BTreeMap::<String, Vec<String>>::new();
    for record in records {
        anyhow::ensure!(
            record["schema"] == 1 && record["kind"] == "consume",
            "invalid compiler consumption record"
        );
        if record["exit_code"] != 0 {
            continue;
        }
        let inputs = record["inputs"]
            .as_array()
            .context("compiler inputs missing")?
            .iter()
            .map(|v| {
                v["sha256"]
                    .as_str()
                    .map(str::to_owned)
                    .context("compiler input digest missing")
            })
            .collect::<Result<Vec<_>>>()?;
        for output in record["outputs"]
            .as_array()
            .context("compiler outputs missing")?
        {
            let digest = output["sha256"]
                .as_str()
                .context("compiler output digest missing")?
                .to_owned();
            if let Some(previous) = graph.insert(digest, inputs.clone()) {
                anyhow::ensure!(previous == inputs, "ambiguous output consumption graph");
            }
        }
    }
    let mut pending = exercised.to_vec();
    let mut consumed = std::collections::BTreeSet::new();
    while let Some(digest) = pending.pop() {
        if consumed.insert(digest.clone()) {
            if let Some(inputs) = graph.get(&digest) {
                pending.extend(inputs.iter().cloned());
            }
        }
    }
    Ok(consumed)
}
fn correlate_actions(
    distributed: &DistributedRun,
    profile: &BuildProfile,
    received: &[serde_json::Value],
    servers: &[dacs_protocol::workflow::CompilerTelemetryRecord],
    consumed: &std::collections::BTreeSet<String>,
) -> Result<Vec<crate::build_execution::CompileActionEvidence>> {
    let mut actions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for receipt in received {
        anyhow::ensure!(
            receipt["schema"] == 1
                && receipt["engine"] == "sccache-0.10.0-dacs-1"
                && receipt["kind"] == "received",
            "unsupported initiator engine record"
        );
        let job = receipt["job_id"]
            .as_str()
            .context("received job identity missing")?;
        let address = receipt["server_addr"]
            .as_str()
            .context("received server address missing")?;
        let member = distributed
            .pool
            .servers
            .iter()
            .find(|server| server.public_addr == address)
            .context("engine used a server outside the reserved compiler pool")?;
        let identity = format!("{}:{job}", member.device_id);
        anyhow::ensure!(seen.insert(identity.clone()), "duplicate compiler receipt");
        let observation = servers
            .iter()
            .find(|server| {
                server.worker_id == member.device_id
                    && distributed
                        .claim
                        .service_attempt_ids
                        .contains(&server.service_attempt_id)
                    && server.record["job_id"].as_str() == Some(job)
            })
            .context("authenticated server completion has not arrived")?;
        let outputs = receipt["outputs"]
            .as_array()
            .context("received outputs missing")?;
        let remote_outputs = observation.record["outputs"]
            .as_array()
            .context("server outputs missing")?;
        let mut consumed_output = None;
        let mut consumed_sha256 = None;
        let transferred = remote_outputs.iter().try_fold(0u64, |total, output| {
            total
                .checked_add(
                    output["transferred_bytes"]
                        .as_u64()
                        .context("transferred byte count missing")?,
                )
                .context("transfer count overflow")
        })?;
        for output in outputs {
            let path = output["path"]
                .as_str()
                .context("received output path missing")?;
            // Stock Rust output rewriting changes dep-info paths; only unchanged
            // compiled libraries establish consumed compiler output provenance.
            if !path.ends_with(".rlib") {
                continue;
            }
            let digest = output["sha256"]
                .as_str()
                .context("received digest missing")?;
            anyhow::ensure!(
                remote_outputs
                    .iter()
                    .any(|remote| remote["sha256"] == output["sha256"]
                        && remote["bytes"] == output["bytes"]),
                "server and initiating output digests differ"
            );
            if path.ends_with(".rlib") && consumed.contains(digest) {
                consumed_output = Some(path);
                consumed_sha256 = Some(digest.to_owned());
            }
        }
        let success = receipt["exit_code"] == 0 && observation.record["exit_code"] == 0;
        actions.push(crate::build_execution::CompileActionEvidence {
            invocation_id: identity,
            crate_name: consumed_output
                .and_then(|p| Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            target: profile.target.clone(),
            toolchain: profile.rustc_version.clone(),
            device_id: member.device_id.clone(),
            physical_host_fingerprint: member.physical_host_fingerprint.clone(),
            remote: true,
            success,
            consumed_by_build: success && consumed_output.is_some(),
            cache_hit: false,
            local_fallback: false,
            attempt_count: 1,
            consumed_output_sha256: if success { consumed_sha256 } else { None },
            elapsed_ms: observation.record["elapsed_ms"]
                .as_u64()
                .context("engine timing missing")?,
            transferred_bytes: transferred,
        });
    }
    Ok(actions)
}
async fn reconcile_distribution(
    distributed: &DistributedRun,
    request: &WorkflowRequest,
    attempt: &AttemptRecord,
    state_dir: &Path,
    root: &Path,
    exercised: &[String],
    deadline: Instant,
    result: &mut WorkflowResult,
) -> Result<()> {
    let received = json_lines(&root.join("client-engine.jsonl"))?;
    let consumption = json_lines(&root.join("consumption.jsonl"))?;
    let consumed = consumed_digests(&consumption, exercised)?;
    let transport: WorkflowTransport =
        serde_json::from_reader(File::open(state_dir.join("workflow-transport.json"))?)?;
    let base = reqwest::Url::parse(&transport.coordinator_url)?;
    let loopback = base.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        base.scheme() == "https" || (base.scheme() == "http" && loopback),
        "compiler evidence transport requires HTTPS"
    );
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = transport.ca_certificate {
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&fs::read(ca)?)?);
    }
    let client = builder.build()?;
    let url = base.join(&format!("/api/compiler-evidence/{}", attempt.id))?;
    let evidence_deadline = deadline.min(Instant::now() + Duration::from_secs(15));
    let (server_records, actions) = loop {
        let mut response = client
            .get(url.clone())
            .bearer_auth(&transport.credential)
            .send()
            .await?
            .error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                body.len() + chunk.len() <= LOG_LIMIT as usize,
                "compiler evidence response exceeds bound"
            );
            body.extend_from_slice(&chunk);
        }
        let records: Vec<dacs_protocol::workflow::CompilerTelemetryRecord> =
            serde_json::from_slice(&body)?;
        match correlate_actions(
            distributed,
            &request.profile,
            &received,
            &records,
            &consumed,
        ) {
            Ok(actions) => break (records, actions),
            Err(error) if Instant::now() >= evidence_deadline => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    let server_path = root.join("authenticated-server-actions.json");
    durable_json(&server_path, &server_records)?;
    let actions_path = root.join("correlated-actions.json");
    durable_json(&actions_path, &actions)?;
    for (path, kind) in [
        (server_path, "authenticated_remote_actions"),
        (actions_path, "correlated_actions"),
        (root.join("client-engine.jsonl"), "client_engine_actions"),
        (root.join("consumption.jsonl"), "compiler_consumption"),
    ] {
        result.artifacts.push(artifact(&path, kind)?);
    }
    let proof = dacs_release_audit::BuildEvidence {
        source_revision: request.profile.source_commit.clone(),
        toolchain: request.profile.rustc_version.clone(),
        target: request.profile.target.clone(),
        initiator_physical_host: distributed.initiator_physical_host.clone(),
        required_remote_hosts: distributed.required_remote_hosts.iter().cloned().collect(),
        native_linux_profile_verified: cfg!(target_os = "linux")
            && request.profile.target.contains("-linux-"),
        clean_outputs: true,
        cold_cache: true,
        incremental_disabled: true,
        linked: result.exit_code == Some(0),
        behavioral_checks_passed: !exercised.is_empty(),
        actions: actions
            .iter()
            .map(|action| dacs_release_audit::RemoteAction {
                invocation_id: action.invocation_id.clone(),
                physical_host: action.physical_host_fingerprint.clone(),
                enrolled_worker: action.device_id.clone(),
                source_revision: request.profile.source_commit.clone(),
                toolchain: action.toolchain.clone(),
                target: action.target.clone(),
                consumed_output_digest: action.consumed_output_sha256.clone().unwrap_or_default(),
                successful_compilation: action.success,
                cache_hit: action.cache_hit,
            })
            .collect(),
    };
    dacs_release_audit::verify_distributed_build(&proof)
        .map_err(|errors| anyhow::anyhow!(errors.join("; ")))
}
