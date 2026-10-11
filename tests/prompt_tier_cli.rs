use chrono::Utc;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn command(home: &Path, usage: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cswitch"));
    command
        .env("HOME", home)
        .env("CSWITCH_USAGE_DIR", usage)
        .env_remove("COLUMNS")
        .env_remove("NO_COLOR")
        .env_remove("TZ")
        .env_remove("CLAUDE_CONFIG_DIR");
    command
}

fn run(home: &Path, usage: &Path, args: &[&str]) -> Output {
    command(home, usage).args(args).output().unwrap()
}

fn rows(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn built_binary_keeps_both_cards_through_refresh_list_and_statusline() {
    // Known-bad: dropping prompt_tier anywhere between transcript and printed money.
    let home = TempDir::new().unwrap();
    let base = home.path().join(".claude-switch");
    let profile = base.join("profiles/work");
    let project = profile.join("projects/demo");
    let usage = home.path().join("usage");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&usage).unwrap();
    fs::write(
        base.join("registry.json"),
        serde_json::to_vec(&json!({"profiles":{"work":{
            "name":"work", "tool":"claude", "email":null,
            "added":"2030-01-01T00:00:00Z", "last_used":null
        }}}))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        profile.join(".claude.json"),
        br#"{"primaryApiKey":"synthetic-key"}"#,
    )
    .unwrap();
    let card = |price: f64| {
        json!({
            "input":price,"output":price,"cache_write_5m":price,
            "cache_write_1h":price,"cache_read":price
        })
    };
    fs::write(
        usage.join("rates.json"),
        serde_json::to_vec(&json!({
            "source":"synthetic", "unit":"USD per million tokens",
            "models":{"invented-model":{
                "standard":card(100.0),
                "long_prompt":{"over_tokens":100_000,"standard":card(500.0)}
            }}, "aliases":{}
        }))
        .unwrap(),
    )
    .unwrap();
    let time = Utc::now().to_rfc3339();
    let line = |id: &str, read: u64| {
        json!({
            "type":"assistant", "timestamp":time, "sessionId":"sample",
            "requestId":id, "message":{"id":format!("message-{id}"),"model":"invented-model","usage":{
                "input_tokens":1_000,"output_tokens":2_000,
                "cache_creation_input_tokens":19_000,
                "cache_creation":{"ephemeral_5m_input_tokens":9_000,"ephemeral_1h_input_tokens":10_000},
                "cache_read_input_tokens":read
            }}
        })
    };
    fs::write(
        project.join("sample.jsonl"),
        format!("{}\n{}\n", line("a", 40_000), line("b", 130_000)),
    )
    .unwrap();

    let refresh = run(home.path(), &usage, &["usage", "refresh"]);
    assert!(refresh.status.success(), "{refresh:?}");
    for file in ["hourly.json", "chats.json"] {
        let saved = rows(&usage.join(file));
        assert_eq!(saved["version"], 1, "{file}");
        let rows = saved["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{file}");
        assert_eq!(
            rows[0]["prompt_tier"],
            json!({"over_tokens":100_000,"long":false})
        );
        assert_eq!(
            rows[1]["prompt_tier"],
            json!({"over_tokens":100_000,"long":true})
        );
    }
    let list = run(home.path(), &usage, &["list"]);
    assert!(list.status.success(), "{list:?}");
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(
        list_text.contains("$82.20") && !list_text.contains("$*"),
        "{list_text}"
    );

    let mut status = command(home.path(), &usage);
    status
        .env("CLAUDE_CONFIG_DIR", &profile)
        .env("COLUMNS", "160")
        .env("NO_COLOR", "1")
        .args(["statusline", "--no-refresh"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = status.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"sample"}"#)
        .unwrap();
    let line = child.wait_with_output().unwrap();
    assert!(line.status.success(), "{line:?}");
    let line_text = String::from_utf8(line.stdout).unwrap();
    assert!(line_text.contains("chat $82.20"), "{line_text}");
    assert!(!line_text.contains("$*"), "{line_text}");

    let path = usage.join("hourly.json");
    let mut stale = rows(&path);
    for row in stale["rows"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("prompt_tier");
    }
    fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
    let stale_list = run(home.path(), &usage, &["list"]);
    assert!(stale_list.status.success(), "{stale_list:?}");
    assert!(
        String::from_utf8_lossy(&stale_list.stdout).contains("$*"),
        "{stale_list:?}"
    );
    let refresh = run(home.path(), &usage, &["usage", "refresh"]);
    assert!(refresh.status.success(), "{refresh:?}");
    let healed = run(home.path(), &usage, &["list"]);
    assert!(healed.status.success(), "{healed:?}");
    let healed_text = String::from_utf8(healed.stdout).unwrap();
    assert!(
        healed_text.contains("$82.20") && !healed_text.contains("$*"),
        "{healed_text}"
    );
}
