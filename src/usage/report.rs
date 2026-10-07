use super::attribute::{Attribution, Config, Labels, Resolver, load_config, load_labels};
use super::ledger::{IngestReport, Ledger, Session, Store};
use super::parse::Request;
use super::rates::{Rates, cost, load_or_seed, read_or_seed};
use crate::atomic;
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub struct Options {
    pub since: String,
    pub profile: Option<String>,
    pub by: Option<String>,
    pub json: bool,
    pub explain: Option<String>,
    pub unattributed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReportRow {
    pub day: String,
    pub profile: String,
    pub workspace: String,
    pub project: String,
    pub session: String,
    pub title: Option<String>,
    pub model: String,
    pub signal: String,
    pub candidates: Vec<String>,
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Default, Clone, Serialize)]
struct Totals {
    requests: usize,
    input: u64,
    output: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_read: u64,
    cost_usd: f64,
    unpriced: bool,
}

impl Totals {
    fn add(&mut self, row: &ReportRow) {
        self.requests += 1;
        self.input += row.input;
        self.output += row.output;
        self.cache_write_5m += row.cache_write_5m;
        self.cache_write_1h += row.cache_write_1h;
        self.cache_read += row.cache_read;
        if let Some(cost) = row.cost_usd {
            self.cost_usd += cost;
        } else {
            self.unpriced = true;
        }
    }

    fn cells(&self) -> String {
        let cost = if self.unpriced {
            "$*".to_string()
        } else {
            format!("${:.2}", self.cost_usd)
        };
        format!(
            "{:>10} {:>10} {:>11} {:>10} {:>11}",
            self.input,
            self.output,
            self.cache_write_5m + self.cache_write_1h,
            self.cache_read,
            cost
        )
    }
}

#[derive(Serialize)]
struct SummaryRow {
    day: String,
    profile: String,
    project: String,
    session: String,
    totals: Totals,
}

#[derive(Serialize)]
struct Footer {
    requests_counted: usize,
    duplicates_collapsed: u64,
    unreadable_lines: u64,
    synthetic_skipped: u64,
    earliest_ingested: Option<String>,
    sidechain_replays: Vec<String>,
    ingest_skipped_lock: bool,
}

fn since_cutoff(value: &str, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    match value {
        "all" => Ok(None),
        "7d" => Ok(Some(now - Duration::days(7))),
        "30d" => Ok(Some(now - Duration::days(30))),
        _ => {
            let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")?;
            Ok(Some(date.and_hms_opt(0, 0, 0).unwrap().and_utc()))
        }
    }
}

fn session_map(ledger: &Ledger) -> HashMap<(String, String), &Session> {
    ledger
        .sessions
        .iter()
        .map(|session| ((session.profile.clone(), session.id.clone()), session))
        .collect()
}

fn display_title(session: Option<&Session>) -> Option<String> {
    let session = session?;
    session
        .titles
        .iter()
        .filter(|title| title.source == "rename")
        .max_by_key(|title| (&title.file, title.offset))
        .or_else(|| {
            session
                .titles
                .iter()
                .filter(|title| title.source == "ai-title")
                .max_by_key(|title| (&title.file, title.offset))
        })
        .map(|title| title.value.clone())
}

fn make_rows(ledger: &Ledger, config: &Config, labels: &Labels, rates: &Rates) -> Vec<ReportRow> {
    let resolver = Resolver::new(&ledger.requests, config, labels);
    let sessions = session_map(ledger);
    ledger
        .requests
        .iter()
        .map(|request| {
            let session = sessions
                .get(&(request.profile.clone(), request.session.clone()))
                .copied();
            let Attribution {
                workspace,
                project,
                signal,
                candidates,
            } = resolver.attribute(request, session);
            ReportRow {
                day: request.time.format("%Y-%m-%d").to_string(),
                profile: request.profile.clone(),
                workspace,
                project,
                session: request.session.clone(),
                title: display_title(session),
                model: request.model.clone(),
                signal: signal.to_string(),
                candidates,
                input: request.input,
                output: request.output,
                cache_write_5m: request.cache_write_5m,
                cache_write_1h: request.cache_write_1h,
                cache_read: request.cache_read,
                cost_usd: cost(request, rates).map(|cost| cost.total()),
            }
        })
        .collect()
}

