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
fn installer_escapes_archive_path_in_logs() {
    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: logguard\nDescription: log test\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();
    fs::create_dir(source.path().join("package")).unwrap();
    let archive = output.path().join("package\n[forged] INFO DONE\r.ark");
    create_ark(source.path(), &archive).unwrap();

    let db = Database::new(root.path());
    let logger = Logger::new(root.path());
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();

    let log = fs::read_to_string(root.path().join("var/log/arkpkg.log")).unwrap();
    assert!(log.contains("package\\n[forged] INFO DONE\\r.ark"));
    assert!(!log.contains("\n[forged]"));
    assert!(!log.contains('\r'));
    assert!(db.is_installed("logguard").unwrap().is_some());
}

#[test]
fn installer_rejects_unsupported_filenames_before_copying_any_files() {
    for filename in [
        "z-last\n[forged] INFO DONE",
        "z-last\rINFO DONE",
        "z-last\ttool",
        "z-last\x1b[2J",
        "z-last=owner",
    ] {
        let source = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let output = TempDir::new().unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: logguard\nDescription: log test\nVersion: 1.0\nArch: any\n",
        )
        .unwrap();
        let payload_dir = source.path().join("package/usr/bin");
        fs::create_dir_all(&payload_dir).unwrap();
        fs::write(payload_dir.join("a-first"), b"valid payload").unwrap();
        fs::write(payload_dir.join(filename), b"invalid payload").unwrap();
        let archive = output.path().join("logguard.ark");
        create_ark(source.path(), &archive).unwrap();

        let db = Database::new(root.path());
        let logger = Logger::new(root.path());
        let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
        assert!(matches!(
            installer.install(&archive),
            Err(ArkError::InvalidPackage(_))
        ));

        let log = fs::read_to_string(root.path().join("var/log/arkpkg.log")).unwrap();
        assert!(!log.contains("COPY "), "logged copy for {filename:?}");
        assert!(!log.contains("[forged]"));
        assert!(!root.path().join("usr").exists());
        assert_eq!(db.is_installed("logguard").unwrap(), None);
    }
}

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
fn verifier_detects_modified_installed_files() {
    let pkg_source_dir = TempDir::new().unwrap();
    let fake_root_dir = TempDir::new().unwrap();
    let output_dir = TempDir::new().unwrap();
    fs::write(
        pkg_source_dir.path().join("ARKPKG"),
        "Name: tampercheck\nDescription: detect modified installed files\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    let payload = pkg_source_dir.path().join("package/usr/bin/tool");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"original package content").unwrap();
    let archive = output_dir.path().join("tampercheck.ark");
    create_ark(pkg_source_dir.path(), &archive).unwrap();

    let db = Database::new(fake_root_dir.path());
    let logger = Logger::new(fake_root_dir.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();

    let installed = fake_root_dir.path().join("usr/bin/tool");
    fs::write(installed, b"modified content").unwrap();
    assert!(matches!(
        Verifier::new(&db).verify("tampercheck"),
        Err(ArkError::VerificationFailed(_))
    ));
}

#[cfg(unix)]
#[test]
fn installer_rejects_trimmed_paths_before_copying_any_payload() {
    use std::os::unix::fs::symlink;

    for suffix in [" ", "\u{00a0}", "\u{2003}"] {
        for link_payload in [false, true] {
            let source = TempDir::new().unwrap();
            let root = TempDir::new().unwrap();
            let output = TempDir::new().unwrap();
            fs::write(
                source.path().join("ARKPKG"),
                "Name: pathattack\nDescription: pathname identity test\nVersion: 1.0\nArch: any\n",
            )
            .unwrap();
            let first = source.path().join("package/aaa-new/first");
            fs::create_dir_all(first.parent().unwrap()).unwrap();
            fs::write(first, b"must not be copied").unwrap();
            let relative = format!("usr/bin/tool{suffix}");
            let payload = source.path().join("package").join(&relative);
            fs::create_dir_all(payload.parent().unwrap()).unwrap();
            if link_payload {
                symlink("tool", payload).unwrap();
            } else {
                fs::write(payload, b"attacker content").unwrap();
            }
            let archive = output.path().join("pathattack.ark");
            create_ark(source.path(), &archive).unwrap();

            let db = Database::new(root.path());
            db.add_file_ownership(&[PathBuf::from("/usr/bin/tool")], "owner")
                .unwrap();
            let ownership = fs::read(root.path().join("etc/arkpkg/file.db")).unwrap();
            fs::create_dir_all(root.path().join("usr/bin")).unwrap();
            fs::write(root.path().join("usr/bin/tool"), b"owned content").unwrap();
            fs::write(root.path().join(&relative), b"untracked original").unwrap();
            let logger = Logger::new(root.path());
            let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, true));

            assert!(matches!(
                installer.install(&archive),
                Err(ArkError::InvalidPackage(_))
            ));
            assert!(!root.path().join("aaa-new").exists());
            assert_eq!(
                fs::read(root.path().join("usr/bin/tool")).unwrap(),
                b"owned content"
            );
            assert_eq!(
                fs::read(root.path().join(&relative)).unwrap(),
                b"untracked original"
            );
            assert_eq!(
                fs::read(root.path().join("etc/arkpkg/file.db")).unwrap(),
                ownership
            );
            assert_eq!(db.is_installed("pathattack").unwrap(), None);
            assert!(
                !root
                    .path()
                    .join("etc/arkpkg/packages/pathattack.arkinfo")
                    .exists()
            );
            assert_eq!(
                fs::read_dir(root.path().join("usr/bin")).unwrap().count(),
                2
            );
        }
    }
}

