use crate::{catalogue, util};
use base64::{Engine, engine::general_purpose::STANDARD};
use dacs_protocol::{ApiError, ErrorCode, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const MAX_ARTIFACT: u64 = 512 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMetadata {
    pub schema_version: u32,
    pub version: String,
    pub platform: String,
    pub arch: String,
    pub min_current_version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub artifact: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedUpdate {
    pub metadata: UpdateMetadata,
    pub signature: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedUpdate {
    schema_version: u32,
    manifest: SignedUpdate,
    public_key: String,
    current_version: String,
    staged_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateJournal {
    schema_version: u32,
    version: String,
    target: PathBuf,
    backup: PathBuf,
    candidate: PathBuf,
    failed: PathBuf,
    previous_sha256: String,
    next_sha256: String,
    phase: String,
    updated_at: u64,
}

// Release owners sign exactly these UTF-8 bytes: compact serde JSON in UpdateMetadata's declared field order.
// No private key or remote-selected trust anchor is accepted by the apply path.
pub fn update_signing_bytes(metadata: &UpdateMetadata) -> Result<Vec<u8>> {
    serde_json::to_vec(metadata).map_err(util::io)
}
fn verify(manifest: &SignedUpdate, public_key: &str, current_version: &str) -> Result<()> {
    let bytes = STANDARD
        .decode(public_key)
        .map_err(|_| util::invalid("Trusted Ed25519 public key must be base64"))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| util::invalid("Ed25519 public key must be 32 bytes"))?;
    let key =
        VerifyingKey::from_bytes(&key).map_err(|_| util::invalid("Invalid Ed25519 public key"))?;
    let signature = STANDARD
        .decode(&manifest.signature)
        .map_err(|_| util::invalid("Ed25519 signature must be base64"))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| util::invalid("Ed25519 signature must be 64 bytes"))?;
    key.verify_strict(&update_signing_bytes(&manifest.metadata)?, &signature)
        .map_err(|_| ApiError::new(ErrorCode::Forbidden, "Update signature verification failed"))?;
    let metadata = &manifest.metadata;
    if metadata.schema_version != 1 || metadata.artifact != "executable" {
        return Err(util::invalid(
            "Unsupported signed release schema or artifact format; archives/scripts are not executable updates",
        ));
    }
    if metadata.platform != catalogue::platform() || metadata.arch != catalogue::arch() {
        return Err(util::invalid(
            "Signed release platform/architecture mismatch",
        ));
    }
    let current = semver::Version::parse(current_version)
        .map_err(|_| util::invalid("Current app version is not semver"))?;
    let next = semver::Version::parse(&metadata.version)
        .map_err(|_| util::invalid("Signed app version is not semver"))?;
    let minimum = semver::Version::parse(&metadata.min_current_version)
        .map_err(|_| util::invalid("Signed minimum version is not semver"))?;
    if current.cmp_precedence(&minimum).is_lt() {
        return Err(util::conflict(
            "Current app is below the signed compatibility minimum; use a compatible intermediate release",
        ));
    }
    if next.cmp_precedence(&current).is_lt() {
        return Err(util::conflict(
            "Downgrade rejected; rollback is a separate explicit operation",
        ));
    }
    if next.cmp_precedence(&minimum).is_lt() {
        return Err(util::invalid(
            "Signed release version is below its compatibility minimum",
        ));
    }
    if metadata.size == 0 || metadata.size > MAX_ARTIFACT {
        return Err(util::invalid(
            "Signed artifact size exceeds executable update bounds",
        ));
    }
    if metadata.sha256.len() != 64
        || !metadata
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(util::invalid(
            "Signed SHA-256 must be lowercase 64-character hex",
        ));
    }
    let url =
        url::Url::parse(&metadata.url).map_err(|_| util::invalid("Invalid signed download URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(util::invalid(
            "Updates require a credential-free HTTPS URL without fragments",
        ));
    }
    Ok(())
}

pub fn verify_update(value: Value, public_key: &str, current_version: &str) -> Result<Value> {
    let manifest: SignedUpdate = util::decode(value)?;
    verify(&manifest, public_key, current_version)?;
    Ok(
        json!({"verified":true,"update_available":semver::Version::parse(&manifest.metadata.version).map_err(util::io)?.cmp_precedence(&semver::Version::parse(current_version).map_err(util::io)?).is_gt(),"metadata":manifest.metadata,"metadata_sha256":util::digest(&update_signing_bytes(&manifest.metadata)?),"trust":"local configured Ed25519 public key","staged":false,"applied":false}),
    )
}

