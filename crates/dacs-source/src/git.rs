use crate::storage::{self, MAX_REPOSITORY, MAX_TEMPORARY, error, io};
use dacs_protocol::{ErrorCode, Result};
use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    io::Read,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

// Git for Windows parses paths itself and cannot use Rust's canonical
// verbatim prefixes. Preserve canonical paths internally; translate only
// at the process boundary and enable Git's own long-path handling.
pub(crate) fn argument(value: &OsStr) -> Cow<'_, OsStr> {
    #[cfg(windows)]
    {
        use std::path::{Component, Path, Prefix};
        let mut components = Path::new(value).components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            let mut path = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => OsString::from(format!("{}:", drive as char)),
                Prefix::VerbatimUNC(server, share) => {
                    let mut path = OsString::from(r"\\");
                    path.push(server);
                    path.push(r"\");
                    path.push(share);
                    path
                }
                _ => return Cow::Borrowed(value),
            };
            path.push(components.as_path());
            return Cow::Owned(path);
        }
    }
    Cow::Borrowed(value)
}

pub(crate) struct Git {
    pub(crate) repo: PathBuf,
    pub(crate) temporary: PathBuf,
    project: PathBuf,
    repository_limit: u64,
}

pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}
impl Output {
    pub(crate) fn checked(self) -> Result<Vec<u8>> {
        if self.status.success() {
            return Ok(self.stdout);
        }
        Err(error(
            ErrorCode::Conflict,
            format!(
                "Git operation failed: {}",
                String::from_utf8_lossy(&self.stderr).trim()
            ),
        ))
    }
    pub(crate) fn text(self) -> Result<String> {
        String::from_utf8(self.checked()?)
            .map_err(|_| error(ErrorCode::Invalid, "Git returned non-UTF-8 metadata"))
    }
}

impl Git {
    pub(crate) fn new(project: PathBuf) -> Result<Self> {
        storage::directory(&project)?;
        let temporary = project.join("temporary");
        storage::directory(&temporary)?;
        storage::directory(&project.join("disabled-hooks"))?;
        Ok(Self {
            repo: project.join("repository.git"),
            temporary,
            project,
            repository_limit: MAX_REPOSITORY,
        })
    }

    pub(crate) fn quarantine(project: PathBuf, repository_limit: u64) -> Result<Self> {
        let mut git = Self::new(project)?;
        git.repository_limit = repository_limit.min(MAX_REPOSITORY);
        Ok(git)
    }

    pub(crate) fn ensure(&self) -> Result<()> {
        match storage::inspect(&self.repo)? {
            None => {
                self.run([
                    OsStr::new("init"),
                    OsStr::new("--bare"),
                    OsStr::new("--template="),
                    OsStr::new("--object-format=sha1"),
                    self.repo.as_os_str(),
                ])?
                .checked()?;
                storage::sync_directory(&self.project)?;
            }
            Some(metadata) if !metadata.is_dir() => {
                return Err(error(
                    ErrorCode::Forbidden,
                    "Repository path is not a directory",
                ));
            }
            Some(_) => {}
        }
        self.verify()
    }

    pub(crate) fn verify(&self) -> Result<()> {
        if storage::inspect(&self.repo)?.is_none() {
            return Err(error(
                ErrorCode::NotFound,
                "Coordinator repository is missing; publication cannot be reconciled",
            ));
        }
        self.check_budget()?;
        if self
            .run(["rev-parse", "--is-bare-repository"])?
            .text()?
            .trim()
            != "true"
        {
            return Err(error(
                ErrorCode::Conflict,
                "Coordinator repository must be bare",
            ));
        }
        if self
            .run(["rev-parse", "--show-object-format"])?
            .text()?
            .trim()
            != "sha1"
        {
            return Err(error(
                ErrorCode::Invalid,
                "This source repository requires SHA-1 Git object IDs",
            ));
        }
        Ok(())
    }

    pub(crate) fn check_budget(&self) -> Result<()> {
        storage::tree_size(&self.repo, self.repository_limit)?;
        storage::tree_size(&self.temporary, MAX_TEMPORARY)?;
        Ok(())
    }