#[test]
fn package_lifecycle_preserves_representable_pathnames() {
    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: pathcheck\nDescription: pathname round trip\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();
    let relative = "usr/share/space dir/ tool :caf\u{00e9}";
    let payload = source.path().join("package").join(relative);
    let installed = root.path().join(relative);
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::write(payload, b"package content").unwrap();
    fs::write(&installed, b"original content").unwrap();
    let archive = output.path().join("pathcheck.ark");
    create_ark(source.path(), &archive).unwrap();

    let db = Database::new(root.path());
    let logger = Logger::new(root.path());
    Installer::new(&db, &logger, PromptHandler::new(true, false, false))
        .install(&archive)
        .unwrap();
    let path = PathBuf::from("/").join(relative);
    let info = db.read_arkinfo("pathcheck").unwrap();
    assert_eq!(info.files, vec![path.clone()]);
    assert_eq!(info.file_hashes[0].0, path);
    assert_eq!(info.preserved_files[0].0, path);
    assert_eq!(
        db.get_file_owner(&path).unwrap().as_deref(),
        Some("pathcheck")
    );
    Verifier::new(&db).verify("pathcheck").unwrap();
    Remover::new(&db, &logger, PromptHandler::new(true, false, false))
        .remove("pathcheck")
        .unwrap();
    assert_eq!(fs::read(installed).unwrap(), b"original content");
    assert!(db.read_file_ownership().unwrap().is_empty());
    assert_eq!(db.is_installed("pathcheck").unwrap(), None);
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

#[cfg(unix)]
#[test]
fn installer_accepts_parent_symlinks_resolving_inside_the_root() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: parentlinks\nDescription: in-root directory links\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();
    for path in ["bin/relative/tool", "absolute/nested/tool"] {
        let payload = source.path().join("package").join(path);
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(payload, b"package content").unwrap();
    }
    fs::create_dir_all(root.path().join("usr/bin")).unwrap();
    symlink("usr/bin", root.path().join("bin")).unwrap();
    symlink(root.path().join("usr/bin"), root.path().join("absolute")).unwrap();
    let archive = output.path().join("parentlinks.ark");
    create_ark(source.path(), &archive).unwrap();
    let db = Database::new(root.path());
    let logger = Logger::new(root.path());
    logger.init().unwrap();
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();

    for path in ["usr/bin/relative/tool", "usr/bin/nested/tool"] {
        assert_eq!(
            fs::read(root.path().join(path)).unwrap(),
            b"package content"
        );
    }
    Verifier::new(&db).verify("parentlinks").unwrap();
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

#[cfg(unix)]
#[test]
fn installer_checks_ownership_through_directory_aliases() {
    use std::os::unix::fs::symlink;

    // Include ownership records written by older installers through an alias.
    for (payload_path, owned_path) in [("bin/tool", "/usr/bin/tool"), ("usr/bin/tool", "/bin/tool")]
    {
        let source = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let output = TempDir::new().unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: aliasattack\nDescription: alias ownership test\nVersion: 1.0.0\nArch: any\n",
        )
        .unwrap();
        let payload = source.path().join("package").join(payload_path);
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(payload, b"attacker content").unwrap();
        let archive = output.path().join("aliasattack.ark");
        create_ark(source.path(), &archive).unwrap();

        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::write(root.path().join("usr/bin/tool"), b"owned content").unwrap();
        symlink("usr/bin", root.path().join("bin")).unwrap();
        let db = Database::new(root.path());
        db.add_file_ownership(&[PathBuf::from(owned_path)], "owner")
            .unwrap();
        let owners = db.read_file_ownership().unwrap();
        let logger = Logger::new(root.path());
        let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, true));

        assert!(matches!(
            installer.install(&archive),
            Err(ArkError::PackageConflict(_))
        ));
        assert_eq!(
            fs::read(root.path().join("usr/bin/tool")).unwrap(),
            b"owned content"
        );
        assert_eq!(db.read_file_ownership().unwrap(), owners);
        assert_eq!(db.is_installed("aliasattack").unwrap(), None);
    }
}

