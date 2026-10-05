#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

const OWN_TOKEN: &str = r#"{"id_token":"h.eyJlbWFpbCI6Im9AZXhhbXBsZS5jb20ifQ.s"}"#;
const NO_EMAIL_TOKEN: &str = r#"{"id_token":"h.e30.s"}"#;
const PLANTED_TOKEN: &str =
    r#"{"email":"other@example.com","id_token":"h.eyJlbWFpbCI6Im90aGVyQGV4YW1wbGUuY29tIn0.s"}"#;

fn plant_home(home: &Path) {
    fs::create_dir_all(home.join("folder")).unwrap();
    fs::write(home.join("plain"), b"plain planted bytes").unwrap();
    fs::write(home.join("folder/entry"), b"nested planted bytes").unwrap();
    let gemini = home.join(".gemini/antigravity-cli");
    fs::create_dir_all(&gemini).unwrap();
    fs::write(gemini.join("antigravity-oauth-token"), PLANTED_TOKEN).unwrap();
}

#[test]
fn fake_agy_login_wiring_keeps_real_home_intact() {
    // Known-bad: CLI dispatch or login verification registers an unverified profile,
    // or the farm copies the source token or deletes real-HOME entries on failure.
    let fake_dir = TempDir::new().unwrap();
    let script = fake_dir.path().join("agy");
    fs::write(
        &script,
        r#"#!/bin/sh
printf '%s|%s\n' "$*" "$HOME" >> "$AGY_TEST_LOG"
if [ "$1" = models ]; then
    exit "$AGY_MODELS_EXIT"
fi
mkdir -p "$HOME/.gemini/antigravity-cli"
case "$AGY_TEST_CASE" in
    token) printf '%s' "$AGY_TEST_TOKEN" > "$HOME/.gemini/antigravity-cli/antigravity-oauth-token" ;;
    empty) : > "$HOME/.gemini/antigravity-cli/antigravity-oauth-token" ;;
