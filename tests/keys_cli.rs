use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn run(home: &Path, args: &[&str], input: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cswitch"));
    command
        .args(args)
        .env("HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn collect_files(root: &Path, bytes: &mut Vec<u8>) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "keys" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, bytes);
        } else {
            bytes.extend(fs::read(path).unwrap());
        }
    }
}

#[test]
fn canary_never_reaches_cli_output_registry_or_backups() {
    // Known-bad: success or validation errors echo even part of the API key.
    let home = TempDir::new().unwrap();
    let base = home.path().join(".claude-switch");
    let profile = base.join("profiles/api");
    fs::create_dir_all(&profile).unwrap();
    fs::write(base.join("registry.json"), r#"{"profiles":{"api":{"name":"api","email":"user@example.com","added":"2030-01-01T00:00:00Z","last_used":null}}}"#).unwrap();
    fs::write(
        profile.join(".claude.json"),
        r#"{"primaryApiKey":"synthetic-managed-key"}"#,
    )
    .unwrap();
    fs::write(profile.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();

    let canary = "sk-ant-api03-TESTKEY000";
    let set = run(
        home.path(),
        &["key", "set", "api"],
        Some(&format!("{canary}\n")),
    );
    assert!(set.status.success(), "set failed");
    assert_eq!(
        fs::read_to_string(base.join("keys/api.key")).unwrap(),
        format!("{canary}\n")
    );
    let info = run(home.path(), &["info", "api"], None);
    let list = run(home.path(), &["list"], None);
    assert!(info.status.success());
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&info.stdout).contains("API key (cswitch)"));
    assert!(String::from_utf8_lossy(&list.stdout).contains("api = billed per token"));
    let clear = run(home.path(), &["key", "clear", "api"], None);
    assert!(clear.status.success());
    assert!(!base.join("keys/api.key").exists());
    let invalid = run(
        home.path(),
        &["key", "set", "api"],
        Some(&format!("{canary}!\n")),
    );
    assert!(!invalid.status.success());
    assert!(!base.join("keys/api.key").exists());

    for output in [&set, &info, &list, &clear, &invalid] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(canary));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(canary));
    }
    let mut files = Vec::new();
    collect_files(&base, &mut files);
    assert!(!String::from_utf8_lossy(&files).contains(canary));
}
