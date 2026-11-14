//! Authenticated coordinator-local source publication and immutable artifacts.
//! Git and SQLite are deliberately separate durable operations: the core intent
//! gate precedes compare-and-swap, and restart reconciliation observes, never replays.

mod git;
mod quarantine;
mod storage;

use dacs_core::Store;
use dacs_protocol::{CandidateRecord, ErrorCode, Principal, Project, Publication, Result};
use git::Git;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use storage::{MAX_ARTIFACT, MAX_BUNDLE, ProjectLock, error, io};

pub const MAX_BUNDLE_BYTES: u64 = MAX_BUNDLE;
pub const MAX_ARTIFACT_BYTES: u64 = MAX_ARTIFACT;
const MAX_BUNDLE_REFS: usize = 128;

#[derive(Clone)]
pub struct SourceService {
    root: Arc<PathBuf>,
    store: Store,
}

impl SourceService {
    pub fn new(root: PathBuf, store: Store) -> Result<Self> {
        let root = std::path::absolute(root).map_err(io)?;
        storage::directory(&root)?;
        storage::directory(&root.join("projects"))?;
        let service = Self {
            root: Arc::new(root),
            store,
        };
        service.reconcile()?;
        Ok(service)
    }

    fn project_path(&self, project_id: &str) -> Result<PathBuf> {
        storage::validate_id(project_id)?;
        Ok(self.root.join("projects").join(project_id))
    }

    fn lock(&self, project_id: &str) -> Result<(ProjectLock, Git)> {
        let path = self.project_path(project_id)?;
        // Recheck the entire ancestor chain on every operation, not just startup.
        storage::directory(&path)?;
        let guard = ProjectLock::acquire(&path.join("project.lock"))?;
        let git = Git::new(path)?;
        // This project's OS lock also covers its temporary files. Leftovers from
        // a killed coordinator are not completed artifacts and can be removed.
        let mut count = 0;
        for entry in fs::read_dir(&git.temporary).map_err(io)? {
            count += 1;
            if count > 100_000 {
                return Err(error(
                    ErrorCode::Invalid,
                    "Too many interrupted temporary files",
                ));
            }
            let path = entry.map_err(io)?.path();
            let metadata = storage::inspect(&path)?
                .ok_or_else(|| error(ErrorCode::Io, "Temporary object disappeared"))?;
            if metadata.is_file() {
                fs::remove_file(path).map_err(io)?;
            } else if metadata.is_dir() {
                storage::remove_temporary_tree(&path)?;
            } else {
                return Err(error(ErrorCode::Forbidden, "Unexpected temporary storage object"));
            }
        }
        Ok((guard, git))
    }

    fn authorized(&self, principal: &Principal, project_id: &str) -> Result<Project> {
        storage::validate_id(project_id)?;
        self.store.authorize_project(principal, project_id)
    }

    pub fn ensure_project(&self, principal: &Principal, project_id: &str) -> Result<()> {
        let project = self.authorized(principal, project_id)?;
        let (_lock, git) = self.lock(project_id)?;
        git.ensure()?;
        git.branch(&project.target_branch)?;
        Ok(())
    }