esac
exit "$AGY_SIGNIN_EXIT"
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();

    for (case, token, signin_exit, models_exit, succeeds, expected_message) in [
        (
            "token",
            OWN_TOKEN,
            "0",
            "0",
            true,
            "Profile 'g' registered (account: o@example.com).",
        ),
        (
            "none",
            OWN_TOKEN,
            "0",
            "0",
            false,
            "Antigravity did not leave a login token.",
        ),
        (
            "empty",
            OWN_TOKEN,
            "0",
            "0",
            false,
            "Antigravity left an empty login token.",
        ),
        (
            "token",
            OWN_TOKEN,
            "0",
            "1",
            false,
            "Antigravity models check failed.",
        ),
        (
            "token",
            NO_EMAIL_TOKEN,
            "0",
            "0",
            true,
            "Antigravity login completed for profile 'g' (email unavailable).",
        ),
        // Known-bad: rejecting sign-in exit 1 even after a good token and models check.
        (
            "token",
            OWN_TOKEN,
            "1",
            "0",
            true,
            "Profile 'g' registered (account: o@example.com).",
        ),
        // Known-bad: exit 1 masks the precise missing-token refusal.
        (
            "none",
            OWN_TOKEN,
            "1",
            "0",
            false,
            "Antigravity did not leave a login token.",
        ),
    ] {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        plant_home(&home);
        let call_log = temp.path().join("agy-calls");
        let args = if succeeds && token == OWN_TOKEN {
            ["add", "g", "--tool", "agy"]
        } else {
            ["login", "g", "--tool", "antigravity"]
        };
        let output = Command::new(env!("CARGO_BIN_EXE_cswitch"))
            .args(args)
            .env("HOME", &home)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", fake_dir.path().display()),
            )
            .env("AGY_TEST_LOG", &call_log)
            .env("AGY_TEST_CASE", case)
            .env("AGY_TEST_TOKEN", token)
            .env("AGY_SIGNIN_EXIT", signin_exit)
            .env("AGY_MODELS_EXIT", models_exit)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.success(), succeeds, "case {case}: {output:?}");
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(message.contains(expected_message), "case {case}: {message}");

        let base = home.join(".claude-switch");
        let profile_dir = base.join("profiles/g");
        let registry_bytes = fs::read(base.join("registry.json")).unwrap_or_default();
        let registry: serde_json::Value = if registry_bytes.is_empty() {
            serde_json::json!({"profiles":{}})
        } else {
            serde_json::from_slice(&registry_bytes).unwrap()
        };
        if succeeds {
            assert!(profile_dir.is_dir());
            assert_eq!(registry["profiles"]["g"]["tool"], "antigravity");
            if token == OWN_TOKEN {
                assert_eq!(registry["profiles"]["g"]["email"], "o@example.com");
            } else {
                assert!(registry["profiles"]["g"]["email"].is_null());
            }
        } else {
            assert!(
                !profile_dir.exists(),
                "case {case} left a profile directory"
            );
            assert!(registry["profiles"].as_object().unwrap().is_empty());
        }
        assert!(!String::from_utf8_lossy(&registry_bytes).contains("other@example.com"));
        assert_eq!(
            fs::read(home.join("plain")).unwrap(),
            b"plain planted bytes"
        );
        assert_eq!(
            fs::read(home.join("folder/entry")).unwrap(),
            b"nested planted bytes"
        );
        assert_eq!(
            fs::read_to_string(home.join(".gemini/antigravity-cli/antigravity-oauth-token"))
                .unwrap(),
            PLANTED_TOKEN
        );
        let calls = fs::read_to_string(&call_log).unwrap();
        let expected_home = profile_dir.join("home");
        for call in calls.lines() {
            assert!(
                call.ends_with(&format!("|{}", expected_home.display())),
                "{call}"
            );
        }
        assert!(calls.lines().next().unwrap().starts_with('|'));
        assert_eq!(calls.lines().count(), if case == "token" { 2 } else { 1 });
        assert_eq!(calls.contains("models|"), case == "token");
    }

    // Known-bad: abort_login(profile_dir, false) leaves a token and farm in an
    // unregistered directory, so a retry cannot proceed.
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    plant_home(&home);
    let profile_dir = home.join(".claude-switch/profiles/g");
    fs::create_dir_all(&profile_dir).unwrap();
    let call_log = temp.path().join("agy-calls");
    let run = |models_exit: &str| {
        Command::new(env!("CARGO_BIN_EXE_cswitch"))
            .args(["login", "g", "--tool", "agy"])
            .env("HOME", &home)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", fake_dir.path().display()),
            )
            .env("AGY_TEST_LOG", &call_log)
            .env("AGY_TEST_CASE", "token")
            .env("AGY_TEST_TOKEN", OWN_TOKEN)
            .env("AGY_SIGNIN_EXIT", "0")
            .env("AGY_MODELS_EXIT", models_exit)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    };
    let refused = run("1");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("models check failed"));
    assert!(profile_dir.is_dir());
    assert_eq!(fs::read_dir(&profile_dir).unwrap().count(), 0);
    assert!(!home.join(".claude-switch/registry.json").exists());
    assert_eq!(
        fs::read(home.join("plain")).unwrap(),
        b"plain planted bytes"
    );
    assert_eq!(
        fs::read(home.join("folder/entry")).unwrap(),
        b"nested planted bytes"
    );
    assert_eq!(
        fs::read_to_string(home.join(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
        PLANTED_TOKEN
    );
    let accepted = run("0");
    assert!(accepted.status.success(), "{accepted:?}");
    let registry: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".claude-switch/registry.json")).unwrap())
            .unwrap();
    assert_eq!(registry["profiles"]["g"]["tool"], "antigravity");
    assert_eq!(registry["profiles"]["g"]["email"], "o@example.com");
    assert!(call_log.exists());
    assert_eq!(fs::read_to_string(&call_log).unwrap().lines().count(), 4);
}
