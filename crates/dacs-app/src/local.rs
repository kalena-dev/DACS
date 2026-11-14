use dacs_protocol::{ApiError, ErrorCode, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn io_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::Io, error.to_string())
}

pub fn secure_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

pub fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(io_error)?;
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ApiError::new(
            ErrorCode::Invalid,
            "State directory must be a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
            ])
            .output()
            .map_err(io_error)?;
        let sid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success()
            || !sid.starts_with("S-")
            || !sid
                .bytes()
                .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-')
        {
            return Err(ApiError::new(
                ErrorCode::Io,
                "Cannot determine current Windows identity for private state ACL",
            ));
        }
        let acl = std::process::Command::new("icacls.exe")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("*{sid}:(OI)(CI)F")])
            .output()
            .map_err(io_error)?;
        if !acl.status.success() {
            return Err(ApiError::new(
                ErrorCode::Io,
                "Cannot restrict DACS state directory ACL",
            ));
        }
    }
    Ok(())
}

pub fn admin_token(data: &Path) -> Result<String> {
    let path = data.join("admin.token");
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 {
                return Err(ApiError::new(
                    ErrorCode::Invalid,
                    "Invalid admin token file",
                ));
            }
            let value = fs::read_to_string(path)
                .map_err(io_error)?
                .trim()
                .to_owned();
            if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ApiError::new(
                    ErrorCode::Invalid,
                    "Invalid admin token format",
                ));
            }
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Written to a private temporary file and renamed into place, so a crash
            // can never leave an empty or partial token file behind.
            let value = secret();
            let temp = data.join(format!("admin.token.{}.tmp", uuid::Uuid::new_v4()));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp).map_err(io_error)?;
            let written = file
                .write_all(value.as_bytes())
                .and_then(|()| file.sync_all());
            drop(file);
            if let Err(error) = written.and_then(|()| fs::rename(&temp, &path)) {
                let _ = fs::remove_file(&temp);
                return Err(io_error(error));
            }
            #[cfg(unix)]
            fs::File::open(data)
                .and_then(|f| f.sync_all())
                .map_err(io_error)?;
            Ok(value)
        }
        Err(error) => Err(io_error(error)),
    }
}

pub fn read_json(path: &Path) -> anyhow::Result<Value> {
    anyhow::ensure!(
        fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "JSON input exceeds 8 MiB"
    );
    Ok(serde_json::from_reader(fs::File::open(path)?)?)
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(io_error)?;
    serde_json::to_writer(&mut file, value).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    // On Windows the standard library renames with MoveFileExW, replacing the destination.
    fs::rename(&temp, path).map_err(io_error)?;
    #[cfg(unix)]
    fs::File::open(path.parent().unwrap_or(Path::new(".")))
        .and_then(|f| f.sync_all())
        .map_err(io_error)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub operation: String,
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub result: Option<Value>,
    pub error: Option<ApiError>,
}

struct JobQueue {
    root: PathBuf,
    serial: Mutex<()>,
    active: AtomicUsize,
    drained: tokio::sync::Notify,
}

struct RunningJob(Arc<JobQueue>);
impl Drop for RunningJob {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.drained.notify_one();
        }
    }
}

#[derive(Clone)]
pub struct Jobs {
    queue: Arc<JobQueue>,
}

