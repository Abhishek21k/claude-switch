//! Claude Code's one-line, read-only status display.

use crate::key;
use crate::limits::read_claude_json;
use crate::profile::{ProfileManager, Tool};
use crate::usage::{attribute, metrics};
use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde_json::Value;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct DiskData {
    account: String,
    project: Option<String>,
    project_short: Option<String>,
    per_token: bool,
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
    let disk = read_disk(&input, &home, config_dir.as_deref(), &usage_dir);
    let columns = std::env::var("COLUMNS")
        .ok()
        .and_then(|raw| raw.parse().ok());
    let colour = std::env::var_os("NO_COLOR").is_none_or(|raw| raw.is_empty());
    let local_now = Local::now();
    render(
        &input,
        &disk,
        columns,
        colour,
        local_now.with_timezone(&Utc),
        local_now.offset().fix(),
    )
}

pub fn read_disk(
    input: &Value,
    home: &Path,
    configured: Option<&Path>,
    usage_dir: &Path,
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
    let per_token = registered.is_some_and(|profile| {
        manager.as_ref().is_some_and(|manager| {
            key::read_auth_mode(
                manager,
                &profile.name,
                read_claude_json(&manager.profile_dir(&profile.name)),
            )
            .api_billed()
        })
    });
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
        account: clean(&account),
        project: project.map(|value| clean(&value)),
        project_short: project_short.map(|value| clean(&value)),
        per_token,
    }
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
    let mut five = stdin_window(input, "five_hour")
        .map(|window| window_text("5h", window, colour, now, offset));
    let mut seven = stdin_window(input, "seven_day")
        .map(|window| window_text("7d", window, colour, now, offset));
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
        let line = compose(&account, &project, &five, &seven, &ctx, &chat);
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
            8 => seven = None,
            9 => project = None,
            10 => five = None,
            _ => {} // Part B adds the first two steps; headroom uses step 7.
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
        }
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
}