fn write_summary(store: &Store, rows: &[ReportRow]) -> Result<()> {
    let mut grouped = BTreeMap::<(String, String, String, String), Totals>::new();
    for row in rows {
        grouped
            .entry((
                row.day.clone(),
                row.profile.clone(),
                row.project.clone(),
                row.session.clone(),
            ))
            .or_default()
            .add(row);
    }
    let summary = grouped
        .into_iter()
        .map(|((day, profile, project, session), totals)| SummaryRow {
            day,
            profile,
            project,
            session,
            totals,
        })
        .collect::<Vec<_>>();
    atomic::write(
        &store.dir.join("summary.json"),
        &serde_json::to_vec_pretty(&summary)?,
    )
}

pub(crate) fn refresh_summary(store: &Store, ledger: &Ledger) -> Result<Vec<ReportRow>> {
    let config = load_config(&store.dir)?;
    let labels = load_labels(&store.dir)?;
    let rates = load_or_seed(&store.dir)?;
    let rows = make_rows(ledger, &config, &labels, &rates);
    write_summary(store, &rows)?;
    let now = Utc::now();
    super::metrics::write(&store.dir, ledger, now)?;
    super::chats::write(&store.dir, ledger, &config, &labels, now)?;
    Ok(rows)
}

fn footer(ledger: &Ledger, ingest: &IngestReport, rows: usize) -> Footer {
    Footer {
        requests_counted: rows,
        duplicates_collapsed: ledger.cursors.duplicates,
        unreadable_lines: ledger.cursors.malformed + ingest.partial,
        synthetic_skipped: ledger.cursors.synthetic_skipped,
        earliest_ingested: ledger
            .requests
            .iter()
            .map(|row| row.time)
            .min()
            .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string()),
        sidechain_replays: ledger.cursors.sidechain_replays.clone(),
        ingest_skipped_lock: ingest.skipped_lock,
    }
}

fn footer_text(footer: &Footer) -> String {
    format!(
        "\n{} requests counted · {} duplicates collapsed · {} unreadable lines · {} synthetic skipped · earliest ingested {}{}\n",
        footer.requests_counted,
        footer.duplicates_collapsed,
        footer.unreadable_lines,
        footer.synthetic_skipped,
        footer.earliest_ingested.as_deref().unwrap_or("none"),
        if footer.ingest_skipped_lock {
            " · ingest skipped (locked)"
        } else {
            ""
        }
    )
}

fn table_header() -> &'static str {
    "                                         INPUT     OUTPUT CACHE WRITE CACHE READ    API-EQ $\n"
}

fn tree(rows: &[ReportRow]) -> String {
    let mut grouped =
        BTreeMap::<String, BTreeMap<String, BTreeMap<String, (Option<String>, Totals)>>>::new();
    for row in rows {
        let entry = grouped
            .entry(row.workspace.clone())
            .or_default()
            .entry(row.project.clone())
            .or_default()
            .entry(row.session.clone())
            .or_default();
        if entry.0.is_none() {
            entry.0 = row.title.clone();
        }
        entry.1.add(row);
    }
    let mut out = format!("API-equivalent USD (not a bill)\n{}", table_header());
    for (workspace, projects) in grouped {
        let mut workspace_totals = Totals::default();
        for sessions in projects.values() {
            for (_, totals) in sessions.values() {
                merge(&mut workspace_totals, totals);
            }
        }
        out.push_str(&format!("{workspace:<39} {}\n", workspace_totals.cells()));
        for (project, sessions) in projects {
            let mut project_totals = Totals::default();
            for (_, totals) in sessions.values() {
                merge(&mut project_totals, totals);
            }
            out.push_str(&format!("  {project:<37} {}\n", project_totals.cells()));
            for (session, (title, totals)) in sessions {
                let name = title.unwrap_or(session);
                out.push_str(&format!("    {name:<35} {}\n", totals.cells()));
            }
        }
    }
    out
}

