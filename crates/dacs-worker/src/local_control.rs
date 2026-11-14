use crate::Journal;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Drain,
    EmergencyCancel,
    PauseDesktop,
    ResumeDesktop,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OwnerControls {
    pub draining: bool,
    pub emergency_cancel: bool,
    pub desktop_paused: bool,
    /// Locally withdrawn capabilities (permission reduction). They leave the
    /// heartbeat at once, so nothing new is admitted with them, but admitted
    /// attempts already exercising them drain instead of being killed.
    #[serde(default)]
    pub withdrawn_capabilities: Vec<String>,
}

/// A broader permission still exercised by admitted work after the owner
/// withdrew it. It disappears only when that work has stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePermission {
    pub capability: String,
    pub attempt_id: String,
}

/// What the worker has actually observed about a stop it is carrying out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopPhase {
    /// Owned process trees are not yet verified terminated by the OS.
    Terminating,
    /// Trees are verified terminated; queued reports (including Stopped) are
    /// not yet acknowledged by the coordinator.
    ReportingStop,
    /// Reports acknowledged; the coordinator has not resolved the task yet,
    /// for example an unresolved publication or child reconciliation.
    AwaitingCoordinator,
    /// Terminal in the coordinator snapshot.
    Cancelled,
    /// Publication won the race with the stop; terminal in the snapshot.
    Accepted,
    Failed,
    /// The coordinator reconciled this attempt and moved the task elsewhere.
    Superseded,
}

