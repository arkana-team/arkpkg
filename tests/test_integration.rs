use arkpkg::archive::create_ark;
use arkpkg::database::Database;
use arkpkg::errors::ArkError;
use arkpkg::installer::Installer;
use arkpkg::logger::Logger;
use arkpkg::prompt::PromptHandler;
use arkpkg::remover::Remover;
use arkpkg::verifier::Verifier;
use arkpkg::version::Version;

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_full_package_lifecycle() {
    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    // 1. Create a dummy package source directory
    let metadata_content = r#"
Name: testpkg
Description: Integration test package
Version: 1.2.3
Arch: x86_64
Provides:
testpkg=/usr/bin/testpkg
"#;
    fs::write(pkg_source_dir.path().join("ARKPKG"), metadata_content).unwrap();

    let bin_dir = pkg_source_dir.path().join("package/usr/bin");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(bin_dir.join("testpkg"), b"#!/bin/sh\necho test").unwrap();

    let ark_file = output_dir.path().join("testpkg-1.2.3-x86_64.ark");

    // 2. Build .ark package
    create_ark(pkg_source_dir.path(), &ark_file).unwrap();
    assert!(ark_file.exists());

    // 3. Initialize Database & Logger in fake-root
    let db = Database::new(fake_root_dir.path());
    db.init().unwrap();

    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();

    // 4. Install package into fake-root
    let prompt = PromptHandler::new(true, false, false); // auto-yes
    let mut installer = Installer::new(&db, &logger, prompt);

    installer.install(&ark_file).unwrap();

    // Verify files copied to fake-root
    let installed_bin = fake_root_dir.path().join("usr/bin/testpkg");
    assert!(installed_bin.exists());

    // Verify package.db
    let is_inst = db.is_installed("testpkg").unwrap();
    assert_eq!(is_inst, Some(Version::parse("1.2.3").unwrap()));

    // 5. Verify package
    let verifier = Verifier::new(&db);
    let verify_res = verifier.verify("testpkg");
    assert!(verify_res.is_ok());

    // 6. Remove package
    let prompt_rem = PromptHandler::new(true, false, false);
    let mut remover = Remover::new(&db, &logger, prompt_rem);
    remover.remove("testpkg").unwrap();

    // Verify binary removed
    assert!(!installed_bin.exists());

    // Verify package.db updated
    assert_eq!(db.is_installed("testpkg").unwrap(), None);
}

#[test]
fn installer_rejects_packages_for_a_different_architecture() {
    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let metadata_content = format!(
        "Name: wrongarch\nDescription: wrong architecture test\nVersion: 1.0.0\nArch: other_{}\n",
        std::env::consts::ARCH
    );
    fs::write(pkg_source_dir.path().join("ARKPKG"), metadata_content).unwrap();
    let payload = pkg_source_dir.path().join("package/usr/bin/wrongarch");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"should not install").unwrap();
    let archive = output_dir.path().join("wrongarch.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();

    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    assert!(matches!(
        installer.install(&archive),
        Err(ArkError::ArchitectureMismatch { .. })
    ));
    assert!(!fake_root_dir.path().join("usr/bin/wrongarch").exists());
}

