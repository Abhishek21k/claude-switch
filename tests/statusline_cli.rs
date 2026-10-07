use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cswitch"));
    command
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CSWITCH_USAGE_DIR")
        .env_remove("COLUMNS")
        .env_remove("NO_COLOR")
        .env_remove("TZ");
    command
}

fn run(home: &Path, args: &[&str]) -> std::process::Output {
    command(home).args(args).output().unwrap()
}

fn run_status(
    home: &Path,
    payload: Option<&[u8]>,
    configure: impl FnOnce(&mut Command),
) -> std::process::Output {
    let mut command = command(home);
    command
        .arg("statusline")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);
    let mut child = command.spawn().unwrap();
    if let Some(payload) = payload {
        let result = child.stdin.take().unwrap().write_all(payload);
        assert!(
            result.is_ok() || result.is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
        );
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().unwrap()
}

#[test]
fn built_binary_prints_one_line_on_json_and_closed_stdin() {
    // Known-bad: malformed or absent stdin returns non-zero and blanks Claude Code's row.
    let home = TempDir::new().unwrap();
    for payload in [Some(r#"{"cost":{"total_cost_usd":1.7}}"#), Some("{"), None] {
        let output = run_status(home.path(), payload.map(str::as_bytes), |command| {
            command.env("COLUMNS", "120");
        });
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty());
        let line = String::from_utf8(output.stdout).unwrap();
        assert_eq!(line.lines().count(), 1);
        assert!(line.starts_with("default"), "{line}");
        assert!(line.ends_with('\n'));
    }
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
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
    let output = run_status(
        home.path(),
        Some(br#"{"rate_limits":{"five_hour":{"used_percentage":95}}}"#),
        |command| {
            command.env("NO_COLOR", "1").env("COLUMNS", "120");
        },
    );
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("5h 95%"));
    assert!(!output.stdout.contains(&0x1b));
}

fn register(home: &Path, name: &str) -> std::path::PathBuf {
    let base = home.join(".claude-switch");
    let profile = base.join("profiles").join(name);
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        base.join("registry.json"),
        serde_json::to_vec(&serde_json::json!({"profiles":{name:{
            "name":name,"tool":"claude","email":null,
            "added":"2030-01-01T00:00:00Z","last_used":null
        }}}))
        .unwrap(),
    )
    .unwrap();
    profile
}

#[test]
fn binary_respects_config_and_usage_directories() {
    // Known-bad: the binary ignores CLAUDE_CONFIG_DIR or CSWITCH_USAGE_DIR,
    // or treats an empty config directory as a registered one.
    let home = TempDir::new().unwrap();
    let profile = register(home.path(), "work");
    let work = run_status(home.path(), Some(b"{}"), |command| {
        command.env("CLAUDE_CONFIG_DIR", &profile);
    });
    assert!(work.status.success());
    assert!(String::from_utf8_lossy(&work.stdout).starts_with("work\n"));
    let empty = run_status(home.path(), Some(b"{}"), |command| {
        command.env("CLAUDE_CONFIG_DIR", "");
    });
    assert_eq!(empty.stdout, b"default\n");

    let root = home.path().join("atlas");
    let site = root.join("apps/site/src");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("apps/site/.git")).unwrap();
    fs::create_dir_all(&site).unwrap();
    let usage = home.path().join("custom-usage");
    fs::create_dir_all(&usage).unwrap();
    fs::write(
        usage.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "superproject":root,"workspaces":[{"glob":"apps/*","name":"acme"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({"cwd":site})).unwrap();
    let output = run_status(home.path(), Some(&payload), |command| {
        command.env("CSWITCH_USAGE_DIR", &usage);
    });
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("acme › site"),
        "{output:?}"
    );
    let without = run_status(home.path(), Some(&payload), |_| {});
    assert!(!String::from_utf8_lossy(&without.stdout).contains("acme › site"));
}

