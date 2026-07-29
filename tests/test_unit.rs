use arkpkg::database::{ArkInfo, Database};
use arkpkg::errors::ArkError;
use arkpkg::metadata::{Metadata, ProvideItem};
use arkpkg::version::{DependencyReq, Version, VersionOp};

use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_version_ops_and_edge_cases() {
    let v1 = Version::parse("1.0.0").unwrap();
    let v2 = Version::parse("1.0").unwrap();
    let v3 = Version::parse("2.0-alpha").unwrap();
    let v4 = Version::parse("2.0").unwrap();

    assert_eq!(v1, v2); // 1.0.0 == 1.0
    assert!(v4 > v3); // 2.0 > 2.0-alpha

    let op = VersionOp::LessThanOrEqual;
    assert!(op.eval(&v1, &v4));

    let dep_req = DependencyReq::parse("foo!=1.0.0").unwrap();
    assert!(!dep_req.is_satisfied_by(&v1));
    assert!(dep_req.is_satisfied_by(&v4));
}

#[test]
fn test_metadata_invalid_and_duplicate_fields() {
    let invalid = "Name: foo\nName: bar\nVersion: 1.0\nArch: x86_64\nDescription: desc";
    let res = Metadata::parse(invalid);
    assert!(res.is_err());
    if let Err(ArkError::InvalidPackage(msg)) = res {
        assert!(msg.contains("Duplicate field"));
    } else {
        panic!("Expected InvalidPackage error");
    }
}

#[test]
fn test_provide_item_parsing() {
    let p1 = ProvideItem::parse("bash=/usr/bin/bash").unwrap();
    assert_eq!(p1.name, "bash");
    assert_eq!(p1.path, "/usr/bin/bash");

    let p2 = ProvideItem::parse("sh").unwrap();
    assert_eq!(p2.name, "sh");
    assert_eq!(p2.path, "");
}

#[test]
fn test_arkinfo_parsing() {
    let arkinfo_text = r#"
Name: testpkg
Version: 1.0.0
Arch: x86_64
InstalledAt: 2026-07-29T12:00:00Z
Checksum: sha256hash
Files:
/usr/bin/testpkg
Provides:
testpkg=/usr/bin/testpkg
"#;
    let arkinfo = ArkInfo::parse(arkinfo_text).unwrap();
    assert_eq!(arkinfo.name, "testpkg");
    assert_eq!(arkinfo.files.len(), 1);
    assert_eq!(arkinfo.files[0], PathBuf::from("/usr/bin/testpkg"));
}

#[test]
fn test_atomic_database_operations() {
    let temp = TempDir::new().unwrap();
    let db = Database::new(temp.path());

    let ver = Version::parse("1.0.0").unwrap();
    db.add_installed_package("mypkg", &ver).unwrap();

    let list = db.read_installed_packages().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, "mypkg");

    db.remove_installed_package("mypkg").unwrap();
    let list_after = db.read_installed_packages().unwrap();
    assert_eq!(list_after.len(), 0);
}