impl Jobs {
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).map_err(io_error)?;
        for entry in fs::read_dir(&root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                let mut job: Job =
                    serde_json::from_reader(fs::File::open(entry.path()).map_err(io_error)?)
                        .map_err(io_error)?;
                if matches!(job.state.as_str(), "queued" | "running") {
                    job.state = "interrupted".into();
                    job.updated_at = now();
                    job.error = Some(ApiError::new(
                        ErrorCode::Conflict,
                        "Coordinator restarted; inspect the operation journal before explicitly resuming",
                    ));
                    save_json(&entry.path(), &job)?;
                }
            }
        }
        Ok(Self {
            queue: Arc::new(JobQueue {
                root,
                serial: Mutex::new(()),
                active: AtomicUsize::new(0),
                drained: tokio::sync::Notify::new(),
            }),
        })
    }

    pub fn start(
        &self,
        operation: &str,
        action: impl FnOnce() -> Result<Value> + Send + 'static,
    ) -> Result<Job> {
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            state: "queued".into(),
            created_at: now(),
            updated_at: now(),
            result: None,
            error: None,
        };
        let path = self.queue.root.join(format!("{}.json", job.id));
        save_json(&path, &job)?;
        let mut running = job.clone();
        self.queue.active.fetch_add(1, Ordering::AcqRel);
        let completion = RunningJob(self.queue.clone());
        tokio::task::spawn_blocking(move || {
            let completion = completion;
            let _guard = match completion.0.serial.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    running.state = "failed".into();
                    running.error = Some(ApiError::new(
                        ErrorCode::Internal,
                        "Local operation queue failed",
                    ));
                    let _ = save_json(&path, &running);
                    return;
                }
            };
            running.state = "running".into();
            running.updated_at = now();
            if let Err(error) = save_json(&path, &running) {
                tracing::error!("Cannot persist local operation start: {error}");
                return;
            }
            match action() {
                Ok(value) => {
                    running.state = "completed".into();
                    running.result = Some(value);
                }
                Err(error) => {
                    running.state = "failed".into();
                    running.error = Some(error);
                }
            }
            running.updated_at = now();
            if let Err(error) = save_json(&path, &running) {
                tracing::error!("Cannot persist local operation outcome: {error}");
            }
        });
        Ok(job)
    }

    pub async fn drain(&self) {
        loop {
            let drained = self.queue.drained.notified();
            if self.queue.active.load(Ordering::Acquire) == 0 {
                return;
            }
            drained.await;
        }
    }

    pub fn get(&self, id: &str) -> Result<Job> {
        uuid::Uuid::parse_str(id)
            .map_err(|_| ApiError::new(ErrorCode::Invalid, "Invalid job ID"))?;
        let file = fs::File::open(self.queue.root.join(format!("{id}.json"))).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ApiError::new(ErrorCode::NotFound, "Job not found")
            } else {
                io_error(error)
            }
        })?;
        serde_json::from_reader(file).map_err(io_error)
    }
}

pub fn launch_update_helper(
    value: &Value,
    restart_arguments: Option<&[String]>,
) -> anyhow::Result<bool> {
    if value.get("exit_required").and_then(Value::as_bool) != Some(true) {
        return Ok(false);
    }
    let executable = value
        .get("helper_executable")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Update helper path is missing"))?;
    let arguments = value
        .get("arguments")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Update helper arguments are missing"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid update helper argument"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut command = std::process::Command::new(executable);
    command.args(arguments);
    if let Some(arguments) = restart_arguments {
        let path =
            Path::new(executable).with_file_name(format!("restart-{}.json", uuid::Uuid::new_v4()));
        save_json(&path, &arguments)?;
        command.arg("--restart-args").arg(path);
    }
    command.spawn()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, task::Poll};

    #[test]
    fn admin_token_is_created_once_without_leaving_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let token = admin_token(root.path()).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(fs::read_to_string(root.path().join("admin.token")).unwrap(), token);
        assert_eq!(admin_token(root.path()).unwrap(), token);
        let names: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["admin.token"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.path().join("admin.token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn shutdown_drains_active_and_queued_operations_before_returning() {
        let root = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(root.path().join("jobs")).unwrap();
        let (first_started, started) = tokio::sync::oneshot::channel();
        let (release_first, first_gate) = std::sync::mpsc::channel();
        let first_path = root.path().join("first");
        let first_output = first_path.clone();
        let first = jobs
            .start("first", move || {
                first_started.send(()).unwrap();
                first_gate.recv().unwrap();
                fs::write(first_output, b"first completed").map_err(io_error)?;
                Ok(Value::Null)
            })
            .unwrap();
        started.await.unwrap();
        let (second_started, started) = tokio::sync::oneshot::channel();
        let (release_second, second_gate) = std::sync::mpsc::channel();
        let second_path = root.path().join("second");
        let second_output = second_path.clone();
        let second = jobs
            .start("second", move || {
                second_started.send(()).unwrap();
                second_gate.recv().unwrap();
                fs::write(second_output, b"second completed").map_err(io_error)?;
                Ok(Value::Null)
            })
            .unwrap();
        let mut drain = std::pin::pin!(jobs.drain());
        std::future::poll_fn(|cx| {
            assert!(drain.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release_first.send(()).unwrap();
        started.await.unwrap();
        std::future::poll_fn(|cx| {
            assert!(drain.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release_second.send(()).unwrap();
        drain.await;
        assert_eq!(fs::read(first_path).unwrap(), b"first completed");
        assert_eq!(fs::read(second_path).unwrap(), b"second completed");
        assert_eq!(jobs.get(&first.id).unwrap().state, "completed");
        assert_eq!(jobs.get(&second.id).unwrap().state, "completed");
    }
}