fn file_digest(path: &Path, maximum: u64) -> Result<(String, u64)> {
    util::reject_links(path)?;
    let file = File::open(path).map_err(util::io)?;
    if !file.metadata().map_err(util::io)?.is_file() {
        return Err(util::invalid("Update artifact must be a regular file"));
    }
    let mut input = file.take(maximum + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let count = input.read(&mut buffer).map_err(util::io)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > maximum {
            return Err(util::invalid("Artifact exceeds size bound"));
        }
        hasher.update(&buffer[..count]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}
fn validate_file(path: &Path, metadata: &UpdateMetadata) -> Result<()> {
    let (digest, size) = file_digest(path, metadata.size)?;
    if size != metadata.size || digest != metadata.sha256 {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "Staged artifact size/SHA-256 differs from signed metadata",
        ));
    }
    Ok(())
}
fn update_directory(state_dir: &Path) -> Result<PathBuf> {
    let state = util::state_directory(state_dir)?;
    util::state_directory(&state.join("updates"))
}
fn enforce_release_ledger(directory: &Path, version: &str) -> Result<()> {
    let path = directory.join("highest-applied.json");
    if path.exists() {
        let ledger: Value = util::read_json(&path)?;
        let highest = ledger["version"]
            .as_str()
            .ok_or_else(|| util::conflict("Invalid applied release ledger"))?;
        let highest = semver::Version::parse(highest).map_err(util::io)?;
        let version = semver::Version::parse(version).map_err(util::io)?;
        if version < highest {
            return Err(util::conflict(
                "Signed release is older than the highest applied release; use explicit rollback instead",
            ));
        }
    }
    Ok(())
}

pub fn stage_update(
    value: Value,
    public_key: &str,
    current_version: &str,
    state_dir: &Path,
) -> Result<Value> {
    let manifest: SignedUpdate = util::decode(value)?;
    verify(&manifest, public_key, current_version)?;
    if !semver::Version::parse(&manifest.metadata.version)
        .map_err(util::io)?
        .cmp_precedence(&semver::Version::parse(current_version).map_err(util::io)?)
        .is_gt()
    {
        return Err(util::conflict("Only a newer signed release may be staged"));
    }
    let directory = update_directory(state_dir)?;
    let _lock = util::lock(&directory.join("update.lock"))?;
    let trust_path = directory.join("trusted-key.json");
    if trust_path.exists() {
        let stored: String = util::read_json(&trust_path)?;
        if stored != public_key {
            return Err(ApiError::new(
                ErrorCode::Forbidden,
                "Update trust anchor changed; key rotation requires explicit local release configuration/state migration",
            ));
        }
    } else {
        util::atomic_json(&trust_path, &public_key)?;
    }
    enforce_release_ledger(&directory, &manifest.metadata.version)?;
    let journal_path = directory.join("apply-journal.json");
    if journal_path.exists() {
        let journal: UpdateJournal = util::read_json(&journal_path)?;
        if !matches!(journal.phase.as_str(), "completed" | "rolled_back") {
            return Err(util::conflict(
                "Reconcile the existing interrupted update before staging another release",
            ));
        }
    }
    let stage_path = directory.join("staged.json");
    let artifact = directory.join("artifact.bin");
    if stage_path.exists() {
        let staged: StagedUpdate = util::read_json(&stage_path)?;
        if staged.manifest.metadata.sha256 == manifest.metadata.sha256
            && staged.manifest.signature == manifest.signature
            && staged.public_key == public_key
            && artifact.exists()
        {
            validate_file(&artifact, &manifest.metadata)?;
            return Ok(
                json!({"status":"staged","metadata":manifest.metadata,"sha256":manifest.metadata.sha256,"reused":true,"apply_requires_local_action":true}),
            );
        }
    }
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(util::io)?;
    let mut response = client
        .get(&manifest.metadata.url)
        .send()
        .map_err(util::io)?
        .error_for_status()
        .map_err(util::io)?;
    if response.status().is_redirection() {
        return Err(util::invalid(
            "Signed update URL must be the final HTTPS URL; redirects are not followed",
        ));
    }
    if let Some(length) = response.content_length() {
        if length != manifest.metadata.size {
            return Err(util::invalid(
                "HTTP Content-Length differs from signed artifact size",
            ));
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(util::io)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = response.read(&mut buffer).map_err(util::io)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > manifest.metadata.size {
            return Err(util::invalid("Download exceeded signed size"));
        }
        hasher.update(&buffer[..count]);
        temporary.write_all(&buffer[..count]).map_err(util::io)?;
    }
    if size != manifest.metadata.size || hex::encode(hasher.finalize()) != manifest.metadata.sha256
    {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "Downloaded update failed signed size/SHA-256 verification",
        ));
    }
    temporary.as_file().sync_all().map_err(util::io)?;
    util::reject_links(&artifact)?;
    let temporary = temporary.into_temp_path();
    util::rename_durable(&temporary, &artifact, true).map_err(util::io)?;
    util::sync_directory(&directory)?;
    let staged = StagedUpdate {
        schema_version: 1,
        manifest,
        public_key: public_key.into(),
        current_version: current_version.into(),
        staged_at: util::now(),
    };
    util::atomic_json(&stage_path, &staged)?;
    Ok(
        json!({"status":"staged","metadata":staged.manifest.metadata,"sha256":staged.manifest.metadata.sha256,"apply_requires_local_action":true,"worker_started":false,"network_joined":false}),
    )
}

