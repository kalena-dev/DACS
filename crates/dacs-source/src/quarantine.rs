use crate::{git::Git, storage::{self, error, io, MAX_REPOSITORY, MAX_TEMPORARY}};
use dacs_protocol::{ErrorCode, Result};
use std::{ffi::OsString, fs, io::Read, path::{Path, PathBuf}};

// Reserve transient ref lock files and the possible initial target ref as well
// as all incoming bytes. Quota rejection occurs before any live object changes.
const REF_WRITE_RESERVE: u64 = 4096;

pub(crate) fn import(
    live: &Git,
    bundle: &Path,
    advertised: &[(String, String, String)],
    import_id: &str,
    bundle_size: u64,
    seed_sha: Option<&str>,
) -> Result<()> {
    let directory = tempfile::Builder::new().prefix("import-").tempdir_in(&live.temporary).map_err(io)?;
    let limit = MAX_TEMPORARY.checked_sub(bundle_size + REF_WRITE_RESERVE)
        .ok_or_else(|| error(ErrorCode::Invalid, "Import exceeds temporary storage budget"))?;
    let quarantine = Git::quarantine(directory.path().to_path_buf(), limit)?;
    quarantine.ensure()?;
    // This path is relative to the quarantine's object directory:
    // project/temporary/import-*/repository.git/objects -> project/repository.git/objects.
    // Only reads use the alternate; all unpacking and refs stay in quarantine.
    fs::write(quarantine.repo.join("objects/info/alternates"), b"../../../../repository.git/objects\n").map_err(io)?;
    let mut arguments = vec![
        "fetch".into(), "--atomic".into(), "--no-tags".into(), "--no-write-fetch-head".into(),
        "--no-recurse-submodules".into(), "--no-auto-maintenance".into(), "--".into(), bundle.as_os_str().to_owned(),
    ];
    for (_, source, destination) in advertised {
        arguments.push(OsString::from(format!("{source}:{destination}")));
    }
    quarantine.run(arguments)?.checked()?;
    for (sha, _, destination) in advertised {
        if quarantine.head(destination)?.as_deref() != Some(sha) {
            return Err(error(ErrorCode::Conflict, "Imported reference does not match the verified bundle"));
        }
    }
    if let Some(sha) = seed_sha { quarantine.commit(sha)?; }
    let (incoming, incoming_entries) = storage::tree_usage(&quarantine.repo, limit)?;
    let (existing, existing_entries) = storage::tree_usage(&live.repo, MAX_REPOSITORY)?;
    if existing.saturating_add(incoming).saturating_add(REF_WRITE_RESERVE) > MAX_REPOSITORY {
        return Err(error(ErrorCode::Invalid, "Import would exceed the coordinator repository size limit"));
    }
    if existing_entries.saturating_add(incoming_entries).saturating_add(8) > 100_000 {
        return Err(error(ErrorCode::Invalid, "Import would exceed the coordinator repository file-count limit"));
    }
    // Account for the complete enclosing temporary tree before promotion too.
    // A killed/over-budget fetch drops this directory, never the live objects.
    live.check_budget()?;
    let files = object_files(&quarantine.repo.join("objects"))?;
    for relative in &files {
        let source = quarantine.repo.join("objects").join(relative);
        let destination = live.repo.join("objects").join(relative);
        storage::directory(destination.parent().expect("object has parent"))?;
        if storage::inspect(&destination)?.is_some() {
            if !same_file_contents(&source, &destination)? {
                return Err(error(ErrorCode::Conflict, "Immutable Git object storage collision"));
            }
            continue;
        }
        // The bounded Git fetch already fsynced object contents, pack metadata
        // and references (core.fsync=committed,reference,pack-metadata).
        // Same-filesystem hard links publish completed immutable files without
        // exposing a partial copy or overwriting existing object content.
        match fs::hard_link(&source, &destination) {
            Ok(()) => {},
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !same_file_contents(&source, &destination)? {
                    return Err(error(ErrorCode::Conflict, "Immutable Git object storage collision"));
                }
            },
            Err(e) => return Err(io(e)),
        }
        storage::sync_directory(destination.parent().expect("object has parent"))?;
    }
    storage::sync_directory(&live.repo.join("objects"))?;
    // No imported ref becomes visible until every required object is complete.
    // On interruption before this rename, only bounded unreachable objects can
    // remain. The project's target and pending publication refs are untouched.
    let destination_parent = live.repo.join("refs/dacs/imports");
    storage::directory(&destination_parent)?;
    let destination = destination_parent.join(import_id);
    if storage::inspect(&destination)?.is_some() {
        return Err(error(ErrorCode::Conflict, "Imported reference namespace collision"));
    }
    fs::rename(quarantine.repo.join("refs/dacs/imports").join(import_id), &destination).map_err(io)?;
    storage::sync_directory(&destination_parent)?;
    storage::sync_directory(&live.repo.join("refs/dacs"))?;
    storage::sync_directory(&live.repo.join("refs"))?;
    // Dropping the quarantine removes its alternates and the extra hard links;
    // the live repository never depends on any temporary directory afterward.
    directory.close().map_err(io)?;
    Ok(())
}