    pub fn import_bundle(
        &self,
        principal: &Principal,
        project_id: &str,
        bundle: &[u8],
        seed: bool,
    ) -> Result<Value> {
        let project = self.authorized(principal, project_id)?;
        if seed && !principal.is_admin {
            return Err(error(
                ErrorCode::Forbidden,
                "Only the coordinator administrator may seed an empty target",
            ));
        }
        if bundle.is_empty() || bundle.len() as u64 > MAX_BUNDLE {
            return Err(error(
                ErrorCode::Invalid,
                "Source bundle must contain 1 to 134217728 bytes",
            ));
        }
        let (_lock, git) = self.lock(project_id)?;
        git.ensure()?;
        let target = git.branch(&project.target_branch)?;
        if seed && git.head(&target)?.is_some() {
            return Err(error(
                ErrorCode::Conflict,
                "Initial seeding cannot replace an existing target",
            ));
        }
        let temporary = storage::write_temporary(&git.temporary, bundle)?.into_temp_path();
        git.run([
            OsString::from("bundle"),
            OsString::from("verify"),
            temporary.as_os_str().to_owned(),
        ])?
        .checked()?;
        let heads = git
            .run([
                OsString::from("bundle"),
                OsString::from("list-heads"),
                temporary.as_os_str().to_owned(),
            ])?
            .text()?;
        let import_id = uuid::Uuid::new_v4().to_string();
        if seed
            && self
                .store
                .pending_publications()?
                .iter()
                .any(|publication| publication.project_id == project_id)
        {
            return Err(error(
                ErrorCode::Conflict,
                "Pending publication must be reconciled before initial seeding",
            ));
        }
        let mut advertised = Vec::<(String, String, String)>::new();
        let mut seed_sha = None;
        for line in heads.lines() {
            if advertised.len() >= MAX_BUNDLE_REFS {
                return Err(error(
                    ErrorCode::Invalid,
                    "Bundle advertises more than 128 references",
                ));
            }
            let (sha, reference) = line
                .split_once(' ')
                .ok_or_else(|| error(ErrorCode::Invalid, "Malformed bundle reference"))?;
            storage::validate_hash(sha, 40)?;
            if reference == "HEAD" {
                continue;
            }
            git.validate_ref(reference)?;
            if advertised
                .iter()
                .any(|(_, existing, _)| existing == reference)
            {
                return Err(error(
                    ErrorCode::Invalid,
                    "Bundle contains duplicate references",
                ));
            }
            if reference == target {
                seed_sha = Some(sha.to_owned());
            }
            let destination = format!("refs/dacs/imports/{import_id}/r{}", advertised.len());
            advertised.push((sha.to_owned(), reference.to_owned(), destination));
        }
        if advertised.is_empty() {
            return Err(error(
                ErrorCode::Invalid,
                "Bundle contains no named references",
            ));
        }
        if seed && seed_sha.is_none() {
            return Err(error(
                ErrorCode::Invalid,
                "Seed bundle must advertise the configured target branch",
            ));
        }
        quarantine::import(&git, &temporary, &advertised, &import_id, bundle.len() as u64,
            if seed { seed_sha.as_deref() } else { None })?;
        if seed {
            let sha = seed_sha.as_deref().expect("seed reference validated");
            git.commit(sha)?;
            // The zero expected value is a true create-if-absent operation, not
            // an unchecked branch overwrite, even against out-of-process Git.
            git.update(&target, sha, None)?;
        }
        let refs: Vec<Value> = advertised.into_iter().map(|(sha, source, imported)| json!({"sha": sha, "source_ref": source, "imported_ref": imported})).collect();
        Ok(
            json!({"project_id": project_id, "import_id": import_id, "seeded": seed, "target_branch": target, "target_head": git.head(&target)?, "refs": refs}),
        )
    }

    pub fn export_bundle(&self, principal: &Principal, project_id: &str) -> Result<Vec<u8>> {
        let project = self.authorized(principal, project_id)?;
        let (_lock, git) = self.lock(project_id)?;
        git.ensure()?;
        let target = git.branch(&project.target_branch)?;
        let head = git.head(&target)?.ok_or_else(|| {
            error(
                ErrorCode::Conflict,
                "Project target is empty; seed it before exporting source",
            )
        })?;
        git.commit(&head)?;
        let temporary = tempfile::NamedTempFile::new_in(&git.temporary)
            .map_err(io)?
            .into_temp_path();
        git.run([
            OsString::from("bundle"),
            OsString::from("create"),
            temporary.as_os_str().to_owned(),
            OsString::from(&target),
        ])?
        .checked()?;
        storage::read_limited(&temporary, MAX_BUNDLE)
    }

    /// Returns the exact staged integration diff when available, otherwise the
    /// submitted commit diff. Both text fields are bounded previews, never an
    /// instruction to execute external diff/textconv drivers from source.
    pub fn diff(&self, principal: &Principal, candidate_id: &str) -> Result<Value> {
        storage::validate_id(candidate_id)?;
        let candidate = self.store.candidate(principal, candidate_id)?;
        self.authorized(principal, &candidate.project_id)?;
        let (_lock, git) = self.lock(&candidate.project_id)?;
        git.verify()?;
        let candidate = self.store.candidate(principal, candidate_id)?;
        let empty_target = candidate.integration_sha.is_some() && candidate.expected_head.is_none();
        let base = match &candidate.expected_head {
            Some(head) => head.clone(),
            None if empty_target => git.run(["hash-object", "-t", "tree", "--stdin"])?.text()?.trim().to_owned(),
            None => {
                self.store
                    .snapshot(principal)?
                    .tasks
                    .into_iter()
                    .find(|task| {
                        task.id == candidate.submission.task_id
                            && task.spec.project_id == candidate.project_id
                    })
                    .ok_or_else(|| {
                        error(ErrorCode::NotFound, "Candidate source task is unavailable")
                    })?
                    .spec
                    .source_commit
            }
        };
        let target = candidate
            .integration_sha
            .as_deref()
            .unwrap_or(&candidate.submission.commit_sha);
        if !empty_target { git.commit(&base)?; }
        git.commit(target)?;
        let (diff, diff_truncated) = git.preview([
            "diff",
            "--patch",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--ignore-submodules=none",
            "--submodule=short",
            &base,
            target,
            "--",
        ])?;
        let (stat, stat_truncated) = git.preview([
            "diff",
            "--stat",
            "--stat-width=120",
            "--stat-name-width=80",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--ignore-submodules=none",
            "--submodule=short",
            &base,
            target,
            "--",
        ])?;
        Ok(json!({
            "candidate_id": candidate.id, "base_sha": base,
            "commit_sha": candidate.submission.commit_sha, "integration_sha": candidate.integration_sha,
            "diff": String::from_utf8_lossy(&diff), "stat": String::from_utf8_lossy(&stat),
            "truncated": diff_truncated || stat_truncated,
        }))
    }