#[cfg(unix)]
#[test]
fn aliased_install_records_resolved_paths_and_restores_originals() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: aliasinstall\nDescription: alias lifecycle test\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    fs::create_dir_all(source.path().join("package/bin")).unwrap();
    fs::write(source.path().join("package/bin/tool"), b"package content").unwrap();
    symlink("tool", source.path().join("package/bin/link")).unwrap();
    let archive = output.path().join("aliasinstall.ark");
    create_ark(source.path(), &archive).unwrap();

    fs::create_dir_all(root.path().join("usr/bin")).unwrap();
    fs::write(root.path().join("usr/bin/tool"), b"original content").unwrap();
    symlink("usr/bin", root.path().join("bin")).unwrap();
    let db = Database::new(root.path());
    let logger = Logger::new(root.path());
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));
    installer.install(&archive).unwrap();

    let info = db.read_arkinfo("aliasinstall").unwrap();
    assert_eq!(
        info.files,
        vec![
            PathBuf::from("/usr/bin/link"),
            PathBuf::from("/usr/bin/tool")
        ]
    );
    assert_eq!(info.preserved_files.len(), 1);
    assert_eq!(info.preserved_files[0].0, PathBuf::from("/usr/bin/tool"));
    assert_eq!(
        db.get_file_owner("/usr/bin/tool").unwrap().as_deref(),
        Some("aliasinstall")
    );
    assert_eq!(
        fs::read_link(root.path().join("usr/bin/link")).unwrap(),
        PathBuf::from("tool")
    );

    // Later changes to the alias must not redirect verification or removal.
    fs::remove_file(root.path().join("bin")).unwrap();
    symlink("elsewhere", root.path().join("bin")).unwrap();
    assert!(Verifier::new(&db).verify("aliasinstall").is_ok());
    Remover::new(&db, &logger, PromptHandler::new(true, false, false))
        .remove("aliasinstall")
        .unwrap();
    assert_eq!(
        fs::read(root.path().join("usr/bin/tool")).unwrap(),
        b"original content"
    );
    assert!(fs::symlink_metadata(root.path().join("usr/bin/link")).is_err());
    assert!(db.read_file_ownership().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn installer_rejects_package_aliases_that_redirect_later_writes() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: redirect\nDescription: package alias redirection test\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    fs::create_dir_all(source.path().join("package/b-path")).unwrap();
    symlink("protected", source.path().join("package/a-link")).unwrap();
    fs::write(
        source.path().join("package/b-path/tool"),
        b"attacker content",
    )
    .unwrap();
    let archive = output.path().join("redirect.ark");
    create_ark(source.path(), &archive).unwrap();

    fs::create_dir(root.path().join("original")).unwrap();
    fs::create_dir(root.path().join("protected")).unwrap();
    fs::write(root.path().join("protected/tool"), b"owned content").unwrap();
    symlink("original", root.path().join("a-link")).unwrap();
    symlink("a-link", root.path().join("b-path")).unwrap();
    let db = Database::new(root.path());
    db.add_file_ownership(&[PathBuf::from("/protected/tool")], "owner")
        .unwrap();
    let logger = Logger::new(root.path());
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));

    assert!(matches!(
        installer.install(&archive),
        Err(ArkError::InvalidPackage(_))
    ));
    assert_eq!(
        fs::read(root.path().join("protected/tool")).unwrap(),
        b"owned content"
    );
    assert_eq!(
        fs::read_link(root.path().join("a-link")).unwrap(),
        PathBuf::from("original")
    );
    assert!(!root.path().join("original/tool").exists());
    assert_eq!(db.is_installed("redirect").unwrap(), None);
}

