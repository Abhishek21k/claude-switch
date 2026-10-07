use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

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
