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
    fs::create_dir_all(home.join(".gemini/skills/warm")).unwrap();
    fs::write(home.join(".gemini/skills/warm/entry"), b"warm skill").unwrap();
    fs::create_dir_all(home.join(".gemini/config/skills/s")).unwrap();
    fs::write(home.join(".gemini/config/.migrated"), b"").unwrap();
    fs::write(
        home.join(".gemini/config/skills/s/SKILL.md"),
        b"synthetic config skill",
    )
    .unwrap();
    fs::write(home.join(".gemini/config/config.json"), b"private config").unwrap();
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
        if token == NO_EMAIL_TOKEN {
            fs::remove_dir_all(home.join(".gemini/skills")).unwrap();
            fs::remove_dir_all(home.join(".gemini/config")).unwrap();
        }
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
        if token == NO_EMAIL_TOKEN {
            assert!(
                message.contains("Antigravity seed from ~/.gemini: nothing to copy."),
                "{message}"
            );
        } else {
            assert!(
                message.contains(
                    "Antigravity seed from ~/.gemini: copied config/skills, config/.migrated, skills."
                ),
                "{message}"
            );
        }

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
            if token != NO_EMAIL_TOKEN {
                assert_eq!(
                    fs::read(profile_dir.join("home/.gemini/skills/warm/entry")).unwrap(),
                    b"warm skill"
                );
                // Known-bad: the wired login omits .migrated, so agy's next start
                // would migrate and replace the two copied MCP configurations.
                let config = profile_dir.join("home/.gemini/config");
                let marker = fs::symlink_metadata(config.join(".migrated")).unwrap();
                assert!(marker.is_file());
                assert_eq!(marker.len(), 0);
                assert_eq!(
                    fs::read(config.join("skills/s/SKILL.md")).unwrap(),
                    b"synthetic config skill"
                );
                assert!(fs::symlink_metadata(config.join("config.json")).is_err());
            }
            assert_eq!(
                fs::read_to_string(
                    profile_dir.join("home/.gemini/antigravity-cli/antigravity-oauth-token")
                )
                .unwrap(),
                token
            );
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
        if token != NO_EMAIL_TOKEN {
            assert_eq!(
                fs::read(home.join(".gemini/config/.migrated")).unwrap(),
                b""
            );
            assert_eq!(
                fs::read(home.join(".gemini/config/skills/s/SKILL.md")).unwrap(),
                b"synthetic config skill"
            );
            assert_eq!(
                fs::read(home.join(".gemini/config/config.json")).unwrap(),
                b"private config"
            );
        }
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

#[test]
fn registered_agy_login_is_refused_before_fake_runs() {
    // Known-bad: removing the non-empty profile check starts agy over an account.
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let fake = tmp.path().join("agy");
    let calls = tmp.path().join("calls");
    fs::write(
        &fake,
        r##"#!/bin/sh
printf '%s\n' "$*" >> "$AGY_TEST_LOG"
if [ "$1" = models ]; then exit 0; fi
mkdir -p "$HOME/.gemini/antigravity-cli"
printf '%s' "$AGY_TEST_TOKEN" > "$HOME/.gemini/antigravity-cli/antigravity-oauth-token"
"##,
    )
    .unwrap();
    let mut perms = fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake, perms).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_cswitch"))
            .args(["login", "g", "--tool", "agy"])
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", tmp.path().display()))
            .env("AGY_TEST_LOG", &calls)
            .env("AGY_TEST_TOKEN", OWN_TOKEN)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    let registry = home.join(".claude-switch/registry.json");
    let token =
        home.join(".claude-switch/profiles/g/home/.gemini/antigravity-cli/antigravity-oauth-token");
    let before_registry = fs::read(&registry).unwrap();
    let before_token = fs::read(&token).unwrap();
    let before_calls = fs::read(&calls).unwrap();
    let refused = run();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("already exists and holds an account")
    );
    assert_eq!(fs::read(&registry).unwrap(), before_registry);
    assert_eq!(fs::read(&token).unwrap(), before_token);
    assert_eq!(fs::read(&calls).unwrap(), before_calls);
}

