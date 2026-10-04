//! Read-only usage views and the hourly aggregate persisted after ingest.
use super::billing::{self, Billing};
use super::ledger::Ledger;
use super::rates::Rates;
use crate::atomic;
use chrono::{DateTime, Duration, FixedOffset, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hourly {
    pub version: u32,
    pub generated_at: DateTime<Utc>,
    pub rows: Vec<Bucket>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bucket {
    pub profile: String,
    pub hour: DateTime<Utc>,
    pub model: String,
    pub speed: Option<String>,
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
}
impl Bucket {
    pub fn tokens(&self) -> [u64; 5] {
        [
            self.input,
            self.output,
            self.cache_write_5m,
            self.cache_write_1h,
            self.cache_read,
        ]
    }
}
pub fn floor_hour(time: DateTime<Utc>) -> DateTime<Utc> {
    time.with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap()
}
pub fn write(dir: &Path, ledger: &Ledger, now: DateTime<Utc>) -> anyhow::Result<()> {
    let mut grouped = BTreeMap::<(String, DateTime<Utc>, String, Option<String>), Bucket>::new();
    for r in &ledger.requests {
        let key = (
            r.profile.clone(),
            floor_hour(r.time),
            r.model.clone(),
            r.speed.clone(),
        );
        let b = grouped.entry(key).or_insert_with(|| Bucket {
            profile: r.profile.clone(),
            hour: floor_hour(r.time),
            model: r.model.clone(),
            speed: r.speed.clone(),
            requests: 0,
            input: 0,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        });
        b.requests += 1;
        b.input += r.input;
        b.output += r.output;
        b.cache_write_5m += r.cache_write_5m;
        b.cache_write_1h += r.cache_write_1h;
        b.cache_read += r.cache_read;
    }
    atomic::write(
        &dir.join("hourly.json"),
        &serde_json::to_vec_pretty(&Hourly {
            version: 1,
            generated_at: now,
            rows: grouped.into_values().collect(),
        })?,
    )
}
pub fn read(dir: &Path) -> Option<Hourly> {
    let value: Hourly = serde_json::from_slice(&fs::read(dir.join("hourly.json")).ok()?).ok()?;
    (value.version == 1).then_some(value)
}
#[derive(Default, Clone, Debug)]
pub struct Amount {
    pub tokens: u64,
    pub value: f64,
    pub spend: f64,
    pub priced: u64,
    pub unpriced: u64,
    pub spend_priced: u64,
    pub spend_unpriced: u64,
}
impl Amount {
    pub fn list_cell(&self) -> String {
        money_cell(self.value, self.priced, self.unpriced, true)
    }
    pub fn list_info_cell(&self) -> String {
        money_cell(self.value, self.priced, self.unpriced, false)
    }
    pub fn spend_cell(&self) -> String {
        money_cell(self.spend, self.spend_priced, self.spend_unpriced, false)
    }
}
fn money_cell(value: f64, priced: u64, unpriced: u64, approx: bool) -> String {
    if priced == 0 && unpriced > 0 {
        return "$*".into();
    }
    if priced == 0 {
        return "—".into();
    }
    let marker = if approx { "~" } else { "" };
    let star = if unpriced > 0 { "*" } else { "" };
    format!("{marker}{}{star}", money(value))
}
pub fn money(value: f64) -> String {
    if (value * 100.0).round().abs() < 100_000.0 {
        return format!("${value:.2}");
    }
    let rounded = value.round() as i64;
    let digits = rounded.abs().to_string();
    let mut parts = Vec::new();
    for chunk in digits.as_bytes().rchunks(3) {
        parts.push(std::str::from_utf8(chunk).unwrap().to_string());
    }
    format!(
        "{}${}",
        if rounded < 0 { "-" } else { "" },
        parts.into_iter().rev().collect::<Vec<_>>().join(",")
    )
}
pub fn rate_money(value: f64) -> String {
    if value > 0.0 && value < 0.0001 {
        return "<$0.0001".into();
    }
    let mut text = format!("${value:.4}");
    while text.ends_with('0') && text.split('.').next_back().unwrap().len() > 2 {
        text.pop();
    }
    text
}
pub fn capacity_money(value: f64) -> String {
    let rounded = value.round() as i64;
    let digits = rounded.abs().to_string();
    let chunks = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join(",");
    format!("{}${chunks}", if rounded < 0 { "-" } else { "" })
}
pub fn start_today(now: DateTime<Utc>, offset: FixedOffset) -> DateTime<Utc> {
    now.with_timezone(&offset)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(offset)
        .single()
        .unwrap()
        .to_utc()
}
pub fn window<'a>(
    rows: impl Iterator<Item = &'a Bucket>,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    rates: &Rates,
    billing: Option<&billing::ProfileBilling>,
    per_token: bool,
) -> Amount {
    let mut amount = Amount::default();
    for b in rows.filter(|b| b.hour >= start && b.hour < now) {
        amount.tokens += b.tokens().iter().sum::<u64>();
        let (list, spend) = billing::price(
            &b.model,
            b.speed.as_deref(),
            b.tokens(),
            rates,
            billing,
            per_token,
        );
        if let Some(value) = list {
            amount.value += value;
            amount.priced += b.requests
        } else {
            amount.unpriced += b.requests
        }
        if per_token {
            if let Some(value) = spend {
                amount.spend += value;
                amount.spend_priced += b.requests
            } else {
                amount.spend_unpriced += b.requests
            }
        }
    }
    amount
}
pub fn windows(
    rows: &[Bucket],
    profile: &str,
    now: DateTime<Utc>,
    offset: FixedOffset,
    rates: &Rates,
    billing: Option<&billing::ProfileBilling>,
    per_token: bool,
) -> [Amount; 3] {
    let select = || rows.iter().filter(|b| b.profile == profile);
    [
        window(
            select(),
            start_today(now, offset),
            now,
            rates,
            billing,
            per_token,
        ),
        window(
            select(),
            floor_hour(now - Duration::days(7)),
            now,
            rates,
            billing,
            per_token,
        ),
        window(
            select(),
            floor_hour(now - Duration::days(30)),
            now,
            rates,
            billing,
            per_token,
        ),
    ]
}
pub fn tokens(value: u64) -> String {
    if value < 1000 {
        return value.to_string();
    }
    let units = [(1e3, "k"), (1e6, "M"), (1e9, "B")];
    let mut unit = if value >= 1_000_000_000 {
        2
    } else if value >= 1_000_000 {
        1
    } else {
        0
    };
    loop {
        let (scale, suffix) = units[unit];
        let n = value as f64 / scale;
        let mut precision = if n >= 100.0 {
            0
        } else if n >= 10.0 {
            1
        } else {
            2
        };
        let mut rounded = format!("{n:.precision$}").parse::<f64>().unwrap();
        if rounded >= 1000.0 && unit < 2 {
            unit += 1;
            continue;
        }
        precision = if rounded >= 100.0 {
            0
        } else if rounded >= 10.0 {
            1
        } else {
            2
        };
        rounded = format!("{n:.precision$}").parse::<f64>().unwrap();
        if rounded >= 1000.0 && unit < 2 {
            unit += 1;
            continue;
        }
        return format!("{n:.precision$} {suffix}");
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Weekly {
    pub percent: f64,
    pub resets_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_started_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LimitRow {
    pub profile: String,
    pub fetched_at: DateTime<Utc>,
    pub weekly: Weekly,
}
pub fn history(dir: &Path) -> Vec<LimitRow> {
    fs::read_to_string(dir.join("limits.jsonl"))
        .ok()
        .map(|data| {
            data.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}
pub fn append_history(dir: &Path, sources: &[super::ledger::Source]) -> anyhow::Result<()> {
    use crate::limits::{Limits, parse_limits, read_claude_json};
    use std::io::Write;
    let path = dir.join("limits.jsonl");
    let mut seen = history(dir);
    for source in sources {
        let Ok(Some(claude)) = read_claude_json(&source.directory) else {
            continue;
        };
        let Limits::Snapshot(snapshot) = parse_limits(&claude) else {
            continue;
        };
        let Some(weekly) = snapshot
            .weekly()
            .filter(|w| w.kind == "weekly_all")
            .and_then(|w| {
                w.resets_at.map(|r| Weekly {
                    percent: w.percent,
                    resets_at: r,
                    window_started_at: snapshot.window_started_at,
                })
            })
        else {
            continue;
        };
        if seen
            .iter()
            .any(|r| r.profile == source.profile && r.fetched_at == snapshot.fetched_at)
        {
            continue;
        }
        seen.push(LimitRow {
            profile: source.profile.clone(),
            fetched_at: snapshot.fetched_at,
            weekly,
        });
    }
    let old = history(dir);
    if seen.len() > old.len() {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        for row in &seen[old.len()..] {
            serde_json::to_writer(&mut file, row)?;
            file.write_all(b"\n")?
        }
        file.sync_all()?;
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct Capacity {
    pub lower: f64,
    pub upper: f64,
    pub partial: bool,
    pub snapshot: LimitRow,
    pub min: f64,
    pub max: f64,
    pub after_reset: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug)]
pub struct CapacityReport {
    pub estimate: Option<Capacity>,
    pub reason: &'static str,
    pub hint: Option<(DateTime<Utc>, DateTime<Utc>)>,
}
#[derive(Clone, Debug)]
struct ResetEvent {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    detected: bool,
    covered: bool,
}
fn overlaps(a: &ResetEvent, b: &ResetEvent) -> bool {
    a.from <= b.to && b.from <= a.to
}
pub fn capacity(
    profile: &str,
    snapshots: &[LimitRow],
    hourly: &Hourly,
    rates: &Rates,
    recorded: &[billing::LimitReset],
) -> CapacityReport {
    let mut windows = BTreeMap::<DateTime<Utc>, Vec<&LimitRow>>::new();
    for row in snapshots.iter().filter(|r| r.profile == profile) {
        windows.entry(row.weekly.resets_at).or_default().push(row);
    }
    let newest_window = windows.keys().next_back().copied();
    let mut estimates = Vec::<(f64, f64, bool, LimitRow, Option<DateTime<Utc>>)>::new();
    let mut hint = None;
    let mut waiting = false;
    for (resets_at, mut rows) in windows {
        rows.sort_by_key(|row| row.fetched_at);
        let start = rows
            .iter()
            .rev()
            .find_map(|row| row.weekly.window_started_at)
            .unwrap_or(resets_at - Duration::days(7));
        let mut events = recorded
            .iter()
            .filter(|reset| reset.to >= start && reset.from < resets_at)
            .map(|reset| ResetEvent {
                from: reset.from.max(start),
                to: reset.to.min(resets_at),
                detected: false,
                covered: true,
            })
            .collect::<Vec<_>>();
        for pair in rows.windows(2) {
            if pair[0].weekly.percent - pair[1].weekly.percent < 1.0 {
                continue;
            }
            let detected = ResetEvent {
                from: pair[0].fetched_at,
                to: pair[1].fetched_at,
                detected: true,
                covered: false,
            };
            if detected.to < start || detected.from >= resets_at {
                continue;
            }
            if let Some(event) = events.iter_mut().find(|event| overlaps(event, &detected)) {
                event.from = event.from.max(detected.from);
                event.to = event.to.min(detected.to);
                event.detected = true;
                event.covered = true;
            } else {
                events.push(detected);
            }
        }
        events.sort_by_key(|event| event.from);
        if Some(resets_at) == newest_window {
            hint = events
                .iter()
                .find(|event| {
                    event.detected && !event.covered && event.to - event.from > Duration::hours(48)
                })
                .map(|event| (event.from, event.to));
        }
        for segment in 0..=events.len() {
            let candidates = rows
                .iter()
                .copied()
                .filter(|row| {
                    let fetched = row.fetched_at;
                    if fetched < start || fetched >= resets_at {
                        return false;
                    }
                    if events
                        .iter()
                        .any(|event| fetched > event.from && fetched < event.to)
                    {
                        return false;
                    }
                    let assigned = events.iter().filter(|event| fetched >= event.to).count();
                    assigned == segment
                })
                .collect::<Vec<_>>();
            if candidates
                .iter()
                .any(|row| row.weekly.percent >= 20.0 && row.fetched_at > hourly.generated_at)
            {
                waiting = true;
            }
            let Some(snapshot) = candidates
                .into_iter()
                .filter(|row| row.fetched_at <= hourly.generated_at)
                .max_by(|a, b| {
                    a.weekly
                        .percent
                        .total_cmp(&b.weekly.percent)
                        .then(a.fetched_at.cmp(&b.fetched_at))
                })
            else {
                continue;
            };
            let u = snapshot.weekly.percent.min(100.0);
            if u < 20.0 {
                continue;
            }
            let reset = segment.checked_sub(1).map(|index| &events[index]);
            if reset.is_some_and(|event| event.to - event.from > Duration::hours(48)) {
                continue;
            }
            let value = |from| {
                window(
                    hourly
                        .rows
                        .iter()
                        .filter(|bucket| bucket.profile == profile),
                    floor_hour(from),
                    floor_hour(snapshot.fetched_at) + Duration::hours(1),
                    rates,
                    None,
                    false,
                )
            };
            let lower = value(reset.map_or(start, |event| event.to));
            let upper = reset.map_or_else(|| lower.clone(), |event| value(event.from));
            estimates.push((
                lower.value / (u / 100.0),
                upper.value / (u / 100.0),
                lower.unpriced > 0 || upper.unpriced > 0,
                snapshot.clone(),
                reset.map(|event| event.from),
            ));
        }
    }
    let estimate = estimates.last().map(|newest| {
        let recent = &estimates[estimates.len().saturating_sub(4)..];
        Capacity {
            lower: newest.0,
            upper: newest.1,
            partial: newest.2,
            snapshot: newest.3.clone(),
            after_reset: newest.4,
            min: recent
                .iter()
                .map(|item| item.0)
                .fold(f64::INFINITY, f64::min),
            max: recent.iter().map(|item| item.1).fold(0.0, f64::max),
        }
    });
    CapacityReport {
        estimate,
        reason: if waiting {
            "waiting for an ingest"
        } else {
            "no snapshot ≥ 20%"
        },
        hint,
    }
}
pub fn load_settings(
    dir: &Path,
    now: DateTime<Utc>,
) -> Result<(Rates, Billing), (&'static str, String)> {
    let rates =
        super::rates::read_or_seed(dir).map_err(|error| ("rates.json", error.to_string()))?;
    let billing = billing::read(dir, now).map_err(|error| {
        let reason = error.to_string();
        (
            "billing.json",
            reason
                .strip_prefix("billing.json: ")
                .unwrap_or(&reason)
                .to_string(),
        )
    })?;
    Ok((rates, billing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::rates;
    #[test]
    fn rate_money_keeps_four_decimal_precision() {
        // Known-bad: formatting a per-million price as cents.
        for (value, expected) in [
            (1.0, "$1.00"),
            (0.85, "$0.85"),
            (0.135, "$0.135"),
            (0.0375, "$0.0375"),
            (12.5, "$12.50"),
            (0.0, "$0.00"),
            (0.00001, "<$0.0001"),
        ] {
            assert_eq!(rate_money(value), expected);
        }
    }
    #[test]
    fn rounded_money_and_tokens_choose_the_right_unit() {
        // Known-bad: $1000.00, 1000 k, and 100.0 k at rounded unit boundaries.
        assert_eq!(money(999.996), "$1,000");
        assert_eq!(money(999.99), "$999.99");
        assert_eq!(tokens(999_999), "1.00 M");
        assert_eq!(tokens(99_999), "100 k");
    }
    fn b(hour: &str, model: &str) -> Bucket {
        Bucket {
            profile: "p".into(),
            hour: DateTime::parse_from_rfc3339(hour).unwrap().to_utc(),
            model: model.into(),
            speed: None,
            requests: 1,
            input: 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        }
    }
    #[test]
    fn windows_use_local_midnight_and_hour_edges() {
        // Known-bad: UTC midnight as today or including a bucket outside [start, now).
        let now = DateTime::parse_from_rfc3339("2030-01-08T03:30:00Z")
            .unwrap()
            .to_utc();
        let offset = FixedOffset::west_opt(3 * 3600).unwrap();
        let rows = vec![
            b("2030-01-08T02:00:00Z", "claude-opus-5"),
            b("2030-01-08T03:00:00Z", "claude-opus-5"),
        ];
        let sums = windows(&rows, "p", now, offset, &rates::seed(), None, false);
        assert_eq!(sums[0].tokens, 1_000_000);
        assert_eq!(sums[1].tokens, 2_000_000);
    }
    #[test]
    fn seven_and_thirty_day_hour_edges_are_half_open() {
        // Known-bad: counting the hour before a rolling-window edge or the current hour.
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let now = at("2030-02-01T12:30:00Z");
        let rates = rates::seed();
        let rows = vec![
            b("2030-01-02T11:00:00Z", "claude-opus-5"),
            b("2030-01-02T12:00:00Z", "claude-opus-5"),
            b("2030-01-25T11:00:00Z", "claude-opus-5"),
            b("2030-01-25T12:00:00Z", "claude-opus-5"),
            b("2030-02-01T12:00:00Z", "claude-opus-5"),
        ];
        let sums = windows(
            &rows,
            "p",
            now,
            FixedOffset::east_opt(0).unwrap(),
            &rates,
            None,
            false,
        );
        assert_eq!(sums[1].tokens, 2_000_000); // edge hour and current hour
        assert_eq!(sums[2].tokens, 4_000_000); // thirty-day edge through current hour
    }
    #[test]
    fn capacity_uses_highest_snapshot_and_marks_unpriced() {
        // Known-bad: using the newest lower percent, or silently treating unpriced rows as complete.
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let reset = at("2030-01-08T00:00:00Z");
        let rows = vec![
            b("2030-01-02T00:00:00Z", "claude-opus-5"),
            b("2030-01-03T00:00:00Z", "unknown"),
        ];
        let snapshots = vec![
            LimitRow {
                profile: "p".into(),
                fetched_at: at("2030-01-04T00:00:00Z"),
                weekly: Weekly {
                    percent: 50.0,
                    resets_at: reset,
                    window_started_at: None,
                },
            },
            LimitRow {
                profile: "p".into(),
                fetched_at: at("2030-01-05T00:00:00Z"),
                weekly: Weekly {
                    percent: 50.0,
                    resets_at: reset,
                    window_started_at: None,
                },
            },
        ];
        let hourly = Hourly {
            version: 1,
            generated_at: at("2030-01-06T00:00:00Z"),
            rows,
        };
        let result = capacity("p", &snapshots, &hourly, &rates::seed(), &[])
            .estimate
            .unwrap();
        assert_eq!(result.lower, 10.0);
        assert!(result.partial);
        let mut low = snapshots.clone();
        low[0].weekly.percent = 3.0;
        low[1].weekly.percent = 4.0;
        assert!(
            capacity("p", &low, &hourly, &rates::seed(), &[])
                .estimate
                .is_none()
        );
        let behind = Hourly {
            version: 1,
            generated_at: at("2030-01-03T00:00:00Z"),
            rows: hourly.rows.clone(),
        };
        assert!(
            capacity("p", &snapshots, &behind, &rates::seed(), &[])
                .estimate
                .is_none()
        ); // Known-bad: estimating before ingest catches up.
    }
}

#[cfg(test)]
mod reset_capacity_tests {
    use super::*;
    use crate::usage::rates;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }
    fn row(fetched: &str, reset: &str, percent: f64, start: Option<&str>) -> LimitRow {
        LimitRow {
            profile: "p".into(),
            fetched_at: at(fetched),
            weekly: Weekly {
                percent,
                resets_at: at(reset),
                window_started_at: start.map(at),
            },
        }
    }
    fn bucket(hour: &str, millions: u64) -> Bucket {
        Bucket {
            profile: "p".into(),
            hour: at(hour),
            model: "claude-opus-5".into(),
            speed: None,
            requests: 1,
            input: millions * 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        }
    }
    fn hourly(generated: &str, rows: Vec<Bucket>) -> Hourly {
        Hourly {
            version: 1,
            generated_at: at(generated),
            rows,
        }
    }
    fn estimate(
        rows: &[LimitRow],
        usage: &Hourly,
        resets: &[billing::LimitReset],
    ) -> CapacityReport {
        capacity("p", rows, usage, &rates::seed(), resets)
    }
    #[test]
    fn recorded_date_reset_splits_a_week_into_a_lower_range() {
        // Known-bad: reading a reset week as one allowance roughly doubles capacity.
        let reset = billing::parse_reset_bracket(
            Some("2030-01-04"),
            at("2030-01-07T12:00:00Z"),
            FixedOffset::west_opt(3 * 3600).unwrap(),
        )
        .unwrap();
        let rows = [
            row("2030-01-03T00:00:00Z", "2030-01-08T00:00:00Z", 100.0, None),
            row("2030-01-06T00:00:00Z", "2030-01-08T00:00:00Z", 60.0, None),
        ];
        let usage = hourly(
            "2030-01-07T00:00:00Z",
            vec![
                bucket("2030-01-02T00:00:00Z", 1),
                bucket("2030-01-05T00:00:00Z", 1),
                bucket("2030-01-06T00:00:00Z", 1),
            ],
        );
        let result = estimate(&rows, &usage, &[reset]).estimate.unwrap();
        assert!(result.after_reset.is_some());
        assert!((result.lower - 5.0 / 0.6).abs() < 0.001);
        assert!((result.upper - 10.0 / 0.6).abs() < 0.001);
        assert!(result.upper < 15.0 / 0.6);
    }
    #[test]
    fn detects_short_reset_and_flags_long_unknown_bracket() {
        // Known-bad: ignoring a percent drop or estimating from a >48 h bracket.
        let short = [
            row("2030-01-02T00:00:00Z", "2030-01-08T00:00:00Z", 80.0, None),
            row("2030-01-03T00:00:00Z", "2030-01-08T00:00:00Z", 30.0, None),
        ];
        let usage = hourly(
            "2030-01-06T00:00:00Z",
            vec![
                bucket("2030-01-02T00:00:00Z", 1),
                bucket("2030-01-03T00:00:00Z", 1),
            ],
        );
        let report = estimate(&short, &usage, &[]);
        let result = report.estimate.unwrap();
        assert!(result.after_reset.is_some());
        assert!(result.upper > result.lower);
        assert!(report.hint.is_none());
        let long = [
            row("2029-12-31T00:00:00Z", "2030-01-08T00:00:00Z", 80.0, None),
            row("2030-01-05T00:00:00Z", "2030-01-08T00:00:00Z", 30.0, None),
        ];
        let report = estimate(&long, &usage, &[]);
        assert!(report.estimate.is_none());
        assert_eq!(
            report.hint,
            Some((at("2029-12-31T00:00:00Z"), at("2030-01-05T00:00:00Z")))
        );
    }
    #[test]
    fn recorded_and_detected_reset_merge_to_the_intersection() {
        // Known-bad: splitting one recorded and detected event twice.
        let rows = [
            row("2030-01-02T00:00:00Z", "2030-01-08T00:00:00Z", 80.0, None),
            row("2030-01-04T00:00:00Z", "2030-01-08T00:00:00Z", 30.0, None),
        ];
        let point = at("2030-01-03T12:00:00Z");
        let reset = billing::LimitReset {
            from: point,
            to: point,
        };
        let usage = hourly(
            "2030-01-05T00:00:00Z",
            vec![
                bucket("2030-01-03T13:00:00Z", 1),
                bucket("2030-01-04T00:00:00Z", 1),
            ],
        );
        let result = estimate(&rows, &usage, &[reset]).estimate.unwrap();
        assert_eq!(result.after_reset, Some(point));
        assert!((result.lower - 10.0 / 0.3).abs() < 0.001);
        assert_eq!(result.lower, result.upper);
    }
    #[test]
    fn snapshot_strictly_inside_recorded_day_has_no_segment() {
        // Known-bad: assigning an ambiguous snapshot to either side of a reset day.
        let reset = billing::parse_reset_bracket(
            Some("2030-01-04"),
            at("2030-01-07T12:00:00Z"),
            FixedOffset::west_opt(3 * 3600).unwrap(),
        )
        .unwrap();
        let rows = [row(
            "2030-01-04T12:00:00Z",
            "2030-01-08T00:00:00Z",
            70.0,
            None,
        )];
        assert!(
            estimate(&rows, &hourly("2030-01-07T00:00:00Z", vec![]), &[reset])
                .estimate
                .is_none()
        );
    }
    #[test]
    fn breakdown_start_wins_and_hour_before_it_is_excluded() {
        // Known-bad: using reset minus seven days despite window_started_at, or excluding its first hour.
        let rows = [row(
            "2030-01-03T00:00:00Z",
            "2030-01-08T00:00:00Z",
            50.0,
            Some("2030-01-02T00:00:00Z"),
        )];
        let usage = hourly(
            "2030-01-04T00:00:00Z",
            vec![
                bucket("2030-01-01T23:00:00Z", 1),
                bucket("2030-01-02T00:00:00Z", 1),
            ],
        );
        assert_eq!(estimate(&rows, &usage, &[]).estimate.unwrap().lower, 10.0);
        let fallback = [row(
            "2030-01-03T00:00:00Z",
            "2030-01-08T00:00:00Z",
            50.0,
            None,
        )];
        assert_eq!(
            estimate(&fallback, &usage, &[]).estimate.unwrap().lower,
            20.0
        );
    }
    #[test]
    fn caught_up_snapshot_remains_usable_when_live_one_is_newer() {
        // Known-bad: a too-new live snapshot blocks an older ingested snapshot in the same segment.
        let rows = [
            row("2030-01-03T00:00:00Z", "2030-01-08T00:00:00Z", 50.0, None),
            row("2030-01-04T00:00:00Z", "2030-01-08T00:00:00Z", 60.0, None),
        ];
        let usage = hourly(
            "2030-01-03T12:00:00Z",
            vec![bucket("2030-01-02T00:00:00Z", 1)],
        );
        let report = estimate(&rows, &usage, &[]);
        assert_eq!(
            report.estimate.unwrap().snapshot.fetched_at,
            at("2030-01-03T00:00:00Z")
        );
        let only_new = estimate(&rows[1..], &usage, &[]);
        assert!(only_new.estimate.is_none());
        assert_eq!(only_new.reason, "waiting for an ingest");
    }
    #[test]
    fn newest_four_estimates_exclude_usage_after_snapshot_hour() {
        // Known-bads: counting later buckets, using only the newest range, or starting the min at zero.
        let mut rows = Vec::new();
        let mut buckets = Vec::new();
        for week in 0..5 {
            let reset = at("2030-01-08T00:00:00Z") + Duration::days(7 * week);
            let fetched = reset - Duration::days(1);
            rows.push(LimitRow {
                profile: "p".into(),
                fetched_at: fetched,
                weekly: Weekly {
                    percent: 50.0,
                    resets_at: reset,
                    window_started_at: None,
                },
            });
            buckets.push(Bucket {
                hour: fetched,
                input: (week as u64 + 1) * 1_000_000,
                ..bucket("2030-01-01T00:00:00Z", 1)
            });
        }
        buckets.push(bucket("2030-02-07T00:00:00Z", 100));
        let usage = Hourly {
            version: 1,
            generated_at: at("2030-02-07T12:00:00Z"),
            rows: buckets,
        };
        let result = estimate(&rows, &usage, &[]).estimate.unwrap();
        assert_eq!(result.lower, 50.0);
        assert_eq!(result.min, 20.0);
        assert_eq!(result.max, 50.0);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::usage::ledger::{Source, Store};
    use std::fs;
    #[test]
    fn rollup_from_deduplicated_ledger_and_incremental_ingest() {
        // Known-bad: summing raw transcript records instead of the deduplicated ledger.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let project = profile.join("projects/demo");
        fs::create_dir_all(&project).unwrap();
        let record = |id: &str| {
            serde_json::json!({"type":"assistant","timestamp":"2030-01-01T12:05:00Z","sessionId":"s",
          "requestId":format!("req-{id}"),"message":{"id":format!("msg-{id}"),"model":"claude-opus-5","usage":{"input_tokens":10}}}).to_string()
        };
        fs::write(
            project.join("a.jsonl"),
            format!("{}\n{}\n", record("1"), record("1")),
        )
        .unwrap();
        let store = Store::new(
            tmp.path().join("usage"),
            vec![Source {
                profile: "p".into(),
                directory: profile,
            }],
        );
        store.ingest().unwrap();
        let first = read(&store.dir).unwrap();
        assert_eq!(first.rows.len(), 1);
        assert_eq!(first.rows[0].requests, 1);
        assert_eq!(first.rows[0].input, 10);
        fs::write(
            project.join("a.jsonl"),
            format!("{}\n{}\n{}\n", record("1"), record("1"), record("2")),
        )
        .unwrap();
        store.ingest().unwrap();
        let second = read(&store.dir).unwrap();
        assert_eq!(second.rows[0].requests, 2);
        assert_eq!(second.rows[0].input, 20);
    }
    #[test]
    fn history_deduplicates_and_never_writes_claude_json() {
        // Known-bad: appending the same snapshot twice or rewriting the source config.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        fs::create_dir_all(&profile).unwrap();
        let source = profile.join(".claude.json");
        let data=serde_json::json!({"oauthAccount":{"accountUuid":"00000000-0000-4000-8000-000000000001"},
          "cachedUsageUtilization":{"accountUuid":"00000000-0000-4000-8000-000000000001","fetchedAtMs":1893456000000_i64,
          "utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":50,
          "resets_at":"2030-01-08T00:00:00Z"}],
          "seven_day_breakdown":{"window_started_at":"2030-01-01T00:00:00Z"}}}}).to_string();
        fs::write(&source, &data).unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        let dir = tmp.path().join("usage");
        fs::create_dir(&dir).unwrap();
        let sources = vec![Source {
            profile: "p".into(),
            directory: profile,
        }];
        append_history(&dir, &sources).unwrap();
        append_history(&dir, &sources).unwrap();
        assert_eq!(history(&dir).len(), 1);
        assert_eq!(
            history(&dir)[0].weekly.window_started_at,
            Some(
                DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
                    .unwrap()
                    .to_utc()
            )
        );
        let mismatch = tmp.path().join("mismatch");
        fs::create_dir_all(&mismatch).unwrap();
        let bad = data.replacen(
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-000000000002",
            1,
        );
        fs::write(mismatch.join(".claude.json"), bad).unwrap();
        append_history(
            &dir,
            &[Source {
                profile: "other".into(),
                directory: mismatch,
            }],
        )
        .unwrap();
        assert_eq!(history(&dir).len(), 1); // Known-bad: saving an account-mismatched snapshot.
        assert_eq!(fs::read_to_string(&source).unwrap(), data);
        assert_eq!(fs::metadata(&source).unwrap().modified().unwrap(), modified);
    }
}