    pub(crate) fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_environment(args, &[])
    }

    pub(crate) fn preview<I, S>(&self, args: I) -> Result<(Vec<u8>, bool)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run_controlled(args, &[], true)?;
        let truncated = output.truncated;
        Ok((output.checked()?, truncated))
    }

    pub(crate) fn run_with_environment<I, S>(
        &self,
        args: I,
        extra_environment: &[(&str, String)],
    ) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_controlled(args, extra_environment, false)
    }

    fn run_controlled<I, S>(
        &self,
        args: I,
        extra_environment: &[(&str, String)],
        truncate_stdout: bool,
    ) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.check_budget()?;
        let mut command = Command::new("git");
        command
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Neither machine/user Git configuration nor caller-supplied Git environment
        // may redirect object storage, run helpers, or change the target repository.
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(key);
            }
        }
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("LC_ALL", "C");
        command.arg("--no-pager").arg("--git-dir").arg(argument(self.repo.as_os_str()));
        for value in [
            "protocol.allow=never",
            "protocol.file.allow=always",
            "core.fsmonitor=false",
            "core.longpaths=true",
            "core.logAllRefUpdates=false",
            "gc.auto=0",
            "maintenance.auto=false",
            "fetch.fsckObjects=true",
            "transfer.fsckObjects=true",
            "fetch.recurseSubmodules=false",
            "core.commitGraph=false",
            "core.fsync=committed",
            "core.fsyncMethod=fsync",
            "pack.threads=1",
            "pack.windowMemory=32m",
            "pack.packSizeLimit=128m",
        ] {
            command.arg("-c").arg(value);
        }
        let mut hooks = OsString::from("core.hooksPath=");
        hooks.push(argument(self.project.join("disabled-hooks").as_os_str()));
        command
            .arg("-c")
            .arg(hooks)
            .arg("-c")
            .arg(format!("core.attributesFile={null}"));
        for (key, value) in extra_environment {
            command.env(key, value);
        }
        for value in args {
            command.arg(argument(value.as_ref()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(io)?;
        let tree = match ProcessTree::attach(&child) {
            Ok(tree) => tree,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let stdout_overflow = Arc::new(AtomicBool::new(false));
        let stderr_overflow = Arc::new(AtomicBool::new(false));
        let stdout = capture(
            child.stdout.take().expect("piped Git stdout"),
            Arc::clone(&stdout_overflow),
        );
        let stderr = capture(
            child.stderr.take().expect("piped Git stderr"),
            Arc::clone(&stderr_overflow),
        );
        let started = Instant::now();
        let mut inspected = Instant::now();
        let result = loop {
            if stderr_overflow.load(Ordering::Relaxed)
                || (!truncate_stdout && stdout_overflow.load(Ordering::Relaxed))
            {
                break Err(error(
                    ErrorCode::Invalid,
                    "Git output exceeds metadata limit",
                ));
            }
            if started.elapsed() > COMMAND_TIMEOUT {
                break Err(error(
                    ErrorCode::Conflict,
                    "Git operation exceeded 120-second deadline",
                ));
            }
            if inspected.elapsed() > Duration::from_millis(100) {
                if let Err(e) = self.check_budget() {
                    break Err(e);
                }
                inspected = Instant::now();
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(e) => break Err(io(e)),
            }
        };
        tree.stop(&mut child);
        let _ = child.wait();
        let stdout = stdout
            .join()
            .map_err(|_| error(ErrorCode::Internal, "Git stdout collector panicked"))?
            .map_err(io)?;
        let stderr = stderr
            .join()
            .map_err(|_| error(ErrorCode::Internal, "Git stderr collector panicked"))?
            .map_err(io)?;
        let status = result?;
        let truncated = stdout_overflow.load(Ordering::Relaxed);
        if stderr_overflow.load(Ordering::Relaxed) || (!truncate_stdout && truncated) {
            return Err(error(
                ErrorCode::Invalid,
                "Git output exceeds metadata limit",
            ));
        }
        self.check_budget()?;
        Ok(Output {
            status,
            stdout,
            stderr,
            truncated,
        })
    }

    pub(crate) fn branch(&self, value: &str) -> Result<String> {
        let full = if value.starts_with("refs/heads/") {
            value.to_owned()
        } else {
            if value.starts_with("refs/") {
                return Err(error(
                    ErrorCode::Invalid,
                    "Target must be a branch reference",
                ));
            }
            format!("refs/heads/{value}")
        };
        self.validate_ref(&full)?;
        Ok(full)
    }

    pub(crate) fn validate_ref(&self, value: &str) -> Result<()> {
        if value.len() > 240
            || !value.starts_with("refs/")
            || !value.is_ascii()
            || value.bytes().any(|c| c <= b' ' || c >= 127)
        {
            return Err(error(ErrorCode::Invalid, "Invalid Git reference"));
        }
        if !self.run(["check-ref-format", value])?.status.success() {
            return Err(error(ErrorCode::Invalid, "Invalid Git reference"));
        }
        Ok(())
    }

    pub(crate) fn head(&self, reference: &str) -> Result<Option<String>> {
        self.validate_ref(reference)?;
        let output = self
            .run([
                "for-each-ref",
                "--format=%(refname)%09%(objectname)%09%(symref)",
                reference,
            ])?
            .text()?;
        for line in output.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 3 {
                return Err(error(
                    ErrorCode::Conflict,
                    "Malformed local Git ref metadata",
                ));
            }
            if fields[0] != reference {
                continue;
            }
            if !fields[2].is_empty() {
                return Err(error(
                    ErrorCode::Conflict,
                    "Symbolic integration references are forbidden",
                ));
            }
            storage::validate_hash(fields[1], 40)?;
            return Ok(Some(fields[1].to_owned()));
        }
        Ok(None)
    }

    pub(crate) fn commit(&self, sha: &str) -> Result<()> {
        storage::validate_hash(sha, 40)?;
        if self.run(["cat-file", "-t", sha])?.text()?.trim() != "commit" {
            return Err(error(
                ErrorCode::Invalid,
                "Candidate must identify an exact Git commit, not a tag or tree",
            ));
        }
        Ok(())
    }

    pub(crate) fn update(&self, reference: &str, new: &str, expected: Option<&str>) -> Result<()> {
        self.validate_ref(reference)?;
        self.commit(new)?;
        if let Some(old) = expected {
            storage::validate_hash(old, 40)?;
        }
        self.run([
            "update-ref",
            "--no-deref",
            reference,
            new,
            expected.unwrap_or("0000000000000000000000000000000000000000"),
        ])?
        .checked()?;
        Ok(())
    }

    pub(crate) fn merge(
        &self,
        head: Option<&str>,
        candidate: &str,
        candidate_id: &str,
    ) -> Result<String> {
        self.commit(candidate)?;
        let Some(head) = head else {
            return Ok(candidate.to_owned());
        };
        self.commit(head)?;
        let ancestor = self.run(["merge-base", "--is-ancestor", head, candidate])?;
        if ancestor.status.success() {
            return Ok(candidate.to_owned());
        }
        if ancestor.status.code() != Some(1) {
            ancestor.checked()?;
        }
        // merge-tree performs a genuine three-way merge without a checkout, index,
        // hooks or running commands from the submitted repository.
        let merged = self.run([
            "merge-tree",
            "--write-tree",
            "--no-messages",
            head,
            candidate,
        ])?;
        if !merged.status.success() {
            if merged.status.code() == Some(1) {
                return Err(error(
                    ErrorCode::Conflict,
                    "Candidate conflicts with the current integration head; resolve and resubmit",
                ));
            }
            merged.checked()?;
            unreachable!();
        }
        let merged = String::from_utf8(merged.stdout)
            .map_err(|_| error(ErrorCode::Invalid, "Malformed merge-tree output"))?;
        let tree = merged
            .lines()
            .next()
            .ok_or_else(|| error(ErrorCode::Conflict, "Merge did not produce a tree"))?;
        storage::validate_hash(tree, 40)?;
        let mut timestamp = 0i64;
        for parent in [head, candidate] {
            let value = self.run(["show", "-s", "--format=%ct", parent])?.text()?;
            let value: i64 = value
                .trim()
                .parse()
                .map_err(|_| error(ErrorCode::Invalid, "Invalid commit timestamp"))?;
            timestamp = timestamp.max(value);
        }
        let timestamp = timestamp
            .checked_add(1)
            .filter(|value| *value <= 253_402_300_799)
            .ok_or_else(|| error(ErrorCode::Invalid, "Commit timestamp out of range"))?;
        let date = format!("@{timestamp} +0000");
        let environment = [
            ("GIT_AUTHOR_NAME", "DACS Coordinator".into()),
            ("GIT_AUTHOR_EMAIL", "coordinator@dacs.invalid".into()),
            ("GIT_COMMITTER_NAME", "DACS Coordinator".into()),
            ("GIT_COMMITTER_EMAIL", "coordinator@dacs.invalid".into()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_DATE", date),
        ];
        let message = format!("Integrate DACS candidate {candidate_id}");
        let sha = self
            .run_with_environment(
                [
                    "commit-tree",
                    tree,
                    "-p",
                    head,
                    "-p",
                    candidate,
                    "-m",
                    &message,
                ],
                &environment,
            )?
            .text()?;
        let sha = sha.trim().to_owned();
        storage::validate_hash(&sha, 40)?;
        Ok(sha)
    }
}

fn capture<R: Read + Send + 'static>(
    mut reader: R,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut result = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(result);
            }
            let remaining = MAX_OUTPUT - result.len();
            result.extend_from_slice(&buffer[..count.min(remaining)]);
            if count > remaining {
                overflow.store(true, Ordering::Relaxed);
            }
        }
    })
}

#[cfg(unix)]
struct ProcessTree {
    group: i32,
}
#[cfg(unix)]
impl ProcessTree {
    fn attach(child: &Child) -> Result<Self> {
        Ok(Self {
            group: child.id() as i32,
        })
    }
    fn stop(&self, child: &mut Child) {
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        let _ = child.kill();
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}
#[cfg(windows)]
impl ProcessTree {
    fn attach(child: &Child) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io(std::io::Error::last_os_error()));
            }
            let result = Self { job };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = 512 * 1024 * 1024;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0
            {
                return Err(io(std::io::Error::last_os_error()));
            }
            Ok(result)
        }
    }
    fn stop(&self, child: &mut Child) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
        let _ = child.kill();
    }
}
#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}
