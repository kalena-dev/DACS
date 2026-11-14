use dacs_protocol::{ApiError, ErrorCode, Result};
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub(crate) const MAX_ARTIFACT: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_BUNDLE: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_REPOSITORY: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_TEMPORARY: u64 = 2 * MAX_BUNDLE;

pub(crate) fn error(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}
pub(crate) fn io(error: std::io::Error) -> ApiError {
    ApiError::new(ErrorCode::Io, error.to_string())
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(error(
            ErrorCode::Invalid,
            "Invalid project/candidate identifier",
        ));
    }
    Ok(())
}

pub(crate) fn validate_hash(value: &str, length: usize) -> Result<()> {
    if value.len() != length
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error(
            ErrorCode::Invalid,
            format!("Expected {length} lowercase hexadecimal characters"),
        ));
    }
    Ok(())
}

fn forbidden_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions and other reparse points can redirect just like symlinks.
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

pub(crate) fn inspect(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if forbidden_link(&metadata) => Err(error(
            ErrorCode::Forbidden,
            "Storage contains a symlink or reparse point",
        )),
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(e)),
    }
}

// The coordinator owns this private tree. These checks reject pre-existing links;
// another process with the same elevated OS identity is not sandboxed by DACS.
pub(crate) fn directory(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(error(
                    ErrorCode::Invalid,
                    "Parent path components are forbidden",
                ));
            }
            Component::CurDir => continue,
            Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue;
            }
            _ => current.push(component.as_os_str()),
        }
        match inspect(&current)? {
            Some(metadata) if !metadata.is_dir() => {
                return Err(error(
                    ErrorCode::Forbidden,
                    "Storage component is not a directory",
                ));
            }
            Some(_) => {}
            None => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(io(e)),
                }
                let metadata = inspect(&current)?
                    .ok_or_else(|| error(ErrorCode::Io, "Storage directory disappeared"))?;
                if !metadata.is_dir() {
                    return Err(error(
                        ErrorCode::Forbidden,
                        "Storage component is not a directory",
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) struct ProjectLock {
    _file: File,
}
impl ProjectLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        if let Some(metadata) = inspect(path)? {
            if !metadata.is_file() {
                return Err(error(ErrorCode::Forbidden, "Invalid project lock file"));
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path).map_err(io)?;
        file.lock_exclusive().map_err(io)?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn read_limited(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata =
        inspect(path)?.ok_or_else(|| error(ErrorCode::NotFound, "Stored object does not exist"))?;
    if !metadata.is_file() {
        return Err(error(
            ErrorCode::Forbidden,
            "Stored object is not a regular file",
        ));
    }
    if metadata.len() > maximum {
        return Err(error(
            ErrorCode::Invalid,
            "Stored object exceeds size limit",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(io)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > maximum {
        return Err(error(
            ErrorCode::Invalid,
            "Stored object exceeds size limit",
        ));
    }
    Ok(bytes)
}

pub(crate) fn write_temporary(dir: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.as_file().sync_all().map_err(io)?;
    Ok(file)
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path).and_then(|f| f.sync_all()).map_err(io)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn tree_size(path: &Path, maximum: u64) -> Result<u64> {
    Ok(tree_usage(path, maximum)?.0)
}

pub(crate) fn tree_usage(path: &Path, maximum: u64) -> Result<(u64, usize)> {
    if inspect(path)?.is_none() {
        return Ok((0, 0));
    }
    let mut pending = vec![path.to_path_buf()];
    let mut total = 0u64;
    let mut entries = 0usize;
    while let Some(path) = pending.pop() {
        let Some(metadata) = inspect(&path)? else {
            continue;
        };
        entries += 1;
        if entries > 100_000 {
            return Err(error(
                ErrorCode::Invalid,
                "Storage file-count limit exceeded",
            ));
        }
        if metadata.is_dir() {
            for child in fs::read_dir(&path).map_err(io)? {
                pending.push(child.map_err(io)?.path());
                if pending.len() > 100_000 {
                    return Err(error(
                        ErrorCode::Invalid,
                        "Storage directory limit exceeded",
                    ));
                }
            }
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
            if total > maximum {
                return Err(error(ErrorCode::Invalid, "Storage byte limit exceeded"));
            }
        } else {
            return Err(error(ErrorCode::Forbidden, "Non-regular object in storage"));
        }
    }
    Ok((total, entries))
}

pub(crate) fn remove_temporary_tree(path: &Path) -> Result<()> {
    // Recovery must be able to remove an over-budget quarantine; applying the
    // live byte quota here would make the rejected import poison later reads.
    // Retain link/type/file-count checks before recursive removal.
    tree_size(path, u64::MAX)?;
    fs::remove_dir_all(path).map_err(io)
}