fn load_staged(directory: &Path) -> Result<StagedUpdate> {
    let staged: StagedUpdate = util::read_json(&directory.join("staged.json"))?;
    let trusted: String = util::read_json(&directory.join("trusted-key.json"))?;
    if staged.schema_version != 1 || staged.public_key != trusted {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "Staged update does not match local trust configuration",
        ));
    }
    verify(&staged.manifest, &trusted, &staged.current_version)?;
    validate_file(&directory.join("artifact.bin"), &staged.manifest.metadata)?;
    Ok(staged)
}
fn release_receipt_path(directory: &Path, digest: &str) -> Result<PathBuf> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(util::invalid("Invalid release receipt digest"));
    }
    Ok(directory.join(format!("release-{digest}.json")))
}

fn load_recorded_release(directory: &Path, journal: &UpdateJournal) -> Result<StagedUpdate> {
    let release: StagedUpdate = util::read_json(&release_receipt_path(directory, &journal.next_sha256)?)?;
    let trusted: String = util::read_json(&directory.join("trusted-key.json"))?;
    if release.schema_version != 1 || release.public_key != trusted {
        return Err(util::conflict("Recorded release does not match the local trust anchor"));
    }
    verify(&release.manifest, &trusted, &release.current_version)?;
    check_journal(journal, &journal.target, &release)?;
    Ok(release)
}

/// Startup recovery never authorizes a merely staged release or creates a new apply journal.
pub fn reconcile_update(state_dir: &Path, target: &Path) -> Result<Value> {
    let directory = state_dir.join("updates");
    util::reject_links(&directory)?;
    if !directory.exists() {
        return Ok(json!({"status":"idle","pending":false}));
    }
    let directory = fs::canonicalize(directory).map_err(util::io)?;
    let _lock = util::lock(&directory.join("update.lock"))?;
    let path = directory.join("apply-journal.json");
    if !path.exists() {
        return Ok(json!({"status":"idle","pending":false,"staged":directory.join("staged.json").exists()}));
    }
    let journal: UpdateJournal = util::read_json(&path)?;
    let target = absolute_target(target)?;
    let release = load_recorded_release(&directory, &journal)?;
    check_journal(&journal, &target, &release)?;
    if journal.phase == "rolled_back" {
        if !digest_matches(&target, &journal.previous_sha256)? {
            return Err(util::conflict("Rolled-back target no longer matches its recorded digest"));
        }
        return Ok(json!({"status":"rolled_back","pending":false,"target":target}));
    }
    apply_journal(&directory, state_dir, &target, &release, journal)
}