#[cfg(unix)]
#[test]
fn installer_rejects_duplicate_destinations_through_aliases() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: duplicatealias\nDescription: duplicate alias test\nVersion: 1.0.0\nArch: any\n",
    )
    .unwrap();
    for path in ["package/bin/tool", "package/usr/bin/tool"] {
        let payload = source.path().join(path);
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(payload, b"package content").unwrap();
    }
    let archive = output.path().join("duplicatealias.ark");
    create_ark(source.path(), &archive).unwrap();
    fs::create_dir_all(root.path().join("usr/bin")).unwrap();
    fs::write(root.path().join("usr/bin/tool"), b"original content").unwrap();
    symlink("usr/bin", root.path().join("bin")).unwrap();
    let db = Database::new(root.path());
    let logger = Logger::new(root.path());
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, false));

    assert!(matches!(
        installer.install(&archive),
        Err(ArkError::InvalidPackage(_))
    ));
    assert_eq!(
        fs::read(root.path().join("usr/bin/tool")).unwrap(),
        b"original content"
    );
    assert_eq!(db.is_installed("duplicatealias").unwrap(), None);
}

#[test]
fn installer_rechecks_consent_and_preserves_files_created_after_preflight() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    for approve_new_file in [false, true] {
        let source = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let output = TempDir::new().unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: lateconflict\nDescription: late overwrite test\nVersion: 1.0.0\nArch: any\n",
        )
        .unwrap();
        fs::create_dir_all(source.path().join("package/usr/bin")).unwrap();
        for name in ["a-first", "z-last"] {
            fs::write(
                source.path().join("package/usr/bin").join(name),
                b"package content",
            )
            .unwrap();
        }
        let archive = output.path().join("lateconflict.ark");
        create_ark(source.path(), &archive).unwrap();
        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::write(root.path().join("usr/bin/a-first"), b"first original").unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_arkpkg"))
            .arg("--root")
            .arg(root.path())
            .arg("install")
            .arg(&archive)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut seen = Vec::new();
        let mut byte = [0];
        while !seen.ends_with(b"Selection [Y/N/A/S]: ") {
            assert_eq!(
                stdout.read(&mut byte).unwrap(),
                1,
                "installer exited before prompting"
            );
            seen.push(byte[0]);
        }
        fs::write(root.path().join("usr/bin/z-last"), b"late original").unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(if approve_new_file {
                b"Y\nY\n"
            } else {
                b"Y\nN\n"
            })
            .unwrap();
        drop(stdin);
        child.stdout = Some(stdout);
        let result = child.wait_with_output().unwrap();
        let db = Database::new(root.path());
        if approve_new_file {
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                db.read_arkinfo("lateconflict")
                    .unwrap()
                    .preserved_files
                    .len(),
                2
            );
            let logger = Logger::new(root.path());
            Remover::new(&db, &logger, PromptHandler::new(true, false, false))
                .remove("lateconflict")
                .unwrap();
        } else {
            assert_eq!(result.status.code(), Some(10));
            assert_eq!(db.is_installed("lateconflict").unwrap(), None);
            assert!(db.read_file_ownership().unwrap().is_empty());
        }
        assert_eq!(
            fs::read(root.path().join("usr/bin/a-first")).unwrap(),
            b"first original"
        );
        assert_eq!(
            fs::read(root.path().join("usr/bin/z-last")).unwrap(),
            b"late original"
        );
    }
}