    pub fn prepare(&self, principal: &Principal, candidate_id: &str) -> Result<CandidateRecord> {
        storage::validate_id(candidate_id)?;
        let candidate = self.store.candidate(principal, candidate_id)?;
        let project = self.authorized(principal, &candidate.project_id)?;
        let (_lock, git) = self.lock(&project.id)?;
        git.ensure()?;
        let candidate = self.store.candidate(principal, candidate_id)?;
        let target = git.branch(&project.target_branch)?;
        let expected_head = git.head(&target)?;
        let integration = git.merge(
            expected_head.as_deref(),
            &candidate.submission.commit_sha,
            candidate_id,
        )?;
        // Pin every exact prepared object independently. Restaging cannot remove
        // a commit still named by an older pending intent or approved decision.
        let reference = format!("refs/dacs/staged/{candidate_id}/{integration}");
        match git.head(&reference)? {
            None => git.update(&reference, &integration, None)?,
            Some(existing) if existing == integration => {}
            Some(_) => {
                return Err(error(
                    ErrorCode::Conflict,
                    "Immutable staged reference collision",
                ));
            }
        }
        self.store
            .stage_candidate(principal, candidate_id, expected_head, integration)
    }

    pub fn publish(&self, principal: &Principal, candidate_id: &str) -> Result<Value> {
        storage::validate_id(candidate_id)?;
        let candidate = self.store.candidate(principal, candidate_id)?;
        let project = self.authorized(principal, &candidate.project_id)?;
        let (_lock, git) = self.lock(&project.id)?;
        git.verify()?;
        let candidate = self.store.candidate(principal, candidate_id)?;
        if candidate.publication_id.is_some() {
            // A retry with an existing intent observes its outcome; it must not
            // turn an earlier unresolved write into a second publication attempt.
            let publication = self.store.prepare_publication(principal, candidate_id)?;
            validate_publication(&publication)?;
            let target = git.branch(&publication.target_branch)?;
            let observed = git.head(&target)?;
            let result = self
                .store
                .finish_publication(&publication.id, observed.as_deref())?;
            if matches!(publication.state.as_str(), "pending" | "reconciling")
                && observed.as_deref() != Some(publication.candidate_sha.as_str())
                && observed != publication.expected_head
            {
                return Err(error(
                    ErrorCode::Conflict,
                    "Existing publication has an ambiguous local head",
                ));
            }
            return Ok(result);
        }
        let integration = candidate.integration_sha.as_deref().ok_or_else(|| {
            error(
                ErrorCode::Conflict,
                "Prepare and review the exact integration commit before publication",
            )
        })?;
        git.commit(integration)?;
        let target = git.branch(&project.target_branch)?;
        if git.head(&target)? != candidate.expected_head {
            return Err(error(
                ErrorCode::Conflict,
                "Target changed since preparation; prepare and review again",
            ));
        }
        // Core atomically serializes this gate against cancellation/invalidation.
        // No target ref may change before this durable intent has been committed.
        let publication = self.store.prepare_publication(principal, candidate_id)?;
        if publication.project_id != project.id
            || publication.candidate_id != candidate_id
            || git.branch(&publication.target_branch)? != target
        {
            return Err(error(
                ErrorCode::Internal,
                "Publication intent does not match the authorized candidate",
            ));
        }
        validate_publication(&publication)?;
        let updated = git.update(
            &target,
            &publication.candidate_sha,
            publication.expected_head.as_deref(),
        );
        let observed = git.head(&target)?;
        let finished = self
            .store
            .finish_publication(&publication.id, observed.as_deref())?;
        // An error after the ref write (for example fsync/output failure) is not
        // grounds to claim failure if the exact candidate is already published.
        if observed.as_deref() == Some(publication.candidate_sha.as_str()) {
            return Ok(finished);
        }
        updated?;
        Ok(finished)
    }

