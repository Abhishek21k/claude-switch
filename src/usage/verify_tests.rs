use super::ledger::{Source, Store};
use super::report::verify;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    projects: PathBuf,
    store: Store,
}

impl Fixture {
    fn new(priced_models: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        let store = Store::new(
            temp.path().join("usage"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        fs::create_dir_all(&store.dir).unwrap();
        let card = json!({
            "input":1000.0,"output":1000.0,"cache_write_5m":1000.0,
            "cache_write_1h":1000.0,"cache_read":1000.0
        });
        let models = priced_models
            .iter()
            .map(|model| ((*model).to_string(), json!({"standard":card})))
            .collect::<Map<_, _>>();
        fs::write(
            store.dir.join("rates.json"),
            serde_json::to_vec(&json!({
                "source":"invented", "unit":"USD per million tokens",
                "models":models, "aliases":{}
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            _temp: temp,
            projects,
            store,
        }
    }

    fn session(&self, id: &str, main: &[Value], subagents: &[Value], state: Value) {
        let mut lines = main.iter().map(Value::to_string).collect::<Vec<_>>();
        lines.push(state.to_string());
        fs::write(
            self.projects.join(format!("{id}.jsonl")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
        if !subagents.is_empty() {
            let directory = self.projects.join(id).join("subagents");
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("agent-a1.jsonl"),
                format!(
                    "{}\n",
                    subagents
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            )
            .unwrap();
        }
    }

    fn verify(&self) -> (String, i32) {
        verify(&self.store).unwrap()
    }
}

fn request(
    session: &str,
    id: &str,
    model: &str,
    tokens: [u64; 5],
    sidechain: bool,
    speed: Option<&str>,
) -> Value {
    let mut row = json!({
        "type":"assistant", "timestamp":"2030-01-01T12:00:00Z",
        "sessionId":session, "requestId":format!("{session}-{id}"),
        "message":{
            "id":format!("message-{session}-{id}"), "model":model,
            "stop_reason":if speed.is_some() { Value::String("end_turn".into()) } else { Value::Null },
            "usage":{
                "input_tokens":tokens[0], "output_tokens":tokens[1],
                "cache_creation_input_tokens":tokens[2] + tokens[3],
                "cache_creation":{
                    "ephemeral_5m_input_tokens":tokens[2],
                    "ephemeral_1h_input_tokens":tokens[3]
                },
                "cache_read_input_tokens":tokens[4]
            }
        }
    });
    if sidechain {
        row["isSidechain"] = json!(true);
        row["agentId"] = json!("agent-a1");
    }
    if let Some(speed) = speed {
        row["message"]["usage"]["speed"] = json!(speed);
    }
    row
}

fn state(session: &str, models: &[(&str, [u64; 4], f64)]) -> Value {
    let usage = models
        .iter()
        .map(|(model, tokens, usd)| {
            (
                (*model).to_string(),
                json!({
                    "inputTokens":tokens[0], "outputTokens":tokens[1],
                    "cacheCreationInputTokens":tokens[2], "cacheReadInputTokens":tokens[3],
                    "costUSD":usd
                }),
            )
        })
        .collect::<Map<_, _>>();
    json!({"type":"cost-state", "sessionId":session, "modelUsage":usage})
}

fn worked_example() -> Fixture {
    let fixture = Fixture::new(&["invented-main", "invented-sub"]);
    fixture.session(
        "S",
        &[request(
            "S",
            "M1",
            "invented-main",
            [100, 1000, 0, 0, 0],
            false,
            Some("standard"),
        )],
        &[
            request(
                "S",
                "A1",
                "invented-sub",
                [10, 500, 1000, 0, 2000],
                true,
                Some("standard"),
            ),
            request("S", "A2", "invented-sub", [10, 3, 0, 0, 3000], true, None),
        ],
        state(
            "S",
            &[
                ("invented-main", [100, 1000, 0, 0], 1.1),
                ("invented-sub", [30, 2000, 1500, 9000], 12.53),
            ],
        ),
    );
    fixture
}

#[test]
fn verify_worked_example_names_sub_agent_shortfall_and_prints_exact_totals() {
    // Known-bads: the old tokens-differ line, counting A1 as a start row, or
    // printing totals after the count line.
    let fixture = worked_example();
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 0, "{output}");
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
    assert!(output.starts_with(expected), "{output}");
}

#[test]
fn verify_classifies_all_boundaries_from_all_four_token_sums_and_sidechain() {
    // Known-bads: output-only classification, mixed counts called shortfall,
    // or a no-speed main-loop row counted as a start-of-response row.
    let fixture = Fixture::new(&["invented-cache", "invented-main", "invented-mixed"]);
    fixture.session(
        "S",
        &[
            request(
                "S",
                "C0",
                "invented-cache",
                [10, 100, 0, 0, 10],
                false,
                None,
            ),
            request("S", "M0", "invented-main", [10, 5, 0, 0, 0], false, None),
        ],
        &[
            request("S", "C1", "invented-cache", [5, 5, 0, 0, 20], true, None),
            request("S", "X1", "invented-mixed", [10, 20, 0, 0, 0], true, None),
        ],
        state(
            "S",
            &[
                ("invented-absent", [2000, 0, 0, 0], 2.0),
                ("invented-cache", [15, 105, 0, 50], 0.170),
                ("invented-main", [10, 15, 0, 0], 0.025),
                ("invented-mixed", [5, 25, 0, 0], 0.030),
            ],
        ),
    );
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("sample S invented-cache: sub-agent shortfall: rows $0.1500 vs cost state $0.1700 (1 of 1 sub-agent rows hold a start-of-response usage)"), "{output}");
    assert!(
        output.contains("sample S invented-mixed: tokens differ (resumed/unflushed session)"),
        "{output}"
    );
    assert!(
        output.contains("sample S invented-main: tokens differ (resumed/unflushed session)"),
        "{output}"
    );
    assert!(
        output.contains(
            "sample S invented-absent: no transcript requests (resumed/unflushed session)"
        ),
        "{output}"
    );
    for class in [
        "sub-agent shortfall: pairs 1",
        "main-loop shortfall: pairs 1",
        "rows above or mixed: pairs 1",
        "no transcript requests: pairs 1; cost state $2.0000; rows $0.0000; short $2.0000 (100.0%)",
    ] {
        assert!(output.contains(class), "missing {class}: {output}");
    }
}

#[test]
fn verify_adds_shortfall_pairs_tokens_dollars_and_start_rows() {
    // Known-bads: retaining only the last pair or dividing the short by rows.
    let fixture = Fixture::new(&["invented-sub"]);
    fixture.session(
        "S1",
        &[],
        &[request(
            "S1",
            "A",
            "invented-sub",
            [10, 2, 3, 0, 4],
            true,
            None,
        )],
        state("S1", &[("invented-sub", [20, 5, 6, 9], 0.040)]),
    );
    fixture.session(
        "S2",
        &[],
        &[request(
            "S2",
            "B",
            "invented-sub",
            [30, 4, 0, 0, 6],
            true,
            Some("standard"),
        )],
        state("S2", &[("invented-sub", [40, 8, 2, 10], 0.060)]),
    );
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("sub-agent shortfall: pairs 2; cost state $0.1000; rows $0.0590; short $0.0410 (41.0%); start-of-response rows 1 of 2 sub-agent rows"), "{output}");
    assert!(output.contains("tokens, cost state / rows: input 60 / 40; output 13 / 6; cache write 8 / 3; cache read 19 / 10"), "{output}");
}

#[test]
fn verify_marks_unpriced_shortfall_rows_and_short_as_incomplete() {
    // Known-bad: unpriced rows silently contribute zero without a star.
    let fixture = Fixture::new(&[]);
    fixture.session(
        "S",
        &[],
        &[request(
            "S",
            "A",
            "invented-unpriced",
            [1, 1, 0, 0, 0],
            true,
            None,
        )],
        state("S", &[("invented-unpriced", [2, 2, 0, 0], 1.0)]),
    );
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 2, "{output}");
    assert!(
        output.contains("sub-agent shortfall: rows $0.0000* vs cost state $1.0000"),
        "{output}"
    );
    assert!(output.contains("sub-agent shortfall: pairs 1; cost state $1.0000; rows $0.0000*; short $1.0000* (100.0%); start-of-response rows 1 of 1 sub-agent rows; unpriced rows 1"), "{output}");
}

#[test]
fn verify_excludes_web_tool_session_from_every_total() {
    // Known-bad: excluded session pairs still inflate totals.
    let fixture = Fixture::new(&["invented-sub"]);
    let mut web = request("S", "W", "invented-sub", [1, 1, 0, 0, 0], false, None);
    web["message"]["usage"]["server_tool_use"] = json!({"web_search_requests":1});
    fixture.session(
        "S",
        &[web],
        &[request(
            "S",
            "A",
            "invented-sub",
            [1, 1, 0, 0, 0],
            true,
            None,
        )],
        state("S", &[("invented-sub", [4, 4, 0, 0], 0.008)]),
    );
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 2, "{output}");
    assert!(
        output.contains("sample S: excluded (web tools, fast, or aliased requests)"),
        "{output}"
    );
    assert!(!output.contains("Totals by class"), "{output}");
}

#[test]
fn verify_shortfall_keeps_exit_zero_with_a_rate_check_and_two_without_one() {
    // Known-bad: treating a token shortfall as a failed rate check exits 1.
    let (output, exit) = worked_example().verify();
    assert_eq!(exit, 0, "{output}");
    let fixture = Fixture::new(&["invented-sub"]);
    fixture.session(
        "S",
        &[],
        &[request(
            "S",
            "A",
            "invented-sub",
            [1, 1, 0, 0, 0],
            true,
            None,
        )],
        state("S", &[("invented-sub", [2, 2, 0, 0], 0.004)]),
    );
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 2, "{output}");
    assert!(output.contains("rate checks 0; failed 0."), "{output}");
}

#[test]
fn verify_omits_zero_state_percentage_and_prints_negative_short() {
    // Known-bads: division by zero in a totals percentage, or losing the
    // minus sign when row dollars exceed the cost state's.
    let fixture = Fixture::new(&["invented-zero", "invented-above"]);
    fixture.session(
        "S",
        &[
            request("S", "Z", "invented-zero", [0, 0, 0, 0, 0], false, None),
            request("S", "A", "invented-above", [500, 0, 0, 0, 0], false, None),
        ],
        &[],
        state(
            "S",
            &[
                ("invented-zero", [0, 0, 0, 0], 0.0),
                ("invented-above", [0, 0, 0, 0], 0.0),
            ],
        ),
    );
    let (output, exit) = fixture.verify();
    assert_eq!(exit, 0, "{output}");
    assert!(
        output.contains("equal: pairs 1; cost state $0.0000; rows $0.0000; short $0.0000\n"),
        "{output}"
    );
    assert!(
        output.contains(
            "rows above or mixed: pairs 1; cost state $0.0000; rows $0.5000; short $-0.5000\n"
        ),
        "{output}"
    );
}