fn absolute_target(target: &Path) -> Result<PathBuf> {
    util::reject_links(target)?;
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir().map_err(util::io)?.join(target)
    };
    let parent = fs::canonicalize(
        target
            .parent()
            .ok_or_else(|| util::invalid("Target lacks parent directory"))?,
    )
    .map_err(util::io)?;
    let name = target
        .file_name()
        .ok_or_else(|| util::invalid("Target lacks executable filename"))?;
    Ok(parent.join(name))
}
fn check_journal(journal: &UpdateJournal, target: &Path, staged: &StagedUpdate) -> Result<()> {
    if journal.schema_version != 1
        || journal.target != target
        || journal.next_sha256 != staged.manifest.metadata.sha256
        || journal.version != staged.manifest.metadata.version
    {
        return Err(util::conflict(
            "Interrupted update journal belongs to a different target/release",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| util::invalid("Invalid target"))?;
    for path in [&journal.backup, &journal.candidate, &journal.failed] {
        util::reject_links(path)?;
        if path.parent() != Some(parent)
            || !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".dacs-"))
        {
            return Err(util::invalid(
                "Update journal contains unsafe recovery path",
            ));
        }
    }
    if journal.previous_sha256.len() != 64
        || !journal
            .previous_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(util::invalid("Invalid rollback digest"));
    }
    Ok(())
}
fn save_journal(directory: &Path, journal: &mut UpdateJournal, phase: &str) -> Result<()> {
    journal.phase = phase.into();
    journal.updated_at = util::now();
    util::atomic_json(&directory.join("apply-journal.json"), journal)
}
fn digest_matches(path: &Path, expected: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(file_digest(path, MAX_ARTIFACT)?.0 == expected)
}
fn rename_wait(source: &Path, target: &Path) -> Result<()> {
    let start = Instant::now();
    loop {
        util::reject_links(source)?;
        util::reject_links(target)?;
        match util::rename_durable(source, target, false) {
            Ok(()) => {
                return util::sync_directory(
                    target
                        .parent()
                        .ok_or_else(|| util::invalid("Missing target parent"))?,
                );
            }
            Err(error) => {
                if cfg!(windows)
                    && start.elapsed() < Duration::from_secs(60)
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                    )
                {
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
                return Err(util::io(error));
            }
        }
    }
}
fn helper_contract(directory: &Path, state_dir: &Path, target: &Path) -> Result<Option<Value>> {
    let current = std::env::current_exe().map_err(util::io)?;
    let current = fs::canonicalize(current).map_err(util::io)?;
    if current != target {
        return Ok(None);
    }
    let name = format!(
        "update-helper-{}{}",
        uuid::Uuid::new_v4(),
        if cfg!(windows) { ".exe" } else { "" }
    );
    let helper = directory.join(name);
    util::reject_links(&helper)?;
    fs::copy(&current, &helper).map_err(util::io)?;
    OpenOptions::new()
        .write(true)
        .open(&helper)
        .and_then(|file| file.sync_all())
        .map_err(util::io)?;
    if file_digest(&helper, MAX_ARTIFACT)?.0 != file_digest(&current, MAX_ARTIFACT)?.0 {
        return Err(util::conflict(
            "Running-executable helper copy verification failed",
        ));
    }
    let state = fs::canonicalize(state_dir).map_err(util::io)?;
    Ok(Some(
        json!({"status":"helper_required","helper_executable":helper,"arguments":["update","apply","--state-dir",state.to_string_lossy(),"--target",target.to_string_lossy()],"exit_required":true,"reason":"Launch this separate local executable with the command array, then exit the running target. The helper verifies staged signatures/digests again and waits up to 60 seconds for Windows executable locks. Do not mark the update complete until its durable journal says completed."}),
    ))
}
// A failed restore leaves no executable at the target path; the caller must hear about both failures.
fn with_rollback_outcome(mut error: ApiError, rollback: Result<Value>) -> ApiError {
    if let Err(rollback) = rollback {
        error.message = format!(
            "{}; automatic rollback also failed and the target may be missing: {}",
            error.message, rollback.message
        );
    }
    error
}
fn recover_rollback(directory: &Path, journal: &mut UpdateJournal) -> Result<Value> {
    if !digest_matches(&journal.backup, &journal.previous_sha256)?
        && !digest_matches(&journal.target, &journal.previous_sha256)?
    {
        return Err(util::conflict(
            "Rollback backup missing or changed; manual recovery required",
        ));
    }
    if digest_matches(&journal.target, &journal.next_sha256)? {
        if journal.failed.exists() {
            if !digest_matches(&journal.failed, &journal.next_sha256)? {
                return Err(util::conflict(
                    "Failed-artifact recovery path already occupied",
                ));
            }
            fs::remove_file(&journal.failed).map_err(util::io)?;
        }
        rename_wait(&journal.target, &journal.failed)?;
    } else if journal.target.exists() && !digest_matches(&journal.target, &journal.previous_sha256)?
    {
        return Err(util::conflict(
            "Target changed externally during rollback; refusing overwrite",
        ));
    }
    if !journal.target.exists() {
        rename_wait(&journal.backup, &journal.target)?;
    }
    if !digest_matches(&journal.target, &journal.previous_sha256)? {
        return Err(util::conflict("Rollback target verification failed"));
    }
    save_journal(directory, journal, "rolled_back")?;
    Ok(
        json!({"status":"rolled_back","target":journal.target,"restored_sha256":journal.previous_sha256,"failed_version":journal.version,"highest_release_ledger_retained":true}),
    )
}

