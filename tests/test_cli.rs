use arkpkg::archive::create_ark;
use arkpkg::database::Database;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn archive_directory() -> TempDir {
    let source = TempDir::new().unwrap();
    let directory = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: planted\nDescription: source selection test\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();
    fs::create_dir_all(source.path().join("package/usr/bin")).unwrap();
    fs::write(source.path().join("package/usr/bin/planted"), b"payload").unwrap();
    create_ark(source.path(), directory.path().join("shadow")).unwrap();
    fs::copy(
        directory.path().join("shadow"),
        directory.path().join("shadow.ark"),
    )
    .unwrap();
    fs::create_dir(directory.path().join("archives")).unwrap();
    fs::copy(
        directory.path().join("shadow"),
        directory.path().join("archives/shadow.ark"),
    )
    .unwrap();
    directory
}

fn install_command(directory: &Path, root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arkpkg"));
    command
        .current_dir(directory)
        .arg("--root")
        .arg(root)
        .args(["--yes", "install"]);
    command
}

fn explicit_paths(directory: &Path) -> Vec<PathBuf> {
    vec![
        "./shadow".into(),
        "./shadow.ark".into(),
        "archives/shadow.ark".into(),
        "archives/../shadow".into(),
        directory.join("shadow"),
    ]
}

#[test]
fn cli_install_bare_names_cannot_be_shadowed_by_local_archives() {
    let directory = archive_directory();
    for name in ["shadow", "shadow.ark"] {
        for repo in [None, Some("trusted")] {
            let root = TempDir::new().unwrap();
            let mut command = install_command(directory.path(), root.path());
            command.arg(name);
            if let Some(repo) = repo {
                command.args(["--repo", repo]);
            }
            let output = command.output().unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(output.status.code(), Some(2), "{name}: {stderr}");
            assert!(
                stderr.contains(if repo.is_some() {
                    "Repository 'trusted' is not configured"
                } else {
                    "No package repositories are configured"
                }),
                "{stderr}"
            );
            assert!(!root.path().join("usr/bin/planted").exists());
            assert!(
                Database::new(root.path())
                    .read_installed_packages()
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[test]
fn cli_install_explicit_local_paths_still_install() {
    let directory = archive_directory();
    for path in explicit_paths(directory.path()) {
        let root = TempDir::new().unwrap();
        let output = install_command(directory.path(), root.path())
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{path:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(root.path().join("usr/bin/planted")).unwrap(),
            b"payload"
        );
        assert!(
            Database::new(root.path())
                .is_installed("planted")
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn cli_install_rejects_local_paths_with_repo() {
    let directory = archive_directory();
    for path in explicit_paths(directory.path()) {
        let root = TempDir::new().unwrap();
        let output = install_command(directory.path(), root.path())
            .arg(&path)
            .args(["--repo", "trusted"])
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(8), "{path:?}: {stderr}");
        assert!(
            stderr.contains("Cannot combine a local archive path with --repo"),
            "{stderr}"
        );
        assert!(!root.path().join("usr/bin/planted").exists());
        assert!(
            Database::new(root.path())
                .read_installed_packages()
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn cli_install_missing_local_archive_does_not_fall_back_to_repositories() {
    let directory = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = install_command(directory.path(), root.path())
        .arg("./missing.ark")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("Package file './missing.ark' not found"),
        "{stderr}"
    );
    assert!(
        Database::new(root.path())
            .read_installed_packages()
            .unwrap()
            .is_empty()
    );
}