impl StopPhase {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Accepted | Self::Failed | Self::Superseded
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopStatus {
    pub attempt_id: String,
    pub reason: String,
    pub phase: StopPhase,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalStatus {
    pub worker_id: String,
    pub running: bool,
    pub draining: bool,
    pub emergency_cancel: bool,
    pub desktop_paused: bool,
    pub requested_controls: OwnerControls,
    pub desktop_active: bool,
    pub desktop_supported: bool,
    pub provider_authenticated: Option<bool>,
    pub elevated: Option<bool>,
    pub active_attempt_ids: Vec<String>,
    pub last_error: Option<String>,
    pub observed_at: i64,
    pub limitations: Vec<String>,
    /// Capabilities offered to the coordinator after local withdrawals.
    pub effective_capabilities: Vec<String>,
    /// Requested permission reductions not yet fully in effect because
    /// admitted work still exercises the broader grant.
    pub pending_permission_changes: Vec<String>,
    /// The broader grants that keep those changes pending.
    pub active_broader_permissions: Vec<ActivePermission>,
    /// Stops in progress or recently completed, phased by observed evidence.
    pub stops: Vec<StopStatus>,
}

/// Local filesystem access is the authority. No coordinator command grants
/// desktop access, reverses a pause, elevates a token, or changes the profile.
pub fn request(data_dir: &Path, action: ControlAction) -> Result<()> {
    let journal = owner_journal(data_dir)?;
    let mut controls: OwnerControls = journal.get_value("owner_controls")?.unwrap_or_default();
    match action {
        ControlAction::Drain => controls.draining = true,
        ControlAction::EmergencyCancel => {
            controls.draining = true;
            controls.emergency_cancel = true;
        }
        ControlAction::PauseDesktop => controls.desktop_paused = true,
        ControlAction::ResumeDesktop => controls.desktop_paused = false,
    }
    journal.set_value("owner_controls", &controls)?;
    Ok(())
}

/// Permission reduction: the capability stops being offered immediately, while
/// admitted work using it drains. Unstarted assignments needing it are refused.
pub fn withdraw_capability(data_dir: &Path, capability: &str) -> Result<()> {
    anyhow::ensure!(
        !capability.trim().is_empty(),
        "capability to withdraw is empty"
    );
    let journal = owner_journal(data_dir)?;
    let mut controls: OwnerControls = journal.get_value("owner_controls")?.unwrap_or_default();
    if !controls
        .withdrawn_capabilities
        .iter()
        .any(|c| c == capability)
    {
        controls.withdrawn_capabilities.push(capability.to_owned());
    }
    journal.set_value("owner_controls", &controls)?;
    Ok(())
}

/// Widening is a local owner action only; it never arrives from the
/// coordinator and cannot exceed the capabilities in the local config.
pub fn restore_capability(data_dir: &Path, capability: &str) -> Result<()> {
    let journal = owner_journal(data_dir)?;
    let mut controls: OwnerControls = journal.get_value("owner_controls")?.unwrap_or_default();
    controls.withdrawn_capabilities.retain(|c| c != capability);
    journal.set_value("owner_controls", &controls)?;
    Ok(())
}

/// The control surface is the owner's private state directory; the worker has
/// no listening control socket. Create it owner-only and tighten it before each
/// request, which also fails for a directory owned by another account.
fn owner_journal(data_dir: &Path) -> Result<Journal> {
    if let Ok(metadata) = std::fs::symlink_metadata(data_dir) {
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "worker state must be a private directory, not a link or file"
        );
    } else {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(data_dir)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
            .context("worker state directory is not owned by this account")?;
    }
    Ok(Journal::open(data_dir.join("worker.sqlite"))?)
}

/// A withdrawal covers the named capability and its refinements, matching the
/// runtime's own prefix test for desktop capabilities. It errs restrictive.
pub fn withdrawn(withdrawn: &[String], capability: &str) -> bool {
    withdrawn
        .iter()
        .any(|w| !w.is_empty() && capability.starts_with(w.as_str()))
}

/// Pending reductions and the broader grants keeping them pending, given the
/// capabilities each admitted, still-running attempt requires.
pub fn permission_drain(
    withdrawn_capabilities: &[String],
    active: &[(String, Vec<String>)],
) -> (Vec<String>, Vec<ActivePermission>) {
    let mut pending = Vec::new();
    let mut broader = Vec::new();
    for withdrawal in withdrawn_capabilities {
        let mut blocking = false;
        for (attempt, capabilities) in active {
            for capability in capabilities {
                if withdrawn(std::slice::from_ref(withdrawal), capability) {
                    blocking = true;
                    broader.push(ActivePermission {
                        capability: capability.clone(),
                        attempt_id: attempt.clone(),
                    });
                }
            }
        }
        if blocking {
            pending.push(format!("withdraw {withdrawal}"));
        }
    }
    broader.sort_by(|a, b| (&a.attempt_id, &a.capability).cmp(&(&b.attempt_id, &b.capability)));
    broader.dedup();
    (pending, broader)
}

/// Terminal only from evidence the worker holds: OS-verified termination of
/// owned trees, acknowledgement of every queued report for the attempt, and a
/// coordinator snapshot in which the task (and any publication) is resolved.
pub fn stop_phase(
    owned_execution_active: bool,
    reports_pending: bool,
    attempt: Option<&dacs_protocol::AttemptRecord>,
    task: Option<&dacs_protocol::TaskRecord>,
) -> StopPhase {
    use dacs_protocol::TaskState;
    if owned_execution_active {
        return StopPhase::Terminating;
    }
    if reports_pending {
        return StopPhase::ReportingStop;
    }
    let (Some(attempt), Some(task)) = (attempt, task) else {
        return StopPhase::AwaitingCoordinator;
    };
    match task.state {
        TaskState::Cancelled => StopPhase::Cancelled,
        TaskState::Accepted => StopPhase::Accepted,
        TaskState::Failed => StopPhase::Failed,
        _ if attempt.settled && task.attempt_id.as_deref() != Some(attempt.id.as_str()) => {
            StopPhase::Superseded
        }
        _ => StopPhase::AwaitingCoordinator,
    }
}

/// Remember why an attempt is being stopped. The first reason wins; the list
/// is bounded by dropping the oldest entries.
pub(crate) fn record_stop(journal: &Journal, attempt: &str, reason: &str) -> Result<()> {
    let mut stops: Vec<(String, String)> = journal.get_value("stop_requests")?.unwrap_or_default();
    if attempt.is_empty() || stops.iter().any(|(id, _)| id == attempt) {
        return Ok(());
    }
    stops.push((attempt.to_owned(), reason.to_owned()));
    let excess = stops.len().saturating_sub(64);
    stops.drain(..excess);
    journal.set_value("stop_requests", &stops)?;
    Ok(())
}

pub fn status(data_dir: &Path) -> Result<LocalStatus> {
    let journal = Journal::open(data_dir.join("worker.sqlite"))?;
    let mut status: LocalStatus = journal
        .get_value("local_status")?
        .context("worker has not reported status")?;
    let controls: OwnerControls = journal.get_value("owner_controls")?.unwrap_or_default();
    status.requested_controls = controls;
    // Stale data is not a live indicator or a successful stop receipt.
    if now() - status.observed_at > 15 {
        status.running = false;
    }
    Ok(status)
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn emergency_remains_distinct_and_pause_is_persistent() {
        let dir = tempfile::tempdir().unwrap();
        request(dir.path(), ControlAction::PauseDesktop).unwrap();
        request(dir.path(), ControlAction::Drain).unwrap();
        let journal = Journal::open(dir.path().join("worker.sqlite")).unwrap();
        let controls: OwnerControls = journal.get_value("owner_controls").unwrap().unwrap();
        assert!(controls.draining && controls.desktop_paused && !controls.emergency_cancel);
        request(dir.path(), ControlAction::EmergencyCancel).unwrap();
        let controls: OwnerControls = journal.get_value("owner_controls").unwrap().unwrap();
        assert!(controls.emergency_cancel);
    }
}

/// Capability facts come from the native tool result, never model prose.
pub(crate) fn desktop_available(value: &serde_json::Value) -> bool {
    value["capture"] == true
        && value["input"] == true
        && value["screenLocked"] != true
        && value["displayAsleep"] != true
        && value["secureInput"] != true
        && value
            .get("displayCount")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|n| n > 0)
        && ["capturePermission", "inputPermission"].iter().all(|key| {
            value
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .is_none_or(|permission| {
                    matches!(permission, "granted" | "allowed" | "not-required")
                })
        })
}

