//! Claude Code's cached plan limits. This module never writes to a profile.

use crate::profile::describe_age;
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

#[cfg(test)]
thread_local! {
    static READ_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SLEEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn reset_read_stats() {
    READ_ATTEMPTS.set(0);
    SLEEPS.set(0);
}

#[cfg(test)]
pub fn read_stats() -> (usize, usize) {
    (READ_ATTEMPTS.get(), SLEEPS.get())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowGroup {
    Session,
    Weekly,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub group: WindowGroup,
    pub kind: String,
    pub percent: f64,
    pub severity: Option<String>,
    pub resets_at: Option<DateTime<Utc>>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub fetched_at: DateTime<Utc>,
    pub windows: Vec<Window>,
    pub window_started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Limits {
    Snapshot(Snapshot),
    NoSnapshot,
    AccountMismatch,
    Unreadable,
}

pub fn parse_limits(claude_json: &Value) -> Limits {
    let Some(cache) = claude_json.get("cachedUsageUtilization") else {
        return Limits::NoSnapshot;
    };
    let cached_account = cache.get("accountUuid");
    let current_account = claude_json
        .get("oauthAccount")
        .and_then(|account| account.get("accountUuid"));
    if let (Some(cached), Some(current)) = (cached_account, current_account)
        && cached != current
    {
        return Limits::AccountMismatch;
    }

    let Some(fetched_at) = cache
        .get("fetchedAtMs")
        .and_then(Value::as_i64)
        .and_then(DateTime::<Utc>::from_timestamp_millis)
    else {
        return Limits::Unreadable;
    };
    let Some(utilization) = cache.get("utilization") else {
        return Limits::Unreadable;
    };

    let source_windows = utilization.get("limits").and_then(Value::as_array);
    let mut windows = source_windows
        .map(|items| items.iter().filter_map(parse_window).collect::<Vec<_>>())
        .unwrap_or_default();
    if source_windows.is_none_or(Vec::is_empty) {
        for (key, group, kind) in [
            ("five_hour", WindowGroup::Session, "session"),
            ("seven_day", WindowGroup::Weekly, "weekly_all"),
        ] {
            if let Some(source) = utilization.get(key)
                && let Some(percent) = source.get("utilization").and_then(Value::as_f64)
            {
                windows.push(Window {
                    group,
                    kind: kind.to_string(),
                    percent,
                    severity: None,
                    resets_at: parse_reset(source.get("resets_at")),
                    is_active: None,
                });
            }
        }
    }
    Limits::Snapshot(Snapshot {
        fetched_at,
        windows,
        window_started_at: parse_reset(
            utilization
                .get("seven_day_breakdown")
                .and_then(|breakdown| breakdown.get("window_started_at")),
        ),
    })
}

fn parse_window(value: &Value) -> Option<Window> {
    let percent = value.get("percent")?.as_f64()?;
    let kind = value.get("kind")?.as_str()?.to_string();
    let group = match value.get("group").and_then(Value::as_str) {
        Some("session") => WindowGroup::Session,
        Some("weekly") => WindowGroup::Weekly,
        Some(other) => WindowGroup::Other(other.to_string()),
        None if kind == "session" => WindowGroup::Session,
        None if kind.starts_with("weekly_") => WindowGroup::Weekly,
        None => WindowGroup::Other(String::new()),
    };
    Some(Window {
        group,
        kind,
        percent,
        severity: value
            .get("severity")
            .and_then(Value::as_str)
            .map(str::to_string),
        resets_at: parse_reset(value.get("resets_at")),
        is_active: value.get("is_active").and_then(Value::as_bool),
    })
}

fn parse_reset(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value?
        .as_str()
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

/// A read can race Claude Code's replacement of `.claude.json`. Retry once.
pub fn read_claude_json(profile_dir: &Path) -> Result<Option<Value>, ()> {
    let path = profile_dir.join(".claude.json");
    if !path.exists() {
        return Ok(None);
    }
    for attempt in 0..2 {
        if let Ok(json) = read_json_attempt(&path) {
            return Ok(Some(json));
        }
        if attempt == 0 {
            #[cfg(test)]
            SLEEPS.set(SLEEPS.get() + 1);
            thread::sleep(Duration::from_millis(50));
        }
    }
    Err(())
}

/// Status-line reads are time bounded by making one filesystem and parse attempt.
pub fn read_claude_json_once(profile_dir: &Path) -> Result<Option<Value>, ()> {
    let path = profile_dir.join(".claude.json");
    if !path.exists() {
        return Ok(None);
    }
    read_json_attempt(&path).map(Some)
}

fn read_json_attempt(path: &Path) -> Result<Value, ()> {
    #[cfg(test)]
    READ_ATTEMPTS.set(READ_ATTEMPTS.get() + 1);
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(())
}

#[cfg(test)]
pub fn read_limits(profile_dir: &Path) -> Limits {
    match read_claude_json(profile_dir) {
        Ok(Some(json)) => parse_limits(&json),
        _ => Limits::Unreadable,
    }
}

impl Snapshot {
    pub fn age(&self, now: DateTime<Utc>) -> String {
        let seconds = now
            .signed_duration_since(self.fetched_at)
            .num_seconds()
            .max(0) as u64;
        describe_age(seconds)
    }

    pub fn session(&self) -> Option<&Window> {
        self.windows
            .iter()
            .find(|window| window.group == WindowGroup::Session)
    }

    pub fn weekly(&self) -> Option<&Window> {
        self.windows
            .iter()
            .find(|window| window.group == WindowGroup::Weekly && window.kind == "weekly_all")
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|window| window.group == WindowGroup::Weekly)
            })
    }
}

impl Window {
    pub fn rolled_over(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_some_and(|reset| reset < now)
    }

    pub fn percent_label(&self, now: DateTime<Utc>) -> String {
        if self.rolled_over(now) {
            "reset".to_string()
        } else {
            format!("{:.0}%", self.percent.round())
        }
    }

    pub fn bar(&self) -> String {
        let full = (self.percent.clamp(0.0, 100.0) / 10.0).round() as usize;
        format!("{}{}", "█".repeat(full), "░".repeat(10 - full))
    }

    pub fn flagged(&self) -> bool {
        self.severity
            .as_deref()
            .is_some_and(|level| level != "normal")
    }

    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "session" => "Session (5h)".to_string(),
            "weekly_all" => "Weekly (7d)".to_string(),
            other => other.replace('_', " "),
        }
    }

    pub fn reset_label(&self, now: DateTime<Utc>) -> String {
        let Some(reset) = self.resets_at else {
            return "reset time unknown".to_string();
        };
        let local = reset.with_timezone(&Local).format("%Y-%m-%d %H:%M");
        if reset < now {
            format!("reset {local}")
        } else {
            let seconds = reset.signed_duration_since(now).num_seconds().max(0) as u64;
            let days = seconds / 86_400;
            let hours = (seconds % 86_400) / 3_600;
            let minutes = (seconds % 3_600) / 60;
            let until = if days > 0 {
                format!("{days} d {hours} h")
            } else if hours > 0 {
                format!("{hours} h {minutes} min")
            } else {
                format!("{minutes} min")
            };
            format!("resets {local} (in {until})")
        }
    }
}

