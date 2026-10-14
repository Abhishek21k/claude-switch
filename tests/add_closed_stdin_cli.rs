#![cfg(unix)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn add_with_closed_stdin_cancels_instead_of_looping() {
    // Known-bad: read_line returns Ok(0) forever and prints an unbounded prompt.
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cswitch"))
        .args(["add", "zz"])
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("add kept prompting after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success());
    assert!(
        stderr.contains("No answer on standard input. Nothing was changed."),
        "{stderr}"
    );
    assert!(!home.path().join(".claude-switch/registry.json").exists());
}
