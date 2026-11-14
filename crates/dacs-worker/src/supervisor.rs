use anyhow::{Context, Result, bail};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::Mutex,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Starting,
    Running { pid: Option<u32> },
    Stopping,
    Exited { code: Option<i32> },
    Unreconciled,
}

/// Process descriptor supplied by the owning runtime/build adapter.
#[derive(Clone, Debug)]
pub struct ManagedChild {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub effect_class: String,
    pub reservation_id: Option<String>,
}

pub struct ProcessIo {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

struct Entry {
    child: Box<dyn ChildWrapper>,
    reservation_id: Option<String>,
    exit: Option<std::process::ExitStatus>,
    pid: u32,
    reconciled: bool,
}

impl Entry {
    fn poll_exit(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        // JobObject::try_wait consumes completion-port notifications, including
        // the last one its later wait needs. Poll only the leader on Windows;
        // terminate still signals and waits on the wrapper to reconcile the job.
        #[cfg(windows)]
        {
            self.child.inner_mut().try_wait()
        }
        #[cfg(not(windows))]
        {
            self.child.try_wait()
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ProcessReceipt {
    id: String,
    reservation_id: Option<String>,
    pid: Option<u32>,
    start_time: Option<u64>,
}

#[derive(Clone)]
pub struct Supervisor {
    children: Arc<Mutex<BTreeMap<String, Entry>>>,
    receipts: Option<PathBuf>,
    fenced: Arc<Mutex<BTreeSet<String>>>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(BTreeMap::new())),
            receipts: None,
            fenced: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub fn persistent(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        Ok(Self {
            receipts: Some(directory.to_owned()),
            ..Self::new()
        })
    }

    fn receipt_path(&self, id: &str) -> Result<Option<PathBuf>> {
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "invalid process identity"
        );
        Ok(self.receipts.as_ref().map(|p| p.join(format!("{id}.json"))))
    }

    fn save_receipt(&self, receipt: &ProcessReceipt) -> Result<()> {
        if let Some(path) = self.receipt_path(&receipt.id)? {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?;
            std::io::Write::write_all(&mut file, &serde_json::to_vec(receipt)?)?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn clear_receipt(&self, id: &str) -> Result<()> {
        if let Some(path) = self.receipt_path(id)? {
            match std::fs::remove_file(path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// No receipt is silently forgotten on restart. A launch interrupted before
    /// its PID was durable cannot prove stop and deliberately blocks admission.
    pub async fn reconcile_previous(&self) -> Result<Vec<String>> {
        let Some(directory) = &self.receipts else {
            return Ok(Vec::new());
        };
        let mut stopped = Vec::new();
        for item in std::fs::read_dir(directory)? {
            let path = item?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let receipt: ProcessReceipt = serde_json::from_slice(&std::fs::read(&path)?)?;
            let pid = receipt
                .pid
                .context("interrupted process launch: local process reconciliation required")?;
            let system = sysinfo::System::new_all();
            if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
                anyhow::ensure!(
                    Some(process.start_time()) == receipt.start_time,
                    "PID reused; original process tree cannot be certified stopped"
                );
                #[cfg(unix)]
                {
                    use nix::{
                        sys::signal::{Signal, killpg},
                        unistd::Pid,
                    };
                    killpg(Pid::from_raw(pid as i32), Signal::SIGKILL)?;
                }
                #[cfg(windows)]
                {
                    // Kill-on-close jobs normally already exited after owner loss.
                    // Never kill an arbitrary PID tree as a substitute for a lost job.
                    bail!("previous Windows job still active; owner must reconcile before restart");
                }
                #[cfg(unix)]
                {
                    let mut system = system;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                    anyhow::ensure!(
                        system.process(sysinfo::Pid::from_u32(pid)).is_none(),
                        "prior process has not terminated"
                    );
                }
            }
            #[cfg(unix)]
            {
                use nix::{sys::signal::killpg, unistd::Pid};
                match killpg(Pid::from_raw(pid as i32), None) {
                    Err(nix::errno::Errno::ESRCH) => (),
                    _ => bail!("prior process group remains or cannot be inspected"),
                }
            }
            std::fs::remove_file(path)?;
            if let Some(attempt) = receipt.reservation_id {
                stopped.push(attempt);
            }
        }
        stopped.sort();
        stopped.dedup();
        Ok(stopped)
    }

    /// Launch with piped stdio and a per-attempt working directory. Callers must
    /// create an independent clone/workspace before invoking this method.
    pub async fn launch(&self, id: impl Into<String>, descriptor: &ManagedChild) -> Result<u32> {
        let id = id.into();
        let mut children = self.children.lock().await;
        if let Some(attempt) = &descriptor.reservation_id {
            anyhow::ensure!(
                !self.fenced.lock().await.contains(attempt),
                "attempt was fenced; new process launch refused"
            );
        }
        anyhow::ensure!(
            !children.contains_key(&id),
            "managed process id is already used"
        );
        let mut receipt = ProcessReceipt {
            id: id.clone(),
            reservation_id: descriptor.reservation_id.clone(),
            pid: None,
            start_time: None,
        };
        self.save_receipt(&receipt)?;
        let mut command = CommandWrap::with_new(&descriptor.program, |command| {
            command
                .args(&descriptor.args)
                .current_dir(&descriptor.cwd)
                .env_clear()
                .envs(&descriptor.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        command.wrap(KillOnDrop);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::tokio::JobObject);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.clear_receipt(&id)?;
                return Err(error).with_context(|| format!("launch {}", descriptor.name));
            }
        };
        let pid = child.id().context("spawned process has no PID")?;
        receipt.pid = Some(pid);
        receipt.start_time = sysinfo::System::new_all()
            .process(sysinfo::Pid::from_u32(pid))
            .map(|p| p.start_time());
        if let Err(error) = self.save_receipt(&receipt) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(error);
        }
        children.insert(
            id,
            Entry {
                child,
                reservation_id: descriptor.reservation_id.clone(),
                exit: None,
                pid,
                reconciled: false,
            },
        );
        Ok(pid)
    }

    pub async fn take_io(&self, id: &str) -> Result<ProcessIo> {
        let mut children = self.children.lock().await;
        let entry = children.get_mut(id).context("unknown managed process")?;
        anyhow::ensure!(
            entry.child.stdin().is_some()
                && entry.child.stdout().is_some()
                && entry.child.stderr().is_some(),
            "process I/O already taken"
        );
        Ok(ProcessIo {
            stdin: entry.child.stdin().take().context("missing stdin")?,
            stdout: entry.child.stdout().take().context("missing stdout")?,
            stderr: entry.child.stderr().take().context("missing stderr")?,
        })
    }

    pub async fn state(&self, id: &str) -> Result<ProcessState> {
        let mut children = self.children.lock().await;
        let entry = children.get_mut(id).context("unknown managed process")?;
        if entry.exit.is_none() {
            entry.exit = entry.poll_exit()?;
        }
        match entry.exit {
            Some(status) => Ok(ProcessState::Exited {
                code: status.code(),
            }),
            None => Ok(ProcessState::Running {
                pid: entry.child.id(),
            }),
        }
    }

    /// Stop and wait for actual process termination. Returns false if the process
    /// remains alive after grace; it must stay accounted and be reconciled.
    pub async fn terminate(&self, id: &str, grace: Duration) -> Result<bool> {
        let mut children = self.children.lock().await;
        let Some(entry) = children.get_mut(id) else {
            return Ok(true);
        };
        if entry.reconciled {
            return Ok(true);
        }
        // Always signal the wrapper, even after the leader exited: descendants
        // may still own pipes, desktop access, or compiler reservations.
        if let Err(error) = entry.child.start_kill() {
            if error.kind() != std::io::ErrorKind::InvalidInput && error.raw_os_error() != Some(3) {
                return Err(error.into());
            }
        }
        match tokio::time::timeout(grace, entry.child.wait()).await {
            Ok(Ok(status)) => {
                if entry.exit.is_none() {
                    entry.exit = Some(status);
                }
                #[cfg(unix)]
                {
                    use nix::{sys::signal::killpg, unistd::Pid};
                    let deadline = tokio::time::Instant::now() + grace;
                    loop {
                        match killpg(Pid::from_raw(entry.pid as i32), None) {
                            Err(nix::errno::Errno::ESRCH) => break,
                            Err(error) => return Err(error.into()),
                            Ok(()) if tokio::time::Instant::now() < deadline => {
                                tokio::time::sleep(Duration::from_millis(25)).await
                            }
                            Ok(()) => return Ok(false),
                        }
                    }
                }
                entry.reconciled = true;
                self.clear_receipt(id)?;
                Ok(true)
            }
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Ok(false),
        }
    }

    pub async fn terminate_attempt(&self, attempt_id: &str, grace: Duration) -> Result<bool> {
        // Acquire the children lock before setting the fence, matching launch's
        // lock order. No process can slip between fencing and enumeration.
        {
            let _children = self.children.lock().await;
            self.fenced.lock().await.insert(attempt_id.to_owned());
        }
        let ids: Vec<_> = self
            .children
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| entry.reservation_id.as_deref() == Some(attempt_id))
            .map(|(id, _)| id.clone())
            .collect();
        let mut stopped = true;
        for id in ids {
            stopped &= self.terminate(&id, grace).await?;
        }
        Ok(stopped)
    }

    /// Drain does not signal admitted processes; it reports which remain active.
    pub async fn active_ids(&self) -> Vec<String> {
        let mut children = self.children.lock().await;
        let mut active = Vec::new();
        for (id, entry) in children.iter_mut() {
            if entry.exit.is_none() {
                if let Ok(Some(status)) = entry.poll_exit() {
                    entry.exit = Some(status);
                }
            }
            if !entry.reconciled {
                active.push(id.clone());
            }
        }
        active
    }

    /// Call only after consumers stored exit/evidence. Keeps stop accounting
    /// separate from releasing process handles, and refuses uncertain trees.
    pub async fn forget_attempt(&self, attempt: &str) -> Result<()> {
        let mut children = self.children.lock().await;
        anyhow::ensure!(
            children
                .values()
                .filter(|e| e.reservation_id.as_deref() == Some(attempt))
                .all(|e| e.reconciled),
            "cannot forget unreconciled attempt"
        );
        children.retain(|_, e| e.reservation_id.as_deref() != Some(attempt));
        Ok(())
    }

    pub async fn active_attempts(&self) -> Vec<String> {
        let children = self.children.lock().await;
        let mut attempts: Vec<_> = children
            .values()
            .filter(|e| !e.reconciled)
            .filter_map(|e| e.reservation_id.clone())
            .collect();
        attempts.sort();
        attempts.dedup();
        attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn descriptor(directory: &Path, script: &str) -> ManagedChild {
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from(
                std::env::var_os("COMSPEC")
                    .unwrap_or_else(|| "C:\\Windows\\System32\\cmd.exe".into()),
            ),
            vec!["/d".into(), "/c".into(), script.into()],
        );
        #[cfg(not(windows))]
        let (program, args) = (PathBuf::from("/bin/sh"), vec!["-c".into(), script.into()]);
        ManagedChild {
            name: "regression process".into(),
            program,
            args,
            cwd: directory.to_owned(),
            env: crate::workspace::owner_environment(),
            effect_class: "workspace".into(),
            reservation_id: Some("attempt".into()),
        }
    }

    #[tokio::test]
    async fn io_is_single_take_and_exit_status_survives_stop() {
        let directory = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        let descriptor = descriptor(directory.path(), "echo durable-output");
        supervisor.launch("child", &descriptor).await.unwrap();
        assert!(supervisor.launch("child", &descriptor).await.is_err());
        let io = supervisor.take_io("child").await.unwrap();
        assert!(supervisor.take_io("child").await.is_err());
        drop(io.stdin);
        let output = tokio::spawn(async move {
            let mut data = String::new();
            let mut stdout = io.stdout;
            stdout.read_to_string(&mut data).await.unwrap();
            data
        });
        let errors = tokio::spawn(async move {
            let mut stderr = io.stderr;
            let mut data = Vec::new();
            stderr.read_to_end(&mut data).await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let ProcessState::Exited { code } = supervisor.state("child").await.unwrap() {
                    assert_eq!(code, Some(0));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            supervisor
                .terminate_attempt("attempt", Duration::from_secs(5))
                .await
                .unwrap()
        );
        assert_eq!(
            supervisor.state("child").await.unwrap(),
            ProcessState::Exited { code: Some(0) }
        );
        assert!(output.await.unwrap().contains("durable-output"));
        errors.await.unwrap();
        assert!(supervisor.launch("late", &descriptor).await.is_err());
        assert!(supervisor.active_ids().await.is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn leader_polling_preserves_job_completion_and_receipt_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let receipts = directory.path().join("processes");
        let supervisor = Supervisor::persistent(&receipts).unwrap();
        let descriptor = descriptor(directory.path(), "exit /b 0");
        supervisor.launch("probe", &descriptor).await.unwrap();
        drop(supervisor.take_io("probe").await.unwrap());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    supervisor.state("probe").await.unwrap(),
                    ProcessState::Exited { .. }
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // More polls than the finite job notifications: none may consume the
        // completion queue that the wrapper's final wait owns.
        {
            let mut children = supervisor.children.lock().await;
            let entry = children.get_mut("probe").unwrap();
            for _ in 0..32 {
                assert!(entry.poll_exit().unwrap().is_some());
            }
        }
        assert!(
            supervisor
                .terminate_attempt("attempt", Duration::from_secs(2))
                .await
                .unwrap()
        );
        assert!(!receipts.join("probe.json").exists());
        assert!(supervisor.active_ids().await.is_empty());
    }

    #[tokio::test]
    async fn interrupted_unidentified_launch_is_not_reported_stopped() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("launch.json"),
            br#"{"id":"launch","reservation_id":"attempt","pid":null,"start_time":null}"#,
        )
        .unwrap();
        let supervisor = Supervisor::persistent(directory.path()).unwrap();
        assert!(supervisor.reconcile_previous().await.is_err());
        assert!(directory.path().join("launch.json").exists());
    }
}