fn object_files(objects: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(objects).map_err(io)? {
        let entry = entry.map_err(io)?;
        let directory = entry.path();
        let metadata = storage::inspect(&directory)?.ok_or_else(|| error(ErrorCode::Io, "Quarantined object directory disappeared"))?;
        if !metadata.is_dir() { return Err(error(ErrorCode::Invalid, "Unexpected quarantined object storage")); }
        let name = entry.file_name().into_string().map_err(|_| error(ErrorCode::Invalid, "Non-UTF-8 object directory"))?;
        if name == "info" { continue; } // In particular, never promote alternates.
        if name != "pack" { storage::validate_hash(&name, 2)?; }
        for file in fs::read_dir(&directory).map_err(io)? {
            let file = file.map_err(io)?;
            let path = file.path();
            let metadata = storage::inspect(&path)?.ok_or_else(|| error(ErrorCode::Io, "Quarantined object disappeared"))?;
            if !metadata.is_file() { return Err(error(ErrorCode::Invalid, "Unexpected quarantined object type")); }
            let filename = file.file_name().into_string().map_err(|_| error(ErrorCode::Invalid, "Non-UTF-8 object filename"))?;
            if name == "pack" {
                let (stem, extension) = filename.rsplit_once('.').ok_or_else(|| error(ErrorCode::Invalid, "Malformed Git pack filename"))?;
                let sha = stem.strip_prefix("pack-").ok_or_else(|| error(ErrorCode::Invalid, "Malformed Git pack filename"))?;
                storage::validate_hash(sha, 40)?;
                if !matches!(extension, "pack" | "idx" | "rev" | "bitmap") {
                    return Err(error(ErrorCode::Invalid, "Unexpected Git pack sidecar"));
                }
            } else { storage::validate_hash(&filename, 38)?; }
            result.push(PathBuf::from(&name).join(filename));
        }
    }
    // An index must never advertise a pack that is not yet completely present.
    result.sort_by_key(|path| match path.extension().and_then(|extension| extension.to_str()) {
        Some("idx") => 2,
        Some("rev" | "bitmap") => 1,
        _ => 0,
    });
    Ok(result)
}

fn same_file_contents(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata = storage::inspect(left)?.ok_or_else(|| error(ErrorCode::Io, "Quarantined object disappeared"))?;
    let right_metadata = storage::inspect(right)?.ok_or_else(|| error(ErrorCode::Io, "Live object disappeared"))?;
    if !left_metadata.is_file() || !right_metadata.is_file() { return Err(error(ErrorCode::Forbidden, "Git object is not a regular file")); }
    if left_metadata.len() != right_metadata.len() { return Ok(false); }
    let mut left = fs::File::open(left).map_err(io)?;
    let mut right = fs::File::open(right).map_err(io)?;
    let mut left_buffer = [0u8; 8192];
    let mut right_buffer = [0u8; 8192];
    let mut remaining = left_metadata.len();
    while remaining > 0 {
        let size = remaining.min(left_buffer.len() as u64) as usize;
        left.read_exact(&mut left_buffer[..size]).map_err(io)?;
        right.read_exact(&mut right_buffer[..size]).map_err(io)?;
        if left_buffer[..size] != right_buffer[..size] { return Ok(false); }
        remaining -= size as u64;
    }
    Ok(true)
}
