use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn run(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cswitch"))
        .args(args)
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CSWITCH_USAGE_DIR")
        .output()
        .unwrap()
}

#[test]
fn built_binary_prints_one_line_on_json_and_closed_stdin() {
    // Known-bad: malformed or absent stdin returns non-zero and blanks Claude Code's row.
    let home = TempDir::new().unwrap();
    for payload in [Some(r#"{"cost":{"total_cost_usd":1.7}}"#), Some("{"), None] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cswitch"))
            .arg("statusline")
            .env("HOME", home.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CSWITCH_USAGE_DIR")
            .env("COLUMNS", "120")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(payload) = payload {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
        } else {
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty());
        let line = String::from_utf8(output.stdout).unwrap();
        assert_eq!(line.lines().count(), 1);
        assert!(line.starts_with("default"), "{line}");
        assert!(line.ends_with('\n'));
    }
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn built_binary_installs_and_uninstalls_only_named_profile() {
    // Known-bad: the CLI writes ~/.claude/settings.json or leaves no backup.
    let home = TempDir::new().unwrap();
    let base = home.path().join(".claude-switch");
    let profile = base.join("profiles/work");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(base.join("registry.json"), r#"{"profiles":{"work":{"name":"work","tool":"claude","email":null,"added":"2030-01-01T00:00:00Z","last_used":null}}}"#).unwrap();
    let settings = profile.join("settings.json");
    let original = br#"{"theme":"dark"}"#;
    std::fs::write(&settings, original).unwrap();
    let output = run(
        home.path(),
        &["statusline", "--install", "work", "--no-refresh"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"work: installed\n");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(value["theme"], "dark");
    let command = value["statusLine"]["command"].as_str().unwrap();
    assert!(command.ends_with(" statusline --no-refresh"), "{command}");
    let backups = base.join("backups/settings/work");
    let saved = std::fs::read_dir(&backups)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(saved.len(), 1);
    assert_eq!(std::fs::read(&saved[0]).unwrap(), original);
    let current = std::fs::read(&settings).unwrap();
    let second = run(
        home.path(),
        &["statusline", "--install", "work", "--no-refresh"],
    );
    assert_eq!(second.stdout, b"work: already installed\n");
    assert_eq!(std::fs::read(&settings).unwrap(), current);
    assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1);
    let removed = run(home.path(), &["statusline", "--uninstall", "work"]);
    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(removed.stdout, b"work: removed\n");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(value, serde_json::json!({"theme":"dark"}));
    assert!(!home.path().join(".claude/settings.json").exists());
}

#[test]
fn no_color_env_removes_limit_escape_sequences() {
    // Known-bad: treating NO_COLOR as a terminal-only setting and emitting escapes into a pipe.
    let home = TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cswitch"))
        .arg("statusline")
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .env("COLUMNS", "120")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"rate_limits":{"five_hour":{"used_percentage":95}}}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("5h 95%"));
    assert!(!output.stdout.contains(&0x1b));
}
