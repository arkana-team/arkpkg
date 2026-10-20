use std::path::Path;

use tempfile::TempDir;

use crate::errors::Result;

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
