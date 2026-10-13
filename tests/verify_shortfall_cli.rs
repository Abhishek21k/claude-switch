use serde_json::json;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn built_binary_preserves_sidechain_from_transcript_through_verify_output() {
    // Known-bad: any path from isSidechain in the transcript to the printed
    // shortfall that loses the flag or its start-of-response count.
    let home = TempDir::new().unwrap();
    let base = home.path().join(".claude-switch");
    let profile = base.join("profiles/sample");
    let project = profile.join("projects/demo");
    let subagents = project.join("S/subagents");
    let usage = base.join("usage");
    fs::create_dir_all(&subagents).unwrap();
    fs::create_dir_all(&usage).unwrap();
    fs::write(
        base.join("registry.json"),
        serde_json::to_vec(&json!({"profiles":{"sample":{
            "name":"sample", "tool":"claude", "email":null,
            "added":"2030-01-01T00:00:00Z", "last_used":null
        }}}))
        .unwrap(),
    )
    .unwrap();
    let card = json!({
        "input":1000.0,"output":1000.0,"cache_write_5m":1000.0,
        "cache_write_1h":1000.0,"cache_read":1000.0
    });
    fs::write(
        usage.join("rates.json"),
        serde_json::to_vec(&json!({
            "source":"invented", "unit":"USD per million tokens",
            "models":{
                "invented-main":{"standard":card},
                "invented-sub":{"standard":card}
            },
            "aliases":{}
        }))
        .unwrap(),
    )
    .unwrap();
    let request = |id: &str,
                   model: &str,
                   input: u64,
                   output: u64,
                   write: u64,
                   read: u64,
                   speed: Option<&str>,
                   sidechain: bool| {
        let mut row = json!({
            "type":"assistant", "timestamp":"2030-01-01T12:00:00Z",
            "sessionId":"S", "requestId":format!("req-{id}"),
            "message":{
                "id":format!("message-{id}"), "model":model,
                "stop_reason":if speed.is_some() { Some("end_turn") } else { None },
                "usage":{
                    "input_tokens":input,"output_tokens":output,
                    "cache_creation_input_tokens":write,
                    "cache_creation":{"ephemeral_5m_input_tokens":write},
                    "cache_read_input_tokens":read
                }
            }
        });
        if let Some(speed) = speed {
            row["message"]["usage"]["speed"] = json!(speed);
        }
        if sidechain {
            row["isSidechain"] = json!(true);
            row["agentId"] = json!("agent-a1");
        }
        row
    };
    let main = request(
        "M1",
        "invented-main",
        100,
        1000,
        0,
        0,
        Some("standard"),
        false,
    );
    let state = json!({
        "type":"cost-state", "sessionId":"S", "modelUsage":{
            "invented-main":{"inputTokens":100,"outputTokens":1000,
                "cacheCreationInputTokens":0,"cacheReadInputTokens":0,"costUSD":1.1},
            "invented-sub":{"inputTokens":30,"outputTokens":2000,
                "cacheCreationInputTokens":1500,"cacheReadInputTokens":9000,"costUSD":12.53}
        }
    });
    fs::write(project.join("S.jsonl"), format!("{main}\n{state}\n")).unwrap();
    let a1 = request(
        "A1",
        "invented-sub",
        10,
        500,
        1000,
        2000,
        Some("standard"),
        true,
    );
    let a2 = request("A2", "invented-sub", 10, 3, 0, 3000, None, true);
    fs::write(subagents.join("agent-a1.jsonl"), format!("{a1}\n{a2}\n")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cswitch"))
        .env("HOME", home.path())
        .env_remove("CSWITCH_USAGE_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .args(["usage", "verify"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let expected = concat!(
        "Cost-state verification (API-equivalent USD):\n",
        "  sample S invented-main: OK $1.1000 vs $1.1000\n",
        "  sample S invented-sub: sub-agent shortfall: rows $6.5230 vs cost state $12.5300 (1 of 2 sub-agent rows hold a start-of-response usage)\n",
        "Totals by class (excluded sessions are left out):\n",
        "  equal: pairs 1; cost state $1.1000; rows $1.1000; short $0.0000 (0.0%)\n",
        "    tokens, cost state / rows: input 100 / 100; output 1000 / 1000; cache write 0 / 0; cache read 0 / 0\n",
        "  sub-agent shortfall: pairs 1; cost state $12.5300; rows $6.5230; short $6.0070 (47.9%); start-of-response rows 1 of 2 sub-agent rows\n",
        "    tokens, cost state / rows: input 30 / 20; output 2000 / 503; cache write 1500 / 1000; cache read 9000 / 5000\n",
        "Cost-state sessions 1; checked sessions 1; rate checks 1; failed 0."
    );
    assert!(text.starts_with(expected), "{text}");
}