#[test]
fn invalid_agy_name_and_flags_never_start_fake() {
    // Known-bad: name or incompatible-flag guard omitted from main or login.
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let fake = tmp.path().join("agy");
    let calls = tmp.path().join("calls");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf called >> \"$AGY_TEST_LOG\"\nexit 0\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake, perms).unwrap();
    for (args, message) in [
        (
            vec!["login", "../g", "--tool", "agy"],
            "Invalid profile name.",
        ),
        (
            vec!["add", "g", "--tool", "agy", "--force"],
            "does not support --force or --include-history",
        ),
        (
            vec!["add", "g", "--tool", "agy", "--include-history"],
            "does not support --force or --include-history",
        ),
        (
            vec!["login", "g", "--tool", "agy", "--console"],
            "does not support --console, --email or --include-history",
        ),
        (
            vec!["login", "g", "--tool", "agy", "--email", "a@example.com"],
            "does not support --console, --email or --include-history",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cswitch"))
            .args(&args)
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", tmp.path().display()))
            .env("AGY_TEST_LOG", &calls)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{args:?}: {output:?}"
        );
        assert!(!calls.exists(), "fake was started for {args:?}");
        assert!(!home.join(".claude-switch/g").exists());
    }
}

#[test]
fn damaged_link_record_warning_reaches_use_and_fake_agy() {
    // Known-bad: prepare_launch drops link_farm_warning before cswitch use starts agy.
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    plant_home(&home);
    let fake = temp.path().join("agy");
    let calls = temp.path().join("calls");
    fs::write(
        &fake,
        r#"#!/bin/sh
printf '%s|%s\n' "$*" "$HOME" >> "$AGY_TEST_LOG"
if [ "$1" = models ]; then exit 0; fi
mkdir -p "$HOME/.gemini/antigravity-cli"
printf '%s' "$AGY_TEST_TOKEN" > "$HOME/.gemini/antigravity-cli/antigravity-oauth-token"
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake, perms).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cswitch"))
            .args(args)
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
            .env("AGY_TEST_LOG", &calls)
            .env("AGY_TEST_TOKEN", OWN_TOKEN)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let login = run(&["login", "g", "--tool", "agy"]);
    assert!(login.status.success(), "{login:?}");
    let profile = home.join(".claude-switch/profiles/g");
    fs::write(profile.join("agy-links.json"), b"not json").unwrap();
    fs::write(home.join("new-real"), b"new synthetic entry").unwrap();
    let use_output = run(&["use", "g"]);
    assert!(use_output.status.success(), "{use_output:?}");
    assert!(
        String::from_utf8_lossy(&use_output.stderr).contains("agy-links.json"),
        "{use_output:?}"
    );
    let rebuilt: Vec<Vec<u8>> =
        serde_json::from_slice(&fs::read(profile.join("agy-links.json")).unwrap()).unwrap();
    assert!(!rebuilt.is_empty());
    let fake_home = profile.join("home");
    assert_eq!(
        fs::read_link(fake_home.join("new-real")).unwrap(),
        home.join("new-real")
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap().lines().last().unwrap(),
        format!("|{}", fake_home.display())
    );
    assert_eq!(
        fs::read(home.join("plain")).unwrap(),
        b"plain planted bytes"
    );
    // Known-bad: a directory at the link-record path reports only OS error 21.
    let record = profile.join("agy-links.json");
    fs::remove_file(&record).unwrap();
    fs::create_dir(&record).unwrap();
    let calls_before = fs::read(&calls).unwrap();
    let refused = run(&["use", "g"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("agy-links.json"),
        "{refused:?}"
    );
    assert_eq!(fs::read(&calls).unwrap(), calls_before);
}

#[test]
fn missing_agy_executable_refuses_before_token_verification() {
    // Known-bad: ignoring the spawn error reports a missing token instead of a missing program.
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    plant_home(&home);
    let output = Command::new(env!("CARGO_BIN_EXE_cswitch"))
        .args(["login", "g", "--tool", "agy"])
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Failed to launch agy"),
        "{output:?}"
    );
    assert!(!home.join(".claude-switch/registry.json").exists());
    assert!(!home.join(".claude-switch/profiles/g").exists());
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
    assert_eq!(
        fs::read(home.join(".gemini/skills/warm/entry")).unwrap(),
        b"warm skill"
    );
}
