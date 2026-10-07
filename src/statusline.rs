//! Claude Code's one-line, read-only status display.

use crate::key;
use crate::limits::{self, Limits, Snapshot, read_claude_json_once};
use crate::profile::{self, ProfileManager, Tool};
use crate::usage::{attribute, metrics};
use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde_json::Value;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct DiskData {
    account: String,
    project: Option<String>,
    project_short: Option<String>,
    per_token: bool,
    snapshot: Option<Snapshot>,
    headroom: Option<Headroom>,
}

#[derive(Clone, Debug)]
struct Headroom {
    name: String,
    five_percent: f64,
    five_reset: bool,
    weekly_percent: Option<f64>,
}

#[derive(Clone, Debug)]
struct PlanWindow {
    percent: f64,
    reset: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
struct WindowText {
    base: String,
    reset: Option<String>,
}

#[derive(Clone, Debug)]
struct HeadroomText {
    base: String,
    weekly: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Action {
    Install { no_refresh: bool, force: bool },
    Uninstall,
}

pub fn manage(
    manager: &ProfileManager,
    name: Option<&str>,
    all: bool,
    action: Action,
    executable: &Path,
    now: DateTime<Utc>,
) -> Result<(String, bool)> {
    let command = match action {
        Action::Install { no_refresh, .. } => {
            let executable =
                fs::canonicalize(executable).context("Cannot resolve cswitch executable")?;
            Some(format!(
                "{} statusline{}",
                profile::shell_quote(&executable.to_string_lossy()),
                if no_refresh { " --no-refresh" } else { "" }
            ))
        }
        Action::Uninstall => None,
    };
    let names = if all {
        manager
            .list_profiles()?
            .into_iter()
            .filter(|profile| profile.tool == Tool::Claude)
            .map(|profile| profile.name)
            .collect::<Vec<_>>()
    } else {
        name.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    let mut output = String::new();
    let mut failed = false;
    for name in names {
        let force = matches!(action, Action::Install { force: true, .. });
        match key::edit_statusline(manager, &name, command.as_deref(), force, now) {
            Ok(changed) => {
                let phrase = match (action, changed) {
                    (Action::Install { .. }, true) => "installed",
                    (Action::Install { .. }, false) => "already installed",
                    (Action::Uninstall, true) => "removed",
                    (Action::Uninstall, false) => "nothing to remove",
                };
                output.push_str(&format!("{name}: {phrase}\n"));
            }
            Err(error) => {
                failed = true;
                output.push_str(&format!("{name}: {error}\n"));
            }
        }
    }
    Ok((output, failed))
}

pub fn command_line() -> String {
    let input = if io::stdin().is_terminal() {
        Value::Null
    } else {
        let mut bytes = Vec::new();
        let _ = io::stdin().take(1_048_576).read_to_end(&mut bytes);
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from);
    let usage_dir = std::env::var_os("CSWITCH_USAGE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude-switch/usage"));
    let local_now = Local::now();
    let now = local_now.with_timezone(&Utc);
    let disk = read_disk(&input, &home, config_dir.as_deref(), &usage_dir, now);
    let columns = std::env::var("COLUMNS")
        .ok()
        .and_then(|raw| raw.parse().ok());
    let colour = std::env::var_os("NO_COLOR").is_none_or(|raw| raw.is_empty());
    render(
        &input,
        &disk,
        columns,
        colour,
        now,
        local_now.offset().fix(),
    )
}

pub fn read_disk(
    input: &Value,
    home: &Path,
    configured: Option<&Path>,
    usage_dir: &Path,
    now: DateTime<Utc>,
) -> DiskData {
    let base = home.join(".claude-switch");
    let manager = ProfileManager::with_paths_read_only(base, home.join(".claude")).ok();
    let default_config_dir = home.join(".claude");
    let config_dir = configured.unwrap_or(&default_config_dir);
    let profiles = manager
        .as_ref()
        .and_then(|manager| manager.list_profiles().ok())
        .unwrap_or_default();
    let registered = configured.and_then(|dir| {
        profiles.iter().find(|profile| {
            profile.tool == Tool::Claude
                && manager
                    .as_ref()
                    .is_some_and(|manager| manager.profile_dir(&profile.name) == dir)
        })
    });
    let account = if configured.is_none() {
        "default".to_string()
    } else if let Some(profile) = registered {
        profile.name.clone()
    } else {
        config_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "cswitch".to_string())
    };
    let claude = read_claude_json_once(config_dir);
    let per_token = registered.is_some_and(|profile| {
        manager.as_ref().is_some_and(|manager| {
            key::read_auth_mode(manager, &profile.name, claude.clone()).api_billed()
        })
    });
    let snapshot = if !per_token
        && input
            .get("rate_limits")
            .and_then(Value::as_object)
            .is_none()
    {
        claude
            .ok()
            .flatten()
            .and_then(|json| match limits::parse_limits(&json) {
                Limits::Snapshot(snapshot) => Some(snapshot),
                _ => None,
            })
    } else {
        None
    };
    let (five, seven) = current_windows(input, snapshot.as_ref(), now);
    let needs_headroom = [five.as_ref(), seven.as_ref()]
        .into_iter()
        .flatten()
        .any(|window| window.percent >= 80.0);
    let headroom = if needs_headroom {
        manager.as_ref().and_then(|manager| {
            profiles
                .iter()
                .filter(|profile| {
                    profile.tool == Tool::Claude
                        && registered.is_none_or(|current| current.name != profile.name)
                })
                .filter_map(|profile| candidate(manager, &profile.name, now))
                .min_by(|a, b| {
                    a.five_percent
                        .total_cmp(&b.five_percent)
                        .then_with(|| a.name.cmp(&b.name))
                })
        })
    } else {
        None
    };
    let cwd = input
        .get("workspace")
        .and_then(|workspace| workspace.get("current_dir"))
        .and_then(Value::as_str)
        .or_else(|| input.get("cwd").and_then(Value::as_str));
    let (project, project_short) = match (attribute::load_config(usage_dir), cwd) {
        (Ok(config), Some(cwd)) => {
            let cwd = Path::new(cwd);
            if let Some((_, raw)) = attribute::project_for_path(cwd, &config) {
                let project = config
                    .aliases
                    .get(&raw)
                    .map_or(raw.as_str(), String::as_str);
                let (full, short) = project_display(project);
                (Some(full), Some(short))
            } else {
                let workspace = attribute::workspace_for_path(cwd, &config);
                (workspace.clone(), workspace)
            }
        }
        _ => (None, None),
    };
    DiskData {
        account: clean(&account).trim().to_string(),
        project: project
            .map(|value| clean(&value).trim().to_string())
            .filter(|value| !value.is_empty()),
        project_short: project_short
            .map(|value| clean(&value).trim().to_string())
            .filter(|value| !value.is_empty()),
        per_token,
        snapshot,
        headroom,
    }
}

fn candidate(manager: &ProfileManager, name: &str, now: DateTime<Utc>) -> Option<Headroom> {
    let dir = manager.profile_dir(name);
    let claude = read_claude_json_once(&dir);
    if key::read_auth_mode(manager, name, claude.clone()) != key::AuthMode::Subscription {
        return None;
    }
    let Limits::Snapshot(snapshot) = limits::parse_limits(&claude.ok()??) else {
        return None;
    };
    let five = snapshot.session()?;
    let five_reset = five.rolled_over(now);
    let weekly_percent = snapshot
        .weekly()
        .filter(|window| !window.rolled_over(now) && window.percent >= 80.0)
        .map(|window| window.percent);
    Some(Headroom {
        name: clean(name),
        five_percent: if five_reset { 0.0 } else { five.percent },
        five_reset,
        weekly_percent,
    })
}

fn project_display(project: &str) -> (String, String) {
    if let Some((workspace, name)) = project.split_once('/') {
        if workspace == name {
            (name.to_string(), name.to_string())
        } else {
            (format!("{workspace} › {name}"), name.to_string())
        }
    } else {
        (project.to_string(), project.to_string())
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn stdin_window(input: &Value, key: &str) -> Option<PlanWindow> {
    let window = input.get("rate_limits")?.get(key)?;
    let percent = window.get("used_percentage")?.as_f64()?;
    if !percent.is_finite() {
        return None;
    }
    let reset = window
        .get("resets_at")
        .and_then(Value::as_i64)
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
    Some(PlanWindow { percent, reset })
}

fn current_windows(
    input: &Value,
    snapshot: Option<&Snapshot>,
    now: DateTime<Utc>,
) -> (Option<PlanWindow>, Option<PlanWindow>) {
    if input
        .get("rate_limits")
        .and_then(Value::as_object)
        .is_some()
    {
        return (
            stdin_window(input, "five_hour"),
            stdin_window(input, "seven_day"),
        );
    }
    let from_snapshot = |window: Option<&limits::Window>| {
        window
            .filter(|window| !window.rolled_over(now) && window.percent.is_finite())
            .map(|window| PlanWindow {
                percent: window.percent,
                reset: window.resets_at,
            })
    };
    (
        from_snapshot(snapshot.and_then(Snapshot::session)),
        from_snapshot(snapshot.and_then(Snapshot::weekly)),
    )
}

fn reset_text(
    reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> Option<String> {
    let reset = reset.filter(|reset| *reset > now)?;
    let seconds = (reset - now).num_seconds();
    if seconds < 3_600 {
        Some(format!("↻{}m", seconds / 60))
    } else if seconds < 86_400 {
        Some(format!(
            "↻{}h{:02}",
            seconds / 3_600,
            (seconds % 3_600) / 60
        ))
    } else {
        Some(format!("↻{}", reset.with_timezone(&offset).format("%a")))
    }
}

fn colour_token(token: String, percent: f64, colour: bool) -> String {
    if !colour || percent < 80.0 {
        token
    } else {
        let code = if percent >= 95.0 { 31 } else { 33 };
        format!("\x1b[{code}m{token}\x1b[0m")
    }
}

fn window_text(
    label: &str,
    window: PlanWindow,
    colour: bool,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> WindowText {
    let percent = colour_token(
        format!("{:.0}%", window.percent.round()),
        window.percent,
        colour,
    );
    WindowText {
        base: format!("{label} {percent}"),
        reset: reset_text(window.reset, now, offset),
    }
}

fn context_text(input: &Value, colour: bool) -> Option<String> {
    let count = input
        .get("context_window")?
        .get("total_input_tokens")?
        .as_f64()?;
    if !count.is_finite() || count <= 0.0 {
        return None;
    }
    let count = count.round() as u64;
    let label = if count < 1_000 {
        count.to_string()
    } else if count < 1_000_000 && count.saturating_add(500) / 1_000 < 1_000 {
        format!("{}k", count.saturating_add(500) / 1_000)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    };
    let token = format!("ctx {label}");
    if colour && input.get("exceeds_200k_tokens").and_then(Value::as_bool) == Some(true) {
        Some(format!("\x1b[33m{token}\x1b[0m"))
    } else {
        Some(token)
    }
}

fn visible_len(value: &str) -> usize {
    let mut chars = value.chars();
    let mut length = 0;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.next() == Some('[') {
            for tail in chars.by_ref() {
                if tail == 'm' {
                    break;
                }
            }
        } else {
            length += 1;
        }
    }
    length
}

fn compose(
    account: &str,
    project: &Option<String>,
    five: &Option<WindowText>,
    seven: &Option<WindowText>,
    headroom: &Option<HeadroomText>,
    ctx: &Option<String>,
    chat: &Option<String>,
) -> String {
    let mut parts = vec![account.to_string()];
    if let Some(project) = project {
        parts.push(project.clone());
    }
    for window in [five, seven].into_iter().flatten() {
        let mut text = window.base.clone();
        if let Some(reset) = &window.reset {
            text.push_str(&format!(" {reset}"));
        }
        parts.push(text);
    }
    if let Some(headroom) = headroom {
        let mut text = headroom.base.clone();
        if let Some(weekly) = &headroom.weekly {
            text.push_str(&format!(" {weekly}"));
        }
        parts.push(text);
    }
    if let Some(ctx) = ctx {
        parts.push(ctx.clone());
    }
    if let Some(chat) = chat {
        parts.push(chat.clone());
    }
    parts.join(" · ")
}

/// Render entirely from supplied values. No file, environment or process access occurs here.
pub fn render(
    input: &Value,
    disk: &DiskData,
    columns: Option<usize>,
    colour: bool,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> String {
    let budget = columns
        .filter(|value| *value > 0)
        .unwrap_or(80)
        .saturating_sub(4)
        .max(20);
    let mut account = if disk.account.is_empty() {
        "cswitch".to_string()
    } else {
        disk.account.clone()
    };
    let mut project = disk.project.clone();
    let (five_window, seven_window) = current_windows(input, disk.snapshot.as_ref(), now);
    let mut five = five_window.map(|window| window_text("5h", window, colour, now, offset));
    let mut seven = seven_window.map(|window| window_text("7d", window, colour, now, offset));
    let mut headroom = disk.headroom.as_ref().map(|candidate| HeadroomText {
        base: format!(
            "→ {} 5h {}",
            candidate.name,
            if candidate.five_reset {
                "reset".to_string()
            } else {
                format!("{:.0}%", candidate.five_percent.round())
            }
        ),
        weekly: candidate
            .weekly_percent
            .map(|percent| format!("7d {:.0}%", percent.round())),
    });
    let mut ctx = context_text(input, colour);
    let mut chat = if disk.per_token {
        None
    } else {
        input
            .get("cost")
            .and_then(|cost| cost.get("total_cost_usd"))
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| format!("chat ~{}", metrics::money(value)))
    };
    for step in 0..11 {
        let line = compose(&account, &project, &five, &seven, &headroom, &ctx, &chat);
        if visible_len(&line) <= budget {
            return line;
        }
        match step {
            2 => chat = None,
            3 => {
                if let Some(window) = &mut seven {
                    window.reset = None;
                }
            }
            4 => {
                if let Some(window) = &mut five {
                    window.reset = None;
                }
            }
            5 => ctx = None,
            6 => project = disk.project_short.clone(),
            7 => {
                let had_weekly = headroom
                    .as_mut()
                    .and_then(|value| value.weekly.take())
                    .is_some();
                if had_weekly {
                    let shorter =
                        compose(&account, &project, &five, &seven, &headroom, &ctx, &chat);
                    if visible_len(&shorter) <= budget {
                        return shorter;
                    }
                }
                headroom = None;
            }
            8 => seven = None,
            9 => project = None,
            10 => five = None,
            _ => {} // Part B adds the first two steps.
        }
    }
    if account.chars().count() > budget {
        account = account.chars().take(budget).collect();
    }
    account
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::SystemTime;
    use tempfile::TempDir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 12, 0, 0).unwrap()
    }
    fn offset() -> FixedOffset {
        FixedOffset::west_opt(3 * 3600).unwrap()
    }
    fn disk() -> DiskData {
        DiskData {
            account: "work".into(),
            project: Some("acme › site".into()),
            project_short: Some("site".into()),
            per_token: false,
            snapshot: None,
            headroom: None,
        }
    }
    fn read_disk(
        input: &Value,
        home: &Path,
        configured: Option<&Path>,
        usage_dir: &Path,
    ) -> DiskData {
        super::read_disk(input, home, configured, usage_dir, now())
    }
    fn line(input: &Value, disk: &DiskData, columns: usize, colour: bool) -> String {
        render(input, disk, Some(columns), colour, now(), offset())
    }
    fn register(home: &Path, profiles: &[(&str, Tool)]) {
        let base = home.join(".claude-switch");
        fs::create_dir_all(base.join("profiles")).unwrap();
        let mut rows = serde_json::Map::new();
        for (name, tool) in profiles {
            fs::create_dir_all(base.join("profiles").join(name)).unwrap();
            rows.insert(
                (*name).into(),
                json!({
                    "name": name, "tool": tool, "email": null,
                    "added": "2030-01-01T00:00:00Z", "last_used": null
                }),
            );
        }
        fs::write(
            base.join("registry.json"),
            serde_json::to_vec(&json!({"profiles": rows})).unwrap(),
        )
        .unwrap();
    }
    fn full_input() -> Value {
        json!({
            "rate_limits": {
                "five_hour": {"used_percentage": 42, "resets_at": now().timestamp() + 4800},
                "seven_day": {"used_percentage": 71, "resets_at": now().timestamp() + 259200}
            },
            "context_window": {"total_input_tokens": 142000},
            "cost": {"total_cost_usd": 1.70}
        })
    }
    fn snapshot(five: f64, five_reset: i64, weekly: f64, weekly_reset: i64) -> Value {
        json!({
            "oauthAccount": {"accountUuid":"00000000-0000-4000-8000-000000000001"},
            "cachedUsageUtilization": {
                "accountUuid":"00000000-0000-4000-8000-000000000001",
                "fetchedAtMs":now().timestamp_millis(),
                "utilization": {
                    "five_hour":{"utilization":five,
                        "resets_at":DateTime::<Utc>::from_timestamp(five_reset, 0).unwrap().to_rfc3339()},
                    "seven_day":{"utilization":weekly,
                        "resets_at":DateTime::<Utc>::from_timestamp(weekly_reset, 0).unwrap().to_rfc3339()}
                }
            }
        })
    }

    #[test]
    fn statusline_reads_each_claude_file_once_without_sleeping() {
        // Known-bad: the retrying reader sleeps on an invalid own or candidate file,
        // or reads the own file again for the snapshot fallback.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        register(
            home,
            &[
                ("work", Tool::Claude),
                ("broken", Tool::Claude),
                ("spare", Tool::Claude),
            ],
        );
        let profiles = home.join(".claude-switch/profiles");
        let work = profiles.join("work");
        let usage = home.join("usage");
        fs::write(work.join(".claude.json"), b"{").unwrap();
        limits::reset_read_stats();
        let disk = read_disk(&Value::Null, home, Some(&work), &usage);
        assert_eq!(limits::read_stats(), (1, 0));
        let output = line(&Value::Null, &disk, 200, false);
        assert_eq!(output, "work");

        fs::write(profiles.join("broken/.claude.json"), b"{").unwrap();
        fs::write(
            profiles.join("spare/.claude.json"),
            serde_json::to_vec(&snapshot(
                10.0,
                now().timestamp() + 3600,
                20.0,
                now().timestamp() + 86400,
            ))
            .unwrap(),
        )
        .unwrap();
        let high = json!({"rate_limits":{"five_hour":{"used_percentage":88}}});
        limits::reset_read_stats();
        let disk = read_disk(&high, home, Some(&work), &usage);
        assert_eq!(limits::read_stats(), (3, 0));
        assert!(line(&high, &disk, 200, false).contains("→ spare 5h 10%"));

        fs::write(
            work.join(".claude.json"),
            serde_json::to_vec(&snapshot(
                42.0,
                now().timestamp() + 3600,
                71.0,
                now().timestamp() + 86400,
            ))
            .unwrap(),
        )
        .unwrap();
        limits::reset_read_stats();
        let disk = read_disk(&Value::Null, home, Some(&work), &usage);
        assert_eq!(limits::read_stats(), (1, 0));
        assert!(line(&Value::Null, &disk, 200, false).contains("5h 42%"));
    }

    #[test]
    fn per_token_profile_ignores_saved_limits_but_accepts_live_limits() {
        // Known-bad: a stale subscription snapshot follows a profile into API billing.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        register(home, &[("metered", Tool::Claude), ("spare", Tool::Claude)]);
        let profiles = home.join(".claude-switch/profiles");
        let metered = profiles.join("metered");
        let mut saved = snapshot(
            88.0,
            now().timestamp() + 3600,
            64.0,
            now().timestamp() + 86400,
        );
        saved["primaryApiKey"] = json!("synthetic-key");
        fs::write(
            metered.join(".claude.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        fs::write(
            profiles.join("spare/.claude.json"),
            serde_json::to_vec(&snapshot(
                10.0,
                now().timestamp() + 3600,
                20.0,
                now().timestamp() + 86400,
            ))
            .unwrap(),
        )
        .unwrap();
        let usage = home.join("usage");
        let output = line(
            &Value::Null,
            &read_disk(&Value::Null, home, Some(&metered), &usage),
            200,
            false,
        );
        assert_eq!(output, "metered");
        let live = json!({"rate_limits":{"five_hour":{"used_percentage":42},
            "seven_day":{"used_percentage":71}}});
        let output = line(
            &live,
            &read_disk(&live, home, Some(&metered), &usage),
            200,
            false,
        );
        assert!(
            output.contains("5h 42%") && output.contains("7d 71%"),
            "{output}"
        );
        assert!(!output.contains('→'));
    }

    #[test]
    fn bad_input_keeps_account_and_one_line() {
        // Known-bad: a parse or field error bubbles through `?` and blanks the row.
        let inputs = [
            "",
            "{",
            "{}",
            "[]",
            "null",
            r#"{"rate_limits":null,"context_window":null,"cost":null}"#,
            r#"{"rate_limits":"bad","context_window":3,"cost":[]}"#,
            r#"{"session_id":null,"transcript_path":2,"cwd":null,"workspace":[],"model":false,"cost":null,"context_window":"bad","rate_limits":null,"exceeds_200k_tokens":"bad"}"#,
        ];
        for raw in inputs {
            let parsed: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            let output = line(&parsed, &disk(), 120, false);
            assert!(output.starts_with("work"), "{raw}: {output}");
            assert_eq!(output.lines().count(), 1);
        }
    }

    #[test]
    fn account_name_uses_registered_profile_and_default() {
        // Known-bad: using the directory's basename even when registry name differs.
        let tmp = TempDir::new().unwrap();
        register(tmp.path(), &[("work", Tool::Claude)]);
        let home = tmp.path();
        let usage = home.join("usage");
        assert_eq!(
            read_disk(&Value::Null, home, None, &usage).account,
            "default"
        );
        let registered = home.join(".claude-switch/profiles/work");
        assert_eq!(
            read_disk(&Value::Null, home, Some(&registered), &usage).account,
            "work"
        );
        let trailing = PathBuf::from(format!("{}/", registered.display()));
        assert_eq!(
            read_disk(&Value::Null, home, Some(&trailing), &usage).account,
            "work"
        );
        assert_eq!(
            read_disk(
                &Value::Null,
                home,
                Some(&home.join("elsewhere/custom")),
                &usage
            )
            .account,
            "custom"
        );
    }

    #[test]
    fn project_resolution_alias_and_bad_config_are_isolated() {
        // Known-bad: bad config fails the entire line, or an alias is ignored.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("atlas");
        let site = root.join("apps/site");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(site.join(".git")).unwrap();
        fs::create_dir_all(site.join("src")).unwrap();
        let usage = tmp.path().join("usage");
        fs::create_dir_all(&usage).unwrap();
        fs::write(
            usage.join("config.json"),
            serde_json::to_vec(&json!({
                "superproject": root, "workspaces": [{"glob":"apps/*", "name":"acme"}],
                "aliases": {"acme/site":"acme/web"}
            }))
            .unwrap(),
        )
        .unwrap();
        let input = json!({"cwd": site.join("src")});
        let disk = read_disk(&input, tmp.path(), None, &usage);
        assert_eq!(disk.project.as_deref(), Some("acme › web"));
        assert_eq!(disk.project_short.as_deref(), Some("web"));
        fs::write(
            usage.join("config.json"),
            serde_json::to_vec(&json!({"superproject": root,
            "workspaces": [{"glob":"apps/*", "name":"site"}]}))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_disk(&input, tmp.path(), None, &usage)
                .project
                .as_deref(),
            Some("site")
        );
        fs::create_dir_all(root.join("docs")).unwrap();
        let root_input = json!({"cwd": root.join("docs")});
        fs::write(
            usage.join("config.json"),
            serde_json::to_vec(&json!({"superproject": root,
            "workspaces": [{"glob":"docs", "name":"acme"}]}))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_disk(&root_input, tmp.path(), None, &usage)
                .project
                .as_deref(),
            Some("acme")
        );
        fs::write(usage.join("config.json"), b"{").unwrap();
        let bad = read_disk(&input, tmp.path(), None, &usage);
        assert_eq!(bad.project, None);
        assert_eq!(
            line(&full_input(), &bad, 200, false).split(" · ").next(),
            Some("default")
        );
    }

    #[test]
    fn stdin_windows_round_without_clamping_or_past_reset() {
        // Known-bad: clamp values over 100 or show a suffix after reset.
        let value = json!({"rate_limits":{"five_hour":{"used_percentage":100.7,
            "resets_at":now().timestamp()-1},"seven_day":{"used_percentage":71.5}}});
        let output = line(&value, &disk(), 200, false);
        assert!(output.contains("5h 101%"), "{output}");
        assert!(output.contains("7d 72%"), "{output}");
        assert!(!output.contains('↻'));
        let value = json!({"rate_limits":{"five_hour":{"used_percentage":42}}});
        let output = line(&value, &disk(), 200, false);
        assert!(output.contains("5h 42%") && !output.contains("7d"));
    }

    #[test]
    fn reset_edges_use_local_weekday() {
        // Known-bad: UTC weekday or an hour boundary printed as minutes.
        let at = now();
        for (seconds, expected) in [
            (3540, "↻59m"),
            (3600, "↻1h00"),
            (86340, "↻23h59"),
            (86400, "↻Wed"),
        ] {
            let text =
                reset_text(Some(at + chrono::Duration::seconds(seconds)), at, offset()).unwrap();
            assert_eq!(text, expected);
        }
        let utc_midnight = Utc.with_ymd_and_hms(2030, 1, 3, 0, 30, 0).unwrap();
        assert_eq!(
            reset_text(Some(utc_midnight), at, offset()).as_deref(),
            Some("↻Wed")
        );
    }

    #[test]
    fn snapshot_fallback_only_when_rate_limits_object_is_absent() {
        // Known-bad: filling a missing stdin window from a snapshot, or keeping a rolled window.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        let path = home.join(".claude/.claude.json");
        let saved = snapshot(
            42.0,
            now().timestamp() + 3600,
            71.0,
            now().timestamp() + 86400,
        );
        fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let bytes = fs::read(&path).unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let usage = home.join("usage");
        let absent = read_disk(&Value::Null, home, None, &usage);
        let output = line(&Value::Null, &absent, 200, false);
        assert!(
            output.contains("5h 42%") && output.contains("7d 71%"),
            "{output}"
        );
        let partial = json!({"rate_limits":{"five_hour":{"used_percentage":15}}});
        let output = line(
            &partial,
            &read_disk(&partial, home, None, &usage),
            200,
            false,
        );
        assert!(
            output.contains("5h 15%") && !output.contains("7d"),
            "{output}"
        );
        let empty = json!({"rate_limits":{}});
        assert!(!line(&empty, &read_disk(&empty, home, None, &usage), 200, false).contains("5h"));
        let rolled = snapshot(42.0, now().timestamp() - 1, 71.0, now().timestamp() + 86400);
        fs::write(&path, serde_json::to_vec(&rolled).unwrap()).unwrap();
        let output = line(
            &Value::Null,
            &read_disk(&Value::Null, home, None, &usage),
            200,
            false,
        );
        assert!(
            !output.contains("5h") && output.contains("7d 71%"),
            "{output}"
        );
        let mut mismatch = saved.clone();
        mismatch["cachedUsageUtilization"]["accountUuid"] =
            json!("00000000-0000-4000-8000-000000000002");
        fs::write(&path, serde_json::to_vec(&mismatch).unwrap()).unwrap();
        let output = line(
            &Value::Null,
            &read_disk(&Value::Null, home, None, &usage),
            200,
            false,
        );
        assert!(!output.contains("5h") && !output.contains("7d"), "{output}");
        fs::write(&path, &bytes).unwrap();
        fs::File::open(&path).unwrap().set_modified(mtime).unwrap();
        let _ = read_disk(&Value::Null, home, None, &usage);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn headroom_requires_high_usage_and_selects_subscription_candidate() {
        // Known-bad: showing headroom below 80, offering API or Codex, or keeping a rolled value.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        register(
            home,
            &[
                ("work", Tool::Claude),
                ("spare", Tool::Claude),
                ("other", Tool::Claude),
                ("api", Tool::Claude),
                ("codex", Tool::Codex),
            ],
        );
        let dir = |name: &str| home.join(".claude-switch/profiles").join(name);
        let write = |name: &str, value: Value| {
            fs::write(
                dir(name).join(".claude.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        };
        write(
            "work",
            snapshot(
                79.0,
                now().timestamp() + 3600,
                79.0,
                now().timestamp() + 86400,
            ),
        );
        write(
            "spare",
            snapshot(
                10.0,
                now().timestamp() + 3600,
                85.0,
                now().timestamp() + 86400,
            ),
        );
        write(
            "other",
            snapshot(
                20.0,
                now().timestamp() + 3600,
                79.0,
                now().timestamp() + 86400,
            ),
        );
        let mut api = snapshot(
            0.0,
            now().timestamp() + 3600,
            10.0,
            now().timestamp() + 86400,
        );
        api["primaryApiKey"] = json!("synthetic");
        write("api", api);
        write(
            "codex",
            snapshot(
                0.0,
                now().timestamp() + 3600,
                10.0,
                now().timestamp() + 86400,
            ),
        );
        let usage = home.join("usage");
        let current = dir("work");
        let low = json!({"rate_limits":{"five_hour":{"used_percentage":79.9},
            "seven_day":{"used_percentage":79.9}}});
        assert!(
            read_disk(&low, home, Some(&current), &usage)
                .headroom
                .is_none()
        );
        for high in [
            json!({"rate_limits":{"five_hour":{"used_percentage":80}}}),
            json!({"rate_limits":{"seven_day":{"used_percentage":80}}}),
        ] {
            let output = line(
                &high,
                &read_disk(&high, home, Some(&current), &usage),
                200,
                false,
            );
            assert!(output.contains("→ spare 5h 10% 7d 85%"), "{output}");
        }
        write(
            "other",
            snapshot(
                10.0,
                now().timestamp() + 3600,
                79.0,
                now().timestamp() + 86400,
            ),
        );
        let high = json!({"rate_limits":{"five_hour":{"used_percentage":80}}});
        assert!(
            line(
                &high,
                &read_disk(&high, home, Some(&current), &usage),
                200,
                false
            )
            .contains("→ other 5h 10%")
        );
        write(
            "other",
            snapshot(88.0, now().timestamp() - 1, 79.0, now().timestamp() + 86400),
        );
        let output = line(
            &high,
            &read_disk(&high, home, Some(&current), &usage),
            200,
            false,
        );
        assert!(output.contains("→ other 5h reset"), "{output}");
        write(
            "other",
            snapshot(
                20.0,
                now().timestamp() + 3600,
                79.0,
                now().timestamp() + 86400,
            ),
        );
        write(
            "spare",
            snapshot(
                10.0,
                now().timestamp() + 3600,
                79.0,
                now().timestamp() + 86400,
            ),
        );
        let output = line(
            &high,
            &read_disk(&high, home, Some(&current), &usage),
            200,
            false,
        );
        assert!(output.contains("→ spare 5h 10%") && !output.contains("7d 79%"));
        write(
            "spare",
            snapshot(
                10.0,
                now().timestamp() + 3600,
                80.0,
                now().timestamp() + 86400,
            ),
        );
        let output = line(
            &high,
            &read_disk(&high, home, Some(&current), &usage),
            200,
            false,
        );
        assert!(output.contains("→ spare 5h 10% 7d 80%"), "{output}");
        let mut mismatch = snapshot(
            0.0,
            now().timestamp() + 3600,
            80.0,
            now().timestamp() + 86400,
        );
        mismatch["cachedUsageUtilization"]["accountUuid"] =
            json!("00000000-0000-4000-8000-000000000002");
        write("spare", mismatch);
        let output = line(
            &high,
            &read_disk(&high, home, Some(&current), &usage),
            200,
            false,
        );
        assert!(output.contains("→ other 5h 20%"), "{output}");
    }

    #[test]
    fn context_edges_and_chat_class() {
        // Known-bad: 999500 stays in k, or a per-token account shows Claude's estimate.
        for (tokens, expected) in [
            (999, "ctx 999"),
            (1000, "ctx 1k"),
            (999499, "ctx 999k"),
            (999500, "ctx 1.0M"),
            (1000000, "ctx 1.0M"),
        ] {
            let output = line(
                &json!({"context_window":{"total_input_tokens":tokens}}),
                &disk(),
                200,
                false,
            );
            assert!(output.contains(expected), "{output}");
        }
        for cost in [json!(0), json!(-1), json!("1.70"), Value::Null] {
            let output = line(
                &json!({"cost":{"total_cost_usd":cost}}),
                &disk(),
                200,
                false,
            );
            assert!(!output.contains("chat"), "{output}");
        }
        let mut api = disk();
        api.per_token = true;
        assert!(!line(&full_input(), &api, 200, false).contains("chat"));
        assert!(line(&full_input(), &disk(), 200, false).contains("chat ~$1.70"));
    }

    #[test]
    fn registered_per_token_profile_ignores_live_chat_cost() {
        // Known-bad: reading the live estimate for an API-billed profile.
        let tmp = TempDir::new().unwrap();
        register(tmp.path(), &[("api", Tool::Claude)]);
        let profile = tmp.path().join(".claude-switch/profiles/api");
        fs::write(
            profile.join(".claude.json"),
            br#"{"primaryApiKey":"synthetic"}"#,
        )
        .unwrap();
        let data = read_disk(
            &full_input(),
            tmp.path(),
            Some(&profile),
            &tmp.path().join("usage"),
        );
        assert!(data.per_token);
        assert!(!line(&full_input(), &data, 200, false).contains("chat"));
    }

    #[test]
    fn shedding_is_ordered_and_bounded() {
        // Known-bad: wrap, cut a segment, or drop context before reset suffixes.
        let input = full_input();
        let full = line(&input, &disk(), 200, false);
        assert_eq!(
            full,
            "work · acme › site · 5h 42% ↻1h20 · 7d 71% ↻Fri · ctx 142k · chat ~$1.70"
        );
        for columns in 10..=200 {
            let output = line(&input, &disk(), columns, false);
            let budget = columns.saturating_sub(4).max(20);
            assert!(visible_len(&output) <= budget, "{columns}: {output}");
            assert!(!output.contains('\n'));
        }
        let after_chat = line(&input, &disk(), full.chars().count() + 3, false);
        assert!(!after_chat.contains("chat") && after_chat.contains("ctx"));
        assert_eq!(
            line(&input, &disk(), 50, false),
            "work · acme › site · 5h 42% · 7d 71%"
        );
        assert_eq!(line(&input, &disk(), 30, false), "work · site · 5h 42%");
    }

    #[test]
    fn shedding_with_headroom_follows_every_step() {
        // Known-bad: dropping headroom before shortening the project or counting ESC bytes.
        let mut input = full_input();
        input["rate_limits"]["five_hour"]["used_percentage"] = json!(88);
        let mut data = disk();
        data.headroom = Some(Headroom {
            name: "spare".into(),
            five_percent: 10.0,
            five_reset: false,
            weekly_percent: Some(85.0),
        });
        let expected = [
            "work · acme › site · 5h 88% ↻1h20 · 7d 71% ↻Fri · → spare 5h 10% 7d 85% · ctx 142k · chat ~$1.70",
            "work · acme › site · 5h 88% ↻1h20 · 7d 71% ↻Fri · → spare 5h 10% 7d 85% · ctx 142k",
            "work · acme › site · 5h 88% ↻1h20 · 7d 71% · → spare 5h 10% 7d 85% · ctx 142k",
            "work · acme › site · 5h 88% · 7d 71% · → spare 5h 10% 7d 85% · ctx 142k",
            "work · acme › site · 5h 88% · 7d 71% · → spare 5h 10% 7d 85%",
            "work · site · 5h 88% · 7d 71% · → spare 5h 10% 7d 85%",
            "work · site · 5h 88% · 7d 71% · → spare 5h 10%",
            "work · site · 5h 88% · 7d 71%",
            "work · site · 5h 88%",
        ];
        assert_eq!(line(&input, &data, 200, false), expected[0]);
        for pair in expected.windows(2) {
            let columns = visible_len(pair[0]) + 3;
            assert_eq!(
                line(&input, &data, columns, false),
                pair[1],
                "columns={columns}"
            );
        }
        for columns in 10..=200 {
            let output = line(&input, &data, columns, false);
            assert!(
                visible_len(&output) <= columns.saturating_sub(4).max(20),
                "{columns}: {output}"
            );
        }
        assert_eq!(line(&input, &data, 120, false), expected[0]);
        assert_eq!(line(&input, &data, 50, false), expected[6]);
        data.account = "accountnamewithmorethantwentycharacters".into();
        assert_eq!(line(&input, &data, 10, false), "accountnamewithmoret");
    }

    #[test]
    fn colour_only_changes_tokens_not_shedding() {
        // Known-bad: counting ESC bytes sheds a coloured line early.
        for (percent, code) in [
            (79, None),
            (80, Some("\x1b[33m80%\x1b[0m")),
            (95, Some("\x1b[31m95%\x1b[0m")),
        ] {
            let input = json!({"rate_limits":{"five_hour":{"used_percentage":percent}}});
            let output = line(&input, &disk(), 120, true);
            assert_eq!(output.contains("\x1b["), code.is_some());
            if let Some(code) = code {
                assert!(output.contains(code));
            }
            assert!(!line(&input, &disk(), 120, false).contains("\x1b["));
            for columns in 10..=120 {
                assert_eq!(
                    visible_len(&line(&input, &disk(), columns, true)),
                    visible_len(&line(&input, &disk(), columns, false))
                );
            }
        }
    }

    #[test]
    fn render_reads_without_creating_or_modifying_files() {
        // Known-bad: using a seeding constructor or writing during a read.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let usage = home.join("usage");
        let empty = read_disk(&Value::Null, home, None, &usage);
        assert_eq!(empty.account, "default");
        assert_eq!(fs::read_dir(home).unwrap().count(), 0);
        register(home, &[("work", Tool::Claude)]);
        let config = home.join(".claude-switch/profiles/work");
        fs::write(config.join(".claude.json"), br#"{"oauthAccount":{}}"#).unwrap();
        fs::write(config.join("settings.json"), br#"{"theme":"dark"}"#).unwrap();
        fs::create_dir_all(&usage).unwrap();
        fs::write(usage.join("config.json"), b"{}").unwrap();
        fn tree(path: &Path, found: &mut BTreeMap<PathBuf, (Option<Vec<u8>>, SystemTime)>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let bytes = metadata.is_file().then(|| fs::read(&path).unwrap());
                found.insert(path.clone(), (bytes, metadata.modified().unwrap()));
                if metadata.is_dir() {
                    tree(&path, found);
                }
            }
        }
        let mut before = BTreeMap::new();
        tree(home, &mut before);
        let data = read_disk(&full_input(), home, Some(&config), &usage);
        let _ = line(&full_input(), &data, 120, false);
        let mut after = BTreeMap::new();
        tree(home, &mut after);
        assert_eq!(after, before);
    }

    #[test]
    fn all_installs_claude_profiles_in_order_and_continues_after_refusal() {
        // Known-bad: --all stops at the first refusal or includes a Codex profile.
        let temp = TempDir::new().unwrap();
        let home = temp.path();
        register(
            home,
            &[
                ("beta", Tool::Claude),
                ("codex", Tool::Codex),
                ("alpha", Tool::Claude),
            ],
        );
        let manager =
            ProfileManager::with_paths_read_only(home.join(".claude-switch"), home.join(".claude"))
                .unwrap();
        let executable = home.join("cswitch");
        fs::write(&executable, b"synthetic executable").unwrap();
        let action = Action::Install {
            no_refresh: true,
            force: false,
        };
        let (output, failed) = manage(&manager, None, true, action, &executable, now()).unwrap();
        assert!(!failed, "{output}");
        assert_eq!(output, "alpha: installed\nbeta: installed\n");
        let settings = |name: &str| manager.profile_dir(name).join("settings.json");
        for name in ["alpha", "beta"] {
            let value: Value = serde_json::from_slice(&fs::read(settings(name)).unwrap()).unwrap();
            assert!(
                value["statusLine"]["command"]
                    .as_str()
                    .unwrap()
                    .ends_with("statusline --no-refresh")
            );
        }
        assert!(!settings("codex").exists());
        fs::write(
            settings("alpha"),
            br#"{"statusLine":{"type":"command","command":"foreign status"}}"#,
        )
        .unwrap();
        fs::write(settings("beta"), b"{}").unwrap();
        let (output, failed) = manage(&manager, None, true, action, &executable, now()).unwrap();
        assert!(failed);
        assert!(
            output.starts_with(
                "alpha: alpha already has a status line; pass --force to replace it\n"
            ),
            "{output}"
        );
        assert!(output.ends_with("beta: installed\n"), "{output}");
        assert!(!settings("codex").exists());
    }
}