pub fn apply_staged_update(state_dir: &Path, target: &Path) -> Result<Value> {
    let directory = update_directory(state_dir)?;
    let _lock = util::lock(&directory.join("update.lock"))?;
    let target = absolute_target(target)?;
    let journal_path = directory.join("apply-journal.json");
    let previous: Option<UpdateJournal> = if journal_path.exists() {Some(util::read_json(&journal_path)?)} else {None};
    if let Some(journal) = &previous {
        if !matches!(journal.phase.as_str(), "completed" | "rolled_back") {
            let release = load_recorded_release(&directory, journal)?;
            return apply_journal(&directory, state_dir, &target, &release, journal.clone());
        }
    }
    let staged = load_staged(&directory)?;
    enforce_release_ledger(&directory, &staged.manifest.metadata.version)?;
    let journal = if let Some(previous) = previous {
        if previous.next_sha256 == staged.manifest.metadata.sha256 {
            check_journal(&previous, &target, &staged)?;
            previous
        } else {
            // A non-terminal journal already returned above, so the previous release is settled.
            new_journal(&directory, &target, &staged)?
        }
    } else {
        new_journal(&directory, &target, &staged)?
    };
    apply_journal(&directory, state_dir, &target, &staged, journal)
}

fn apply_journal(directory: &Path, state_dir: &Path, target: &Path, staged: &StagedUpdate, mut journal: UpdateJournal) -> Result<Value> {
    check_journal(&journal, &target, &staged)?;
    if journal.phase == "rollback_pending" {
        if let Some(contract) = helper_contract(&directory, state_dir, &target)? {
            return Ok(contract);
        }
        return recover_rollback(&directory, &mut journal);
    }
    if journal.phase == "rolled_back" {
        return Err(util::conflict(
            "This staged release was explicitly rolled back; stage a newer release",
        ));
    }
    if journal.phase == "completed" {
        if !digest_matches(&target, &journal.next_sha256)? {
            return Err(util::conflict(
                "Applied target no longer matches signed release",
            ));
        }
        return Ok(
            json!({"status":"completed","version":journal.version,"target":target,"sha256":journal.next_sha256,"rollback_available":journal.backup.exists()}),
        );
    }
    if !matches!(
        journal.phase.as_str(),
        "prepared" | "backed_up" | "installed"
    ) {
        return Err(util::conflict(
            "Unknown update journal phase; refusing blind replay",
        ));
    }
    if !digest_matches(target, &journal.next_sha256)? {
        if let Some(contract) = helper_contract(directory, state_dir, target)? {
            return Ok(contract);
        }
    }
    // Reconcile observed filesystem state before replaying a move; each rename is durably recorded.
    if digest_matches(&target, &journal.next_sha256)? {
        if !digest_matches(&journal.backup, &journal.previous_sha256)? {
            return Err(util::conflict(
                "New target exists but rollback backup is missing/changed",
            ));
        }
        save_journal(&directory, &mut journal, "installed")?;
    } else {
        let target_present = target.exists();
        if target_present {
            if !digest_matches(&target, &journal.previous_sha256)? {
                return Err(util::conflict(
                    "Target changed since update preparation; refusing overwrite",
                ));
            }
            if journal.backup.exists() {
                return Err(util::conflict(
                    "Both original target and backup exist; manual reconciliation required",
                ));
            }
        } else if !digest_matches(&journal.backup, &journal.previous_sha256)? {
            return Err(util::conflict(
                "Original executable and valid rollback backup are both unavailable",
            ));
        }
        // Verify the candidate while the original executable is still at the target path.
        if !digest_matches(&journal.candidate, &journal.next_sha256)? {
            save_journal(&directory, &mut journal, "rollback_pending")?;
            let rollback = recover_rollback(&directory, &mut journal);
            return Err(with_rollback_outcome(
                util::conflict("Prepared update candidate changed; rollback recovery initiated"),
                rollback,
            ));
        }
        if target_present {
            rename_wait(&target, &journal.backup)?;
        }
        save_journal(&directory, &mut journal, "backed_up")?;
        if let Err(error) = rename_wait(&journal.candidate, &target) {
            save_journal(&directory, &mut journal, "rollback_pending")?;
            let rollback = recover_rollback(&directory, &mut journal);
            return Err(with_rollback_outcome(error, rollback));
        }
        validate_file(&target, &staged.manifest.metadata)?;
        save_journal(&directory, &mut journal, "installed")?;
    }
    util::atomic_json(
        &directory.join("highest-applied.json"),
        &json!({"version":journal.version,"sha256":journal.next_sha256,"applied_at":util::now()}),
    )?;
    save_journal(&directory, &mut journal, "completed")?;
    Ok(
        json!({"status":"completed","version":journal.version,"target":target,"sha256":journal.next_sha256,"rollback_available":true,"backup":journal.backup}),
    )
}