fn merge(target: &mut Totals, other: &Totals) {
    target.requests += other.requests;
    target.input += other.input;
    target.output += other.output;
    target.cache_write_5m += other.cache_write_5m;
    target.cache_write_1h += other.cache_write_1h;
    target.cache_read += other.cache_read;
    target.cost_usd += other.cost_usd;
    target.unpriced |= other.unpriced;
}

fn by(rows: &[ReportRow], grouping: &str) -> Result<String> {
    let mut grouped = BTreeMap::<String, Totals>::new();
    for row in rows {
        let key = match grouping {
            "profile" => row.profile.clone(),
            "workspace" => row.workspace.clone(),
            "project" => row.project.clone(),
            "session" => row
                .title
                .as_ref()
                .map(|title| format!("{title} [{}]", row.session))
                .unwrap_or_else(|| row.session.clone()),
            "model" => row.model.clone(),
            "day" => row.day.clone(),
            _ => bail!("--by must be profile, workspace, project, session, model, or day"),
        };
        grouped.entry(key).or_default().add(row);
    }
    let mut out = format!(
        "API-equivalent USD by {grouping} (not a bill)\n{}",
        table_header()
    );
    for (key, totals) in grouped {
        out.push_str(&format!("{key:<39} {}\n", totals.cells()));
    }
    Ok(out)
}