#[cfg(unix)]
#[test]
fn installer_does_not_create_parent_directories_through_out_of_root_symlinks() {
    use std::os::unix::fs::symlink;

    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let outside_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    fs::write(
        pkg_source_dir.path().join("ARKPKG"),
        "Name: pathguard\nDescription: reject out-of-root parent symlinks\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    let payload = pkg_source_dir
        .path()
        .join("package/usr/arkpkg-escape/deep/payload");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"must stay inside the install root").unwrap();
    let archive = output_dir.path().join("pathguard.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();

    symlink(outside_root_dir.path(), fake_root_dir.path().join("usr")).unwrap();

    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    assert!(matches!(
        installer.install(&archive),
        Err(ArkError::InvalidPackage(_))
    ));
    assert!(!outside_root_dir.path().join("arkpkg-escape").exists());
}

#[test]
fn failed_install_restores_preexisting_files() {
    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    let metadata_content =
        "Name: rollbacktest\nDescription: install rollback test\nVersion: 1.0.0\nArch: any\n";
    fs::write(pkg_source_dir.path().join("ARKPKG"), metadata_content).unwrap();
    let payload_dir = pkg_source_dir.path().join("package/usr/bin");
    fs::create_dir_all(&payload_dir).unwrap();
    fs::write(payload_dir.join("a-first"), b"new content").unwrap();
    fs::write(payload_dir.join("z-last"), b"cannot replace directory").unwrap();
    let archive = output_dir.path().join("rollbacktest.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();

    let install_bin_dir = fake_root_dir.path().join("usr/bin");
    fs::create_dir_all(&install_bin_dir).unwrap();
    fs::write(install_bin_dir.join("a-first"), b"old content").unwrap();
    fs::create_dir(install_bin_dir.join("z-last")).unwrap();

    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    assert!(installer.install(&archive).is_err());
    assert_eq!(
        fs::read(install_bin_dir.join("a-first")).unwrap(),
        b"old content"
    );
    assert!(install_bin_dir.join("z-last").is_dir());
    assert_eq!(db.is_installed("rollbacktest").unwrap(), None);
}

#[test]
fn removing_package_restores_untracked_file_replaced_during_install() {
    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    fs::write(
        pkg_source_dir.path().join("ARKPKG"),
        "Name: replacement\nDescription: restore overwritten base file\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    let payload = pkg_source_dir.path().join("package/usr/bin/tool");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"package version").unwrap();

    let installed = fake_root_dir.path().join("usr/bin/tool");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::write(&installed, b"original base version").unwrap();
    let archive = output_dir.path().join("replacement.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();

    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();
    assert_eq!(fs::read(&installed).unwrap(), b"package version");

    let arkinfo = db.read_arkinfo("replacement").unwrap();
    assert_eq!(arkinfo.preserved_files.len(), 1);
    assert!(Verifier::new(&db).verify("replacement").is_ok());
    let (original, backup) = &arkinfo.preserved_files[0];
    assert_eq!(original, &PathBuf::from("/usr/bin/tool"));
    let backup_path = fake_root_dir.path().join(backup.strip_prefix("/").unwrap());
    assert!(backup_path.exists());

    let mut remover = Remover::new(&db, &logger, PromptHandler::new(true, false, false));
    remover.remove("replacement").unwrap();
    assert_eq!(fs::read(&installed).unwrap(), b"original base version");
    assert!(!backup_path.exists());
    assert_eq!(db.is_installed("replacement").unwrap(), None);
}

#[test]
fn remover_keeps_package_installed_when_preserved_backup_is_missing() {
    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    fs::write(
        pkg_source_dir.path().join("ARKPKG"),
        "Name: backupcheck\nDescription: detect missing preserved original\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    let payload = pkg_source_dir.path().join("package/usr/bin/tool");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"package version").unwrap();

    let installed = fake_root_dir.path().join("usr/bin/tool");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::write(&installed, b"original base version").unwrap();
    let archive = output_dir.path().join("backupcheck.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();

    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();

    let arkinfo = db.read_arkinfo("backupcheck").unwrap();
    let backup = &arkinfo.preserved_files[0].1;
    let backup_path = fake_root_dir.path().join(backup.strip_prefix("/").unwrap());
    fs::remove_file(backup_path).unwrap();
    assert!(Verifier::new(&db).verify("backupcheck").is_err());

    let mut remover = Remover::new(&db, &logger, PromptHandler::new(true, false, false));
    assert!(remover.remove("backupcheck").is_err());
    assert!(db.is_installed("backupcheck").unwrap().is_some());
    assert_eq!(fs::read(&installed).unwrap(), b"package version");
}

#[cfg(unix)]
#[test]
fn installer_preserves_safe_package_symlinks() {
    use std::os::unix::fs::symlink;

    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    fs::write(
        pkg_source_dir.path().join("ARKPKG"),
        "Name: linktest\nDescription: symlink install test\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    let bin_dir = pkg_source_dir.path().join("package/usr/bin");
    let lib_dir = pkg_source_dir.path().join("package/usr/lib");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&lib_dir).unwrap();
    fs::write(lib_dir.join("real-command"), b"payload").unwrap();
    symlink("../lib/real-command", bin_dir.join("command")).unwrap();

    let archive = output_dir.path().join("linktest.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();
    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();

    let installed_link = fake_root_dir.path().join("usr/bin/command");
    assert!(
        fs::symlink_metadata(&installed_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&installed_link).unwrap(),
        PathBuf::from("../lib/real-command")
    );

    let mut remover = Remover::new(&db, &logger, PromptHandler::new(true, false, false));
    remover.remove("linktest").unwrap();
    assert!(fs::symlink_metadata(installed_link).is_err());
}

#[test]
fn remover_warns_before_breaking_installed_dependents() {
    let base_source = TempDir::new().unwrap();
    let dependent_source = TempDir::new().unwrap();
    let fake_root = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();

    fs::write(
        base_source.path().join("ARKPKG"),
        "Name: basepkg\nDescription: dependency target\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    fs::create_dir(base_source.path().join("package")).unwrap();
    fs::write(
        dependent_source.path().join("ARKPKG"),
        "Name: dependent\nDescription: dependent package\nVersion: 1.0.0\nArch: any\nDependencies:\nbasepkg>=1.0.0\n",
    )
    .unwrap();
    fs::create_dir(dependent_source.path().join("package")).unwrap();

    let base_archive = output_dir.path().join("basepkg.ark");
    let dependent_archive = output_dir.path().join("dependent.ark");
    create_ark(base_source.path(), &base_archive).unwrap();
    create_ark(dependent_source.path(), &dependent_archive).unwrap();

    let db = Database::new(fake_root.path());
    let logger = Logger::new(fake_root.path());
    logger.init().unwrap();
    for archive in [&base_archive, &dependent_archive] {
        let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
        installer.install(archive).unwrap();
    }

    let mut cancel_remove = Remover::new(&db, &logger, PromptHandler::new(false, true, false));
    assert!(matches!(
        cancel_remove.remove("basepkg"),
        Err(ArkError::UserCancelled)
    ));
    assert!(db.is_installed("basepkg").unwrap().is_some());

    let mut confirm_remove = Remover::new(&db, &logger, PromptHandler::new(true, false, false));
    confirm_remove.remove("basepkg").unwrap();
    assert_eq!(db.is_installed("basepkg").unwrap(), None);
    assert!(db.is_installed("dependent").unwrap().is_some());
}