fn new_journal(directory: &Path, target: &Path, staged: &StagedUpdate) -> Result<UpdateJournal> {
    let metadata = fs::metadata(target).map_err(util::io)?;
    if !metadata.is_file() {
        return Err(util::invalid(
            "Update target must be an existing regular executable",
        ));
    }
    let previous_sha256 = file_digest(target, MAX_ARTIFACT)?.0;
    let parent = target
        .parent()
        .ok_or_else(|| util::invalid("Target has no parent"))?;
    let id = uuid::Uuid::new_v4();
    let candidate = parent.join(format!(".dacs-next-{id}"));
    let backup = parent.join(format!(".dacs-backup-{id}"));
    let failed = parent.join(format!(".dacs-failed-{id}"));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&candidate)
        .map_err(util::io)?;
    let mut input = File::open(directory.join("artifact.bin")).map_err(util::io)?;
    std::io::copy(&mut input, &mut output).map_err(util::io)?;
    output
        .set_permissions(metadata.permissions())
        .map_err(util::io)?;
    output.sync_all().map_err(util::io)?;
    drop(output);
    validate_file(&candidate, &staged.manifest.metadata)?;
    util::sync_directory(parent)?;
    // Apply metadata is immutable per artifact; staging another download cannot erase rollback evidence.
    util::atomic_json(&release_receipt_path(directory, &staged.manifest.metadata.sha256)?, staged)?;
    let mut journal = UpdateJournal {
        schema_version: 1,
        version: staged.manifest.metadata.version.clone(),
        target: target.to_path_buf(),
        backup,
        candidate,
        failed,
        previous_sha256,
        next_sha256: staged.manifest.metadata.sha256.clone(),
        phase: "prepared".into(),
        updated_at: util::now(),
    };
    save_journal(directory, &mut journal, "prepared")?;
    Ok(journal)
}