    pub fn reconcile(&self) -> Result<()> {
        let mut first_error = None;
        for publication in self.store.pending_publications()? {
            let outcome = (|| {
                validate_publication(&publication)?;
                let (_lock, git) = self.lock(&publication.project_id)?;
                // Never create an empty repository to replace missing durable
                // state, and never replay update-ref during crash recovery.
                git.verify()?;
                let target = git.branch(&publication.target_branch)?;
                let observed = git.head(&target)?;
                self.store
                    .finish_publication(&publication.id, observed.as_deref())?;
                if observed.as_deref() != Some(publication.candidate_sha.as_str())
                    && observed != publication.expected_head
                {
                    return Err(error(
                        ErrorCode::Conflict,
                        format!(
                            "Publication {} has an ambiguous local head; manual investigation is required",
                            publication.id
                        ),
                    ));
                }
                Ok(())
            })();
            if let Err(e) = outcome {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn put_artifact(
        &self,
        principal: &Principal,
        project_id: &str,
        bytes: &[u8],
        expected_digest: Option<&str>,
    ) -> Result<Value> {
        self.authorized(principal, project_id)?;
        if bytes.len() as u64 > MAX_ARTIFACT {
            return Err(error(
                ErrorCode::Invalid,
                "Artifact exceeds 67108864-byte limit",
            ));
        }
        if let Some(expected) = expected_digest {
            storage::validate_hash(expected, 64)?;
        }
        let digest = hex::encode(Sha256::digest(bytes));
        if expected_digest.is_some_and(|expected| expected != digest) {
            return Err(error(
                ErrorCode::Invalid,
                "Artifact SHA-256 does not match the supplied digest",
            ));
        }
        let (_lock, git) = self.lock(project_id)?;
        let directory = self.project_path(project_id)?.join("artifacts");
        storage::directory(&directory)?;
        let destination = directory.join(&digest);
        let created = if storage::inspect(&destination)?.is_some() {
            verify_artifact(&destination, &digest)?;
            false
        } else {
            let temporary = storage::write_temporary(&git.temporary, bytes)?;
            match temporary.persist_noclobber(&destination) {
                Ok(file) => {
                    file.sync_all().map_err(io)?;
                    storage::sync_directory(&directory)?;
                    true
                }
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_artifact(&destination, &digest)?;
                    false
                }
                Err(e) => return Err(io(e.error)),
            }
        };
        Ok(
            json!({"project_id": project_id, "digest": digest, "size": bytes.len(), "created": created}),
        )
    }

    pub fn get_artifact(
        &self,
        principal: &Principal,
        project_id: &str,
        digest: &str,
    ) -> Result<Vec<u8>> {
        self.authorized(principal, project_id)?;
        storage::validate_hash(digest, 64)?;
        let (_lock, _git) = self.lock(project_id)?;
        let directory = self.project_path(project_id)?.join("artifacts");
        storage::directory(&directory)?;
        let bytes = storage::read_limited(&directory.join(digest), MAX_ARTIFACT)?;
        if hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(error(
                ErrorCode::Conflict,
                "Stored artifact failed SHA-256 verification",
            ));
        }
        Ok(bytes)
    }
}

fn validate_publication(publication: &Publication) -> Result<()> {
    storage::validate_id(&publication.project_id)?;
    storage::validate_id(&publication.candidate_id)?;
    storage::validate_hash(&publication.candidate_sha, 40)?;
    if let Some(expected) = &publication.expected_head {
        storage::validate_hash(expected, 40)?;
    }
    Ok(())
}

fn verify_artifact(path: &Path, expected: &str) -> Result<()> {
    use std::io::Read;
    let metadata = storage::inspect(path)?
        .ok_or_else(|| error(ErrorCode::NotFound, "Artifact disappeared"))?;
    if !metadata.is_file() || metadata.len() > MAX_ARTIFACT {
        return Err(error(
            ErrorCode::Conflict,
            "Invalid existing artifact object",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(io)?.take(MAX_ARTIFACT + 1);
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer).map_err(io)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        hasher.update(&buffer[..count]);
    }
    if total > MAX_ARTIFACT || hex::encode(hasher.finalize()) != expected {
        return Err(error(
            ErrorCode::Conflict,
            "Immutable artifact collision or on-disk corruption; existing object was not overwritten",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
