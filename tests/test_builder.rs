use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn builder_escapes_rejected_metadata_in_diagnostics() {
    let source = TempDir::new().unwrap();
    let output = source.path().join("output.ark");
    fs::create_dir(source.path().join("package")).unwrap();
    let metadata = "Name: testpkg\nDescription: test\nVersion: 1.0\nArch: any\n";
    let payload = "bad\x1b[2J\x1b[H\rforged\t\x08\x1b]0;title\x07\0\x7f\u{9b}2J\u{2028}\u{2029} café 'quoted' \\path";
    let escaped = r"bad\u{1b}[2J\u{1b}[H\rforged\t\u{8}\u{1b}]0;title\u{7}\u{0}\u{7f}\u{9b}2J\u{2028}\u{2029} café 'quoted' \path";

    for content in [
        metadata.replace("testpkg", payload),
        metadata.replace("1.0", payload),
        metadata.replace("any", payload),
        format!("{metadata}{payload}: value\n"),
        format!("{metadata}{payload}\n"),
        format!("{metadata}Dependencies: {payload}\n"),
        format!("{metadata}Provides: {payload}\n"),
        format!("{metadata}Conflicts: {payload}\n"),
    ] {
        fs::write(source.path().join("ARKPKG"), content).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_arkpkg-build"))
            .arg(source.path())
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();

        assert_eq!(result.status.code(), Some(8));
        assert!(result.stdout.is_empty());
        assert!(!output.exists());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.starts_with("Build failed: Invalid package: "));
        assert!(stderr.ends_with('\n'));
        assert!(stderr.contains(escaped), "{stderr:?}");
        assert!(!stderr[..stderr.len() - 1].chars().any(char::is_control));
    }
}

#[cfg(unix)]
#[test]
fn builder_escapes_paths_in_diagnostics() {
    let source = TempDir::new().unwrap();
    let missing = source.path().join("missing\nforged\r\x1b[2J");
    let result = Command::new(env!("CARGO_BIN_EXE_arkpkg-build"))
        .arg(&missing)
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(8));
    assert!(result.stdout.is_empty());
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        format!(
            "Build failed: Invalid package: Directory '{}/missing\\nforged\\r\\u{{1b}}[2J' does not exist or is not a directory\n",
            source.path().display()
        )
    );
}

#[test]
fn builder_still_builds_valid_packages() {
    let source = TempDir::new().unwrap();
    let output = source.path().join("output.ark");
    fs::create_dir(source.path().join("package")).unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: testpkg\nDescription: café package\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_arkpkg-build"))
        .arg(source.path())
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert!(output.is_file());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!(
            "Building package...\nCompressing...\nWriting {}\nDone.\n",
            output.display()
        )
    );
}