pub(crate) fn desktop_lock() -> Result<std::fs::File> {
    owner_lock("desktop")
}

pub(crate) fn owner_lock(name: &str) -> Result<std::fs::File> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .context("owner home unavailable")?;
    owner_lock_at(&std::path::PathBuf::from(home).join(".omp"), name)
}

/// One exclusive slot per owner profile (and so per interactive session): the
/// lock is held for the whole attempt, so unrelated multi-step desktop
/// workflows from other attempts or workers cannot interleave with it.
pub(crate) fn owner_lock_at(root: &Path, name: &str) -> Result<std::fs::File> {
    crate::workspace::safe_id(name)?;
    use fs2::FileExt;
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!("dacs-{name}.lock")))?;
    file.try_lock_exclusive()
        .context("another DACS worker or attempt owns this owner session")?;
    Ok(file)
}

#[cfg(test)]
mod desktop_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn desktop_permission_and_session_uncertainty_fail_closed() {
        let mut native = json!({"capture":true,"input":true,"displayCount":1,"screenLocked":false,"inputPermission":"granted"});
        assert!(desktop_available(&native));
        native["screenLocked"] = json!(true);
        assert!(!desktop_available(&native));
        native["screenLocked"] = json!(false);
        native["inputPermission"] = json!("prompt-or-granted");
        assert!(!desktop_available(&native));
        assert!(!desktop_available(&json!({"capture":true,"input":true})));
    }

    #[test]
    fn one_desktop_slot_per_owner_session_until_released() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".omp");
        let first = owner_lock_at(&root, "desktop").unwrap();
        // A second attempt or worker of the same owner session cannot take it.
        let error = owner_lock_at(&root, "desktop").unwrap_err();
        assert!(format!("{error:#}").contains("owns this owner session"));
        // Unrelated owner locks are independent of the desktop slot.
        let _worker = owner_lock_at(&root, "worker-x").unwrap();
        drop(first);
        let _again = owner_lock_at(&root, "desktop").unwrap();
        assert!(owner_lock_at(&root, "../desktop").is_err());
    }

    #[test]
    fn requested_pause_does_not_fabricate_observed_stop() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("worker.sqlite")).unwrap();
        journal
            .set_value(
                "local_status",
                &LocalStatus {
                    running: true,
                    desktop_active: true,
                    observed_at: now(),
                    ..LocalStatus::default()
                },
            )
            .unwrap();
        request(dir.path(), ControlAction::PauseDesktop).unwrap();
        let current = status(dir.path()).unwrap();
        assert!(current.requested_controls.desktop_paused);
        assert!(!current.desktop_paused);
        assert!(current.desktop_active);
    }
}
