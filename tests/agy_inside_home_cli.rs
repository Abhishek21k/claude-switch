#![cfg(unix)]

use std::fs;
use std::process::Command;

#[test]
fn list_inside_antigravity_home_refuses_without_writing() {
    // Known-bad: list says there are no profiles and creates a switch tree here.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".claude-switch/profiles/g/home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("local-note"), b"local").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cswitch"))
        .arg("list")
        .env("HOME", &home)
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "Error: cswitch is running inside the Antigravity profile 'g'. Run it from a normal shell."
    );
    assert!(!home.join(".claude-switch").exists());
    assert_eq!(fs::read(home.join("local-note")).unwrap(), b"local");
}
