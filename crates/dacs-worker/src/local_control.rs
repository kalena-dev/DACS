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
}

/// Local filesystem access is the authority. No coordinator command grants
/// desktop access, reverses a pause, elevates a token, or changes the profile.
pub fn request(data_dir: &Path, action: ControlAction) -> Result<()> {
    let journal = Journal::open(data_dir.join("worker.sqlite"))?;
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
    crate::workspace::safe_id(name)?;
    use fs2::FileExt;
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .context("owner home unavailable")?;
    let root = std::path::PathBuf::from(home).join(".omp");
    std::fs::create_dir_all(&root)?;
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
