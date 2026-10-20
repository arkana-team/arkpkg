use std::path::Path;

use tempfile::{NamedTempFile, TempDir};

use crate::errors::Result;

/// Prefer `/var/tmp` so large package payloads do not fill a tmpfs-backed `/tmp`.
pub fn file() -> Result<NamedTempFile> {
    let persistent_temp = Path::new("/var/tmp");
    if persistent_temp.is_dir()
        && let Ok(temporary) = NamedTempFile::new_in(persistent_temp)
    {
        return Ok(temporary);
    }
    Ok(NamedTempFile::new()?)
}

/// Prefer `/var/tmp` for package extraction to limit memory pressure on small systems.
pub fn directory() -> Result<TempDir> {
    let persistent_temp = Path::new("/var/tmp");
    if persistent_temp.is_dir()
        && let Ok(temporary) = TempDir::new_in(persistent_temp)
    {
        return Ok(temporary);
    }
    Ok(TempDir::new()?)
}