/// Explicit local rollback action; never invoked solely because a provider/worker is unavailable.
pub fn rollback_update(state_dir: &Path, target: &Path) -> Result<Value> {
    let directory = update_directory(state_dir)?;
    let _lock = util::lock(&directory.join("update.lock"))?;
    let target = absolute_target(target)?;
    let mut journal: UpdateJournal = util::read_json(&directory.join("apply-journal.json"))?;
    let staged = load_recorded_release(&directory, &journal)?;
    check_journal(&journal, &target, &staged)?;
    if journal.phase == "rolled_back" {
        return Ok(
            json!({"status":"rolled_back","target":target,"restored_sha256":journal.previous_sha256}),
        );
    }
    if journal.phase != "completed" && journal.phase != "rollback_pending" {
        return Err(util::conflict(
            "Finish/reconcile apply before explicitly rolling it back",
        ));
    }
    save_journal(&directory, &mut journal, "rollback_pending")?;
    if let Some(contract) = helper_contract(&directory, state_dir, &target)? {
        return Ok(contract);
    }
    recover_rollback(&directory, &mut journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    fn stage_fixture(directory: &Path, key: &SigningKey, bytes: &[u8], version: &str, current: &str) -> StagedUpdate {
        let metadata = UpdateMetadata {
            schema_version:1,version:version.into(),platform:catalogue::platform().into(),arch:catalogue::arch().into(),
            min_current_version:"1.0.0".into(),url:"https://downloads.example.com/dacs".into(),
            sha256:util::digest(bytes),size:bytes.len() as u64,artifact:"executable".into(),
        };
        let signature = STANDARD.encode(key.sign(&update_signing_bytes(&metadata).unwrap()).to_bytes());
        let staged = StagedUpdate {schema_version:1,manifest:SignedUpdate{metadata,signature},
            public_key:STANDARD.encode(key.verifying_key().to_bytes()),current_version:current.into(),staged_at:1};
        util::atomic_json(&directory.join("trusted-key.json"), &staged.public_key).unwrap();
        fs::write(directory.join("artifact.bin"), bytes).unwrap();
        util::atomic_json(&directory.join("staged.json"), &staged).unwrap();
        staged
    }

    #[test]
    fn startup_reconciliation_never_applies_an_unapproved_stage() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let target = root.path().join("dacs-bin");
        fs::write(&target, b"original").unwrap();
        assert_eq!(reconcile_update(&state, &target).unwrap()["status"], "idle");
        assert!(!state.exists());
        let directory = update_directory(&state).unwrap();
        let key = SigningKey::from_bytes(&rand::random::<[u8;32]>());
        stage_fixture(&directory, &key, b"staged", "2.0.0", "1.0.0");
        assert_eq!(reconcile_update(&state, &target).unwrap()["status"], "idle");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(!directory.join("apply-journal.json").exists());
    }

    #[test]
    fn next_stage_does_not_destroy_previous_release_rollback() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let directory = update_directory(&state).unwrap();
        let target = absolute_target(&root.path().join("dacs-bin")).unwrap();
        let key = SigningKey::from_bytes(&rand::random::<[u8;32]>());
        fs::write(&target, b"original").unwrap();
        stage_fixture(&directory, &key, b"release-a", "2.0.0", "1.0.0");
        assert_eq!(apply_staged_update(&state, &target).unwrap()["status"], "completed");
        stage_fixture(&directory, &key, b"release-b", "3.0.0", "2.0.0");
        assert_eq!(rollback_update(&state, &target).unwrap()["status"], "rolled_back");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read(directory.join("artifact.bin")).unwrap(), b"release-b");
    }

    #[test]
    fn startup_recovers_prepared_and_already_installed_transactions() {
        for already_installed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = root.path().join("state");
            let directory = update_directory(&state).unwrap();
            let target = absolute_target(&root.path().join("dacs-bin")).unwrap();
            let key = SigningKey::from_bytes(&rand::random::<[u8;32]>());
            fs::write(&target, b"original").unwrap();
            let release = stage_fixture(&directory, &key, b"release-a", "2.0.0", "1.0.0");
            let journal = new_journal(&directory, &target, &release).unwrap();
            if already_installed {
                fs::rename(&target, &journal.backup).unwrap();
                fs::rename(&journal.candidate, &target).unwrap();
            }
            assert_eq!(reconcile_update(&state, &target).unwrap()["status"], "completed");
            assert_eq!(fs::read(&target).unwrap(), b"release-a");
            let repeated = reconcile_update(&state, &target).unwrap();
            assert_eq!(repeated["status"], "completed");
            assert!(repeated.get("exit_required").is_none());
        }
    }

    #[test]
    fn completed_running_executable_does_not_request_helper_or_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let directory = update_directory(&state).unwrap();
        let target = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let key = SigningKey::from_bytes(&rand::random::<[u8;32]>());
        let mut release = stage_fixture(&directory, &key, b"fixture", "2.0.0", "1.0.0");
        let (digest, size) = file_digest(&target, MAX_ARTIFACT).unwrap();
        release.manifest.metadata.sha256 = digest.clone();
        release.manifest.metadata.size = size;
        release.manifest.signature = STANDARD.encode(key.sign(&update_signing_bytes(&release.manifest.metadata).unwrap()).to_bytes());
        util::atomic_json(&release_receipt_path(&directory, &digest).unwrap(), &release).unwrap();
        let parent = target.parent().unwrap();
        let id = uuid::Uuid::new_v4();
        let mut journal = UpdateJournal {schema_version:1,version:"2.0.0".into(),target:target.clone(),
            backup:parent.join(format!(".dacs-backup-{id}")),candidate:parent.join(format!(".dacs-next-{id}")),
            failed:parent.join(format!(".dacs-failed-{id}")),previous_sha256:util::digest(b"original"),next_sha256:digest,
            phase:"completed".into(),updated_at:1};
        save_journal(&directory, &mut journal, "completed").unwrap();
        let result = reconcile_update(&state, &target).unwrap();
        assert_eq!(result["status"], "completed");
        assert!(result.get("exit_required").is_none());
        assert!(!fs::read_dir(&directory).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with("update-helper-")));
    }

    fn signed(bytes: &[u8]) -> (SignedUpdate, String) {
        let key = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let metadata = UpdateMetadata {
            schema_version: 1,
            version: "2.0.0".into(),
            platform: catalogue::platform().into(),
            arch: catalogue::arch().into(),
            min_current_version: "1.0.0".into(),
            url: "https://downloads.example.com/dacs".into(),
            sha256: util::digest(bytes),
            size: bytes.len() as u64,
            artifact: "executable".into(),
        };
        let signature = STANDARD.encode(
            key.sign(&update_signing_bytes(&metadata).unwrap())
                .to_bytes(),
        );
        (
            SignedUpdate {
                metadata,
                signature,
            },
            STANDARD.encode(key.verifying_key().to_bytes()),
        )
    }
    #[test]
    fn tampered_metadata_signature_is_rejected() {
        let (mut manifest, key) = signed(b"executable");
        manifest.metadata.version = "3.0.0".into();
        assert_eq!(
            verify(&manifest, &key, "1.0.0").unwrap_err().code,
            ErrorCode::Forbidden
        );
    }
    #[test]
    fn altered_artifact_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary");
        fs::write(&path, b"tampered!!").unwrap();
        let (manifest, _) = signed(b"executable");
        assert!(validate_file(&path, &manifest.metadata).is_err());
    }
    #[test]
    fn rollback_and_platform_constraints_are_enforced() {
        let (manifest, key) = signed(b"binary");
        assert!(verify(&manifest, &key, "2.0.0").is_ok());
        assert!(verify(&manifest, &key, "3.0.0").is_err());
        assert!(verify(&manifest, &key, "0.9.0").is_err());
    }
    #[test]
    fn interrupted_backup_move_recovers_and_rolls_back() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let directory = update_directory(&state).unwrap();
        let target = absolute_target(&root.path().join("dacs-bin")).unwrap();
        fs::write(&target, b"previous").unwrap();
        let (manifest, key) = signed(b"new binary");
        let staged = StagedUpdate {
            schema_version: 1,
            manifest,
            public_key: key.clone(),
            current_version: "1.0.0".into(),
            staged_at: 1,
        };
        util::atomic_json(&directory.join("trusted-key.json"), &key).unwrap();
        util::atomic_json(&directory.join("staged.json"), &staged).unwrap();
        fs::write(directory.join("artifact.bin"), b"new binary").unwrap();
        let journal = new_journal(&directory, &target, &staged).unwrap();
        fs::rename(&target, &journal.backup).unwrap();
        let result = apply_staged_update(&state, &target).unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(fs::read(&target).unwrap(), b"new binary");
        let result = rollback_update(&state, &target).unwrap();
        assert_eq!(result["status"], "rolled_back");
        assert_eq!(fs::read(&target).unwrap(), b"previous");
    }
    #[test]
    fn corrupted_candidate_is_detected_before_the_target_is_moved() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let directory = update_directory(&state).unwrap();
        let target = absolute_target(&root.path().join("dacs-bin")).unwrap();
        let key = SigningKey::from_bytes(&rand::random::<[u8;32]>());
        fs::write(&target, b"original").unwrap();
        let release = stage_fixture(&directory, &key, b"release-a", "2.0.0", "1.0.0");
        let journal = new_journal(&directory, &target, &release).unwrap();
        fs::write(&journal.candidate, b"corrupted").unwrap();
        let error = apply_staged_update(&state, &target).unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(!error.message.contains("rollback also failed"));
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(!journal.backup.exists());
        let stored: UpdateJournal = util::read_json(&directory.join("apply-journal.json")).unwrap();
        assert_eq!(stored.phase, "rolled_back");
    }

    #[test]
    fn failed_rollback_is_reported_with_the_original_failure() {
        let combined = with_rollback_outcome(
            util::conflict("swap failed"),
            Err(util::conflict("Rollback backup missing or changed; manual recovery required")),
        );
        assert_eq!(combined.code, ErrorCode::Conflict);
        assert!(combined.message.starts_with("swap failed"));
        assert!(combined.message.contains("Rollback backup missing or changed"));
        assert_eq!(with_rollback_outcome(util::conflict("swap failed"), Ok(Value::Null)).message, "swap failed");
    }
}
