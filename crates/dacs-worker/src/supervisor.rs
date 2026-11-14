use anyhow::{Context, Result};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    process::{Child, Command},
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

pub struct Supervisor {
    children: Arc<Mutex<BTreeMap<String, Child>>>,
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
        }
    }

    /// Launch with piped stdio and a per-attempt working directory. Callers must
    /// create an independent clone/workspace before invoking this method.
    pub async fn launch(&self, id: impl Into<String>, descriptor: &ManagedChild) -> Result<u32> {
        let id = id.into();
        let mut command = Command::new(&descriptor.program);
        command
            .args(&descriptor.args)
            .current_dir(&descriptor.cwd)
            .envs(&descriptor.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        configure_owner_identity(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("launch {}", descriptor.name))?;
        let pid = child.id().unwrap_or_default();
        self.children.lock().await.insert(id, child);
        Ok(pid)
    }

    pub async fn state(&self, id: &str) -> Result<ProcessState> {
        let mut children = self.children.lock().await;
        let child = children.get_mut(id).context("unknown managed process")?;
        match child.try_wait()? {
            Some(status) => Ok(ProcessState::Exited {
                code: status.code(),
            }),
            None => Ok(ProcessState::Running { pid: child.id() }),
        }
    }

    /// Stop and wait for actual process termination. Returns false if the process
    /// remains alive after grace; it must stay accounted and be reconciled.
    pub async fn terminate(&self, id: &str, grace: Duration) -> Result<bool> {
        let mut children = self.children.lock().await;
        let Some(child) = children.get_mut(id) else {
            return Ok(true);
        };
        if child.try_wait()?.is_some() {
            children.remove(id);
            return Ok(true);
        }
        child.start_kill()?;
        let finished = tokio::time::timeout(grace, child.wait()).await.is_ok();
        if finished {
            children.remove(id);
        }
        Ok(finished)
    }

    /// Drain does not signal admitted processes; it reports which remain active.
    pub async fn active_ids(&self) -> Vec<String> {
        let mut children = self.children.lock().await;
        let mut active = Vec::new();
        children.retain(|id, child| match child.try_wait() {
            Ok(Some(_)) => false,
            _ => {
                active.push(id.clone());
                true
            }
        });
        active
    }
}

#[cfg(windows)]
fn configure_owner_identity(_command: &mut Command) {
    // Inherited token is the current interactive owner's token. Windows elevation
    // must be requested via an OS-mediated consented launch adapter.
}
#[cfg(not(windows))]
fn configure_owner_identity(_command: &mut Command) {
    // Inherit the owner uid, environment and interactive session. Never switch to
    // root or another home directory; any escalation adapter is separately consented.
}