fn explain(rows: &[ReportRow], session: &str) -> String {
    let mut grouped = BTreeMap::<(String, String, String), Totals>::new();
    let mut candidates = BTreeSet::new();
    for row in rows.iter().filter(|row| row.session == session) {
        grouped
            .entry((row.profile.clone(), row.project.clone(), row.signal.clone()))
            .or_default()
            .add(row);
        candidates.extend(row.candidates.iter().cloned());
    }
    let mut out = format!("Attribution for session {session}:\n");
    if grouped.is_empty() {
        out.push_str("  no matching requests\n");
    }
    for ((profile, project, signal), totals) in grouped {
        out.push_str(&format!(
            "  {profile}: {project} via {signal} ({} requests)\n",
            totals.requests
        ));
    }
    if !candidates.is_empty() {
        out.push_str(&format!(
            "  candidates: {}\n",
            candidates.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    out
}

fn unattributed(rows: &[ReportRow]) -> String {
    let mut grouped = BTreeMap::<(String, String), (usize, Vec<String>)>::new();
    for row in rows.iter().filter(|row| {
        row.project == "(unattributed)" || row.project.ends_with("/(workspace files)")
    }) {
        let entry = grouped
            .entry((row.profile.clone(), row.session.clone()))
            .or_default();
        entry.0 += 1;
        entry.1.extend(row.candidates.clone());
    }
    let mut out = String::from("Unattributed sessions:\n");
    if grouped.is_empty() {
        out.push_str("  none\n");
    }
    for ((profile, session), (count, mut candidates)) in grouped {
        candidates.sort();
        candidates.dedup();
        out.push_str(&format!(
            "  {profile} {session}: {count} requests; candidates: {}\n",
            if candidates.is_empty() {
                "none".to_string()
            } else {
                candidates.join(", ")
            }
        ));
    }
    out
}

pub fn run(store: &Store, options: &Options, now: DateTime<Utc>) -> Result<String> {
    let cutoff = since_cutoff(&options.since, now)?;
    let ingest = store.ingest()?;
    let ledger = store.load()?;
    let config = load_config(&store.dir)?;
    let labels = load_labels(&store.dir)?;
    let rates = if ingest.skipped_lock {
        read_or_seed(&store.dir)?
    } else {
        load_or_seed(&store.dir)?
    };
    let all = make_rows(&ledger, &config, &labels, &rates);
    let rows = all
        .into_iter()
        .zip(&ledger.requests)
        .filter(|(row, request)| {
            (options.explain.is_some() || cutoff.is_none_or(|start| request.time >= start))
                && options
                    .profile
                    .as_deref()
                    .is_none_or(|profile| row.profile == profile)
        })
        .map(|(row, _)| row)
        .filter(|row| {
            options
                .explain
                .as_deref()
                .is_none_or(|session| row.session == session)
        })
        .filter(|row| {
            !options.unattributed
                || row.project == "(unattributed)"
                || row.project.ends_with("/(workspace files)")
        })
        .collect::<Vec<_>>();
    let footer = footer(&ledger, &ingest, rows.len());
    if options.json {
        return Ok(serde_json::to_string_pretty(
            &serde_json::json!({"rows":rows,"footer":footer}),
        )? + "\n");
    }
    let mut output = if let Some(session) = &options.explain {
        explain(&rows, session)
    } else if options.unattributed {
        unattributed(&rows)
    } else if let Some(grouping) = &options.by {
        by(&rows, grouping)?
    } else {
        tree(&rows)
    };
    output.push_str(&footer_text(&footer));
    Ok(output)
}

pub fn label(store: &Store, session: &str, project: &str) -> Result<String> {
    let Some(_lock) = store.try_lock()? else {
        bail!("usage ledger is busy");
    };
    let mut labels = load_labels(&store.dir)?;
    labels
        .sessions
        .insert(session.to_string(), project.to_string());
    atomic::write(
        &store.dir.join("labels.json"),
        &serde_json::to_vec_pretty(&labels)?,
    )?;
    let ledger = store.load()?;
    let rows = refresh_summary(store, &ledger)?;
    let explanations = explain(&rows, session);
    Ok(format!("Label saved in labels.json.\n{explanations}"))
}

pub fn verify(store: &Store) -> Result<(String, i32)> {
    let ingest = store.ingest()?;
    let ledger = store.load()?;
    let rates = if ingest.skipped_lock {
        read_or_seed(&store.dir)?
    } else {
        load_or_seed(&store.dir)?
    };
    let mut grouped = BTreeMap::<(String, String, String), Vec<&Request>>::new();
    for row in &ledger.requests {
        grouped
            .entry((row.profile.clone(), row.session.clone(), row.model.clone()))
            .or_default()
            .push(row);
    }
    let mut output = String::from("Cost-state verification (API-equivalent USD):\n");
    let mut cost_state_sessions = 0;
    let mut checked_sessions = 0;
    let mut checked = 0;
    let mut failed = 0;
    for session in &ledger.sessions {
        let Some(state) = &session.cost_state else {
            continue;
        };
        cost_state_sessions += 1;
        let checked_before = checked;
        let name = format!("{} {}", session.profile, session.id);
        let session_requests = ledger
            .requests
            .iter()
            .filter(|row| row.profile == session.profile && row.session == session.id)
            .collect::<Vec<_>>();
        if session_requests.iter().any(|row| {
            row.web_searches > 0
                || row.web_fetches > 0
                || row.speed.as_deref() == Some("fast")
                || rates.aliases.contains_key(&row.model)
        }) {
            output.push_str(&format!(
                "  {name}: excluded (web tools, fast, or aliased requests)\n"
            ));
            continue;
        }
        for (model, expected) in &state.models {
            let model_name = format!("{name} {model}");
            let Some(requests) =
                grouped.get(&(session.profile.clone(), session.id.clone(), model.clone()))
            else {
                let category = if model.contains("haiku") {
                    "background Haiku"
                } else {
                    "resumed/unflushed session"
                };
                output.push_str(&format!(
                    "  {model_name}: no transcript requests ({category})\n"
                ));
                continue;
            };
            let input: u64 = requests.iter().map(|row| row.input).sum();
            let output_tokens: u64 = requests.iter().map(|row| row.output).sum();
            let write: u64 = requests
                .iter()
                .map(|row| row.cache_write_5m + row.cache_write_1h)
                .sum();
            let read: u64 = requests.iter().map(|row| row.cache_read).sum();
            if (input, output_tokens, write, read)
                != (
                    expected.input,
                    expected.output,
                    expected.cache_write,
                    expected.cache_read,
                )
            {
                output.push_str(&format!(
                    "  {model_name}: tokens differ (resumed/unflushed session)\n"
                ));
                continue;
            }
            let amounts = requests
                .iter()
                .map(|row| cost(row, &rates).map(|value| value.total()))
                .collect::<Option<Vec<_>>>();
            let Some(amounts) = amounts else {
                output.push_str(&format!("  {model_name}: unpriced model\n"));
                continue;
            };
            let computed: f64 = amounts.iter().sum();
            checked += 1;
            if (computed - expected.usd).abs() > expected.usd.abs().max(0.01) * 0.01 {
                failed += 1;
                output.push_str(&format!(
                    "  {model_name}: FAIL rate check ${computed:.4} vs ${:.4}\n",
                    expected.usd
                ));
            } else {
                output.push_str(&format!(
                    "  {model_name}: OK ${computed:.4} vs ${:.4}\n",
                    expected.usd
                ));
            }
        }
        if checked > checked_before {
            checked_sessions += 1;
        }
    }
    output.push_str(&format!(
        "Cost-state sessions {cost_state_sessions}; checked sessions {checked_sessions}; rate checks {checked}; failed {failed}."
    ));
    output.push_str(&footer_text(&footer(
        &ledger,
        &ingest,
        ledger.requests.len(),
    )));
    Ok((
        output,
        if failed > 0 {
            1
        } else if checked == 0 {
            2
        } else {
            0
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::super::ledger::Source;
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;

    #[test]
    fn timestamp_free_titles_name_a_metadata_only_session() {
        // Known-bad: requiring metadata timestamps leaves a title-only session
        // unnamed; min_by_key on the rename branch displays the first rename.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        fs::write(projects.join("session.jsonl"),
            concat!(
                "{\"type\":\"ai-title\",\"sessionId\":\"sample\",\"aiTitle\":\"first idea\"}\n",
                "{\"type\":\"ai-title\",\"sessionId\":\"sample\",\"aiTitle\":\"current idea\"}\n",
                "{\"type\":\"custom-title\",\"sessionId\":\"sample\",\"customTitle\":\"first name\"}\n",
                "{\"type\":\"custom-title\",\"sessionId\":\"sample\",\"customTitle\":\"current name\"}\n"
            )).unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        assert_eq!(store.ingest().unwrap().malformed, 0);
        let ledger = store.load().unwrap();
        assert_eq!(
            display_title(ledger.sessions.first()),
            Some("current name".into())
        );
        assert_eq!(ledger.sessions[0].first, None);
        assert_eq!(ledger.sessions[0].last, None);
    }

    #[test]
    fn explain_names_the_attribution_signal_and_json_has_footer() {
        // Known-bad: --explain omits the signal, JSON drops the footer, or a
        // renamed session appears only as its opaque ID in the default tree.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/usage/profile/projects/demo");
        fs::copy(source.join("a.jsonl"), projects.join("a.jsonl")).unwrap();
        fs::copy(source.join("b.jsonl"), projects.join("b.jsonl")).unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        let mut options = Options {
            since: "all".into(),
            profile: None,
            by: None,
            json: false,
            explain: Some("00000000-0000-4000-8000-000000000001".into()),
            unattributed: false,
        };
        let now = DateTime::parse_from_rfc3339("2030-01-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let text = run(&store, &options, now).unwrap();
        assert!(text.contains("via none"));
        assert!(text.contains("duplicates collapsed"));
        options.json = true;
        let value: serde_json::Value =
            serde_json::from_str(&run(&store, &options, now).unwrap()).unwrap();
        assert_eq!(value["footer"]["requests_counted"], 2);
        assert_eq!(value["footer"]["synthetic_skipped"], 1);
        assert_eq!(
            value["footer"]["sidechain_replays"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(store.dir.join("summary.json").exists());
        options.json = false;
        options.explain = None;
        assert!(
            run(&store, &options, now)
                .unwrap()
                .contains("sample-task: synthetic title")
        );
    }

    #[test]
    fn verify_checks_matching_model_tokens_and_reports_bad_rate_without_editing_rates() {
        // Known-bad: fitting rates to cost-state makes the verification circular;
        // a failed check must report the gap and leave rates.json unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        let path = projects.join("session.jsonl");
        let assistant = json!({"type":"assistant","timestamp":"2030-01-01T12:00:00Z","sessionId":"sample",
            "requestId":"req-1","message":{"id":"msg-1","model":"claude-opus-5-5","usage":{
                "input_tokens":1_000_000,"output_tokens":1_000_000,"cache_creation_input_tokens":2_000_000,
                "cache_creation":{"ephemeral_5m_input_tokens":1_000_000,"ephemeral_1h_input_tokens":1_000_000},
                "cache_read_input_tokens":1_000_000,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0}}}});
        let state = |usd: f64| {
            json!({"type":"cost-state","sessionId":"sample",
            "modelUsage":{"claude-opus-5-5":{"inputTokens":1_000_000,"outputTokens":1_000_000,
                "cacheCreationInputTokens":2_000_000,"cacheReadInputTokens":1_000_000,"costUSD":usd}},
            "totalCostUSD":usd})
        };
        fs::write(&path, format!("{}\n{}\n", assistant, state(37.2))).unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        let (good, exit_code) = verify(&store).unwrap();
        assert_eq!(exit_code, 0, "{good}");
        assert!(good.contains("OK $37.2000 vs $37.2000"));
        assert!(good.contains("Cost-state sessions 1; checked sessions 1"));
        // Known-bad: the ingest placeholder overwrites the attributed
        // summary with a flat, incompatible row during verify.
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(store.dir.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary[0]["totals"]["requests"], 1);
        assert!(summary[0].get("requests").is_none());
        let before = fs::read(store.dir.join("rates.json")).unwrap();
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            state(99.0)
        )
        .unwrap();
        let (bad, exit_code) = verify(&store).unwrap();
        assert_eq!(exit_code, 1, "{bad}");
        assert!(bad.contains("FAIL rate check"));
        assert_eq!(fs::read(store.dir.join("rates.json")).unwrap(), before);
    }

    #[test]
    fn verify_returns_exit_two_when_no_rate_check_is_possible() {
        // Known-bad: a vacuous verify exits 0 when no cost-state model can be checked.
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("usage"), Vec::new());
        let (output, exit_code) = verify(&store).unwrap();
        assert_eq!(exit_code, 2, "{output}");
        assert!(output.contains("Cost-state sessions 0; checked sessions 0"));
    }

    #[test]
    fn explain_lists_each_typed_candidate_once() {
        // Known-bad: one candidate is printed for every request in a
        // multi-request session, and file touches are called label candidates.
        let row = ReportRow {
            day: "2030-01-01".into(),
            profile: "sample".into(),
            workspace: "(unattributed)".into(),
            project: "(unattributed)".into(),
            session: "sample".into(),
            title: None,
            model: "claude-sonnet-5".into(),
            signal: "none".into(),
            candidates: vec![
                "unresolved label 'unknown'".into(),
                "file touch 'blue/alpha'".into(),
            ],
            input: 1,
            output: 2,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
            cost_usd: Some(0.0),
        };
        let mut other_slice = row.clone();
        other_slice.project = "blue/beta".into();
        other_slice.signal = "cwd".into();
        let output = explain(&[row.clone(), row, other_slice], "sample");
        assert!(output.contains("via none (2 requests)"));
        assert!(output.contains("via cwd (1 requests)"));
        assert!(output.contains("candidates: file touch 'blue/alpha', unresolved label 'unknown'"));
        assert_eq!(output.matches("file touch 'blue/alpha'").count(), 1);
        assert_eq!(output.matches("unresolved label 'unknown'").count(), 1);
    }

    #[test]
    fn a_later_label_rebuilds_attributed_summary() {
        // Known-bad: fixing project attribution only at ingest makes labels
        // ineffective for a session whose title or cwd later changes.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("atlas");
        let one = root.join("teams/blue/apps/one");
        let two = root.join("teams/blue/apps/two");
        fs::create_dir_all(one.join(".git")).unwrap();
        fs::create_dir_all(two.join(".git")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        let profile = tmp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        let request = |id: &str, cwd: &std::path::Path| {
            json!({
            "type":"assistant","timestamp":"2030-01-01T12:00:00Z","sessionId":id,
            "requestId":format!("req-{id}"),"cwd":cwd,"message":{"id":format!("msg-{id}"),
                "model":"claude-sonnet-5","usage":{"input_tokens":1,"output_tokens":1}}})
        };
        fs::write(
            projects.join("session.jsonl"),
            format!("{}\n{}\n", request("first", &one), request("second", &two)),
        )
        .unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(
            store.dir.join("config.json"),
            serde_json::to_vec(&json!({
                "superproject":root,"project_globs":["teams/*/apps/*"],
                "workspaces":[{"glob":"teams/*","segment":1}]
            }))
            .unwrap(),
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2030-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let options = Options {
            since: "all".into(),
            profile: None,
            by: None,
            json: true,
            explain: None,
            unattributed: false,
        };
        run(&store, &options, now).unwrap();
        let before: serde_json::Value =
            serde_json::from_slice(&fs::read(store.dir.join("summary.json")).unwrap()).unwrap();
        assert_eq!(before[0]["project"], "blue/one");
        let explanation = label(&store, "first", "two").unwrap();
        assert!(explanation.contains("via label"));
        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(store.dir.join("summary.json")).unwrap()).unwrap();
        assert!(
            after
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["project"] == "blue/two")
        );
    }
}

#[cfg(test)]
mod alias_verify_tests {
    use super::*;
    use crate::usage::ledger::Source;
    use std::fs;
    #[test]
    fn verify_skips_aliased_gateway_requests() {
        // Known-bad: comparing an alias-priced gateway request with Claude Code cost-state.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let projects = profile.join("projects/demo");
        fs::create_dir_all(&projects).unwrap();
        let request = serde_json::json!({"type":"assistant","timestamp":"2030-01-01T12:00:00Z","sessionId":"s",
            "requestId":"req","message":{"id":"msg","model":"acme/claude-x.5","usage":{"input_tokens":1_000_000}}});
        let state = serde_json::json!({"type":"cost-state","sessionId":"s","modelUsage":{"acme/claude-x.5":{
            "inputTokens":1_000_000,"outputTokens":0,"cacheCreationInputTokens":0,"cacheReadInputTokens":0,"costUSD":999.0}}});
        fs::write(projects.join("s.jsonl"), format!("{request}\n{state}\n")).unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "p".into(),
                directory: profile,
            }],
        );
        fs::create_dir_all(&store.dir).unwrap();
        let mut rates = super::super::rates::seed();
        rates
            .aliases
            .insert("acme/claude-x.5".into(), "claude-opus-5".into());
        fs::write(
            store.dir.join("rates.json"),
            serde_json::to_vec(&rates).unwrap(),
        )
        .unwrap();
        let (output, exit) = verify(&store).unwrap();
        assert_eq!(exit, 2, "{output}");
        assert!(output.contains("aliased requests"), "{output}");
    }
}
