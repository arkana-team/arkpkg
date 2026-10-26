#![cfg(unix)]

use arkpkg::database::Database;
use arkpkg::logger::Logger;
use std::fs;
use std::os::unix::fs::symlink;
use tempfile::TempDir;

#[test]
fn logger_creates_directories_and_appends_without_truncating() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("new/root");
    let logger = Logger::new(&root);
    logger.info("first message");
    logger.clone().action("COPY", "/usr/bin/tool");

    let contents = fs::read_to_string(root.join("var/log/arkpkg.log")).unwrap();
    let lines: Vec<_> = contents.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with('['));
    assert!(lines[0].ends_with("] INFO first message"));
    assert!(lines[1].ends_with("] COPY /usr/bin/tool"));
}

#[test]
fn logger_rejects_symlinked_log_directories_without_creating_outside_directories() {
    for component in ["var", "var/log"] {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let link = root.path().join(component);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(outside.path(), &link).unwrap();

        let logger = Logger::new(root.path());
        assert!(logger.init().is_err(), "accepted {component} symlink");
        logger.info("must stay inside root");
        logger.action("COPY", "/usr/bin/tool");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[test]
fn logger_does_not_follow_existing_or_dangling_log_symlinks() {
    for existing in [false, true] {
        for relative in [false, true] {
            let temp = TempDir::new().unwrap();
            let root = temp.path().join("root");
            let outside_file = temp.path().join("outside.log");
            if existing {
                fs::write(&outside_file, b"untouched\n").unwrap();
            }
            let logger = Logger::new(&root);
            logger.init().unwrap();
            let target = if relative {
                std::path::Path::new("../../../outside.log")
            } else {
                &outside_file
            };
            symlink(target, root.join("var/log/arkpkg.log")).unwrap();

            logger.info("must not be appended outside root");
            logger.action("REMOVE", "/usr/bin/tool");
            if existing {
                assert_eq!(fs::read(&outside_file).unwrap(), b"untouched\n");
            } else {
                assert!(!outside_file.exists());
            }
        }
    }
}

#[test]
fn logger_checks_directories_again_after_initialization() {
    for component in ["var", "var/log"] {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let logger = Logger::new(root.path());
        logger.init().unwrap();
        let directory = root.path().join(component);
        fs::remove_dir_all(&directory).unwrap();
        symlink(outside.path(), directory).unwrap();

        logger.info("must stay inside root");
        logger.action("COPY", "/usr/bin/tool");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[test]
fn database_initialization_does_not_create_log_directories_through_symlinks() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    symlink(outside.path(), root.path().join("var")).unwrap();

    assert!(Database::new(root.path()).init().is_err());
    assert!(!outside.path().join("log").exists());
}