#[cfg(unix)]
#[test]
fn installer_rejects_database_state_payloads_even_with_force() {
    use std::os::unix::fs::symlink;

    for relative in [
        "etc",
        "etc/arkpkg",
        "etc/arkpkg/package.db",
        "etc/arkpkg/file.db",
        "etc/arkpkg/packages/other.arkinfo",
        "state-alias/package.db",
        "state-alias/file.db",
        "state-alias/packages/other.arkinfo",
    ] {
        for link_payload in [false, true] {
            let source = TempDir::new().unwrap();
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let output = TempDir::new().unwrap();
            let target = outside.path().join("must-not-be-created");
            fs::write(
                source.path().join("ARKPKG"),
                "Name: stateattack\nDescription: reserved state test\nVersion: 1.0\nArch: any\n",
            )
            .unwrap();
            let payload = source.path().join("package").join(relative);
            fs::create_dir_all(payload.parent().unwrap()).unwrap();
            if link_payload {
                symlink(&target, &payload).unwrap();
            } else {
                fs::write(&payload, b"replacement").unwrap();
            }
            let archive = output.path().join("attack.ark");
            create_ark(source.path(), &archive).unwrap();

            let db = Database::new(root.path());
            db.init().unwrap();
            db.add_installed_package("existing", &Version::parse("1.0").unwrap())
                .unwrap();
            db.add_file_ownership(&[PathBuf::from("/usr/bin/existing")], "existing")
                .unwrap();
            let db_dir = root.path().join("etc/arkpkg");
            let info_path = db_dir.join("packages/other.arkinfo");
            fs::write(&info_path, b"original metadata").unwrap();
            let package_before = fs::read(db_dir.join("package.db")).unwrap();
            let file_before = fs::read(db_dir.join("file.db")).unwrap();
            symlink("etc/arkpkg", root.path().join("state-alias")).unwrap();
            let logger = Logger::new(root.path());
            let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, true));

            assert!(
                matches!(
                    installer.install(&archive),
                    Err(ArkError::InvalidPackage(_))
                ),
                "accepted {relative} (symlink: {link_payload})"
            );
            assert!(!target.exists());
            assert_eq!(fs::read(db_dir.join("package.db")).unwrap(), package_before);
            assert_eq!(fs::read(db_dir.join("file.db")).unwrap(), file_before);
            assert_eq!(fs::read(&info_path).unwrap(), b"original metadata");
            assert!(!db_dir.join("packages/stateattack.arkinfo").exists());
        }
    }
}
#[cfg(unix)]
#[test]
fn installer_rejects_database_state_through_etc_config_alias() {
    use std::os::unix::fs::symlink;

    for relative in [
        "config/arkpkg/package.db",
        "config/arkpkg/packages/x.arkinfo",
    ] {
        let source = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let output = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("config/arkpkg/packages")).unwrap();
        symlink("config", root.path().join("etc")).unwrap();
        let payload = source.path().join("package").join(relative);
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(&payload, b"replacement").unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: statealiasattack\nDescription: reject database aliases\nVersion: 1.0\nArch: any\n",
        )
        .unwrap();
        let archive = output.path().join("attack.ark");
        create_ark(source.path(), &archive).unwrap();

        let db = Database::new(root.path());
        db.init().unwrap();
        db.add_installed_package("existing", &Version::parse("1.0").unwrap())
            .unwrap();
        let package_db = root.path().join("config/arkpkg/package.db");
        let package_db_before = fs::read(&package_db).unwrap();
        let arkinfo = root.path().join("config/arkpkg/packages/x.arkinfo");
        fs::write(&arkinfo, b"original metadata").unwrap();

        let logger = Logger::new(root.path());
        let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, true));
        assert!(matches!(
            installer.install(&archive),
            Err(ArkError::InvalidPackage(_))
        ));
        assert_eq!(fs::read(package_db).unwrap(), package_db_before);
        assert_eq!(fs::read(arkinfo).unwrap(), b"original metadata");
        assert!(db.is_installed("existing").unwrap().is_some());
    }
}

