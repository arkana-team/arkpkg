use arkpkg::archive::create_ark;
use std::fs::{self, File};
use std::process::{Command, Output};
use tempfile::TempDir;

const CONTROLS: &str =
    "\x1b[2J\x1b[H\r\nforged\t\u{8}\x1b]0;title\u{7}\u{9b}2J\u{85}\u{2028}\u{2029}";
const ESCAPED: &str =
    r"\u{1b}[2J\u{1b}[H\r\nforged\t\u{8}\u{1b}]0;title\u{7}\u{9b}2J\u{85}\u{2028}\u{2029}";

fn assert_safe_rejection(output: Output, expected: &str) {
    assert_eq!(output.status.code(), Some(8));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "stderr: {stderr:?}");
    assert_eq!(stderr.matches('\n').count(), 1);
    assert!(
        !stderr
            .chars()
            .any(|c| { (c.is_control() && c != '\n') || matches!(c, '\u{2028}' | '\u{2029}') })
    );
    assert!(stderr.contains(expected), "stderr: {stderr:?}");
}

#[test]
fn cli_escapes_rejected_archive_entry() {
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    let archive_path = output.path().join("attack.ark");
    let encoder = lz4_flex::frame::FrameEncoder::new(File::create(&archive_path).unwrap());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(0);
    archive
        .append_data(&mut header, format!("unexpected{CONTROLS}"), &b""[..])
        .unwrap();
    archive.into_inner().unwrap().finish().unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_arkpkg"))
        .arg("--root")
        .arg(root.path())
        .arg("install")
        .arg(&archive_path)
        .output()
        .unwrap();
    assert_safe_rejection(
        result,
        &format!("Unsafe or unexpected archive path 'unexpected{ESCAPED}'"),
    );
}

#[test]
fn cli_escapes_rejected_repository_field_decoded_from_json() {
    let root = TempDir::new().unwrap();
    let config_dir = root.path().join("etc/arkpkg/repos.d");
    fs::create_dir_all(&config_dir).unwrap();
    let config = serde_json::json!({
        "name": format!("main{CONTROLS}"),
        "url": "https://example.com/"
    });
    fs::write(config_dir.join("main.json"), config.to_string()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_arkpkg"))
        .arg("--root")
        .arg(root.path())
        .args(["repo", "list"])
        .output()
        .unwrap();
    assert_safe_rejection(result, &format!("Invalid package name 'main{ESCAPED}'"));
}

#[test]
fn cli_escapes_rejected_payload_filename() {
    let source = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let output = TempDir::new().unwrap();
    fs::write(
        source.path().join("ARKPKG"),
        "Name: guard\nDescription: test\nVersion: 1.0\nArch: any\n",
    )
    .unwrap();
    let payload = source.path().join("package/usr/bin");
    fs::create_dir_all(&payload).unwrap();
    fs::write(payload.join(format!("tool{CONTROLS}")), b"payload").unwrap();
    let archive = output.path().join("attack.ark");
    create_ark(source.path(), &archive).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_arkpkg"))
        .arg("--root")
        .arg(root.path())
        .arg("install")
        .arg(&archive)
        .output()
        .unwrap();
    assert_safe_rejection(result, &format!("/usr/bin/tool{ESCAPED}"));
    assert!(!root.path().join("usr/bin").exists());
}
