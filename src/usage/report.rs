use super::attribute::{Attribution, Config, Labels, Resolver, load_config, load_labels};
use super::ledger::{IngestReport, Ledger, Session, Store};
use super::parse::Request;
use super::rates::{Rates, cost, load_or_seed, read_or_seed};
use crate::atomic;
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

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
        .max_by_key(|title| title.time)
        .or_else(|| {
            session
                .titles
                .iter()
                .filter(|title| title.source == "ai-title")
                .max_by_key(|title| title.time)
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

fn footer(ledger: &Ledger, ingest: &IngestReport, rows: usize) -> Footer {
    Footer {
        requests_counted: rows,
        duplicates_collapsed: ledger.cursors.duplicates,
        unreadable_lines: ledger.cursors.malformed + ingest.partial,
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
        "\n{} requests counted · {} duplicates collapsed · {} unreadable lines · earliest ingested {}{}\n",
        footer.requests_counted,
        footer.duplicates_collapsed,
        footer.unreadable_lines,
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
    let mut grouped = BTreeMap::<(String, String, String), (Totals, Vec<String>)>::new();
    for row in rows.iter().filter(|row| row.session == session) {
        let entry = grouped
            .entry((row.profile.clone(), row.project.clone(), row.signal.clone()))
            .or_default();
        entry.0.add(row);
        entry.1.extend(row.candidates.clone());
    }
    let mut out = format!("Attribution for session {session}:\n");
    if grouped.is_empty() {
        out.push_str("  no matching requests\n");
    }
    for ((profile, project, signal), (totals, candidates)) in grouped {
        out.push_str(&format!(
            "  {profile}: {project} via {signal} ({} requests)",
            totals.requests
        ));
        if !candidates.is_empty() {
            out.push_str(&format!(
                "; unresolved label candidates: {}",
                candidates.join(", ")
            ));
        }
        out.push('\n');
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
    if !ingest.skipped_lock {
        write_summary(store, &all)?;
    }
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
    let config = load_config(&store.dir)?;
    let rates = load_or_seed(&store.dir)?;
    let rows = make_rows(&ledger, &config, &labels, &rates);
    write_summary(store, &rows)?;
    let explanations = explain(&rows, session);
    Ok(format!("Label saved in labels.json.\n{explanations}"))
}

pub fn purge_profile(store: &Store, profile: &str) -> Result<usize> {
    let before = store.load()?;
    let removed_sessions = before
        .sessions
        .iter()
        .filter(|session| session.profile == profile)
        .map(|session| session.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let removed = store.purge_profile(profile)?;
    let ledger = store.load()?;
    let mut labels = load_labels(&store.dir)?;
    labels.sessions.retain(|session, _| {
        !removed_sessions.contains(session)
            || ledger
                .sessions
                .iter()
                .any(|remaining| &remaining.id == session)
    });
    if store.dir.join("labels.json").exists() {
        atomic::write(
            &store.dir.join("labels.json"),
            &serde_json::to_vec_pretty(&labels)?,
        )?;
    }
    let config = load_config(&store.dir)?;
    let rates = load_or_seed(&store.dir)?;
    write_summary(store, &make_rows(&ledger, &config, &labels, &rates))?;
    Ok(removed)
}

pub fn verify(store: &Store) -> Result<(String, bool)> {
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
    let mut checked = 0;
    let mut failed = 0;
    for session in &ledger.sessions {
        let Some(state) = &session.cost_state else {
            continue;
        };
        let name = format!("{} {}", session.profile, session.id);
        let session_requests = ledger
            .requests
            .iter()
            .filter(|row| row.profile == session.profile && row.session == session.id)
            .collect::<Vec<_>>();
        if session_requests.iter().any(|row| {
            row.web_searches > 0 || row.web_fetches > 0 || row.speed.as_deref() == Some("fast")
        }) {
            output.push_str(&format!(
                "  {name}: excluded (web tools or fast requests)\n"
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
    }
    output.push_str(&format!("Checked {checked}; failed {failed}."));
    output.push_str(&footer_text(&footer(
        &ledger,
        &ingest,
        ledger.requests.len(),
    )));
    Ok((output, failed > 0))
}

#[cfg(test)]
mod tests {
    use super::super::ledger::Source;
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;

    #[test]
    fn explain_names_the_attribution_signal_and_json_has_footer() {
        // Known-bad: --explain omits the signal, or JSON drops the report footer.
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
        let state = |time: &str, usd: f64| {
            json!({"type":"cost-state","timestamp":time,"sessionId":"sample",
            "modelUsage":{"claude-opus-5-5":{"inputTokens":1_000_000,"outputTokens":1_000_000,
                "cacheCreationInputTokens":2_000_000,"cacheReadInputTokens":1_000_000,"costUSD":usd}},
            "totalCostUSD":usd})
        };
        fs::write(
            &path,
            format!("{}\n{}\n", assistant, state("2030-01-01T12:01:00Z", 37.2)),
        )
        .unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "sample".into(),
                directory: profile,
            }],
        );
        let (good, failed) = verify(&store).unwrap();
        assert!(!failed, "{good}");
        assert!(good.contains("OK $37.2000 vs $37.2000"));
        let before = fs::read(store.dir.join("rates.json")).unwrap();
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            state("2030-01-01T12:02:00Z", 99.0)
        )
        .unwrap();
        let (bad, failed) = verify(&store).unwrap();
        assert!(failed, "{bad}");
        assert!(bad.contains("FAIL rate check"));
        assert_eq!(fs::read(store.dir.join("rates.json")).unwrap(), before);
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