#[cfg(unix)]
#[test]
fn verifier_rejects_file_symlink_substitutions() {
    use arkpkg::checksum::compute_sha256_path;
    use arkpkg::verifier::VerifyStatus;
    use std::os::unix::fs::symlink;

    for originally_symlink in [false, true] {
        let source = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let output = TempDir::new().unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: typecheck\nDescription: object type integrity\nVersion: 1.0\nArch: any\n",
        )
        .unwrap();
        let payload = source.path().join("package/usr/bin/tool");
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        if originally_symlink {
            symlink("target", &payload).unwrap();
        } else {
            fs::write(&payload, b"target").unwrap();
        }
        let archive = output.path().join("typecheck.ark");
        create_ark(source.path(), &archive).unwrap();
        let db = Database::new(root.path());
        let logger = Logger::new(root.path());
        logger.init().unwrap();
        Installer::new(&db, &logger, PromptHandler::new(true, false, false))
            .install(&archive)
            .unwrap();
        let installed = root.path().join("usr/bin/tool");
        fs::write(
            root.path().join("usr/bin/target"),
            b"different reader content",
        )
        .unwrap();
        let verifier = Verifier::new(&db);
        assert!(matches!(
            verifier.verify("typecheck").unwrap().as_slice(),
            [VerifyStatus::Ok(_)]
        ));
        // Digest hex remains case-insensitive in typed records as in legacy records.
        let mut info = db.read_arkinfo("typecheck").unwrap();
        for (_, hash) in &mut info.file_hashes {
            let (tag, digest) = hash.split_once('=').unwrap();
            *hash = format!("{tag}={}", digest.to_ascii_uppercase());
        }
        db.write_arkinfo(&info).unwrap();
        assert!(matches!(
            verifier.verify("typecheck").unwrap().as_slice(),
            [VerifyStatus::Ok(_)]
        ));
        let original_hash = compute_sha256_path(&installed).unwrap();
        let original_content = fs::read(&installed).unwrap();
        fs::remove_file(&installed).unwrap();
        if originally_symlink {
            fs::write(&installed, b"target").unwrap();
        } else {
            symlink("target", &installed).unwrap();
        }
        assert_eq!(compute_sha256_path(&installed).unwrap(), original_hash);
        assert_ne!(fs::read(&installed).unwrap(), original_content);
        assert!(
            matches!(
                verifier.verify("typecheck"),
                Err(ArkError::VerificationFailed(_))
            ),
            "accepted substitution (originally_symlink={originally_symlink})"
        );
    }
}

#[cfg(unix)]
#[test]
fn installer_rejects_package_metadata_alias_targets() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("etc/arkpkg")).unwrap();
    fs::create_dir_all(root.path().join("config/pkgmeta")).unwrap();
    symlink(
        "../../config/pkgmeta",
        root.path().join("etc/arkpkg/packages"),
    )
    .unwrap();
    let arkinfo = root.path().join("config/pkgmeta/existing.arkinfo");
    fs::write(&arkinfo, b"original metadata").unwrap();

    let payload = source
        .path()
        .join("package/config/pkgmeta/existing.arkinfo");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, b"replacement metadata").unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: metadataaliasattack\nDescription: reject package metadata aliases\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();
    let archive = output.path().join("attack.ark");
    create_ark(source.path(), &archive).unwrap();

    let db = Database::new(root.path());
    db.init().unwrap();
    let logger = Logger::new(root.path());
    let mut installer = Installer::new(&db, &logger, PromptHandler::new(true, false, true));
    assert!(matches!(
        installer.install(&archive),
        Err(ArkError::InvalidPackage(_))
    ));
    assert_eq!(fs::read(arkinfo).unwrap(), b"original metadata");
}

