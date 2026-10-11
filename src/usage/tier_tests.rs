use super::attribute::{Config, Labels};
use super::billing::{self, ProfileBilling, Rate, RatePrice};
use super::chats;
use super::ledger::{Ledger, Source, Store};
use super::metrics::{self, Bucket};
use super::parse::{self, Request};
use super::rates::{self, PromptTier};
use super::report;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::fs;

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2030-01-01T12:00:00Z")
        .unwrap()
        .to_utc()
}

fn request(id: &str, model: &str, read: u64) -> Request {
    let line = json!({
        "type":"assistant", "timestamp":"2030-01-01T12:00:00Z", "sessionId":"sample",
        "requestId":id, "message":{"id":format!("message-{id}"),"model":model,"usage":{
            "input_tokens":1_000,"output_tokens":2_000,
            "cache_creation_input_tokens":19_000,
            "cache_creation":{"ephemeral_5m_input_tokens":9_000,"ephemeral_1h_input_tokens":10_000},
            "cache_read_input_tokens":read
        }}
    });
    parse::parse(line.to_string().as_bytes(), "work")
        .unwrap()
        .unwrap()
        .requests
        .remove(0)
}

fn ledger(requests: Vec<Request>) -> Ledger {
    Ledger {
        requests,
        ..Ledger::default()
    }
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn hourly_splits_requests_by_their_own_prompt_card() {
    // Known-bads: pricing summed or average prompt tokens, or keeping both cards in one row.
    let tmp = tempfile::tempdir().unwrap();
    let rates = rates::seed();
    metrics::write(
        tmp.path(),
        &ledger(vec![
            request("a", "claude-haiku-5-5", 40_000),
            request("b", "claude-haiku-5-5", 130_000),
        ]),
        &rates,
        at(),
    )
    .unwrap();
    let saved = metrics::read(tmp.path()).unwrap();
    assert_eq!(saved.version, 1);
    assert_eq!(saved.generated_at, at());
    assert_eq!(saved.rows.len(), 2);
    let short = &saved.rows[0];
    let long = &saved.rows[1];
    assert_eq!(
        short.prompt_tier,
        Some(PromptTier {
            over_tokens: 100_000,
            long: false
        })
    );
    assert_eq!(
        long.prompt_tier,
        Some(PromptTier {
            over_tokens: 100_000,
            long: true
        })
    );
    assert_eq!(short.tokens(), [1_000, 2_000, 9_000, 10_000, 40_000]);
    assert_eq!(long.tokens(), [1_000, 2_000, 9_000, 10_000, 130_000]);
    let total = metrics::window(
        saved.rows.iter(),
        at(),
        at() + Duration::hours(1),
        &rates,
        None,
        false,
    );
    close(total.value, 0.03225);
    assert_eq!((total.priced, total.unpriced), (2, 0));

    let three = (0..3)
        .map(|n| request(&format!("c{n}"), "claude-haiku-5-5", 20_000))
        .collect();
    metrics::write(tmp.path(), &ledger(three), &rates, at()).unwrap();
    let saved = metrics::read(tmp.path()).unwrap();
    assert_eq!(saved.rows.len(), 1);
    assert_eq!(saved.rows[0].requests, 3);
    assert!(!saved.rows[0].prompt_tier.unwrap().long);
    let total = metrics::window(
        saved.rows.iter(),
        at(),
        at() + Duration::hours(1),
        &rates,
        None,
        false,
    );
    close(total.value, 0.013275);
}

#[test]
fn chats_splits_requests_by_prompt_card() {
    // Known-bad: per-session grouping drops the card and prices the summed tokens.
    let tmp = tempfile::tempdir().unwrap();
    let rates = rates::seed();
    chats::write(
        tmp.path(),
        &ledger(vec![
            request("a", "claude-haiku-5-5", 40_000),
            request("b", "claude-haiku-5-5", 130_000),
        ]),
        &Config::default(),
        &Labels::default(),
        &rates,
        at(),
    )
    .unwrap();
    let saved = chats::read(tmp.path()).unwrap();
    assert_eq!(saved.version, 1);
    assert_eq!(saved.generated_at, at());
    assert_eq!(saved.rows.len(), 2);
    assert_eq!(saved.rows[0].session, "sample");
    assert!(!saved.rows[0].prompt_tier.unwrap().long);
    assert!(saved.rows[1].prompt_tier.unwrap().long);
    let total = saved
        .rows
        .iter()
        .map(|row| {
            billing::price(
                &row.model,
                row.speed.as_deref(),
                row.prompt_tier,
                row.tokens(),
                &rates,
                None,
                false,
            )
            .0
            .unwrap()
        })
        .sum::<f64>();
    close(total, 0.03225);
}

#[test]
fn stale_or_mismatched_tier_is_unpriced_with_a_partial_star() {
    // Known-bads: defaulting a stale row to the short card or ignoring its threshold.
    let rates = rates::seed();
    let tmp = tempfile::tempdir().unwrap();
    metrics::write(
        tmp.path(),
        &ledger(vec![request("b", "claude-haiku-5-5", 130_000)]),
        &rates,
        at(),
    )
    .unwrap();
    let fresh = metrics::read(tmp.path()).unwrap().rows.remove(0);
    let mut absent = fresh.clone();
    absent.prompt_tier = None;
    let mut mismatched = fresh.clone();
    mismatched.prompt_tier = Some(PromptTier {
        over_tokens: 200_000,
        long: false,
    });
    let opus = Bucket {
        model: "claude-opus-5-5".into(),
        prompt_tier: None,
        input: 1_000_000,
        output: 0,
        cache_write_5m: 0,
        cache_write_1h: 0,
        cache_read: 0,
        ..fresh
    };
    let rows = [absent, mismatched, opus];
    let amount = metrics::window(
        rows.iter(),
        at(),
        at() + Duration::hours(1),
        &rates,
        None,
        false,
    );
    close(amount.value, 4.0);
    assert_eq!((amount.priced, amount.unpriced), (1, 2));
    assert_eq!(amount.list_cell(), "~$4.00*");
}

#[test]
fn one_card_models_keep_old_rollup_shape_and_ignore_tier() {
    // Known-bad: writing prompt_tier for every model or rejecting it for one-card models.
    let rates = rates::seed();
    let tmp = tempfile::tempdir().unwrap();
    let one = ledger(vec![request("one", "claude-opus-5-5", 40_000)]);
    metrics::write(tmp.path(), &one, &rates, at()).unwrap();
    chats::write(
        tmp.path(),
        &one,
        &Config::default(),
        &Labels::default(),
        &rates,
        at(),
    )
    .unwrap();
    for file in ["hourly.json", "chats.json"] {
        assert!(
            !fs::read_to_string(tmp.path().join(file))
                .unwrap()
                .contains("prompt_tier"),
            "{file}"
        );
    }
    let mut row = metrics::read(tmp.path()).unwrap().rows.remove(0);
    row.prompt_tier = Some(PromptTier {
        over_tokens: 1,
        long: true,
    });
    let amount = metrics::window(
        std::iter::once(&row),
        at(),
        at() + Duration::hours(1),
        &rates,
        None,
        false,
    );
    close(
        amount.value,
        rates::cost(&one.requests[0], &rates).unwrap().total(),
    );
    assert_eq!((amount.priced, amount.unpriced), (1, 0));
}

#[test]
fn alias_uses_its_target_threshold_in_request_and_rollup() {
    // Known-bad: looking up the tier only under the transcript model name.
    let mut rates = rates::seed();
    rates
        .aliases
        .insert("acme/tiny".into(), "claude-haiku-5-5".into());
    let b = request("b", "acme/tiny", 130_000);
    close(rates::cost(&b, &rates).unwrap().total(), 0.027625);
    let tmp = tempfile::tempdir().unwrap();
    metrics::write(tmp.path(), &ledger(vec![b]), &rates, at()).unwrap();
    assert_eq!(
        metrics::read(tmp.path()).unwrap().rows[0].prompt_tier,
        Some(PromptTier {
            over_tokens: 100_000,
            long: true
        })
    );
}

#[test]
fn configured_rate_does_not_inherit_the_long_card_multiplier() {
    // Known-bad: multiplying a configured per-type rate by the long card's ratio.
    let rates = rates::seed();
    let tier = Some(PromptTier {
        over_tokens: 100_000,
        long: true,
    });
    let tokens = [1_000, 2_000, 9_000, 10_000, 130_000];
    let mut entry = ProfileBilling {
        rate: Some(Rate {
            model_prefixes: vec!["claude-".into()],
            price: RatePrice::PerType(rates::Price {
                input: 1.0,
                output: 2.0,
                cache_write_5m: 3.0,
                cache_write_1h: 4.0,
                cache_read: 5.0,
            }),
        }),
        ..ProfileBilling::default()
    };
    let (list, spend) = billing::price(
        "claude-haiku-5-5",
        None,
        tier,
        tokens,
        &rates,
        Some(&entry),
        true,
    );
    close(list.unwrap(), 0.027625);
    close(spend.unwrap(), 0.722);
    entry.rate.as_mut().unwrap().model_prefixes = vec!["acme/".into()];
    let (list, spend) = billing::price(
        "claude-haiku-5-5",
        None,
        tier,
        tokens,
        &rates,
        Some(&entry),
        true,
    );
    close(list.unwrap(), 0.027625);
    close(spend.unwrap(), 0.027625);
}

#[test]
fn verify_prices_each_request_on_its_own_card() {
    // Known-bad: a flat short or long card fails the mixed-card cost-state check.
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path().join("profile");
    let projects = profile.join("projects/demo");
    fs::create_dir_all(&projects).unwrap();
    let line = |id: &str, read: u64| {
        json!({
            "type":"assistant", "timestamp":"2030-01-01T12:00:00Z", "sessionId":"sample",
            "requestId":id, "message":{"id":format!("message-{id}"),"model":"claude-haiku-5-5","usage":{
                "input_tokens":1_000,"output_tokens":2_000,
                "cache_creation_input_tokens":19_000,
                "cache_creation":{"ephemeral_5m_input_tokens":9_000,"ephemeral_1h_input_tokens":10_000},
                "cache_read_input_tokens":read
            }}
        })
    };
    let state = json!({"type":"cost-state","sessionId":"sample",
        "modelUsage":{"claude-haiku-5-5":{
            "inputTokens":2_000,"outputTokens":4_000,"cacheCreationInputTokens":38_000,
            "cacheReadInputTokens":170_000,"costUSD":0.03225}},"totalCostUSD":0.03225});
    fs::write(
        projects.join("sample.jsonl"),
        format!("{}\n{}\n{}\n", line("a", 40_000), line("b", 130_000), state),
    )
    .unwrap();
    let store = Store::new(
        tmp.path().join("usage"),
        vec![Source {
            profile: "work".into(),
            directory: profile,
        }],
    );
    let (output, exit) = report::verify(&store).unwrap();
    assert_eq!(exit, 0, "{output}");
    assert!(output.contains("claude-haiku-5-5: OK "), "{output}");
    assert!(output.contains("rate checks 1; failed 0"), "{output}");
}
