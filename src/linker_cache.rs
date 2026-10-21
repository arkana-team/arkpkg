use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::logger::Logger;

const CHROOT_CANDIDATES: [&str; 2] = ["/usr/bin/chroot", "/usr/sbin/chroot"];

/// Refresh the dynamic linker cache inside `root` after a transaction changes
/// shared libraries or the target's ld.so.conf files. Never invoke ldconfig
/// against the host root as a side effect of operating on another root.
pub fn refresh_if_needed(root: &Path, files: &[PathBuf], logger: &Logger) {
    if !files.iter().any(|path| needs_refresh(path)) || !root.join("usr/sbin/ldconfig").is_file() {
        return;
    }
    let Some(chroot) = CHROOT_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
    else {
        warn(
            logger,
            "chroot utility is unavailable; target linker cache was not refreshed",
        );
        return;
    };
    refresh_with_chroot(root, logger, chroot);
}

fn refresh_with_chroot(root: &Path, logger: &Logger, chroot: &Path) {
    let root = match fs::canonicalize(root) {
        Ok(root) => root,
        Err(error) => {
            warn(logger, &format!("cannot resolve target root: {error}"));
            return;
        }
    };
    logger.info(&format!(
        "Refreshing dynamic linker cache inside {}",
        root.display()
    ));
    match Command::new(chroot)
        .arg(&root)
        .arg("/usr/sbin/ldconfig")
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            warn(
                logger,
                &format!("target ldconfig failed: {}", detail.trim()),
            );
        }
        Err(error) => warn(logger, &format!("could not run target ldconfig: {error}")),
    }
}

fn needs_refresh(file: &Path) -> bool {
    let file = file.strip_prefix("/").unwrap_or(file);
    if file == Path::new("etc/ld.so.conf")
        || file
            .strip_prefix("etc/ld.so.conf.d")
            .is_ok_and(|suffix| !suffix.as_os_str().is_empty())
    {
        return true;
    }

    let in_library_directory = file.starts_with("lib")
        || file.starts_with("lib64")
        || file.starts_with("usr/lib")
        || file.starts_with("usr/lib64");
    in_library_directory
        && file
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(".so"))
}

fn warn(logger: &Logger, message: &str) {
    let message = format!("WARNING: {message}");
    eprintln!("{message}");
    logger.info(&message);
}

#[cfg(test)]
mod tests {
    use super::{needs_refresh, refresh_with_chroot};
    use crate::logger::Logger;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn refreshes_only_library_and_linker_configuration_paths() {
        assert!(needs_refresh(Path::new(
            "/usr/lib/x86_64-linux-gnu/libsample.so.1"
        )));
        assert!(needs_refresh(Path::new("/etc/ld.so.conf.d/arkpkg.conf")));
        assert!(!needs_refresh(Path::new("/usr/lib/python3.13/site.py")));
        assert!(!needs_refresh(Path::new("/usr/bin/sample")));
    }

    #[test]
    fn runs_ldconfig_with_the_requested_root_as_chroot() {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join("usr/sbin")).unwrap();
        fs::write(target.path().join("usr/sbin/ldconfig"), "target binary").unwrap();

        let tools = TempDir::new().unwrap();
        let args_file = tools.path().join("args.txt");
        let chroot = tools.path().join("chroot");
        fs::write(
            &chroot,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                args_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&chroot, fs::Permissions::from_mode(0o755)).unwrap();

        let logger = Logger::new(target.path());
        logger.init().unwrap();
        let host_cache_before = fs::read("/etc/ld.so.cache").ok();
        refresh_with_chroot(target.path(), &logger, &chroot);

        let args = fs::read_to_string(args_file).unwrap();
        let canonical_root = target.path().canonicalize().unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![canonical_root.to_str().unwrap(), "/usr/sbin/ldconfig"]
        );
        assert_eq!(host_cache_before, fs::read("/etc/ld.so.cache").ok());
    }
}