#[test]
fn binary_columns_fallback_colour_and_pipe_are_stable() {
    // Known-bads: COLUMNS ignored, zero used literally, or colour limited to a TTY.
    let home = TempDir::new().unwrap();
    let profile = register(home.path(), "work");
    let mut own =
        serde_json::json!({"oauthAccount":{"accountUuid":"00000000-0000-4000-8000-000000000001"}});
    own["cachedUsageUtilization"] = serde_json::json!({"accountUuid":"00000000-0000-4000-8000-000000000001",
        "fetchedAtMs": chrono::Utc::now().timestamp_millis(), "utilization":{"five_hour":{"utilization":60}}});
    fs::write(
        profile.join(".claude.json"),
        serde_json::to_vec(&own).unwrap(),
    )
    .unwrap();
    let spare = home.path().join(".claude-switch/profiles/spare");
    fs::create_dir_all(&spare).unwrap();
    let mut registry: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join(".claude-switch/registry.json")).unwrap(),
    )
    .unwrap();
    registry["profiles"]["spare"] = serde_json::json!({"name":"spare","tool":"claude","email":null,
        "added":"2030-01-01T00:00:00Z","last_used":null});
    fs::write(
        home.path().join(".claude-switch/registry.json"),
        serde_json::to_vec(&registry).unwrap(),
    )
    .unwrap();
    let mut candidate = own;
    candidate["cachedUsageUtilization"]["utilization"]["five_hour"]["utilization"] =
        serde_json::json!(10);
    fs::write(
        spare.join(".claude.json"),
        serde_json::to_vec(&candidate).unwrap(),
    )
    .unwrap();
    let root = home.path().join("atlas");
    let site = root.join("apps/site/src");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("apps/site/.git")).unwrap();
    fs::create_dir_all(&site).unwrap();
    let usage = home.path().join("usage");
    fs::create_dir_all(&usage).unwrap();
    fs::write(
        usage.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "superproject":root,"workspaces":[{"glob":"apps/*","name":"acme"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({
        "cwd":site,
        "rate_limits":{"five_hour":{"used_percentage":88,"resets_at":chrono::Utc::now().timestamp()+4800},
            "seven_day":{"used_percentage":71,"resets_at":chrono::Utc::now().timestamp()+259200}},
        "context_window":{"total_input_tokens":142000},"cost":{"total_cost_usd":1.70}
    })).unwrap();
    let run_at = |columns: Option<&str>, no_color: Option<&str>| {
        run_status(home.path(), Some(&payload), |command| {
            command
                .env("CLAUDE_CONFIG_DIR", &profile)
                .env("CSWITCH_USAGE_DIR", &usage);
            if let Some(columns) = columns {
                command.env("COLUMNS", columns);
            }
            if let Some(no_color) = no_color {
                command.env("NO_COLOR", no_color);
            }
        })
    };
    let wide = run_at(Some("120"), Some("1"));
    let narrow = run_at(Some("50"), Some("1"));
    assert!(String::from_utf8_lossy(&wide.stdout).contains("→ spare 5h 10%"));
    assert_eq!(
        String::from_utf8_lossy(&narrow.stdout),
        "work · site · 5h 88% · 7d 71% · → spare 5h 10%\n"
    );
    let at_80 = run_at(Some("80"), Some("1"));
    for columns in [None, Some("0"), Some("abc")] {
        assert_eq!(run_at(columns, Some("1")).stdout, at_80.stdout);
    }
    assert_ne!(at_80.stdout, wide.stdout);
    for no_color in [None, Some("")] {
        let red = run_status(
            home.path(),
            Some(br#"{"rate_limits":{"five_hour":{"used_percentage":95}}}"#),
            |command| {
                command
                    .env("CLAUDE_CONFIG_DIR", &profile)
                    .env("COLUMNS", "120");
                if let Some(no_color) = no_color {
                    command.env("NO_COLOR", no_color);
                }
            },
        );
        assert!(String::from_utf8_lossy(&red.stdout).contains("\x1b[31m95%\x1b[0m"));
    }
}

#[cfg(unix)]
#[test]
fn binary_uses_tz_for_weekly_reset_weekday() {
    // Known-bad: UTC weekday is used instead of the command's local timezone.
    use chrono::{Datelike, Duration, FixedOffset, TimeZone, Utc};
    let home = TempDir::new().unwrap();
    let day = Utc::now().date_naive() + Duration::days(3);
    let reset = Utc.from_utc_datetime(&day.and_hms_opt(0, 30, 0).unwrap());
    let payload = serde_json::to_vec(&serde_json::json!({
        "rate_limits":{"seven_day":{"used_percentage":71,"resets_at":reset.timestamp()}}
    }))
    .unwrap();
    let mut suffixes = Vec::new();
    for (zone, offset) in [("<-12>12", -12), ("<+14>-14", 14)] {
        let output = run_status(home.path(), Some(&payload), |command| {
            command.env("TZ", zone).env("COLUMNS", "120");
        });
        let expected = reset
            .with_timezone(&FixedOffset::east_opt(offset * 3600).unwrap())
            .weekday()
            .to_string();
        let expected = &expected[..3];
        let line = String::from_utf8(output.stdout).unwrap();
        assert!(line.contains(&format!("↻{expected}")), "{zone}: {line}");
        suffixes.push(expected.to_string());
    }
    assert_ne!(suffixes[0], suffixes[1]);
}
