use dacs_protocol::{ApiError, ErrorCode, Result};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}
pub(crate) fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Conflict, message)
}
pub(crate) fn io(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::Io, error.to_string())
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub(crate) fn json<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(io)
}
pub(crate) fn decode<T: DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(invalid_json)
}
fn invalid_json(error: serde_json::Error) -> ApiError {
    invalid(format!("Invalid request: {error}"))
}
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn reject_links(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io)?.join(path)
    };
    let mut walked = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err(invalid("Parent path components are not allowed"));
        }
        walked.push(component);
        // A Windows drive/verbatim prefix is not an independently queryable path.
        // Inspect only complete descendant paths, after Prefix + RootDir are joined.
        if matches!(component, Component::Prefix(_) | Component::RootDir | Component::CurDir) {
            continue;
        }
        match fs::symlink_metadata(&walked) {
            Ok(meta) => {
                let mut link = meta.file_type().is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    link |= meta.file_attributes() & 0x400 != 0;
                }
                if link {
                    return Err(invalid(format!(
                        "Symlink/reparse path is not allowed: {}",
                        walked.display()
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(io(e)),
        }
    }
    Ok(())
}

pub(crate) fn state_directory(path: &Path) -> Result<PathBuf> {
    reject_links(path)?;
    fs::create_dir_all(path).map_err(io)?;
    reject_links(path)?;
    let path = fs::canonicalize(path).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(path)
}

pub(crate) fn lock(path: &Path) -> Result<File> {
    reject_links(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(io)?;
    file.try_lock_exclusive()
        .map_err(|_| conflict("Another operation owns this state directory"))?;
    Ok(file)
}

pub(crate) fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value).map_err(io)?)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    reject_links(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Missing parent directory"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(io)?;
    temp.write_all(bytes).map_err(io)?;
    temp.as_file().sync_all().map_err(io)?;
    let temporary = temp.into_temp_path();
    rename_durable(&temporary, path, true).map_err(io)?;
    Ok(())
}

pub(crate) fn rename_durable(source: &Path, target: &Path, replace: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        let flags = MOVEFILE_WRITE_THROUGH | if replace {MOVEFILE_REPLACE_EXISTING} else {0};
        // Both buffers are NUL-terminated and live through the synchronous Win32 call.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        if !replace && target.exists() {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Rename destination already exists"));
        }
        fs::rename(source, target)?;
        if let Some(parent) = target.parent() {File::open(parent)?.sync_all()?;}
        Ok(())
    }
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path).and_then(|f| f.sync_all()).map_err(io)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    reject_links(path)?;
    let file = File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err(invalid("Expected regular file"));
    }
    let mut data = Vec::new();
    file.take(max + 1).read_to_end(&mut data).map_err(io)?;
    if data.len() as u64 > max {
        return Err(invalid(format!("File exceeds {max} byte limit")));
    }
    Ok(data)
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_bounded(path, 4 * 1024 * 1024)?).map_err(io)
}

// Only absolute PATH entries are considered; never execute a repository-local lookalike.
pub(crate) fn executable(name: &str) -> Option<PathBuf> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        dirs.push(PathBuf::from(home).join(".cargo/bin"));
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(&local).join("omp"));
            dirs.push(PathBuf::from(local).join("Microsoft/WinGet/Links"));
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(variable) {
                for sub in ["Git/bin", "LLVM/bin", "Geany/bin", "Mozilla Firefox"] {
                    dirs.push(PathBuf::from(&base).join(sub));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for dir in [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/opt/homebrew/opt/rustup/bin",
            "/usr/local/opt/rustup/bin",
            "/opt/homebrew/opt/llvm/bin",
            "/usr/local/opt/llvm/bin",
            "/Applications/Geany.app/Contents/MacOS",
            "/Applications/Firefox.app/Contents/MacOS",
        ] {
            dirs.push(PathBuf::from(dir));
        }
    }
    dirs.into_iter()
        .map(|d| d.join(format!("{name}{suffix}")))
        .find(|p| p.is_file())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}
impl CommandSpec {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
        }
    }
}

pub(crate) struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}
pub(crate) fn run(spec: &CommandSpec, timeout: Duration) -> Result<CommandOutput> {
    let program = executable(&spec.program).ok_or_else(|| {
        conflict(format!(
            "Required executable {} is unavailable; install its documented prerequisite locally",
            spec.program
        ))
    })?;
    let neutral = tempfile::tempdir().map_err(io)?;
    let mut child = Command::new(program)
        .args(&spec.args)
        .current_dir(neutral.path())
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_INSTALL_UPGRADE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io("Missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io("Missing stderr pipe"))?;
    fn collect(reader: impl Read) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        reader.take(1024 * 1024 + 1).read_to_end(&mut out)?;
        Ok(out)
    }
    let out_thread = thread::spawn(move || collect(stdout));
    let err_thread = thread::spawn(move || collect(stderr));
    let start = Instant::now();
    let status = loop {
        match child.try_wait().map_err(io)? {
            Some(status) => break status,
            None if start.elapsed() < timeout => thread::sleep(Duration::from_millis(50)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(conflict(format!(
                    "{} timed out; effects may be incomplete and require reconciliation",
                    spec.program
                )));
            }
        }
    };
    let stdout = out_thread
        .join()
        .map_err(|_| io("stdout reader panicked"))?
        .map_err(io)?;
    let stderr = err_thread
        .join()
        .map_err(|_| io("stderr reader panicked"))?
        .map_err(io)?;
    if stdout.len() > 1024 * 1024 || stderr.len() > 1024 * 1024 {
        return Err(io("Command output exceeded safety limit"));
    }
    Ok(CommandOutput {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths_support_nested_state_and_atomic_replacement() {
        let root = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        reject_links(&canonical).unwrap();
        let state = state_directory(&canonical.join("state").join("updates")).unwrap();
        let path = state.join("journal.json");
        atomic_json(&path, &serde_json::json!({"phase":"prepared"})).unwrap();
        atomic_json(&path, &serde_json::json!({"phase":"completed"})).unwrap();
        let observed: serde_json::Value = read_json(&path).unwrap();
        assert_eq!(observed["phase"], "completed");
    }
}
