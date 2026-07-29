use arkpkg::archive::create_ark;
use arkpkg::database::Database;
use arkpkg::installer::Installer;
use arkpkg::logger::Logger;
use arkpkg::prompt::PromptHandler;
use arkpkg::remover::Remover;
use arkpkg::verifier::Verifier;
use arkpkg::version::Version;

use std::fs;
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
