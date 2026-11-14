use crate::{ManagedChild, ProcessState, Supervisor};
use anyhow::{Context, Result};
use dacs_protocol::{AttemptRecord, TaskRecord};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) fn safe_id(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "unsafe filesystem identity"
    );
    Ok(())
}

/// Explicit owner environment, never a root/system profile replacement. Device
/// credentials are loaded separately and never placed in the child's environment.
pub(crate) fn owner_environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| {
            let n = name.to_ascii_uppercase();
            matches!(
                n.as_str(),
                "PATH"
                    | "HOME"
                    | "USERPROFILE"
                    | "HOMEDRIVE"
                    | "HOMEPATH"
                    | "APPDATA"
                    | "LOCALAPPDATA"
                    | "SYSTEMROOT"
                    | "WINDIR"
                    | "COMSPEC"
                    | "PATHEXT"
                    | "TEMP"
                    | "TMP"
                    | "TMPDIR"
                    | "USER"
                    | "USERNAME"
                    | "LOGNAME"
                    | "LANG"
                    | "DISPLAY"
                    | "WAYLAND_DISPLAY"
                    | "XDG_RUNTIME_DIR"
                    | "DBUS_SESSION_BUS_ADDRESS"
                    | "SSH_AUTH_SOCK"
                    | "SSL_CERT_FILE"
                    | "SSL_CERT_DIR"
                    | "CARGO_HOME"
                    | "RUSTUP_HOME"
                    | "OMP_PROFILE"
                    | "PI_CODING_AGENT_DIR"
            ) || n.starts_with("LC_")
                || n.ends_with("_API_KEY")
        })
        .collect()
}

async fn bounded_read(mut input: impl AsyncRead + Unpin, maximum: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 16384];
    let mut overflow = false;
    loop {
        let size = input.read(&mut buffer).await?;
        if size == 0 {
            break;
        }
        if data.len() + size <= maximum {
            data.extend_from_slice(&buffer[..size]);
        } else {
            overflow = true;
        }
    }
    anyhow::ensure!(
        !overflow,
        "command output exceeded capture limit (stream was drained)"
    );
    Ok(data)
}

pub(crate) async fn capture(
    supervisor: &Supervisor,
    attempt: Option<&str>,
    cwd: &Path,
    program: PathBuf,
    args: Vec<String>,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    let id = uuid::Uuid::new_v4().to_string();
    supervisor
        .launch(
            &id,
            &ManagedChild {
                name: program.display().to_string(),
                program,
                args,
                cwd: cwd.to_owned(),
                env: owner_environment(),
                effect_class: "workspace".into(),
                reservation_id: attempt.map(str::to_owned),
            },
        )
        .await?;
    let io = supervisor.take_io(&id).await?;
    drop(io.stdin);
    let stdout = tokio::spawn(bounded_read(io.stdout, 16 * 1024 * 1024));
    let stderr = tokio::spawn(bounded_read(io.stderr, 4 * 1024 * 1024));
    let exit = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            if let ProcessState::Exited { code } = supervisor.state(&id).await? {
                return Ok::<_, anyhow::Error>(code);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    let stopped = supervisor.terminate(&id, Duration::from_secs(10)).await?;
    anyhow::ensure!(stopped, "command process tree is unreconciled");
    let out = tokio::time::timeout(Duration::from_secs(10), stdout).await???;
    let err = tokio::time::timeout(Duration::from_secs(10), stderr).await???;
    let code = exit.context("command timed out")??.unwrap_or(-1);
    Ok((code, out, err))
}

pub(crate) async fn git(
    supervisor: &Supervisor,
    attempt: &str,
    cwd: &Path,
    args: &[&str],
) -> Result<String> {
    let mut options = vec![
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "protocol.file.allow=always".into(),
    ];
    options.extend(args.iter().map(|s| (*s).to_owned()));
    let (code, out, err) = capture(
        supervisor,
        Some(attempt),
        cwd,
        PathBuf::from("git"),
        options,
    )
    .await?;
    anyhow::ensure!(
        code == 0,
        "git failed ({code}): {}",
        String::from_utf8_lossy(&err)
    );
    Ok(String::from_utf8(out)?.trim().to_owned())
}

pub(crate) async fn prepare(
    supervisor: &Supervisor,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    root: &Path,
    bundle: &[u8],
) -> Result<PathBuf> {
    safe_id(&attempt.id)?;
    let directory = root.join(&attempt.id);
    let marker = directory.join(".dacs-source-pin");
    if directory.exists() {
        let pin = std::fs::read_to_string(&marker)
            .context("partial workspace exists; refuse to overwrite owner work")?;
        anyhow::ensure!(
            pin == task.spec.source_commit,
            "workspace source pin changed"
        );
        git(
            supervisor,
            &attempt.id,
            &directory,
            &[
                "cat-file",
                "-e",
                &format!("{}^{{commit}}", task.spec.source_commit),
            ],
        )
        .await?;
        return Ok(directory);
    }
    std::fs::create_dir_all(root)?;
    let bundle_path = root.join(format!("{}.source.bundle", attempt.id));
    std::fs::write(&bundle_path, bundle)?;
    git(
        supervisor,
        &attempt.id,
        root,
        &[
            "clone",
            "--no-checkout",
            "--no-hardlinks",
            bundle_path.to_str().context("non UTF-8 bundle path")?,
            directory.to_str().context("non UTF-8 workspace path")?,
        ],
    )
    .await?;
    git(
        supervisor,
        &attempt.id,
        &directory,
        &["checkout", "--detach", &task.spec.source_commit],
    )
    .await?;
    let actual = git(supervisor, &attempt.id, &directory, &["rev-parse", "HEAD"]).await?;
    anyhow::ensure!(
        actual == task.spec.source_commit,
        "source checkout does not match assignment"
    );
    git(
        supervisor,
        &attempt.id,
        &directory,
        &["checkout", "-b", &format!("dacs/{}", attempt.id)],
    )
    .await?;
    std::fs::write(&marker, &task.spec.source_commit)?;
    // Keep the local receipt outside candidate content without touching .gitignore.
    let exclude = directory.join(".git/info/exclude");
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(exclude)?;
    writeln!(file, "\n.dacs-source-pin")?;
    std::fs::remove_file(bundle_path)?;
    Ok(directory)
}

pub(crate) async fn candidate(
    supervisor: &Supervisor,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    directory: &Path,
    output: &Path,
) -> Result<String> {
    let sha = git(supervisor, &attempt.id, directory, &["rev-parse", "HEAD"]).await?;
    git(
        supervisor,
        &attempt.id,
        directory,
        &[
            "merge-base",
            "--is-ancestor",
            &task.spec.source_commit,
            &sha,
        ],
    )
    .await?;
    anyhow::ensure!(
        git(
            supervisor,
            &attempt.id,
            directory,
            &["status", "--porcelain"]
        )
        .await?
        .is_empty(),
        "commit all intended changes before dacs_submit"
    );
    git(
        supervisor,
        &attempt.id,
        directory,
        &[
            "bundle",
            "create",
            output.to_str().context("non UTF-8 candidate bundle path")?,
            "HEAD",
            "--branches=dacs/*",
        ],
    )
    .await?;
    Ok(sha)
}

#[cfg(test)]
mod tests {
    #[test]
    fn assignment_ids_cannot_escape_workspace_root() {
        for invalid in ["", "../owner", "a/b", "a\\\\b", ".", "C:target"] {
            assert!(super::safe_id(invalid).is_err());
        }
        assert!(super::safe_id("attempt-123_abc").is_ok());
    }
}