#[cfg(unix)]
#[test]
fn verifier_does_not_certify_legacy_object_types() {
    use arkpkg::checksum::compute_sha256_bytes;
    use arkpkg::database::ArkInfo;
    use arkpkg::verifier::VerifyStatus;
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let db = Database::new(root.path());
    let installed = root.path().join("tool");
    let base = "Name: legacy\nVersion: 1.0\nFiles:\n/tool\n";
    for hashes in [
        String::new(),
        format!("FileHashes:\n/tool:{}\n", compute_sha256_bytes(b"target")),
    ] {
        db.write_arkinfo(&ArkInfo::parse(&format!("{base}{hashes}")).unwrap())
            .unwrap();
        fs::write(&installed, b"target").unwrap();
        let verifier = Verifier::new(&db);
        let result = verifier.verify("legacy");
        if hashes.is_empty() {
            assert!(matches!(result, Err(ArkError::VerificationFailed(_))));
        } else {
            assert!(matches!(
                result.unwrap().as_slice(),
                [VerifyStatus::Unverified(_)]
            ));
        }
        fs::remove_file(&installed).unwrap();
        symlink("target", &installed).unwrap();
        let result = verifier.verify("legacy");
        if hashes.is_empty() {
            assert!(matches!(result, Err(ArkError::VerificationFailed(_))));
        } else {
            assert!(matches!(
                result.unwrap().as_slice(),
                [VerifyStatus::Unverified(_)]
            ));
        }
        fs::remove_file(&installed).unwrap();
        if !hashes.is_empty() {
            fs::write(&installed, b"changed").unwrap();
            assert!(matches!(
                verifier.verify("legacy"),
                Err(ArkError::VerificationFailed(_))
            ));
            fs::remove_file(&installed).unwrap();
        }
        assert!(matches!(
            verifier.verify("legacy"),
            Err(ArkError::VerificationFailed(_))
        ));
    }
}

#[test]
fn verifier_requires_expected_hashes_for_success() {
    use std::process::Command;

    let fake_root_dir = TempDir::new().unwrap();
    let db = Database::new(fake_root_dir.path());
    db.init().unwrap();
    let installed = fake_root_dir.path().join("usr/bin/legacy");
    let metadata_path = fake_root_dir
        .path()
        .join("etc/arkpkg/packages/legacy.arkinfo");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    let content = b"installed content without a trusted baseline";
    fs::write(&installed, content).unwrap();

    let expected_hash = format!(
        "file-sha256={}",
        arkpkg::checksum::compute_sha256_bytes(content)
    );
    for (hashes, expected_exit) in [
        (String::new(), 5),
        ("FileHashes:\n".to_string(), 5),
        (format!("FileHashes:\n/usr/bin/legacy:{expected_hash}\n"), 0),
    ] {
        let metadata = format!("Name: legacy\nVersion: 1.0.0\nFiles:\n/usr/bin/legacy\n{hashes}");
        fs::write(&metadata_path, &metadata).unwrap();

        let result = Verifier::new(&db).verify("legacy");
        if expected_exit == 0 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(ArkError::VerificationFailed(_))));
        }

        let output = Command::new(env!("CARGO_BIN_EXE_arkpkg"))
            .arg("--root")
            .arg(fake_root_dir.path())
            .args(["verify", "legacy"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected_exit));
        let stdout = String::from_utf8(output.stdout).unwrap();
        if expected_exit != 0 {
            assert!(stdout.contains("UNVERIFIED /usr/bin/legacy (legacy metadata)"));
        } else {
            assert!(stdout.contains("OK      /usr/bin/legacy"));
        }
        assert_eq!(fs::read(&installed).unwrap(), content);
        assert_eq!(fs::read_to_string(&metadata_path).unwrap(), metadata);
    }
}