pub fn format_info(limits: &Limits, now: DateTime<Utc>) -> String {
    match limits {
        Limits::NoSnapshot => {
            "Plan limits: Claude Code hasn't cached one for this profile yet.\n".to_string()
        }
        Limits::AccountMismatch => {
            "Plan limits: the snapshot belongs to another account, so it's hidden.\n".to_string()
        }
        Limits::Unreadable => "Plan limits: the file couldn't be read.\n".to_string(),
        Limits::Snapshot(snapshot) => {
            let mut output = format!(
                "Plan limits (Claude Code's snapshot, fetched {}):\n",
                snapshot.age(now)
            );
            if snapshot.windows.is_empty() {
                output.push_str("  No limit windows in this snapshot.\n");
            }
            for window in &snapshot.windows {
                let value = if window.rolled_over(now) {
                    format!(
                        "reset since the snapshot (was {:.0}%)",
                        window.percent.round()
                    )
                } else {
                    format!("{}  {}", window.percent_label(now), window.bar())
                };
                let severity = if window.flagged() {
                    format!("  {}", window.severity.as_deref().unwrap_or_default())
                } else {
                    String::new()
                };
                output.push_str(&format!(
                    "  {:<14} {}  {}{}\n",
                    window.label(),
                    value,
                    window.reset_label(now),
                    severity
                ));
            }
            output
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    const ACCOUNT: &str = "00000000-0000-4000-8000-000000000001";
    const OTHER_ACCOUNT: &str = "00000000-0000-4000-8000-000000000002";

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cache() -> Value {
        json!({
            "oauthAccount": {"accountUuid": ACCOUNT},
            "cachedUsageUtilization": {
                "fetchedAtMs": 1894021200000_i64,
                "accountUuid": ACCOUNT,
                "utilization": {
                    "five_hour": {"utilization": 12, "resets_at": "2030-01-07T15:00:00.000001+00:00"},
                    "seven_day": {"utilization": 88, "resets_at": "2030-01-10T20:00:00.000002+00:00"},
                    "nimbus_quill": {"utilization": 0, "resets_at": null},
                    "limits": [
                        {"kind": "session", "group": "session", "percent": 12,
                         "severity": "normal", "resets_at": "2030-01-07T15:00:00.000001+00:00",
                         "scope": null, "is_active": false},
                        {"kind": "weekly_all", "group": "weekly", "percent": 88,
                         "severity": "critical", "resets_at": "2030-01-10T20:00:00.000002+00:00",
                         "scope": null, "is_active": true}
                    ]
                }
            }
        })
    }

    fn snapshot(limits: Limits) -> Snapshot {
        let Limits::Snapshot(snapshot) = limits else {
            panic!("expected snapshot");
        };
        snapshot
    }

    #[test]
    fn weekly_breakdown_start_is_optional_and_malformed_values_are_ignored() {
        // Known-bad: ignoring window_started_at or failing a snapshot when it is malformed.
        let mut value = cache();
        value["cachedUsageUtilization"]["utilization"]["seven_day_breakdown"] =
            json!({"window_started_at":"2030-01-03T20:00:00Z", "other":123});
        assert_eq!(
            snapshot(parse_limits(&value)).window_started_at,
            Some(at("2030-01-03T20:00:00Z"))
        );
        value["cachedUsageUtilization"]["utilization"]["seven_day_breakdown"]["window_started_at"] =
            json!("bad");
        assert_eq!(snapshot(parse_limits(&value)).window_started_at, None);
    }

    #[test]
    fn current_shape_uses_limits_and_keeps_severity_and_microseconds() {
        // Known-bad: reading only five_hour/seven_day loses severity and is_active.
        let result = snapshot(parse_limits(&cache()));
        assert_eq!(result.fetched_at.timestamp_millis(), 1_894_021_200_000);
        assert_eq!(result.windows.len(), 2);
        assert_eq!(result.session().unwrap().percent, 12.0);
        assert_eq!(result.weekly().unwrap().percent, 88.0);
        assert_eq!(
            result.weekly().unwrap().severity.as_deref(),
            Some("critical")
        );
        assert_eq!(result.weekly().unwrap().is_active, Some(true));
        assert_eq!(
            result.weekly().unwrap().resets_at,
            Some(at("2030-01-10T20:00:00.000002+00:00"))
        );
    }

    #[test]
    fn old_numeric_fields_work_without_limits_array() {
        // Known-bad: requiring limits[] leaves a valid older snapshot empty.
        let mut value = cache();
        value["cachedUsageUtilization"]["utilization"]
            .as_object_mut()
            .unwrap()
            .remove("limits");
        let result = snapshot(parse_limits(&value));
        assert_eq!(result.session().unwrap().percent, 12.0);
        assert_eq!(result.weekly().unwrap().percent, 88.0);
        value["cachedUsageUtilization"]["utilization"]["limits"] = json!([]);
        assert_eq!(snapshot(parse_limits(&value)).windows.len(), 2);
    }

    #[test]
    fn a_present_invalid_limits_array_skips_bad_percent_without_fallback() {
        // Known-bad: falling back after filtering a present limits[] revives older values.
        let mut value = cache();
        value["cachedUsageUtilization"]["utilization"]["limits"] = json!([
            {"kind":"session", "group":"session", "percent":"unknown"}
        ]);
        assert!(snapshot(parse_limits(&value)).windows.is_empty());
    }

    #[test]
    fn unknown_fields_and_prior_weekly_kind_do_not_change_seven_day() {
        // Known-bad: strict typed parsing rejects codenames, or first-weekly selection picks opus.
        let mut value = cache();
        let usage = &mut value["cachedUsageUtilization"]["utilization"];
        usage["future_codename"] = json!({"unrelated": true});
        usage["limits"].as_array_mut().unwrap().insert(
            1,
            json!({"kind":"weekly_opus", "group":"weekly", "percent": 72,
                   "severity":"warning", "future_field": 5}),
        );
        usage["limits"].as_array_mut().unwrap().push(json!({
            "kind":"bad_percent", "group":"weekly", "percent":"secret"
        }));
        let result = snapshot(parse_limits(&value));
        assert_eq!(result.windows.len(), 3);
        assert_eq!(result.weekly().unwrap().kind, "weekly_all");
        assert!(
            format_info(&Limits::Snapshot(result), at("2030-01-07T14:00:00Z"))
                .contains("weekly opus")
        );
    }

    #[test]
    fn a_rolled_window_hides_its_old_percent() {
        // Known-bad: ignoring resets_at presents a stale percent as current.
        let result = snapshot(parse_limits(&cache()));
        let session = result.session().unwrap();
        assert_eq!(session.percent_label(at("2030-01-07T14:00:00Z")), "12%");
        assert_eq!(session.percent_label(at("2030-01-07T16:00:00Z")), "reset");
        let info = format_info(&Limits::Snapshot(result), at("2030-01-07T16:00:00Z"));
        assert!(info.contains("reset since the snapshot (was 12%)"));
    }

    #[test]
    fn account_mismatch_hides_all_numbers() {
        // Known-bad: skipping the account check can show another account's usage.
        let mut value = cache();
        value["cachedUsageUtilization"]["accountUuid"] = json!(OTHER_ACCOUNT);
        let limits = parse_limits(&value);
        assert_eq!(limits, Limits::AccountMismatch);
        let info = format_info(&limits, at("2030-01-07T14:00:00Z"));
        assert!(info.contains("another account"));
        assert!(!info.contains("88%"));
        assert!(!info.contains(OTHER_ACCOUNT));
    }

    #[test]
    fn missing_key_is_no_data_and_truncated_json_is_unreadable() {
        // Known-bad: unwrap on truncated JSON panics, or an error echoes file content.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".claude.json");
        fs::write(&path, b"{\"other\":true}").unwrap();
        assert_eq!(read_limits(tmp.path()), Limits::NoSnapshot);
        fs::write(&path, b"{\"private_canary\":").unwrap();
        let limits = read_limits(tmp.path());
        assert_eq!(limits, Limits::Unreadable);
        let info = format_info(&limits, at("2030-01-07T14:00:00Z"));
        assert!(info.contains("couldn't be read"));
        assert!(!info.contains("private_canary"));
    }

    #[test]
    fn reading_a_profile_keeps_its_bytes_and_mtime() {
        // Known-bad: a read helper that normalizes or rewrites .claude.json.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".claude.json");
        let bytes = serde_json::to_vec(&cache()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(matches!(read_limits(tmp.path()), Limits::Snapshot(_)));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn absent_account_id_does_not_suppress_a_snapshot() {
        // Known-bad: treating a missing comparison field as a mismatch hides valid data.
        let mut value = cache();
        value["oauthAccount"]
            .as_object_mut()
            .unwrap()
            .remove("accountUuid");
        assert!(matches!(parse_limits(&value), Limits::Snapshot(_)));
    }

    #[test]
    fn printed_percent_is_unclamped_while_the_bar_is_bounded() {
        // Known-bad: clamping the visible percent or letting the bar exceed ten cells.
        let mut result = snapshot(parse_limits(&cache()));
        result.windows[0].percent = 143.6;
        assert_eq!(
            result.windows[0].percent_label(at("2030-01-07T14:00:00Z")),
            "144%"
        );
        assert_eq!(result.windows[0].bar(), "██████████");
        result.windows[0].percent = -4.0;
        assert_eq!(result.windows[0].bar(), "░░░░░░░░░░");
    }
}
