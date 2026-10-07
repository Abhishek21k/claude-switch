use crate::agy::{self, FarmHealth, TokenState};
use crate::atomic;
use crate::codex;
use crate::key::{remove_key, strip_copied_helper};
use crate::skills_sync::{self, SyncAction, SyncOptions, SyncReport};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub tool: Tool,
    pub email: Option<String>,
    pub added: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod stage_b_tests {
    use super::*;
    use tempfile::TempDir;

    const SYNTHETIC_CODEX_AUTH: &[u8] =
        br#"{"tokens":{"id_token":"h.eyJlbWFpbCI6Im9AZXhhbXBsZS5jb20ifQ.s"}}"#;

    fn manager(tmp: &TempDir) -> ProfileManager {
        ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude")).unwrap()
    }

    fn register(manager: &ProfileManager, name: &str, tool: Tool, email: &str) {
        register_with_email(manager, name, tool, Some(email));
    }

    fn register_with_email(manager: &ProfileManager, name: &str, tool: Tool, email: Option<&str>) {
        let mut registry = manager.load_registry().unwrap();
        registry.profiles.insert(
            name.into(),
            Profile {
                name: name.into(),
                tool,
                email: email.map(str::to_owned),
                added: Utc::now(),
                last_used: None,
            },
        );
        manager.save_registry(&registry).unwrap();
        fs::create_dir_all(manager.profile_dir(name)).unwrap();
    }

    #[test]
    fn legacy_registry_defaults_to_claude() {
        // Known-bad: requiring `tool` breaks registries written before Stage B.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        fs::write(&manager.registry_path, r#"{"profiles":{"old":{"name":"old","email":"old@example.com","added":"2030-01-01T00:00:00Z","last_used":null}}}"#).unwrap();
        assert_eq!(manager.get_profile("old").unwrap().tool, Tool::Claude);
    }

    #[test]
    fn antigravity_profile_home_is_detected_by_path_components() {
        // Known-bad: no guard, or ignoring the .claude-switch ancestor or final home component.
        let detect = antigravity_profile_home_name;
        assert_eq!(
            detect(Path::new("/tmp/user/.claude-switch/profiles/g/home")),
            Some("g".into())
        );
        assert_eq!(detect(Path::new("/tmp/user")), None);
        assert_eq!(detect(Path::new("/tmp/home")), None);
        assert_eq!(detect(Path::new("/tmp/profiles/g/home")), None);
        assert_eq!(detect(Path::new("/tmp/.claude-switch/other/g/home")), None);
        assert_eq!(
            detect(Path::new("/tmp/.claude-switch/profiles/g/other")),
            None
        );
    }

    #[test]
    fn unknown_tool_loads_and_refuses_use_and_login() {
        // Known-bad: a strict enum fails the whole registry instead of isolating the unknown profile.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        fs::write(&manager.registry_path, r#"{"profiles":{"alien":{"name":"alien","tool":"martian","email":"alien@example.com","added":"2030-01-01T00:00:00Z","last_used":null}}}"#).unwrap();
        fs::create_dir_all(manager.profile_dir("alien")).unwrap();
        assert_eq!(
            manager.get_profile("alien").unwrap().tool,
            Tool::Unknown("martian".into())
        );
        assert!(
            manager
                .prepare_launch("alien")
                .unwrap_err()
                .to_string()
                .contains("unknown tool")
        );
        fs::remove_dir_all(manager.profile_dir("alien")).unwrap();
        assert!(
            manager
                .prepare_launch("alien")
                .unwrap_err()
                .to_string()
                .contains("unknown tool")
        );
        assert!(
            manager
                .login_codex_profile("alien")
                .unwrap_err()
                .to_string()
                .contains("unknown tool")
        );
    }

    #[test]
    fn unknown_tool_value_survives_unrelated_save_without_aliases() {
        // Known-bad: serde(other) rewrites an unknown tool as "unknown" on the next save.
        for raw_tool in ["martian", "venusian"] {
            let tmp = TempDir::new().unwrap();
            let manager = manager(&tmp);
            let registry = format!(
                r#"{{"profiles":{{"alien":{{"name":"alien","tool":"{raw_tool}","email":"alien@example.com","added":"2030-01-01T00:00:00Z","last_used":null}}}}}}"#
            );
            fs::write(&manager.registry_path, registry).unwrap();
            register(&manager, "other", Tool::Claude, "other@example.com");
            let saved: serde_json::Value =
                serde_json::from_slice(&fs::read(&manager.registry_path).unwrap()).unwrap();
            assert_eq!(saved["profiles"]["alien"]["tool"], raw_tool);
            let profiles = manager.list_profiles().unwrap();
            assert_eq!(
                manager.get_profile("alien").unwrap().tool.label(),
                "unknown tool"
            );
            let shell = manager.generate_shell_aliases(&profiles).unwrap();
            let powershell = manager.generate_powershell_aliases(&profiles).unwrap();
            assert!(!shell.contains("alien"), "{shell}");
            assert!(!powershell.contains("alien"), "{powershell}");
            assert!(shell.contains("claude-other"));
            assert!(powershell.contains("claude-other"));
        }
    }

    #[test]
    fn codex_seed_copies_only_five_warm_entries() {
        // Known-bad: a skip list misses a new versioned sqlite file or copies auth.json.
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join(".codex");
        let dest = tmp.path().join("profile");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        for name in ["config.toml", "AGENTS.md"] {
            fs::write(source.join(name), "synthetic warm state").unwrap();
        }
        for name in ["agents", "rules", "skills"] {
            fs::create_dir_all(source.join(name)).unwrap();
            fs::write(source.join(name).join("entry"), "synthetic warm state").unwrap();
        }
        for name in [
            "auth.json",
            "installation_id",
            "history.jsonl",
            "session_index.jsonl",
            "logs_2.sqlite",
            "logs_2.sqlite-shm",
            "logs_2.sqlite-wal",
            "state_5.sqlite",
            "thread_history_1.sqlite",
            "goals_1.sqlite",
            "queue_1.sqlite",
            "memories_1.sqlite",
            "models_cache.json",
            "version.json",
            "tui-thread-reference-capabilities",
            ".personality_migration",
            ".sandbox_migration",
        ] {
            fs::write(source.join(name), "synthetic excluded state").unwrap();
        }
        for stem in [
            "logs_2",
            "state_5",
            "thread_history_1",
            "goals_1",
            "queue_1",
            "memories_1",
        ] {
            for suffix in ["", ".sqlite", ".sqlite-shm", ".sqlite-wal"] {
                fs::write(
                    source.join(format!("{stem}{suffix}")),
                    "synthetic excluded state",
                )
                .unwrap();
            }
        }
        for name in [
            "secrets",
            "sessions",
            "archived_sessions",
            "memories",
            "packages",
            "cache",
            ".tmp",
            "tmp",
            "log",
            "plugins",
            "attachments",
            "shell_snapshots",
            "bin",
            "ipc",
            "app-server-control",
            "app-server-daemon",
            "mcp-oauth-locks",
            "thread-writer-locks",
        ] {
            fs::create_dir_all(source.join(name)).unwrap();
            fs::write(source.join(name).join("entry"), "synthetic excluded state").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(source.join("agents/entry"), source.join("skills/linked"))
            .unwrap();
        seed_codex_from(&source, &dest).unwrap();
        let mut entries: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            ["AGENTS.md", "agents", "config.toml", "rules", "skills"]
        );
        #[cfg(unix)]
        assert!(dest.join("skills/linked").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn codex_seed_keeps_top_level_skills_symlink() {
        // Known-bad: metadata() follows the linked skills directory and copies its tree.
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        let linked = tmp.path().join("linked-skills");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&linked).unwrap();
        fs::write(linked.join("first"), "synthetic").unwrap();
        std::os::unix::fs::symlink(&linked, source.join("skills")).unwrap();
        assert!(seed_codex_from(&source, &dest).unwrap());
        assert!(
            fs::symlink_metadata(dest.join("skills"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(dest.join("skills")).unwrap(), linked);
        fs::write(linked.join("later"), "synthetic").unwrap();
        assert!(dest.join("skills/later").exists());
    }

    #[test]
    fn codex_seed_without_source_leaves_destination_empty() {
        // Known-bad: a missing source home blocks a first login with a read error.
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let missing = tmp.path().join("absent");
        assert!(!seed_codex_from(&missing, &dest).unwrap());
        let file = tmp.path().join("source-file");
        fs::write(&file, "synthetic").unwrap();
        assert!(!seed_codex_from(&file, &dest).unwrap());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn launch_spec_sets_exactly_the_selected_tool_home() {
        // Known-bad: a Codex launch inherits the Claude env key instead of CODEX_HOME.
        let dir = PathBuf::from("/synthetic/profile");
        assert_eq!(
            launch_spec(Tool::Codex, dir.clone()).unwrap(),
            LaunchSpec {
                program: "codex",
                env_key: "CODEX_HOME",
                env_value: dir.clone()
            }
        );
        assert_eq!(
            launch_spec(Tool::Claude, dir.clone()).unwrap(),
            LaunchSpec {
                program: "claude",
                env_key: "CLAUDE_CONFIG_DIR",
                env_value: dir
            }
        );
    }

    #[test]
    fn codex_source_uses_nonempty_override() {
        // Known-bad: an empty CODEX_HOME redirects seeding away from the default home.
        let home = Path::new("/synthetic/home");
        assert_eq!(codex_source_home(home, None), home.join(".codex"));
        assert_eq!(
            codex_source_home(home, Some(std::ffi::OsStr::new(""))),
            home.join(".codex")
        );
        assert_eq!(
            codex_source_home(home, Some(std::ffi::OsStr::new("/synthetic/other"))),
            PathBuf::from("/synthetic/other")
        );
    }

    #[test]
    fn codex_login_verdict_accepts_valid_identity() {
        // Known-bad: successful login and status still reject a valid local identity.
        assert_eq!(
            codex_login_verdict(true, true, true, Some(SYNTHETIC_CODEX_AUTH))
                .unwrap()
                .unwrap()
                .email,
            "o@example.com"
        );
    }

    #[test]
    fn codex_login_command_failure_refuses_registration() {
        // Known-bad: a failed browser login is accepted because an old auth.json remains,
        // or is reported as a missing auth.json instead of a failed login.
        assert_eq!(
            codex_login_verdict(false, true, true, Some(SYNTHETIC_CODEX_AUTH))
                .unwrap_err()
                .message("work"),
            "Codex login did not complete for profile 'work'. Nothing was registered."
        );
        for auth_exists in [false, true] {
            assert_eq!(
                codex_login_verdict(false, false, auth_exists, None)
                    .unwrap_err()
                    .message("work"),
                "Codex login did not complete for profile 'work'. Nothing was registered."
            );
        }
    }

    #[test]
    fn codex_login_status_failure_refuses_registration() {
        // Known-bad: a failed `login status` with auth.json present reports the file as missing.
        assert_eq!(
            codex_login_verdict(true, false, true, Some(SYNTHETIC_CODEX_AUTH))
                .unwrap_err()
                .message("work"),
            "Codex login status failed for profile 'work'. Nothing was registered."
        );
    }

    #[test]
    fn codex_missing_auth_file_refuses_registration() {
        // Known-bad: exit code zero alone registers a profile without auth.json,
        // or reports a failed login status instead of the missing file.
        assert_eq!(
            codex_login_verdict(true, true, false, None)
                .unwrap_err()
                .message("work"),
            "Codex did not leave auth.json. Nothing was registered."
        );
    }

    #[test]
    fn codex_unreadable_identity_still_registers_without_email() {
        // Known-bad: successful login with auth.json containing unreadable claims is discarded.
        assert_eq!(codex_login_verdict(true, true, true, Some(b"{}")), Ok(None));
        assert_eq!(codex_login_verdict(true, true, true, None), Ok(None));
    }

    #[test]
    fn mixed_aliases_are_tool_scoped() {
        // Known-bad: Codex receives a claude- alias or an overlong email comment breaks the line limit.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        register(&manager, "c", Tool::Claude, "same@example.com");
        register(&manager, "o", Tool::Codex, "same@example.com");
        register(
            &manager,
            "long",
            Tool::Codex,
            &format!("{}@example.com", "x".repeat(150)),
        );
        let aliases = manager.generate_aliases().unwrap();
        assert!(aliases.contains("claude-c"));
        assert!(aliases.contains("codex-o"));
        assert!(!aliases.contains("claude-o"));
        assert!(aliases.lines().all(|line| line.chars().count() <= 120));
        assert!(aliases.contains("alias codex-long='cswitch use long'"));
        let powershell = manager
            .generate_powershell_aliases(&manager.list_profiles().unwrap())
            .unwrap();
        assert!(powershell.contains("function codex-o { cswitch use o @args }"));
        assert!(powershell.lines().all(|line| line.chars().count() <= 120));
        assert!(powershell.contains("function codex-long { cswitch use long @args }"));
    }

    #[test]
    fn long_profile_names_keep_aliases_in_both_shells() {
        // Known-bad: the display line limit replaces long aliases with a comment.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        let claude_name = "c".repeat(50);
        let codex_name = "o".repeat(50);
        register(&manager, &claude_name, Tool::Claude, "c@example.com");
        register(&manager, &codex_name, Tool::Codex, "o@example.com");
        let profiles = manager.list_profiles().unwrap();
        let shell = manager.generate_shell_aliases(&profiles).unwrap();
        let powershell = manager.generate_powershell_aliases(&profiles).unwrap();
        for (prefix, name) in [("claude", claude_name), ("codex", codex_name)] {
            assert!(
                shell.contains(&format!("alias {prefix}-{name}='cswitch use {name}'")),
                "{shell}"
            );
            assert!(
                powershell.contains(&format!(
                    "function {prefix}-{name} {{ cswitch use {name} @args }}"
                )),
                "{powershell}"
            );
        }
        assert!(!shell.contains("alias omitted"));
        assert!(!powershell.contains("alias omitted"));
    }

    #[test]
    fn long_profile_names_without_email_keep_aliases_in_both_shells() {
        // Known-bad: the no-comment return in limit_alias_line replaces long aliases
        // with "# alias omitted: profile name exceeds 120 columns".
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        let claude_name = "c".repeat(50);
        let codex_name = "o".repeat(50);
        register_with_email(&manager, &claude_name, Tool::Claude, None);
        register_with_email(&manager, &codex_name, Tool::Codex, None);
        let profiles = manager.list_profiles().unwrap();
        assert!(profiles.iter().all(|profile| profile.email.is_none()));
        let shell = manager.generate_shell_aliases(&profiles).unwrap();
        let powershell = manager.generate_powershell_aliases(&profiles).unwrap();
        for (prefix, name) in [("claude", claude_name), ("codex", codex_name)] {
            assert!(
                shell.contains(&format!("alias {prefix}-{name}='cswitch use {name}'")),
                "{shell}"
            );
            assert!(
                powershell.contains(&format!(
                    "function {prefix}-{name} {{ cswitch use {name} @args }}"
                )),
                "{powershell}"
            );
        }
        assert!(!shell.contains("alias omitted"));
        assert!(!powershell.contains("alias omitted"));
    }

    #[test]
    fn same_email_across_tools_is_not_the_same_account() {
        // Known-bad: same-account detection compares email across Claude and Codex.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        register(&manager, "c", Tool::Claude, "same@example.com");
        register(&manager, "o", Tool::Codex, "same@example.com");
        assert_eq!(
            manager
                .profiles_with_email("same@example.com", Tool::Codex)
                .unwrap(),
            ["o"]
        );
        assert_eq!(
            manager
                .profiles_with_email("same@example.com", Tool::Claude)
                .unwrap(),
            ["c"]
        );
    }

    #[test]
    fn codex_log_marker_counts_as_activity() {
        // Known-bad: only Claude session markers are checked for Codex profiles.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        register(&manager, "o", Tool::Codex, "o@example.com");
        fs::create_dir_all(manager.profile_dir("o").join("log")).unwrap();
        fs::write(manager.profile_dir("o").join("log/entry"), "synthetic").unwrap();
        assert!(manager.maybe_in_use("o").is_some());
    }

    #[test]
    fn codex_skills_sync_refuses_and_launch_skips_it() {
        // Known-bad: Codex launch links Claude skills into its own skills directory.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        register(&manager, "o", Tool::Codex, "o@example.com");
        assert!(
            manager
                .sync_skills(
                    "o",
                    &SyncOptions {
                        dry_run: false,
                        adopt: vec![]
                    }
                )
                .unwrap_err()
                .to_string()
                .contains("Claude-only")
        );
        assert!(!manager.profile_dir("o").join("skills").exists());
        assert_eq!(manager.prepare_launch("o").unwrap().spec.program, "codex");
        assert!(!manager.profile_dir("o").join("skills").exists());
    }

    #[cfg(unix)]
    #[test]
    fn agy_prepare_launch_relinks_and_updates_last_used() {
        // Known-bad: prepare_launch stops calling link_farm.
        let tmp = TempDir::new().unwrap();
        let manager = ProfileManager::with_base_dir(tmp.path().join(".claude-switch")).unwrap();
        register(&manager, "g", Tool::Antigravity, "g@example.com");
        fs::write(tmp.path().join("gone"), b"old").unwrap();
        agy::link_farm(tmp.path(), &manager.profile_dir("g")).unwrap();
        fs::remove_file(tmp.path().join("gone")).unwrap();
        fs::write(tmp.path().join("new"), b"new").unwrap();

        let prepared = manager.prepare_launch("g").unwrap();
        assert_eq!(prepared.spec.program, "agy");
        assert_eq!(prepared.spec.env_key, "HOME");
        assert_eq!(
            prepared.spec.env_value,
            manager.profile_dir("g").join("home")
        );
        assert_eq!(
            fs::read_link(prepared.spec.env_value.join("new")).unwrap(),
            tmp.path().join("new")
        );
        assert!(fs::symlink_metadata(prepared.spec.env_value.join("gone")).is_err());
        assert!(manager.get_profile("g").unwrap().last_used.is_some());
    }

    #[test]
    fn agy_activity_uses_each_nested_marker_and_ignores_cache() {
        // Known-bad: no Antigravity activity branch, Codex markers, or a missing agy marker.
        for marker in ["log", "conversations", "brain"] {
            let tmp = TempDir::new().unwrap();
            let manager = manager(&tmp);
            register(&manager, "g", Tool::Antigravity, "g@example.com");
            let cli = manager
                .profile_dir("g")
                .join("home/.gemini/antigravity-cli");
            let root = cli.join(marker);
            let nested = root.join("year/month");
            fs::create_dir_all(&nested).unwrap();
            let entry = nested.join("entry");
            fs::write(&entry, b"old").unwrap();
            let old = SystemTime::now() - std::time::Duration::from_secs(3600);
            for path in [&cli, &root, &root.join("year"), &nested, &entry] {
                fs::File::open(path).unwrap().set_modified(old).unwrap();
            }
            let cache = cli.join("cache");
            fs::create_dir_all(&cache).unwrap();
            fs::write(cache.join("fresh"), b"ignored").unwrap();
            assert_eq!(
                manager.maybe_in_use("g"),
                None,
                "{marker} cache must not count"
            );
            fs::write(&entry, b"fresh marker").unwrap();
            assert!(manager.maybe_in_use("g").is_some(), "{marker} must count");
        }
    }

    #[test]
    fn codex_nested_session_rewrite_counts_as_recent_activity() {
        // Known-bad: checking only the sessions/ directory misses a rewrite several levels below it.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        register(&manager, "o", Tool::Codex, "o@example.com");
        let sessions = manager.profile_dir("o").join("sessions");
        let year = sessions.join("2030");
        let month = year.join("01");
        fs::create_dir_all(&month).unwrap();
        let session = month.join("session.jsonl");
        fs::write(&session, "synthetic").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        for directory in [&sessions, &year, &month] {
            fs::File::open(directory)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        assert!(manager.maybe_in_use("o").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn codex_activity_walk_does_not_follow_directory_link() {
        // Known-bad: metadata() follows a linked sessions directory outside the profile.
        let tmp = TempDir::new().unwrap();
        let manager = manager(&tmp);
        register(&manager, "o", Tool::Codex, "o@example.com");
        let profile = manager.profile_dir("o");
        let sessions = profile.join("sessions");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, sessions.join("linked")).unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        for directory in [&profile, &sessions] {
            fs::File::open(directory)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        fs::write(outside.join("fresh"), "synthetic").unwrap();
        assert_eq!(manager.maybe_in_use("o"), None);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Tool {
    #[default]
    Claude,
    Codex,
    Antigravity,
    Unknown(String),
}

impl Tool {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Unknown(_) => "unknown tool",
        }
    }

    fn alias_prefix(&self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::Antigravity => Some("agy"),
            Self::Unknown(_) => None,
        }
    }
}

impl Serialize for Tool {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let value = match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Unknown(value) => value,
        };
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "antigravity" => Self::Antigravity,
            _ => Self::Unknown(value),
        })
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Registry {
    pub profiles: HashMap<String, Profile>,
}

/// The verified result of a completed `login_profile`.
///
/// `email` is what Claude reported after authenticating — never what the user
/// typed. `same_account_as` lists profiles that were already registered to that
/// same account, so the caller can say "this is the account you already had"
/// instead of implying a new one was added.
#[derive(Debug, Clone, PartialEq)]
pub struct LoginOutcome {
    pub email: Option<String>,
    pub same_account_as: Vec<String>,
    pub tool: Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    ClaudeAi,
    Console,
}

pub fn login_args(method: LoginMethod, email_hint: Option<&str>) -> Vec<String> {
    let mut args = vec!["auth".to_string(), "login".to_string()];
    if method == LoginMethod::Console {
        args.push("--console".to_string());
    }
    if let Some(hint) = email_hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        args.extend(["--email".to_string(), hint.to_string()]);
    }
    args
}

pub fn login_verdict(
    method: LoginMethod,
    status_json: &serde_json::Value,
) -> Result<Option<String>> {
    if status_json
        .get("loggedIn")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        bail!("Claude did not report an authenticated session.");
    }
    let email = status_json
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    match method {
        LoginMethod::ClaudeAi if email.is_some() => Ok(email),
        LoginMethod::Console
            if status_json
                .get("apiKeySource")
                .and_then(serde_json::Value::as_str)
                == Some("/login managed key") =>
        {
            Ok(email)
        }
        _ => bail!("Claude did not report the requested login method."),
    }
}

impl LoginOutcome {
    pub fn display_email(&self) -> &str {
        self.email.as_deref().unwrap_or("unknown account")
    }
}

// ── ProfileManager ────────────────────────────────────────────────────────────

pub struct ProfileManager {
    #[allow(dead_code)]
    pub base_dir: PathBuf,
    pub profiles_dir: PathBuf,
    registry_path: PathBuf,
    claude_home: PathBuf,
    codex_home: PathBuf,
    codex_source_label: &'static str,
}

#[derive(Debug)]
struct LaunchPreparation {
    spec: LaunchSpec,
}

#[derive(Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: &'static str,
    pub env_key: &'static str,
    pub env_value: PathBuf,
}

pub fn launch_spec(tool: Tool, profile_dir: PathBuf) -> Result<LaunchSpec> {
    let (program, env_key, env_value) = match tool {
        Tool::Claude => ("claude", "CLAUDE_CONFIG_DIR", profile_dir),
        Tool::Codex => ("codex", "CODEX_HOME", profile_dir),
        Tool::Antigravity => (agy::AGY_PROGRAM, "HOME", agy::profile_home(&profile_dir)),
        Tool::Unknown(_) => bail!("Profile has an unknown tool; cannot use or log in."),
    };
    Ok(LaunchSpec {
        program,
        env_key,
        env_value,
    })
}

fn codex_source_home(home: &Path, configured: Option<&std::ffi::OsStr>) -> PathBuf {
    configured
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

/// Detect a HOME supplied by an Antigravity profile launch.
pub fn antigravity_profile_home_name(home: &Path) -> Option<String> {
    use std::ffi::OsStr;
    let parts: Vec<_> = home.components().collect();
    let [prefix @ .., profiles, name, last] = parts.as_slice() else {
        return None;
    };
    if profiles.as_os_str() != OsStr::new("profiles") || last.as_os_str() != OsStr::new("home") {
        return None;
    }
    if !prefix
        .iter()
        .any(|part| part.as_os_str() == OsStr::new(".claude-switch"))
    {
        return None;
    }
    let name = name.as_os_str().to_str()?;
    (!name.is_empty()).then(|| name.to_owned())
}

impl ProfileManager {
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir().context("Cannot determine home directory")?;
        if let Some(name) = antigravity_profile_home_name(&home) {
            bail!(
                "cswitch is running inside the Antigravity profile '{name}'. Run it from a normal shell."
            );
        }
        let mut manager = Self::with_base_dir(home.join(".claude-switch"))?;
        let configured = std::env::var_os("CODEX_HOME");
        manager.codex_home = codex_source_home(&home, configured.as_deref());
        if configured.as_deref().is_some_and(|value| !value.is_empty()) {
            manager.codex_source_label = "CODEX_HOME";
        }
        Ok(manager)
    }

    /// Build a manager rooted at an arbitrary directory.
    ///
    /// Exists so tests can drive a manager that cannot reach the real
    /// `~/.claude-switch`; `new()` is the same call with the home path.
    pub fn with_base_dir(base_dir: PathBuf) -> Result<Self> {
        let home = base_dir
            .parent()
            .context("Cannot determine parent of profile base directory")?;
        Self::with_paths(base_dir.clone(), home.join(".claude"))
    }

    /// Inject both roots so tests never consult the caller's real home.
    pub fn with_paths(base_dir: PathBuf, claude_home: PathBuf) -> Result<Self> {
        let manager = Self::with_paths_read_only(base_dir, claude_home)?;
        fs::create_dir_all(&manager.profiles_dir)?;
        Ok(manager)
    }

    /// Inspect profiles without creating the profile store on a status-line read.
    pub fn with_paths_read_only(base_dir: PathBuf, claude_home: PathBuf) -> Result<Self> {
        let home = base_dir
            .parent()
            .context("Cannot determine parent of profile base directory")?;
        let codex_home = home.join(".codex");
        let profiles_dir = base_dir.join("profiles");
        let registry_path = base_dir.join("registry.json");
        Ok(Self {
            base_dir,
            profiles_dir,
            registry_path,
            claude_home,
            codex_home,
            codex_source_label: "~/.codex",
        })
    }

    // ── Registry I/O ─────────────────────────────────────────────────────────

    pub fn load_registry(&self) -> Result<Registry> {
        if !self.registry_path.exists() {
            return Ok(Registry::default());
        }
        let content = fs::read_to_string(&self.registry_path)?;
        Ok(serde_json::from_str(&content)?)
    }

    fn save_registry(&self, registry: &Registry) -> Result<()> {
        let content = serde_json::to_string_pretty(registry)?;
        atomic::write(&self.registry_path, content.as_bytes())
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Returns all profiles sorted alphabetically by name.
    pub fn list_profiles(&self) -> Result<Vec<Profile>> {
        let registry = self.load_registry()?;
        let mut profiles: Vec<Profile> = registry.profiles.into_values().collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(profiles)
    }

    /// Add a profile from an explicit source directory.
    /// Seeding from a caller-supplied path; exercised by the tests.
    #[allow(dead_code)]
    pub fn add_profile_from(&self, name: &str, src: &Path) -> Result<Profile> {
        self.copy_and_register(name, src, false, false)
    }

    /// Same as `add_profile_from` but overwrites an existing profile.
    /// Kept as the sibling of `add_profile_from`; exercised by the tests.
    #[allow(dead_code)]
    pub fn add_profile_from_force(&self, name: &str, src: &Path) -> Result<Profile> {
        self.copy_and_register(name, src, false, true)
    }

    /// Add the current logged-in session as a named profile.
    /// Copies `~/.claude/` dir, `~/.claude.json` (home root), and on macOS
    /// extracts Keychain credentials into `.credentials.json`.
    pub fn add_profile(&self, name: &str, include_history: bool) -> Result<Profile> {
        let home = self
            .claude_home
            .parent()
            .context("Claude home has no parent")?;
        let src = &self.claude_home;
        if !src.exists() {
            bail!("~/.claude does not exist. Is Claude Code installed and logged in?");
        }
        let mut profile = self.copy_and_register(name, src, include_history, false)?;
        self.copy_extra_credentials(home, name, &mut profile)?;
        Ok(profile)
    }

    /// Same as `add_profile` but overwrites an existing profile.
    pub fn add_profile_force(&self, name: &str, include_history: bool) -> Result<Profile> {
        let home = self
            .claude_home
            .parent()
            .context("Claude home has no parent")?;
        let src = &self.claude_home;
        if !src.exists() {
            bail!("~/.claude does not exist. Is Claude Code installed and logged in?");
        }
        let mut profile = self.copy_and_register(name, src, include_history, true)?;
        self.copy_extra_credentials(home, name, &mut profile)?;
        Ok(profile)
    }

    /// Copy the extra files that live outside `~/.claude/`:
    /// 1. `~/.claude.json` (home root — has oauthAccount metadata)
    /// 2. macOS Keychain credentials → `.credentials.json`
    fn copy_extra_credentials(&self, home: &Path, name: &str, profile: &mut Profile) -> Result<()> {
        let dest = self.profile_dir(name);

        // 1. Copy ~/.claude.json from home root (contains oauthAccount w/ email)
        let home_claude_json = home.join(".claude.json");
        if home_claude_json.exists() {
            fs::copy(&home_claude_json, dest.join(".claude.json"))?;
            // Re-read email now that we have the full config
            if profile.email.is_none() {
                profile.email = read_email_from_dir(&dest);
                self.upsert_profile(profile.clone())?;
            }
        }

        // 2. Extract platform-specific credentials if not already present
        if !dest.join(".credentials.json").exists()
            && let Some(creds) = extract_platform_credentials()
        {
            fs::write(dest.join(".credentials.json"), creds)?;
        }

        Ok(())
    }

    pub fn remove_profile(&self, name: &str) -> Result<()> {
        let mut registry = self.load_registry()?;
        let profile = registry
            .profiles
            .get(name)
            .context(format!("Profile '{name}' not found."))?;
        if profile.tool == Tool::Claude {
            remove_key(&self.base_dir, name)?;
        }
        let dest = self.profiles_dir.join(name);
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        registry.profiles.remove(name);
        self.save_registry(&registry)
    }

    pub fn get_profile(&self, name: &str) -> Result<Profile> {
        let registry = self.load_registry()?;
        registry
            .profiles
            .get(name)
            .cloned()
            .context(format!("Profile '{}' not found.", name))
    }

    pub fn profile_dir(&self, name: &str) -> PathBuf {
        self.profiles_dir.join(name)
    }

    pub fn agy_farm_health(&self, name: &str) -> Result<FarmHealth> {
        if self.get_profile(name)?.tool != Tool::Antigravity {
            bail!("Profile is not an Antigravity profile.");
        }
        agy::farm_health(&agy::profile_home(&self.profile_dir(name)))
    }

    pub fn sync_skills(&self, name: &str, opts: &SyncOptions) -> Result<SyncReport> {
        if self.get_profile(name)?.tool != Tool::Claude {
            bail!("Skills sync is Claude-only.");
        }
        skills_sync::sync_skills(
            &self.claude_home.join("skills"),
            &self.profile_dir(name).join("skills"),
            &self.base_dir.join("backups/skills").join(name),
            opts,
        )
    }

    // ── Live-session detection ───────────────────────────────────────────────

    /// Seconds since a Claude session last wrote to this profile.
    ///
    /// Profiles are isolated by `CLAUDE_CONFIG_DIR`, so several can run at once
    /// in different terminals. That makes "overwrite this profile" a question
    /// about *other people's live sessions*, not just about stored credentials —
    /// hence this check.
    ///
    /// `None` means no evidence either way: never launched, markers removed, or
    /// the directory is gone. Callers must read it as *unknown*, never as *idle*.
    pub fn seconds_since_session_write(&self, name: &str) -> Option<u64> {
        let dir = self.profile_dir(name);
        let now = SystemTime::now();
        let tool = self
            .get_profile(name)
            .map(|profile| profile.tool)
            .unwrap_or(Tool::Claude);
        let markers = match tool {
            Tool::Claude => SESSION_ACTIVITY_MARKERS,
            Tool::Codex => CODEX_ACTIVITY_MARKERS,
            Tool::Antigravity if agy::activity_root_is_local(&dir) => agy::AGY_ACTIVITY_MARKERS,
            Tool::Antigravity => return None,
            Tool::Unknown(_) => return None,
        };
        markers
            .iter()
            .filter_map(|marker| {
                if tool == Tool::Claude {
                    newest_write(&dir.join(marker))
                } else {
                    newest_write_tree(&dir.join(marker))
                }
            })
            // A timestamp ahead of the clock means skew, not staleness. Round it
            // to "just now" so skew can never make a live profile look idle.
            .map(|t| now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0))
            .min()
    }

    /// How long ago this profile was written to, if that was recent enough that
    /// another session may still have it open.
    ///
    /// Deliberately *may*: mtime is evidence of a session, not proof of a live
    /// process. Callers should word their warnings the same way.
    pub fn maybe_in_use(&self, name: &str) -> Option<u64> {
        self.seconds_since_session_write(name)
            .filter(|secs| *secs <= SESSION_ACTIVE_WINDOW_SECS)
    }

    /// Whether this profile has accumulated conversation content of its own.
    ///
    /// A refresh replaces the directory wholesale and reseeds without history,
    /// so anything here is destroyed. That is worth saying out loud: unlike
    /// credentials, transcripts cannot be recovered by logging in again.
    pub fn has_local_history(&self, name: &str) -> bool {
        let dir = self.profile_dir(name);
        SEED_SKIP_HISTORY
            .iter()
            .any(|entry| dir.join(entry).exists())
    }

    /// Launch the selected tool with its own home pointed at the named profile.
    pub fn launch_profile(&self, name: &str, args: &[OsString]) -> Result<()> {
        let preparation = self.prepare_launch(name)?;
        let spec = preparation.spec;
        let mut command = std::process::Command::new(spec.program);
        command.args(args).env(spec.env_key, &spec.env_value);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let error = command.exec();
            Err(error).with_context(|| {
                format!(
                    "Failed to launch {}. Is it installed and in your PATH?",
                    spec.program
                )
            })
        }
        #[cfg(not(unix))]
        {
            let status = command.status().with_context(|| {
                format!(
                    "Failed to launch {}. Is it installed and in your PATH?",
                    spec.program
                )
            })?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    fn prepare_launch(&self, name: &str) -> Result<LaunchPreparation> {
        let profile = self.get_profile(name)?;
        let profile_dir = self.profile_dir(name);
        let spec = launch_spec(profile.tool.clone(), profile_dir.clone())?;
        if !profile_dir.exists() {
            bail!(
                "Profile directory for '{}' not found. Re-add it with: cswitch add {}",
                name,
                name
            );
        }
        let mut warnings = Vec::new();
        if profile.tool == Tool::Antigravity {
            agy::ensure_supported(cfg!(unix))?;
            let real_home = self
                .base_dir
                .parent()
                .context("Profile base has no parent")?;
            let report = agy::link_farm(real_home, &profile_dir)?;
            if let Some(warning) = agy::link_farm_warning(&report) {
                warnings.push(warning.to_string());
            }
        }
        if profile.tool == Tool::Claude {
            match self.sync_skills(
                name,
                &SyncOptions {
                    dry_run: false,
                    adopt: Vec::new(),
                },
            ) {
                Ok(report) => {
                    let changed: Vec<&str> = report
                        .entries
                        .iter()
                        .filter(|entry| {
                            matches!(
                                entry.action,
                                SyncAction::Linked
                                    | SyncAction::Migrated { .. }
                                    | SyncAction::RemovedDangling
                            )
                        })
                        .map(|entry| entry.name.as_str())
                        .collect();
                    if let Some(summary) = launch_sync_summary(&changed) {
                        eprintln!("{summary}");
                    }
                    if report.has_failures() {
                        warnings.push("some skills could not be synced".to_string());
                    }
                }
                Err(e) => warnings.push(format!("could not sync skills: {e}")),
            }
        }
        let bookkeeping = (|| -> Result<()> {
            let mut registry = self.load_registry()?;
            if let Some(profile) = registry.profiles.get_mut(name) {
                profile.last_used = Some(Utc::now());
                self.save_registry(&registry)?;
            }
            Ok(())
        })();
        if let Err(e) = bookkeeping {
            warnings.push(format!("could not update last used time: {e}"));
        }
        if !warnings.is_empty() {
            eprintln!("cswitch: warning: {}", warnings.join("; "));
        }
        Ok(LaunchPreparation { spec })
    }

    /// Create a profile for a *different* account, pre-seeded with the current
    /// account's warm state, then launch Claude so the user can log in.
    ///
    /// An empty profile dir is a blank Claude Code: every project MCP server
    /// drops back to "pending approval", per-directory trust is gone, and
    /// skills/settings are missing — because `CLAUDE_CONFIG_DIR` relocates
    /// `.claude.json` too, not just credentials. So the warm parts of
    /// `~/.claude` are copied first (minus transcripts and prompt history, see
    /// [`SEED_SKIP_HISTORY`]), and everything identifying the *old* account is
    /// removed so Claude runs its normal login flow.
    ///
    /// Safety property: the credential file is always removed. Even if a future
    /// Claude version introduces an identity key this code does not know about,
    /// the profile still cannot authenticate as the old account — Claude has to
    /// re-authenticate and overwrites the stale metadata itself.
    pub fn login_profile(
        &self,
        name: &str,
        include_history: bool,
        email_hint: Option<&str>,
        method: LoginMethod,
    ) -> Result<LoginOutcome> {
        self.ensure_target_tool(name, Tool::Claude)?;
        let profile_dir = self.profiles_dir.join(name);
        // Never authenticate into a directory we did not just create: a
        // half-written login must not be able to clobber a working profile.
        let we_created_dir = !profile_dir.exists();
        if !we_created_dir && profile_dir.read_dir()?.next().is_some() {
            bail!(
                "Profile '{}' already exists and holds an account. Delete it first \
                 (cswitch remove {}) or pick a different name.",
                name,
                name
            );
        }
        fs::create_dir_all(&profile_dir)?;

        // Any early return past this point must not leave a staged directory
        // behind, so failures funnel through `abort_login`.
        let seeded = match self.seed_profile_dir(&profile_dir, include_history) {
            Ok(seeded) => seeded,
            Err(e) => {
                abort_login(&profile_dir, we_created_dir);
                return Err(e);
            }
        };

        if seeded {
            println!(
                "Seeded '{}' from your current setup (settings, skills, project trust).",
                name
            );
            println!("Conversation history and session transcripts were not copied.\n");
        }

        println!(
            "Opening your browser — sign in as the account for profile '{}'.",
            name
        );
        if method == LoginMethod::ClaudeAi {
            // The OAuth grant follows the browser's claude.ai session.
            println!(
                "  If claude.ai is already signed in as another account, sign out first\n  \
                 or complete this login in a private window.\n"
            );
        } else {
            println!("  Anthropic Console login uses API billing.\n");
        }

        // `claude auth login` is the purpose-built flow: it opens the browser,
        // waits for the OAuth round-trip, and exits. Launching the full TUI
        // instead would leave the user to remember `/exit`, and would trip
        // Claude's nested-session guard when run from inside a Claude session.
        let mut cmd = std::process::Command::new("claude");
        cmd.args(login_args(method, email_hint));
        let status = cmd
            .env("CLAUDE_CONFIG_DIR", &profile_dir)
            .status()
            .context("Failed to launch claude. Is it installed and in your PATH?");

        let status = match status {
            Ok(status) => status,
            Err(e) => {
                abort_login(&profile_dir, we_created_dir);
                return Err(e);
            }
        };

        if !status.success() {
            abort_login(&profile_dir, we_created_dir);
            bail!(
                "Login did not complete for profile '{}'. Nothing was registered — \
                 retry with: cswitch login {}",
                name,
                name
            );
        }

        // Ask Claude who it ended up as, rather than re-parsing the config we
        // just sanitized. A successful exit code is not proof of a session.
        let email =
            read_login_status(&profile_dir).and_then(|json| login_verdict(method, &json).ok());
        if email.is_none() {
            abort_login(&profile_dir, we_created_dir);
            bail!(
                "Claude exited without an authenticated session, so profile '{}' was \
                 not registered. Retry with: cswitch login {}",
                name,
                name
            );
        }

        // Same Claude account under two profile names is legitimate (isolated
        // settings, separate MCP trust), so this warns rather than fails.
        let email = email.expect("login verdict checked");
        let same_account_as = match email.as_deref() {
            Some(e) => self.profiles_with_email(e, Tool::Claude)?,
            None => Vec::new(),
        };

        let profile = Profile {
            name: name.to_string(),
            tool: Tool::Claude,
            email: email.clone(),
            added: Utc::now(),
            last_used: Some(Utc::now()),
        };
        self.upsert_profile(profile)?;

        Ok(LoginOutcome {
            email,
            same_account_as,
            tool: Tool::Claude,
        })
    }

    fn ensure_target_tool(&self, name: &str, tool: Tool) -> Result<()> {
        if let Some(existing) = self.load_registry()?.profiles.get(name) {
            if matches!(existing.tool, Tool::Unknown(_)) {
                bail!("Profile '{name}' has an unknown tool; cannot use or log in.");
            }
            if existing.tool != tool {
                bail!(
                    "Profile '{name}' already belongs to {}.",
                    existing.tool.label()
                );
            }
        }
        Ok(())
    }

    pub fn codex_identity(&self, name: &str) -> Option<codex::Identity> {
        if self.get_profile(name).ok()?.tool != Tool::Codex {
            return None;
        }
        let bytes = fs::read(self.profile_dir(name).join("auth.json")).ok()?;
        codex::identity_from_auth(&bytes)
    }

    pub fn login_codex_profile(&self, name: &str) -> Result<LoginOutcome> {
        if !safe_shell_name(name) {
            bail!("Invalid profile name.");
        }
        self.ensure_target_tool(name, Tool::Codex)?;
        let profile_dir = self.profile_dir(name);
        let we_created_dir = !profile_dir.exists();
        if !we_created_dir && profile_dir.read_dir()?.next().is_some() {
            bail!(
                "Profile '{name}' already exists and holds an account. Delete it first or pick a different name."
            );
        }
        fs::create_dir_all(&profile_dir)?;
        let result = (|| -> Result<LoginOutcome> {
            self.seed_codex_profile_dir(&profile_dir)?;
            let copied: Vec<&str> = CODEX_SEED_ALLOWLIST
                .iter()
                .copied()
                .filter(|entry| fs::symlink_metadata(profile_dir.join(*entry)).is_ok())
                .collect();
            println!("{}", codex_seed_message(self.codex_source_label, &copied));
            println!("Opening your browser — sign in to ChatGPT for profile '{name}'.");
            let logged_in = std::process::Command::new("codex")
                .arg("login")
                .env("CODEX_HOME", &profile_dir)
                .status()
                .context("Failed to launch codex. Is it installed and in your PATH?")?
                .success();
            let status = if logged_in {
                std::process::Command::new("codex")
                    .args(["login", "status"])
                    .env("CODEX_HOME", &profile_dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .context("Could not check Codex login status")?
                    .success()
            } else {
                false
            };
            let auth_path = profile_dir.join("auth.json");
            let auth = if status {
                fs::read(&auth_path).ok()
            } else {
                None
            };
            let identity =
                codex_login_verdict(logged_in, status, auth_path.exists(), auth.as_deref())
                    .map_err(|reason| anyhow::anyhow!("{}", reason.message(name)))?;
            let email = identity.map(|identity| identity.email);
            let same_account_as = match email.as_deref() {
                Some(email) => self.profiles_with_email(email, Tool::Codex)?,
                None => Vec::new(),
            };
            self.upsert_profile(Profile {
                name: name.to_string(),
                tool: Tool::Codex,
                email: email.clone(),
                added: Utc::now(),
                last_used: Some(Utc::now()),
            })?;
            Ok(LoginOutcome {
                email,
                same_account_as,
                tool: Tool::Codex,
            })
        })();
        if result.is_err() {
            abort_login(&profile_dir, we_created_dir);
        }
        result
    }

    pub fn login_agy_profile(&self, name: &str) -> Result<LoginOutcome> {
        agy::ensure_supported(cfg!(unix))?;
        if !safe_shell_name(name) {
            bail!("Invalid profile name.");
        }
        self.ensure_target_tool(name, Tool::Antigravity)?;
        let profile_dir = self.profile_dir(name);
        let we_created_dir = match fs::symlink_metadata(&profile_dir) {
            Ok(meta) if meta.is_dir() => false,
            Ok(_) => bail!("Antigravity profile path is not a directory."),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        };
        if !we_created_dir && profile_dir.read_dir()?.next().is_some() {
            bail!(
                "Profile '{name}' already exists and holds an account. Delete it first or pick a different name."
            );
        }
        fs::create_dir_all(&profile_dir)?;
        let result = (|| -> Result<LoginOutcome> {
            let real_home = self
                .base_dir
                .parent()
                .context("Profile base has no parent")?;
            agy::link_farm(real_home, &profile_dir)?;
            let home = agy::profile_home(&profile_dir);
            agy::seed_gemini(real_home, &home)?;
            let copied: Vec<&str> = agy::AGY_SEED_ALLOWLIST
                .iter()
                .copied()
                .filter(|relative| {
                    fs::symlink_metadata(home.join(".gemini").join(relative)).is_ok()
                })
                .collect();
            if copied.is_empty() {
                println!("Antigravity seed from ~/.gemini: nothing to copy.");
            } else {
                println!(
                    "Antigravity seed from ~/.gemini: copied {}.",
                    copied.join(", ")
                );
            }
            println!("Sign in to Antigravity for profile '{name}', then exit agy.");
            std::process::Command::new(agy::AGY_PROGRAM)
                .env("HOME", &home)
                .status()
                .context("Failed to launch agy. Is it installed and in your PATH?")?;
            let token = agy::token_state(&home);
            let models_ok = if token == TokenState::NonEmpty {
                std::process::Command::new(agy::AGY_PROGRAM)
                    .args(agy::AGY_AUTH_CHECK_ARGS)
                    .env("HOME", &home)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .context("Could not check Antigravity login status")?
                    .success()
            } else {
                false
            };
            agy_login_verdict(token, models_ok)
                .map_err(|reason| anyhow::anyhow!(reason.message()))?;
            let email = fs::read(home.join(agy::AGY_TOKEN_RELATIVE))
                .ok()
                .and_then(|bytes| agy::identity_from_token(&bytes));
            let same_account_as = match email.as_deref() {
                Some(email) => self.profiles_with_email(email, Tool::Antigravity)?,
                None => Vec::new(),
            };
            self.upsert_profile(Profile {
                name: name.to_string(),
                tool: Tool::Antigravity,
                email: email.clone(),
                added: Utc::now(),
                last_used: Some(Utc::now()),
            })?;
            Ok(LoginOutcome {
                email,
                same_account_as,
                tool: Tool::Antigravity,
            })
        })();
        if result.is_err() {
            if we_created_dir {
                abort_login(&profile_dir, true);
            } else {
                agy::cleanup_staged_home(&profile_dir);
            }
        }
        result
    }

    fn seed_codex_profile_dir(&self, profile_dir: &Path) -> Result<bool> {
        seed_codex_from(&self.codex_home, profile_dir)
    }

    /// Names of already-registered profiles authenticated as `email`.
    /// Case-insensitive: Claude echoes the address as the user typed it.
    pub fn profiles_with_email(&self, email: &str, tool: Tool) -> Result<Vec<String>> {
        let target = email.trim().to_lowercase();
        let mut names: Vec<String> = self
            .load_registry()?
            .profiles
            .into_values()
            .filter(|p| {
                p.tool == tool
                    && p.email
                        .as_deref()
                        .map(|e| e.trim().to_lowercase() == target)
                        .unwrap_or(false)
            })
            .map(|p| p.name)
            .collect();
        names.sort();
        Ok(names)
    }

    /// Print shell alias/function lines for all managed profiles.
    /// Auto-detects platform: bash/zsh on Unix, PowerShell on Windows.
    pub fn generate_aliases(&self) -> Result<String> {
        let profiles = self.list_profiles()?;
        if profiles.is_empty() {
            return Ok("# No profiles found. Add one with: cswitch add <name>".to_string());
        }

        if cfg!(target_os = "windows") {
            self.generate_powershell_aliases(&profiles)
        } else {
            self.generate_shell_aliases(&profiles)
        }
    }

    fn generate_shell_aliases(&self, profiles: &[Profile]) -> Result<String> {
        let mut lines = vec![
            "# claude-switch aliases — add to ~/.zshrc or ~/.bashrc".to_string(),
            "# Generated by: cswitch aliases".to_string(),
            String::new(),
        ];
        for p in profiles {
            let Some(prefix) = p.tool.alias_prefix() else {
                continue;
            };
            let comment = p
                .email
                .as_deref()
                .map(|e| format!("  # {}", e.replace(['\r', '\n'], " ")))
                .unwrap_or_default();
            lines.push(limit_alias_line(format!(
                "alias {}={}{}",
                shell_word(&format!("{prefix}-{}", p.name)),
                shell_quote(&format!("cswitch use {}", shell_word(&p.name))),
                comment
            )));
        }
        Ok(lines.join("\n"))
    }

    fn generate_powershell_aliases(&self, profiles: &[Profile]) -> Result<String> {
        let mut lines = vec![
            "# claude-switch aliases — add to your PowerShell profile".to_string(),
            "# Run: notepad $PROFILE  to edit your profile".to_string(),
            "# Generated by: cswitch aliases".to_string(),
            String::new(),
        ];
        for p in profiles {
            let Some(prefix) = p.tool.alias_prefix() else {
                continue;
            };
            let comment = p
                .email
                .as_deref()
                .map(|e| format!("  # {}", e.replace(['\r', '\n'], " ")))
                .unwrap_or_default();
            lines.push(limit_alias_line(format!(
                "function {} {{ cswitch use {} @args }}{}",
                powershell_word(&format!("{prefix}-{}", p.name)),
                powershell_word(&p.name),
                comment
            )));
        }
        Ok(lines.join("\n"))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Copy the warm half of the live `~/.claude` setup into a fresh profile
    /// dir, then strip everything account-specific so Claude prompts for login.
    ///
    /// Returns `false` without copying anything when there is no live
    /// `~/.claude` to seed from — a first-ever login is still a clean
    /// empty-directory login.
    fn seed_profile_dir(&self, profile_dir: &Path, include_history: bool) -> Result<bool> {
        let src = &self.claude_home;
        if !src.exists() {
            return Ok(false);
        }

        copy_dir_all_filtered(src, profile_dir, &seed_skip(include_history))?;
        self.seed_skills_from(&src.join("skills"), profile_dir);

        // Account metadata lives at the home root, not inside ~/.claude.
        let home_claude_json = src
            .parent()
            .context("Claude home has no parent")?
            .join(".claude.json");
        if home_claude_json.exists() {
            fs::copy(&home_claude_json, profile_dir.join(".claude.json"))?;
        }

        // Force a fresh login: no credentials, no stale identity.
        let creds = profile_dir.join(".credentials.json");
        if creds.exists() {
            fs::remove_file(&creds)?;
        }
        sanitize_claude_json(&profile_dir.join(".claude.json"))?;
        strip_copied_helper(&profile_dir.join("settings.json"), &self.base_dir)?;

        Ok(true)
    }

    /// Copy a source config dir into a named profile and register it.
    ///
    /// `include_history` keeps conversation content (see [`SEED_SKIP_HISTORY`]);
    /// `force` replaces an existing profile instead of erroring.
    fn copy_and_register(
        &self,
        name: &str,
        src: &Path,
        include_history: bool,
        force: bool,
    ) -> Result<Profile> {
        self.ensure_target_tool(name, Tool::Claude)?;
        let src = std::path::absolute(src)?;
        if !src.exists() {
            bail!("Source directory '{}' does not exist.", src.display());
        }
        if force {
            remove_key(&self.base_dir, name)?;
        }
        let dest = self.profiles_dir.join(name);
        if dest.exists() {
            if force {
                fs::remove_dir_all(&dest)?;
            } else {
                bail!(
                    "Profile '{}' already exists. Use --force to overwrite.",
                    name
                );
            }
        }
        copy_dir_all_filtered(&src, &dest, &seed_skip(include_history))?;
        strip_copied_helper(&dest.join("settings.json"), &self.base_dir)?;
        self.seed_skills_from(&src.join("skills"), &dest);
        let email = read_email_from_dir(&dest);
        let profile = Profile {
            name: name.to_string(),
            tool: Tool::Claude,
            email,
            added: Utc::now(),
            last_used: None,
        };
        self.upsert_profile(profile.clone())?;
        Ok(profile)
    }

    fn seed_skills_from(&self, source_skills: &Path, profile_dir: &Path) {
        let Some(name) = profile_dir.file_name() else {
            eprintln!("cswitch: warning: could not determine profile name for skills sync");
            return;
        };
        let result = skills_sync::sync_skills(
            source_skills,
            &profile_dir.join("skills"),
            &self.base_dir.join("backups/skills").join(name),
            &SyncOptions {
                dry_run: false,
                adopt: Vec::new(),
            },
        );
        match result {
            Ok(report) if report.has_failures() => {
                eprintln!("cswitch: warning: some skills could not be linked while seeding");
            }
            Err(e) => eprintln!("cswitch: warning: could not link skills while seeding: {e}"),
            _ => {}
        }
    }

    fn upsert_profile(&self, profile: Profile) -> Result<()> {
        let mut registry = self.load_registry()?;
        registry.profiles.insert(profile.name.clone(), profile);
        self.save_registry(&registry)
    }
}

fn safe_shell_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn limit_alias_line(line: String) -> String {
    if line.chars().count() <= 120 {
        return line;
    }
    if let Some((command, comment)) = line.rsplit_once("  # ") {
        let command_len = command.chars().count();
        if command_len <= 120 {
            let available = 120 - command_len;
            if available < 5 {
                return command.to_string();
            }
            return format!(
                "{command}  # {}",
                comment.chars().take(available - 4).collect::<String>()
            );
        }
        return command.to_string();
    }
    line
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexLoginRefusal {
    LoginFailed,
    StatusFailed,
    MissingAuth,
}

impl CodexLoginRefusal {
    fn message(self, name: &str) -> String {
        match self {
            Self::LoginFailed => format!(
                "Codex login did not complete for profile '{name}'. Nothing was registered."
            ),
            Self::StatusFailed => {
                format!("Codex login status failed for profile '{name}'. Nothing was registered.")
            }
            Self::MissingAuth => "Codex did not leave auth.json. Nothing was registered.".into(),
        }
    }
}

fn codex_login_verdict(
    login_ok: bool,
    status_ok: bool,
    auth_exists: bool,
    auth: Option<&[u8]>,
) -> std::result::Result<Option<codex::Identity>, CodexLoginRefusal> {
    if !login_ok {
        return Err(CodexLoginRefusal::LoginFailed);
    }
    if !status_ok {
        return Err(CodexLoginRefusal::StatusFailed);
    }
    if !auth_exists {
        return Err(CodexLoginRefusal::MissingAuth);
    }
    Ok(auth.and_then(codex::identity_from_auth))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgyLoginRefusal {
    MissingToken,
    EmptyToken,
    ModelsFailed,
}

impl AgyLoginRefusal {
    fn message(self) -> &'static str {
        match self {
            Self::MissingToken => {
                "Antigravity did not leave a login token. Nothing was registered."
            }
            Self::EmptyToken => "Antigravity left an empty login token. Nothing was registered.",
            Self::ModelsFailed => "Antigravity models check failed. Nothing was registered.",
        }
    }
}

fn agy_login_verdict(
    token: TokenState,
    models_ok: bool,
) -> std::result::Result<(), AgyLoginRefusal> {
    match token {
        TokenState::Missing => return Err(AgyLoginRefusal::MissingToken),
        TokenState::Empty => return Err(AgyLoginRefusal::EmptyToken),
        TokenState::NonEmpty => {}
    }
    if !models_ok {
        return Err(AgyLoginRefusal::ModelsFailed);
    }
    Ok(())
}

const CODEX_SEED_ALLOWLIST: &[&str] = &["config.toml", "AGENTS.md", "agents", "rules", "skills"];

fn codex_seed_message(source: &str, copied: &[&str]) -> String {
    if copied.is_empty() {
        format!("Codex seed from {source}: nothing to copy.")
    } else {
        format!("Codex seed from {source}: copied {}.", copied.join(", "))
    }
}

fn seed_codex_from(source: &Path, destination: &Path) -> Result<bool> {
    if !source.is_dir() {
        return Ok(false);
    }
    for name in CODEX_SEED_ALLOWLIST {
        let from = source.join(name);
        let metadata = match fs::symlink_metadata(&from) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let to = destination.join(name);
        if metadata.file_type().is_symlink() {
            copy_symlink(&from, &to)?;
        } else if metadata.is_dir() {
            copy_dir_all_filtered(&from, &to, &[])?;
        } else if metadata.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(true)
}

fn launch_sync_summary(changed: &[&str]) -> Option<String> {
    if changed.is_empty() {
        return None;
    }
    let noun = if changed.len() == 1 {
        "skill"
    } else {
        "skills"
    };
    Some(format!(
        "cswitch: synced {} {noun} ({})",
        changed.len(),
        changed.join(", ")
    ))
}

fn shell_word(value: &str) -> String {
    if safe_shell_name(value) {
        value.to_string()
    } else {
        shell_quote(value)
    }
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn powershell_word(value: &str) -> String {
    if safe_shell_name(value) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

// ── First-run detection ───────────────────────────────────────────────────────

/// Account details read from the live `~/.claude` directory.
pub struct DetectedAccount {
    pub email: Option<String>,
    #[allow(dead_code)]
    pub config_dir: std::path::PathBuf,
}

/// Try to read the currently logged-in Claude account.
/// Checks both `~/.claude/` (config dir) and `~/.claude.json` (home root)
/// since on macOS the account metadata lives at the root, not inside the dir.
/// Returns `None` if neither exists.
pub fn detect_current_account() -> Option<DetectedAccount> {
    let home = dirs::home_dir()?;
    let config_dir = home.join(".claude");
    if !config_dir.exists() {
        return None;
    }
    // Try ~/.claude/ first, then fallback to ~/.claude.json at home root
    let email = read_email_from_dir(&config_dir).or_else(|| read_email_from_home_root(&home));
    Some(DetectedAccount { email, config_dir })
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Top-level entries inside a Claude config dir that are never copied into a
/// profile.
///
/// Two independent reasons, both load-bearing:
///
/// - **Privacy.** `projects/` holds full session transcripts and `history.jsonl`
///   holds every prompt ever typed. Copying them puts one account's
///   conversations inside another account's profile — the exact separation a
///   profile exists to create.
/// - **Size.** Transcripts and file-history dominate a real config dir (187 MB
///   of 198 MB on the machine this was developed against), and none of it is
///   state Claude needs in order to start warm.
///
/// Note the name collision this list does *not* touch: the `projects` **key**
/// inside `.claude.json` carries per-directory trust and MCP-server approvals
/// and is deliberately preserved. Only the `projects/` **directory** — the
/// transcripts — is skipped.
/// Conversation content. Skipped by default; kept with `--include-history`.
///
/// Separating a profile's sessions is usually the *point* — but a user who
/// relies on `claude --resume` across a switch can opt back in.
const SEED_SKIP_HISTORY: &[&str] = &[
    "projects",      // session transcripts (NOT the .claude.json "projects" key)
    "history.jsonl", // every prompt ever typed
    "transcripts",   // ses_*.jsonl — verbatim prompts, tool calls and results
    "plans",         // plan-mode documents, written from conversation
    "file-history",
    "todos",
];

/// Machine-local caches and runtime state. Never copied, under any flag —
/// stale here at best, confusing at worst.
const SEED_SKIP_ALWAYS: &[&str] = &[
    "skills",
    "sessions",
    "session-env",
    "shell-snapshots",
    "paste-cache",
    "tasks",
    "jobs",
    "daemon",
    "daemon.log",
    "backups",
    "telemetry",
    "cache",
    "ide",
    "statsig",
    "debug",            // debug logs for this machine's sessions
    "usage-data",       // per-account token accounting
    "stats-cache.json", // per-day message and tool-call counts
];

/// Build the skip list for a seed operation.
fn seed_skip(include_history: bool) -> Vec<&'static str> {
    let mut skip = SEED_SKIP_ALWAYS.to_vec();
    if !include_history {
        skip.extend_from_slice(SEED_SKIP_HISTORY);
    }
    skip
}

// ── Live-session detection ────────────────────────────────────────────────────

/// Paths a running Claude session writes to, and that no seed operation copies.
///
/// Every entry here is in [`SEED_SKIP_ALWAYS`], which is the point: a fresh
/// `cswitch add` gives every copied file a current mtime, so anything copyable
/// would read as "active" the moment it was created. These are only ever
/// written by a real session in that directory.
const SESSION_ACTIVITY_MARKERS: &[&str] = &["sessions", "session-env", "shell-snapshots"];
const CODEX_ACTIVITY_MARKERS: &[&str] = &["sessions", "log", "shell_snapshots"];

/// How long after its last write a profile is still treated as possibly in use.
///
/// Generous on purpose. An open-but-idle session touches its files only every
/// few minutes, so a tight window would report "idle" for precisely the session
/// this guard exists to protect. A false positive costs one extra line in a
/// confirmation dialog; a false negative costs another account's credentials.
pub const SESSION_ACTIVE_WINDOW_SECS: u64 = 30 * 60;

/// Most recent write at `path`, looking one level into a directory.
///
/// A directory's own mtime moves only when entries are added or removed, so a
/// session rewriting an existing session file in place would look idle if the
/// directory alone were consulted.
fn newest_write(path: &Path) -> Option<SystemTime> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    let mut newest = meta.modified().ok();
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            if let Ok(t) = entry.path().symlink_metadata().and_then(|m| m.modified()) {
                newest = Some(newest.map_or(t, |n| n.max(t)));
            }
        }
    }
    newest
}

fn newest_write_tree(path: &Path) -> Option<SystemTime> {
    let mut newest = None;
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&next) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if let Ok(time) = metadata.modified() {
            newest = Some(newest.map_or(time, |previous: SystemTime| previous.max(time)));
        }
        if metadata.is_dir()
            && let Ok(entries) = fs::read_dir(next)
        {
            pending.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    newest
}

/// Phrase an age the way the confirmation dialogs report it.
pub fn describe_age(seconds: u64) -> String {
    match seconds {
        0..=90 => "seconds ago".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

/// Keys stripped from a seeded `.claude.json` when the profile is going to hold
/// a *different* account.
///
/// `projects` is deliberately absent — it holds the per-directory trust and MCP
/// approvals that are the entire reason to seed a profile instead of starting
/// empty.
const IDENTITY_KEYS: &[&str] = &[
    "oauthAccount",             // email, org, account uuid, billing type
    "userID",                   // per-account analytics id; Claude regenerates it
    "orgModelDefaultCache",     // org-scoped
    "penguinModeOrgEnabled",    // org-scoped
    "claudeAiMcpEverConnected", // account-bound connector list
    "cachedUsageUtilization",
    "cachedExtraUsageDisabledReason",
    "modelAccessCache",
    "additionalModelCostsCache",
    "additionalModelOptionsCache",
    "primaryApiKey",
    "customApiKeyResponses",
];

/// Recursive copy that skips a set of **top-level** entry names.
///
/// Pass an empty `skip_top_level` for a plain full copy.
///
/// The filter is intentionally shallow: the skipped names are all
/// top-level entries of a Claude config dir, and matching at every depth would
/// silently drop a user's own directory that happened to share a name (a
/// project literally called `cache/`, for instance).
///
/// `fs::copy` carries the source permission bits across, so `.credentials.json`
/// keeps its `0600` mode rather than landing world-readable.
pub(crate) fn copy_dir_all_filtered(src: &Path, dst: &Path, skip_top_level: &[&str]) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip_top_level.iter().any(|s| name == *s) {
            continue;
        }
        let dest_path = dst.join(&name);
        let file_type = entry.file_type()?;
        // Symlinks are checked first, and deliberately: on Unix `file_type`
        // does not follow links, so a symlink *to a directory* reports itself
        // as neither dir nor file. It would otherwise fall through to
        // `fs::copy`, which fails with "Is a directory" and aborts the whole
        // profile creation. Config directories really do contain these —
        // linking a skill from a repo into `~/.claude/skills/` is common.
        if file_type.is_symlink() {
            copy_symlink(&entry.path(), &dest_path)?;
        } else if file_type.is_dir() {
            copy_dir_all_filtered(&entry.path(), &dest_path, &[])?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

/// Recreate a symlink in the profile, pointing where the original pointed.
///
/// Linked content is kept linked rather than duplicated, so a skill symlinked
/// out of a repository keeps tracking that repository from every profile.
///
/// Relative targets cannot be copied verbatim. `skills/x -> ../../repo/x`
/// resolves against the *link's own* directory, and a profile lives somewhere
/// else entirely — copied as-is it would silently point at nothing. So a
/// relative target is resolved to an absolute one first.
pub(crate) fn copy_symlink(link: &Path, dest: &Path) -> Result<()> {
    let raw = fs::read_link(link)?;
    let target = if raw.is_absolute() {
        raw
    } else {
        // `canonicalize` resolves the link against its real location. It fails
        // on a dangling link, and that is not worth failing a profile over:
        // fall back to joining, which reproduces the same broken link rather
        // than aborting.
        fs::canonicalize(link)
            .unwrap_or_else(|_| link.parent().unwrap_or(Path::new(".")).join(&raw))
    };
    symlink_to(&target, dest)
        .with_context(|| format!("Failed to recreate symlink {}", dest.display()))?;
    Ok(())
}

#[cfg(unix)]
fn symlink_to(target: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}

#[cfg(windows)]
fn symlink_to(target: &Path, dest: &Path) -> std::io::Result<()> {
    let made = if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, dest)
    } else {
        std::os::windows::fs::symlink_file(target, dest)
    };
    // Windows only allows symlink creation under Developer Mode or elevation.
    // Copying the target keeps the profile usable everywhere; it simply stops
    // tracking the source from that point on.
    match made {
        Ok(()) => Ok(()),
        Err(_) if target.is_dir() => copy_dir_all_filtered(target, dest, &[])
            .map_err(|e| std::io::Error::other(e.to_string())),
        Err(_) => fs::copy(target, dest).map(|_| ()),
    }
}

/// Remove the keys that identify a specific Claude account from a profile's
/// `.claude.json`, leaving the warm state (trust, MCP approvals, onboarding)
/// intact. No-op if the file is missing or unparseable.
/// Roll back a failed login attempt.
///
/// Only removes the staging directory when *this* attempt created it, so a
/// cancelled login can never delete a directory that already existed. Cleanup
/// failure is deliberately swallowed: the caller is already reporting the real
/// error, and a leftover directory is recoverable while a masked cause is not.
fn abort_login(profile_dir: &Path, we_created_dir: bool) {
    if we_created_dir {
        let _ = fs::remove_dir_all(profile_dir);
    }
}

fn sanitize_claude_json(path: &Path) -> Result<()> {
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(());
    };
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(());
    };
    if let Some(obj) = val.as_object_mut() {
        for key in IDENTITY_KEYS {
            obj.remove(*key);
        }
    }
    fs::write(path, serde_json::to_string_pretty(&val)?)?;
    Ok(())
}

/// Extract credentials from the platform's native credential store.
/// - macOS: Keychain via `security`
/// - Windows: Credential Manager via PowerShell
/// - Linux: returns None (credentials are file-based in ~/.claude/.credentials.json,
///   already copied by copy_dir_all)
fn extract_platform_credentials() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return extract_macos_keychain();
    }
    #[cfg(target_os = "windows")]
    {
        return extract_windows_credentials();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn extract_macos_keychain() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let creds = String::from_utf8(output.stdout).ok()?.trim().to_string();
    // Validate it's JSON
    serde_json::from_str::<serde_json::Value>(&creds).ok()?;
    Some(creds)
}

#[cfg(target_os = "windows")]
fn extract_windows_credentials() -> Option<String> {
    // Claude Code on Windows stores credentials in Credential Manager.
    // Use PowerShell to extract them.
    let script = r#"
        $cred = Get-StoredCredential -Target "Claude Code-credentials" -ErrorAction SilentlyContinue
        if ($cred) {
            $cred.GetNetworkCredential().Password
        } else {
            # Fallback: try cmdkey-based extraction via generic credentials
            $bytes = [System.Text.Encoding]::Unicode.GetBytes("")
            $vault = New-Object Windows.Security.Credentials.PasswordVault
            try {
                $entry = $vault.Retrieve("Claude Code-credentials", "")
                $entry.RetrievePassword()
                $entry.Password
            } catch { }
        }
    "#;

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let creds = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if creds.is_empty() {
        return None;
    }
    // Validate it's JSON
    serde_json::from_str::<serde_json::Value>(&creds).ok()?;
    Some(creds)
}

/// Ask Claude which account a profile is actually authenticated as.
///
/// Authoritative where the config files are not: it reflects the live
/// credential, not whatever metadata happens to be on disk. Returns `None` if
/// the profile is logged out or the CLI is too old to have `auth status`.
fn read_login_status(profile_dir: &Path) -> Option<serde_json::Value> {
    let output = std::process::Command::new("claude")
        .args(["auth", "status", "--json"])
        .env("CLAUDE_CONFIG_DIR", profile_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Read email from `~/.claude.json` at home root (macOS stores account metadata here).
fn read_email_from_home_root(home: &Path) -> Option<String> {
    let path = home.join(".claude.json");
    if let Ok(content) = fs::read_to_string(&path)
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(email) = val
            .get("oauthAccount")
            .and_then(|o| o.get("emailAddress"))
            .and_then(|e| e.as_str())
    {
        return Some(email.to_string());
    }
    None
}

/// Extract the account email from a Claude config directory.
/// Checks `.claude.json` → `oauthAccount.emailAddress`, then
/// `.credentials.json` → `claudeAiOauth.email` as fallback.
fn read_email_from_dir(dir: &Path) -> Option<String> {
    for filename in &[".claude.json", "claude.json"] {
        if let Ok(content) = fs::read_to_string(dir.join(filename))
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(email) = val
                .get("oauthAccount")
                .and_then(|o| o.get("emailAddress"))
                .and_then(|e| e.as_str())
        {
            return Some(email.to_string());
        }
    }
    if let Ok(content) = fs::read_to_string(dir.join(".credentials.json"))
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(email) = val
            .get("claudeAiOauth")
            .and_then(|o| o.get("email"))
            .and_then(|e| e.as_str())
    {
        return Some(email.to_string());
    }
    None
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limits, read_limits};
    use std::fs;
    use tempfile::TempDir;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Construct a ProfileManager fully isolated inside a temp directory.
    fn make_manager(tmp: &TempDir) -> ProfileManager {
        let base_dir = tmp.path().join(".claude-switch");
        ProfileManager::with_paths(base_dir, tmp.path().join(".claude")).unwrap()
    }

    /// Populate a fake `~/.claude` directory with the two files Claude Code
    /// actually writes: `.claude.json` and `.credentials.json`.
    fn make_claude_dir(root: &Path, email: &str) -> PathBuf {
        let dir = root.to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        // .claude.json — contains oauthAccount block
        let claude_json = serde_json::json!({
            "oauthAccount": {
                "emailAddress": email,
                "accountUuid": "uuid-0000-test"
            },
            "someOtherConfig": true
        });
        fs::write(
            dir.join(".claude.json"),
            serde_json::to_string_pretty(&claude_json).unwrap(),
        )
        .unwrap();

        // .credentials.json — contains OAuth tokens
        let creds_json = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access_tok",
                "refreshToken": "refresh_tok",
                "expiresAt": 9_999_999_999_u64,
                "scopes": ["user:inference"],
                "subscriptionType": "max"
            }
        });
        fs::write(
            dir.join(".credentials.json"),
            serde_json::to_string_pretty(&creds_json).unwrap(),
        )
        .unwrap();

        dir
    }

    /// Same but email is only in `.credentials.json` to test the fallback path.
    fn make_claude_dir_creds_only(root: &Path, email: &str) -> PathBuf {
        let dir = root.to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        let creds_json = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "tok",
                "email": email
            }
        });
        fs::write(
            dir.join(".credentials.json"),
            serde_json::to_string_pretty(&creds_json).unwrap(),
        )
        .unwrap();

        dir
    }

    // ── read_email_from_dir ───────────────────────────────────────────────────

    #[test]
    fn email_read_from_claude_json() {
        let tmp = TempDir::new().unwrap();
        let dir = make_claude_dir(tmp.path(), "oauth@test.com");
        assert_eq!(read_email_from_dir(&dir), Some("oauth@test.com".into()));
    }

    #[test]
    fn email_fallback_to_credentials_json() {
        let tmp = TempDir::new().unwrap();
        let dir = make_claude_dir_creds_only(tmp.path(), "creds@test.com");
        assert_eq!(read_email_from_dir(&dir), Some("creds@test.com".into()));
    }

    #[test]
    fn email_returns_none_when_no_config_files() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path()).unwrap();
        assert_eq!(read_email_from_dir(tmp.path()), None);
    }

    // ── copy_dir_all ──────────────────────────────────────────────────────────

    #[test]
    fn copy_dir_all_copies_flat_files() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), "hello").unwrap();
        fs::write(src.join("b.txt"), "world").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all_filtered(&src, &dst, &[]).unwrap();

        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::read_to_string(dst.join("b.txt")).unwrap(), "world");
    }

    #[test]
    fn copy_dir_all_copies_nested_directories() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub/deep")).unwrap();
        fs::write(src.join("root.txt"), "root").unwrap();
        fs::write(src.join("sub").join("mid.txt"), "mid").unwrap();
        fs::write(src.join("sub/deep").join("leaf.txt"), "leaf").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all_filtered(&src, &dst, &[]).unwrap();

        assert_eq!(fs::read_to_string(dst.join("root.txt")).unwrap(), "root");
        assert_eq!(fs::read_to_string(dst.join("sub/mid.txt")).unwrap(), "mid");
        assert_eq!(
            fs::read_to_string(dst.join("sub/deep/leaf.txt")).unwrap(),
            "leaf"
        );
    }

    // ── Registry I/O ──────────────────────────────────────────────────────────

    #[test]
    fn load_registry_returns_empty_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let reg = mgr.load_registry().unwrap();
        assert!(reg.profiles.is_empty());
    }

    #[test]
    fn save_and_load_registry_round_trips() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);

        let mut reg = Registry::default();
        reg.profiles.insert(
            "work".into(),
            Profile {
                name: "work".into(),
                tool: Tool::Claude,
                email: Some("work@acme.com".into()),
                added: Utc::now(),
                last_used: None,
            },
        );
        mgr.save_registry(&reg).unwrap();

        let loaded = mgr.load_registry().unwrap();
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(
            loaded.profiles["work"].email.as_deref(),
            Some("work@acme.com")
        );
        assert!(loaded.profiles["work"].last_used.is_none());
    }

    // ── add_profile_from ──────────────────────────────────────────────────────

    #[test]
    fn add_profile_copies_files_into_profiles_dir() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "u@test.com");

        mgr.add_profile_from("work", &src).unwrap();

        let dest = mgr.profile_dir("work");
        assert!(dest.join(".claude.json").exists(), ".claude.json missing");
        assert!(
            dest.join(".credentials.json").exists(),
            ".credentials.json missing"
        );
    }

    #[test]
    fn add_profile_records_email_from_config() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "email@test.com");

        let p = mgr.add_profile_from("personal", &src).unwrap();

        assert_eq!(p.email.as_deref(), Some("email@test.com"));
    }

    #[test]
    fn add_profile_records_entry_in_registry() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "x@y.com");

        mgr.add_profile_from("slot", &src).unwrap();

        let reg = mgr.load_registry().unwrap();
        assert!(reg.profiles.contains_key("slot"));
    }

    #[test]
    fn add_profile_stores_none_email_when_config_unreadable() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);

        // Source dir exists but contains no recognisable config files
        let src = tmp.path().join("empty-claude");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("something-unrelated.txt"), "hi").unwrap();

        let p = mgr.add_profile_from("mystery", &src).unwrap();
        assert!(p.email.is_none());
    }

    #[test]
    fn add_profile_errors_on_nonexistent_source() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let err = mgr
            .add_profile_from("bad", &tmp.path().join("does-not-exist"))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn add_profile_errors_on_duplicate_without_force() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "a@b.com");

        mgr.add_profile_from("dup", &src).unwrap();
        let err = mgr.add_profile_from("dup", &src).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    // ── add_profile_from_force ────────────────────────────────────────────────

    #[test]
    fn force_add_overwrites_existing_profile() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);

        let src = make_claude_dir(&tmp.path().join("v1"), "first@test.com");
        mgr.add_profile_from("slot", &src).unwrap();

        // Change source to a different account
        let src2 = make_claude_dir(&tmp.path().join("v2"), "second@test.com");
        mgr.add_profile_from_force("slot", &src2).unwrap();

        let reg = mgr.load_registry().unwrap();
        assert_eq!(
            reg.profiles["slot"].email.as_deref(),
            Some("second@test.com")
        );
        // Old files replaced
        let content = fs::read_to_string(mgr.profile_dir("slot").join(".claude.json")).unwrap();
        assert!(content.contains("second@test.com"));
    }

    #[test]
    fn force_add_works_when_profile_does_not_yet_exist() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "new@test.com");

        let p = mgr.add_profile_from_force("brand-new", &src).unwrap();
        assert_eq!(p.name, "brand-new");
        assert_eq!(p.email.as_deref(), Some("new@test.com"));
    }

    // ── list_profiles ─────────────────────────────────────────────────────────

    #[test]
    fn list_profiles_returns_empty_vec_when_none_added() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        assert!(mgr.list_profiles().unwrap().is_empty());
    }

    #[test]
    fn list_profiles_returns_sorted_by_name() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);

        for name in &["zebra", "alpha", "mango"] {
            let src = make_claude_dir(
                &tmp.path().join(format!("src-{name}")),
                &format!("{name}@test.com"),
            );
            mgr.add_profile_from(name, &src).unwrap();
        }

        let profiles = mgr.list_profiles().unwrap();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alpha", "mango", "zebra"]);
    }

    // ── remove_profile ────────────────────────────────────────────────────────

    #[test]
    fn remove_profile_deletes_directory_and_registry_entry() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "del@test.com");
        mgr.add_profile_from("to-delete", &src).unwrap();

        mgr.remove_profile("to-delete").unwrap();

        assert!(!mgr.profile_dir("to-delete").exists());
        assert!(
            !mgr.load_registry()
                .unwrap()
                .profiles
                .contains_key("to-delete")
        );
    }

    #[test]
    fn remove_profile_errors_when_profile_not_found() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let err = mgr.remove_profile("ghost").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn remove_profile_leaves_other_profiles_intact() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);

        for name in &["keep", "delete-me"] {
            let src = make_claude_dir(&tmp.path().join(name), &format!("{name}@x.com"));
            mgr.add_profile_from(name, &src).unwrap();
        }

        mgr.remove_profile("delete-me").unwrap();

        let profiles = mgr.list_profiles().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "keep");
    }

    #[test]
    fn failed_key_deletion_preserves_profile_and_registry() {
        // Known-bad: remove_profile deletes the profile directory before a key removal error.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        mgr.add_profile_from("saved", &source).unwrap();
        let profile_dir = mgr.profile_dir("saved");
        let registry_before = fs::read(&mgr.registry_path).unwrap();
        let settings_before = fs::read(profile_dir.join("settings.json")).unwrap();

        let key_as_directory = crate::key::key_path(&mgr.base_dir, "saved");
        fs::create_dir_all(&key_as_directory).unwrap();
        fs::write(key_as_directory.join("child"), "synthetic").unwrap();

        assert!(mgr.remove_profile("saved").is_err());
        assert_eq!(fs::read(&mgr.registry_path).unwrap(), registry_before);
        assert_eq!(
            fs::read(profile_dir.join("settings.json")).unwrap(),
            settings_before
        );
        assert!(key_as_directory.join("child").exists());
    }

    // ── get_profile ───────────────────────────────────────────────────────────

    #[test]
    fn get_profile_returns_correct_entry() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "found@test.com");
        mgr.add_profile_from("found", &src).unwrap();

        let p = mgr.get_profile("found").unwrap();
        assert_eq!(p.name, "found");
        assert_eq!(p.email.as_deref(), Some("found@test.com"));
    }

    #[test]
    fn get_profile_errors_when_missing() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let err = mgr.get_profile("nope").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    // ── profile_dir ───────────────────────────────────────────────────────────

    #[test]
    fn profile_dir_returns_correct_path() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        assert_eq!(mgr.profile_dir("foo"), mgr.profiles_dir.join("foo"));
    }

    // ── generate_aliases ──────────────────────────────────────────────────────

    #[test]
    fn generate_aliases_when_empty_returns_hint() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let out = mgr.generate_aliases().unwrap();
        assert!(out.contains("No profiles"), "{out}");
    }

    #[test]
    fn generate_aliases_routes_each_profile_through_cswitch_use() {
        // Direct CLAUDE_CONFIG_DIR aliases bypass the launch-time sync.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);

        for name in &["work", "personal"] {
            let src = make_claude_dir(&tmp.path().join(name), &format!("{name}@x.com"));
            mgr.add_profile_from(name, &src).unwrap();
        }

        let profiles = mgr.list_profiles().unwrap();
        let bash = mgr.generate_shell_aliases(&profiles).unwrap();
        assert!(
            bash.contains("alias claude-work='cswitch use work'"),
            "{bash}"
        );
        assert!(
            bash.contains("alias claude-personal='cswitch use personal'"),
            "{bash}"
        );
        let powershell = mgr.generate_powershell_aliases(&profiles).unwrap();
        assert!(
            powershell.contains("function claude-work { cswitch use work @args }"),
            "{powershell}"
        );
        assert!(!bash.contains("CLAUDE_CONFIG_DIR="), "{bash}");
    }

    // ── login_profile ──────────────────────────────────────────────────────

    // ── profiles_with_email ───────────────────────────────────────────────

    #[test]
    fn profiles_with_email_finds_every_profile_on_that_account() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        for (name, email) in [
            ("work", "same@x.com"),
            ("solo", "same@x.com"),
            ("alt", "other@x.com"),
        ] {
            let src = make_claude_dir(&tmp.path().join(name), email);
            mgr.add_profile_from(name, &src).unwrap();
        }

        assert_eq!(
            mgr.profiles_with_email("same@x.com", Tool::Claude).unwrap(),
            vec!["solo", "work"]
        );
        assert_eq!(
            mgr.profiles_with_email("other@x.com", Tool::Claude)
                .unwrap(),
            vec!["alt"]
        );
    }

    #[test]
    fn profiles_with_email_ignores_case_and_padding() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("work"), "Me@Example.COM");
        mgr.add_profile_from("work", &src).unwrap();

        assert_eq!(
            mgr.profiles_with_email("  me@example.com ", Tool::Claude)
                .unwrap(),
            vec!["work"]
        );
    }

    #[test]
    fn profiles_with_email_returns_empty_for_an_unseen_account() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("work"), "me@x.com");
        mgr.add_profile_from("work", &src).unwrap();

        assert!(
            mgr.profiles_with_email("nobody@x.com", Tool::Claude)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn profiles_with_unknown_email_never_match_each_other() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        // A profile whose email could not be read stores None. Two of those are
        // not evidence of a shared account, so they must not be reported as one.
        let src = make_claude_dir_creds_only(&tmp.path().join("a"), "unreadable");
        mgr.add_profile_from("a", &src).unwrap();

        assert!(
            mgr.profiles_with_email("", Tool::Claude)
                .unwrap()
                .is_empty()
        );
    }

    // ── abort_login ───────────────────────────────────────────────────────

    #[test]
    fn abort_login_removes_only_a_directory_this_attempt_created() {
        let tmp = TempDir::new().unwrap();
        let staged = tmp.path().join("staged");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("settings.json"), "{}").unwrap();

        abort_login(&staged, true);

        assert!(
            !staged.exists(),
            "a staged dir must not survive a failed login"
        );
    }

    #[test]
    fn abort_login_never_deletes_a_preexisting_directory() {
        let tmp = TempDir::new().unwrap();
        let existing = tmp.path().join("existing");
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join(".credentials.json"), "{}").unwrap();

        abort_login(&existing, false);

        assert!(existing.exists());
        assert!(existing.join(".credentials.json").exists());
    }

    #[test]
    fn login_profile_failure_leaves_other_profiles_byte_for_byte_intact() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "keep@x.com");
        mgr.add_profile_from("keep", &src).unwrap();

        let creds = mgr.profile_dir("keep").join(".credentials.json");
        let before = fs::read(&creds).unwrap();
        let registry_before = fs::read_to_string(&mgr.registry_path).unwrap();

        // Refusing a taken name is the failure path a user hits most often.
        assert!(
            mgr.login_profile("keep", false, None, LoginMethod::ClaudeAi)
                .is_err()
        );

        assert_eq!(fs::read(&creds).unwrap(), before);
        assert_eq!(
            fs::read_to_string(&mgr.registry_path).unwrap(),
            registry_before
        );
    }

    #[test]
    fn login_profile_rejects_existing_nonempty_dir() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "a@b.com");
        mgr.add_profile_from("taken", &src).unwrap();

        // login_profile should refuse because the dir is non-empty
        let err = mgr
            .login_profile("taken", false, None, LoginMethod::ClaudeAi)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    // ── read_email_from_home_root ─────────────────────────────────────────

    #[test]
    fn read_email_from_home_root_finds_oauth_account() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let claude_json = serde_json::json!({
            "oauthAccount": {
                "emailAddress": "root@test.com",
                "accountUuid": "uuid"
            },
            "numStartups": 42
        });
        fs::write(
            root.join(".claude.json"),
            serde_json::to_string_pretty(&claude_json).unwrap(),
        )
        .unwrap();

        assert_eq!(
            read_email_from_home_root(root),
            Some("root@test.com".into())
        );
    }

    #[test]
    fn read_email_from_home_root_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read_email_from_home_root(tmp.path()), None);
    }

    #[test]
    fn generate_aliases_includes_email_comment() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        mgr.add_profile_from("work", &src).unwrap();

        let out = mgr.generate_aliases().unwrap();
        assert!(out.contains("# me@work.com"), "{out}");
    }

    // ── Profile seeding ───────────────────────────────────────────────────────

    #[test]
    fn filtered_copy_skips_named_top_level_entries() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("projects")).unwrap();
        fs::create_dir_all(src.join("skills")).unwrap();
        fs::write(src.join("projects/transcript.jsonl"), "secret").unwrap();
        fs::write(src.join("history.jsonl"), "every prompt").unwrap();
        fs::write(src.join("settings.json"), "{}").unwrap();
        fs::write(src.join("skills/a.md"), "keep me").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all_filtered(&src, &dst, &seed_skip(false)).unwrap();

        assert!(
            !dst.join("projects").exists(),
            "transcripts must not be copied"
        );
        assert!(
            !dst.join("history.jsonl").exists(),
            "prompt history must not be copied"
        );
        assert!(
            dst.join("settings.json").exists(),
            "settings must be copied"
        );
        assert!(!dst.join("skills").exists(), "skills must be linked later");
    }

    #[test]
    fn filtered_copy_only_matches_at_top_level() {
        // A user directory that happens to share a skipped name must survive
        // when it is nested — the filter is deliberately shallow.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("custom/skills/cache")).unwrap();
        fs::write(src.join("custom/skills/cache/keep.txt"), "nested").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all_filtered(&src, &dst, &seed_skip(false)).unwrap();

        assert!(dst.join("custom/skills/cache/keep.txt").exists());
    }

    #[test]
    fn unfiltered_copy_still_copies_everything() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("projects")).unwrap();
        fs::write(src.join("projects/t.jsonl"), "x").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all_filtered(&src, &dst, &[]).unwrap();

        assert!(dst.join("projects/t.jsonl").exists());
    }

    #[test]
    fn every_directory_holding_conversation_content_is_skipped_by_default() {
        // `projects` alone is not the whole of it. Claude also writes verbatim
        // transcripts to `transcripts/` and plan-mode documents to `plans/`,
        // and a profile seeded for a *different account* must not inherit
        // either — that is the entire point of seeding without history.
        for dir in ["projects", "transcripts", "plans", "history.jsonl"] {
            assert!(
                seed_skip(false).contains(&dir),
                "'{dir}' holds conversation content and must not be copied by default"
            );
            assert!(
                !seed_skip(true).contains(&dir),
                "'{dir}' must come back under --include-history"
            );
        }
    }

    #[test]
    fn conversation_content_does_not_reach_a_seeded_profile() {
        // The list above is only worth as much as the copy that honours it.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake-claude"), "old@test.com");

        fs::create_dir_all(src.join("transcripts")).unwrap();
        fs::write(
            src.join("transcripts").join("ses_abc.jsonl"),
            r#"{"type":"user","content":"a private prompt"}"#,
        )
        .unwrap();
        fs::create_dir_all(src.join("plans")).unwrap();
        fs::write(src.join("plans").join("plan.md"), "# secret plan").unwrap();

        mgr.add_profile_from("work", &src).unwrap();
        let dest = mgr.profile_dir("work");

        assert!(!dest.join("transcripts").exists(), "transcripts leaked");
        assert!(!dest.join("plans").exists(), "plans leaked");
    }

    #[test]
    fn usage_accounting_is_never_copied() {
        // Per-account token accounting and activity counts describe the source
        // account, not the profile — stale at best, misleading at worst.
        for entry in ["usage-data", "stats-cache.json", "debug"] {
            assert!(
                seed_skip(true).contains(&entry),
                "'{entry}' must be skipped even with --include-history"
            );
        }
    }

    #[test]
    fn sanitize_strips_identity_but_keeps_trust_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".claude.json");
        let original = serde_json::json!({
            "oauthAccount": { "emailAddress": "old@work.com" },
            "userID": "abc123",
            "claudeAiMcpEverConnected": ["gmail"],
            "cachedUsageUtilization": { "five_hour": 12 },
            "projects": { "/home/me/repo": { "enabledMcpjsonServers": ["playwright"] } },
            "hasCompletedOnboarding": true
        });
        fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        sanitize_claude_json(&path).unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for key in IDENTITY_KEYS {
            assert!(out.get(*key).is_none(), "{key} should have been stripped");
        }
        // The whole point of seeding: trust + MCP approvals survive.
        assert_eq!(
            out["projects"]["/home/me/repo"]["enabledMcpjsonServers"][0],
            "playwright"
        );
        assert_eq!(out["hasCompletedOnboarding"], true);
    }

    #[test]
    fn sanitize_is_a_noop_on_missing_or_invalid_file() {
        let tmp = TempDir::new().unwrap();
        sanitize_claude_json(&tmp.path().join("absent.json")).unwrap();

        let bad = tmp.path().join("bad.json");
        fs::write(&bad, "not json at all").unwrap();
        sanitize_claude_json(&bad).unwrap();
        assert_eq!(fs::read_to_string(&bad).unwrap(), "not json at all");
    }

    #[test]
    fn a_newly_seeded_profile_has_no_source_limit_snapshot() {
        // Known-bad: removing cachedUsageUtilization from IDENTITY_KEYS leaks the source account's numbers.
        let tmp = TempDir::new().unwrap();
        let manager = make_manager(&tmp);
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let source = serde_json::json!({
            "oauthAccount": {"accountUuid": "00000000-0000-4000-8000-000000000001"},
            "cachedUsageUtilization": {
                "accountUuid": "00000000-0000-4000-8000-000000000001",
                "fetchedAtMs": 1_894_021_200_000_i64,
                "utilization": {"limits": [
                    {"kind":"session", "group":"session", "percent":40}
                ]}
            },
            "projects": {"/synthetic/project": {"trusted": true}}
        });
        let source_path = tmp.path().join(".claude.json");
        fs::write(&source_path, serde_json::to_vec(&source).unwrap()).unwrap();
        let dest = manager.profile_dir("new");
        manager.seed_profile_dir(&dest, false).unwrap();
        assert!(matches!(read_limits(tmp.path()), Limits::Snapshot(_)));
        assert_eq!(read_limits(&dest), Limits::NoSnapshot);
        let output: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(output["projects"], source["projects"]);
    }

    #[test]
    fn include_history_keeps_conversations_but_never_machine_state() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("projects")).unwrap();
        fs::create_dir_all(src.join("shell-snapshots")).unwrap();
        fs::write(src.join("projects/t.jsonl"), "conversation").unwrap();
        fs::write(src.join("history.jsonl"), "prompts").unwrap();
        fs::write(src.join("shell-snapshots/s.sh"), "machine-local").unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all_filtered(&src, &dst, &seed_skip(true)).unwrap();

        assert!(
            dst.join("projects/t.jsonl").exists(),
            "opt-in keeps transcripts"
        );
        assert!(
            dst.join("history.jsonl").exists(),
            "opt-in keeps prompt history"
        );
        assert!(
            !dst.join("shell-snapshots").exists(),
            "machine-local state is skipped under every flag"
        );
    }

    #[test]
    fn add_profile_does_not_copy_conversation_history() {
        // Regression guard: `add` clones the *current* account, so it keeps
        // credentials — but transcripts and prompt history are still private
        // and must never land in a profile directory.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        fs::create_dir_all(src.join("projects")).unwrap();
        fs::write(src.join("projects/session.jsonl"), "private conversation").unwrap();
        fs::write(src.join("history.jsonl"), "every prompt").unwrap();

        mgr.add_profile_from("work", &src).unwrap();
        let dest = mgr.profile_dir("work");

        assert!(!dest.join("projects").exists());
        assert!(!dest.join("history.jsonl").exists());
        // ...while the things that make a profile warm are still there.
        assert!(dest.join(".claude.json").exists());
        assert!(
            dest.join(".credentials.json").exists(),
            "add keeps the same account logged in"
        );
    }

    // ── Live-session detection ────────────────────────────────────────────────

    /// Backdate a path's mtime so a test can describe an old session.
    fn backdate(path: &Path, secs: u64) {
        let when = SystemTime::now() - std::time::Duration::from_secs(secs);
        // A directory cannot be opened for writing, and does not need to be:
        // `set_modified` is futimens, which only wants a handle.
        let file = fs::File::options()
            .write(true)
            .open(path)
            .or_else(|_| fs::File::open(path))
            .unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn session_markers_can_never_arrive_by_copy() {
        // This is what makes a fresh mtime mean "a session ran here". Copying
        // stamps every file it writes with the current time, so a marker that
        // any seed could copy would make a brand-new profile look occupied.
        for marker in SESSION_ACTIVITY_MARKERS {
            assert!(
                SEED_SKIP_ALWAYS.contains(marker),
                "'{marker}' is copyable, so `cswitch add` would look like a live session"
            );
        }
    }

    #[test]
    fn a_profile_that_was_just_written_reads_as_maybe_in_use() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let dir = mgr.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session-env"), "live").unwrap();

        assert!(mgr.seconds_since_session_write("work").unwrap() < 60);
        assert!(mgr.maybe_in_use("work").is_some());
    }

    #[test]
    fn a_profile_left_alone_past_the_window_is_not_in_use() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let dir = mgr.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session-env"), "done").unwrap();
        backdate(&dir.join("session-env"), SESSION_ACTIVE_WINDOW_SECS + 60);

        assert!(mgr.seconds_since_session_write("work").unwrap() > SESSION_ACTIVE_WINDOW_SECS);
        assert!(mgr.maybe_in_use("work").is_none());
    }

    #[test]
    fn the_most_recent_marker_wins() {
        // Any one live marker means a session touched the profile, even if the
        // others have been sitting untouched for hours.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let dir = mgr.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session-env"), "old").unwrap();
        fs::write(dir.join("shell-snapshots"), "new").unwrap();
        backdate(&dir.join("session-env"), 86_400);

        assert!(mgr.maybe_in_use("work").is_some());
    }

    #[test]
    fn a_profile_with_no_markers_reports_unknown_not_idle() {
        // A profile can be warm, registered, and simply never launched. That is
        // absence of evidence — callers must not render it as "safe".
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let dir = mgr.profile_dir("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".credentials.json"), "{}").unwrap();

        assert_eq!(mgr.seconds_since_session_write("work"), None);
        assert_eq!(mgr.maybe_in_use("work"), None);
    }

    #[test]
    fn a_missing_profile_directory_reports_unknown() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        assert_eq!(mgr.seconds_since_session_write("never-existed"), None);
    }

    #[test]
    fn a_rewritten_file_inside_a_marker_directory_still_counts() {
        // A directory's own mtime moves only when entries are added or removed.
        // Consulting it alone would call an actively-writing session idle.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let sessions = mgr.profile_dir("work").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("current.jsonl"), "turn").unwrap();
        backdate(&sessions, 86_400);

        let dir_only = fs::metadata(&sessions).unwrap().modified().unwrap();
        let seen = newest_write(&sessions).unwrap();
        assert!(seen > dir_only, "must look past the directory's own mtime");
        assert!(mgr.maybe_in_use("work").is_some());
    }

    // ── Symlinked config content ──────────────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn a_symlinked_skill_does_not_abort_profile_creation() {
        // The reported failure mode: `file_type()` does not follow links, so a
        // symlink to a directory used to reach `fs::copy` and come back with
        // "Is a directory", killing the whole copy.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        let repo_skill = tmp.path().join("repo/skills/checkpoint");
        fs::create_dir_all(&repo_skill).unwrap();
        fs::write(repo_skill.join("SKILL.md"), "# checkpoint").unwrap();
        fs::create_dir_all(src.join("skills")).unwrap();
        std::os::unix::fs::symlink(&repo_skill, src.join("skills/checkpoint")).unwrap();

        mgr.add_profile_from("work", &src).unwrap();

        let copied = mgr.profile_dir("work").join("skills/checkpoint");
        assert!(copied.is_symlink(), "the link must stay a link");
        assert_eq!(
            fs::read_to_string(copied.join("SKILL.md")).unwrap(),
            "# checkpoint"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_relative_link_is_absolutized_so_it_still_resolves() {
        // `../../repo/x` resolves against the link's own directory. A profile
        // lives elsewhere, so copying the target verbatim would point at
        // nothing — silently, which is the dangerous part.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        let repo_skill = tmp.path().join("repo/skills/checkpoint");
        fs::create_dir_all(&repo_skill).unwrap();
        fs::write(repo_skill.join("SKILL.md"), "linked").unwrap();
        fs::create_dir_all(src.join("skills")).unwrap();
        // Relative to the link's own directory, `<tmp>/fake/skills/`.
        std::os::unix::fs::symlink(
            "../../repo/skills/checkpoint",
            src.join("skills/checkpoint"),
        )
        .unwrap();

        mgr.add_profile_from("work", &src).unwrap();

        let copied = mgr.profile_dir("work").join("skills/checkpoint");
        assert!(fs::read_link(&copied).unwrap().is_absolute());
        assert_eq!(
            fs::read_to_string(copied.join("SKILL.md")).unwrap(),
            "linked"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_absolute_link_is_preserved_verbatim() {
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        let target = tmp.path().join("elsewhere/notes.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "notes").unwrap();
        std::os::unix::fs::symlink(&target, src.join("notes.md")).unwrap();

        mgr.add_profile_from("work", &src).unwrap();

        let copied = mgr.profile_dir("work").join("notes.md");
        assert_eq!(fs::read_link(&copied).unwrap(), target);
    }

    #[test]
    #[cfg(unix)]
    fn a_dangling_link_is_reproduced_rather_than_fatal() {
        // A broken link in `~/.claude` is the user's existing state. Copying it
        // faithfully is honest; refusing to create the profile is not.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        std::os::unix::fs::symlink("/nonexistent/target", src.join("dangling")).unwrap();

        mgr.add_profile_from("work", &src).unwrap();

        let copied = mgr.profile_dir("work").join("dangling");
        assert!(copied.is_symlink());
        assert!(!copied.exists(), "still dangling, as it was");
    }

    #[test]
    #[cfg(unix)]
    fn linked_content_is_never_duplicated_into_the_profile() {
        // The point of keeping the link: edits at the source reach every
        // profile, instead of each profile freezing its own stale copy.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("fake"), "me@work.com");
        let repo_skill = tmp.path().join("repo/skills/checkpoint");
        fs::create_dir_all(&repo_skill).unwrap();
        fs::write(repo_skill.join("SKILL.md"), "v1").unwrap();
        fs::create_dir_all(src.join("skills")).unwrap();
        std::os::unix::fs::symlink(&repo_skill, src.join("skills/checkpoint")).unwrap();
        mgr.add_profile_from("work", &src).unwrap();

        fs::write(repo_skill.join("SKILL.md"), "v2").unwrap();

        let copied = mgr.profile_dir("work").join("skills/checkpoint/SKILL.md");
        assert_eq!(fs::read_to_string(copied).unwrap(), "v2");
    }

    #[test]
    #[cfg(unix)]
    fn seeding_links_skills_and_never_carries_synced() {
        // A recursive skills copy would carry account-managed synced state.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let source = make_claude_dir(&mgr.claude_home, "test-account");
        let source_skills = source.join("skills");
        fs::create_dir_all(source_skills.join("plain")).unwrap();
        fs::write(source_skills.join("plain/SKILL.md"), "plain").unwrap();
        fs::create_dir_all(source_skills.join("synced/bucket")).unwrap();
        fs::write(source_skills.join("synced/bucket/SKILL.md"), "separate").unwrap();
        let repo_skill = tmp.path().join("repo/linked");
        fs::create_dir_all(&repo_skill).unwrap();
        fs::write(repo_skill.join("SKILL.md"), "linked").unwrap();
        std::os::unix::fs::symlink(&repo_skill, source_skills.join("linked")).unwrap();

        let profile_dir = mgr.profile_dir("work");
        fs::create_dir_all(&profile_dir).unwrap();
        assert!(mgr.seed_profile_dir(&profile_dir, false).unwrap());
        assert_eq!(
            fs::read_link(profile_dir.join("skills/plain")).unwrap(),
            source_skills.join("plain")
        );
        assert_eq!(
            fs::read_link(profile_dir.join("skills/linked")).unwrap(),
            source_skills.join("linked")
        );
        assert_eq!(
            fs::read_to_string(profile_dir.join("skills/linked/SKILL.md")).unwrap(),
            "linked"
        );
        assert!(!profile_dir.join("skills/synced").exists());
    }

    #[test]
    fn sync_error_does_not_block_launch_preparation() {
        // An eager sync error used to stop the launch before Claude could run.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let src = make_claude_dir(&tmp.path().join("seed"), "test-account");
        mgr.add_profile_from("work", &src).unwrap();
        fs::create_dir_all(&mgr.claude_home).unwrap();
        fs::write(mgr.claude_home.join("skills"), "not a directory").unwrap();

        let prepared = mgr.prepare_launch("work").unwrap();
        assert_eq!(prepared.spec.env_value, mgr.profile_dir("work"));
        assert!(mgr.get_profile("work").unwrap().last_used.is_some());
    }

    #[test]
    fn sync_requires_a_registered_profile() {
        // A directory alone is not an account that `sync --all` should manage.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let opts = SyncOptions {
            dry_run: false,
            adopt: Vec::new(),
        };
        assert!(mgr.sync_skills("missing", &opts).is_err());
        assert!(!mgr.profile_dir("missing").exists());
    }

    #[test]
    fn launch_summary_uses_singular_for_one_skill_and_plural_for_more() {
        // A fixed "skills" suffix produces the visible "1 skills" error.
        assert_eq!(launch_sync_summary(&[]), None);
        assert_eq!(
            launch_sync_summary(&["x"]),
            Some("cswitch: synced 1 skill (x)".to_string())
        );
        assert_eq!(
            launch_sync_summary(&["x", "y"]),
            Some("cswitch: synced 2 skills (x, y)".to_string())
        );
    }

    #[test]
    fn describe_age_reads_as_a_sentence() {
        assert_eq!(describe_age(3), "seconds ago");
        assert_eq!(describe_age(90), "seconds ago");
        assert_eq!(describe_age(240), "4 min ago");
        assert_eq!(describe_age(7_200), "2 h ago");
    }

    #[test]
    fn login_args_include_console_only_when_requested() {
        // Known-bad: the Console flag is dropped while keeping the email hint.
        assert_eq!(
            login_args(LoginMethod::Console, Some(" user@example.com ")),
            ["auth", "login", "--console", "--email", "user@example.com"]
        );
        assert_eq!(
            login_args(LoginMethod::ClaudeAi, Some("user@example.com")),
            ["auth", "login", "--email", "user@example.com"]
        );
    }

    #[test]
    fn console_login_verdict_allows_null_email_but_requires_managed_source() {
        // Known-bad: requiring an email for Console rejects a valid managed-key login.
        let console = serde_json::json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","apiKeySource":"/login managed key","email":null});
        assert_eq!(login_verdict(LoginMethod::Console, &console).unwrap(), None);
        assert!(login_verdict(LoginMethod::ClaudeAi, &console).is_err());
        let subscription = serde_json::json!({"loggedIn":true,"authMethod":"claude.ai","email":"user@example.com"});
        assert_eq!(
            login_verdict(LoginMethod::ClaudeAi, &subscription).unwrap(),
            Some("user@example.com".to_string())
        );
        assert!(login_verdict(LoginMethod::Console, &subscription).is_err());
        let logged_out = serde_json::json!({"loggedIn":false,"apiKeySource":"/login managed key"});
        assert!(login_verdict(LoginMethod::Console, &logged_out).is_err());
        let wrong_source = serde_json::json!({"loggedIn":true,"apiKeySource":"apiKeyHelper"});
        assert!(login_verdict(LoginMethod::Console, &wrong_source).is_err());
    }

    #[test]
    fn login_seed_strips_new_identity_keys() {
        // Known-bad: omitting either primaryApiKey or customApiKeyResponses leaks the source login.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        fs::create_dir_all(&mgr.claude_home).unwrap();
        fs::write(mgr.claude_home.parent().unwrap().join(".claude.json"), serde_json::json!({
            "primaryApiKey":"synthetic", "customApiKeyResponses":{"approved":["synthetic"]}, "projects":{"/synthetic": {"trusted":true}}
        }).to_string()).unwrap();
        let dest = mgr.profile_dir("new");
        fs::create_dir_all(&dest).unwrap();
        mgr.seed_profile_dir(&dest, false).unwrap();
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.join(".claude.json")).unwrap()).unwrap();
        assert!(result.get("primaryApiKey").is_none());
        assert!(result.get("customApiKeyResponses").is_none());
        assert_eq!(result["projects"]["/synthetic"]["trusted"], true);
    }

    #[test]
    fn seeded_and_copied_settings_drop_source_managed_helper() {
        // Known-bad: a copied helper or base URL sends the new login to the source gateway.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        fs::create_dir_all(&mgr.claude_home).unwrap();
        fs::create_dir_all(mgr.base_dir.join("keys")).unwrap();
        fs::write(
            mgr.base_dir.join("keys/source.gateway"),
            r#"["ANTHROPIC_BASE_URL","ANTHROPIC_MODEL"]"#,
        )
        .unwrap();
        let settings = serde_json::json!({"theme":"dark", "apiKeyHelper":"'/opt/tools/cswitch' key print source", "env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com","ANTHROPIC_MODEL":"vendor/claude-model","OTHER":"keep"}});
        fs::write(mgr.claude_home.join("settings.json"), settings.to_string()).unwrap();
        let dest = mgr.profile_dir("seeded");
        fs::create_dir_all(&dest).unwrap();
        mgr.seed_profile_dir(&dest, false).unwrap();
        let seeded: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.join("settings.json")).unwrap()).unwrap();
        assert!(seeded.get("apiKeyHelper").is_none());
        assert_eq!(seeded["theme"], "dark");
        assert_eq!(seeded["env"], serde_json::json!({"OTHER":"keep"}));
        mgr.add_profile_from("copied", &mgr.claude_home).unwrap();
        let copied: serde_json::Value = serde_json::from_slice(
            &fs::read(mgr.profile_dir("copied").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert!(copied.get("apiKeyHelper").is_none());
        assert_eq!(copied["theme"], "dark");
        assert_eq!(copied["env"], serde_json::json!({"OTHER":"keep"}));
    }

    #[test]
    fn t1_seed_and_copy_drop_base_url_without_source_manifest() {
        // Known-bad: relying on the source manifest leaves an older helper's base URL in a new profile.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        fs::create_dir_all(&mgr.claude_home).unwrap();
        let settings = serde_json::json!({
            "apiKeyHelper": "'/opt/tools/cswitch' key print source",
            "env": {"ANTHROPIC_BASE_URL": "https://gateway.example.com", "OTHER": "keep"},
            "theme": "dark"
        });
        fs::write(mgr.claude_home.join("settings.json"), settings.to_string()).unwrap();
        assert!(!crate::key::manifest_path(&mgr.base_dir, "source").exists());

        let seeded_dir = mgr.profile_dir("seeded");
        fs::create_dir_all(&seeded_dir).unwrap();
        mgr.seed_profile_dir(&seeded_dir, false).unwrap();
        let seeded: serde_json::Value =
            serde_json::from_slice(&fs::read(seeded_dir.join("settings.json")).unwrap()).unwrap();
        assert!(seeded.get("apiKeyHelper").is_none());
        assert_eq!(seeded["env"], serde_json::json!({"OTHER":"keep"}));
        assert_eq!(seeded["theme"], "dark");

        mgr.add_profile_from("copied", &mgr.claude_home).unwrap();
        let copied: serde_json::Value = serde_json::from_slice(
            &fs::read(mgr.profile_dir("copied").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert!(copied.get("apiKeyHelper").is_none());
        assert_eq!(copied["env"], serde_json::json!({"OTHER":"keep"}));
        assert_eq!(copied["theme"], "dark");
    }

    #[test]
    fn remove_and_refresh_delete_saved_key() {
        // Known-bad: deleting or refreshing a profile leaves a key or gateway manifest orphaned.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        mgr.add_profile_from("remove", &source).unwrap();
        mgr.add_profile_from("refresh", &source).unwrap();
        fs::create_dir_all(mgr.base_dir.join("keys")).unwrap();
        fs::write(crate::key::key_path(&mgr.base_dir, "remove"), "synthetic\n").unwrap();
        fs::write(
            crate::key::key_path(&mgr.base_dir, "refresh"),
            "synthetic\n",
        )
        .unwrap();
        fs::write(
            crate::key::manifest_path(&mgr.base_dir, "remove"),
            r#"["ANTHROPIC_BASE_URL"]"#,
        )
        .unwrap();
        fs::write(
            crate::key::manifest_path(&mgr.base_dir, "refresh"),
            r#"["ANTHROPIC_BASE_URL"]"#,
        )
        .unwrap();
        mgr.remove_profile("remove").unwrap();
        assert!(!crate::key::has_key(&mgr, "remove"));
        assert!(!crate::key::manifest_path(&mgr.base_dir, "remove").exists());
        mgr.add_profile_from_force("refresh", &source).unwrap();
        assert!(!crate::key::has_key(&mgr, "refresh"));
        assert!(!crate::key::manifest_path(&mgr.base_dir, "refresh").exists());
    }

    #[test]
    fn g11_bad_manifest_refuses_remove_or_refresh_before_profile_changes() {
        // Known-bad: profile removal or refresh changes the registry before a manifest deletion fails.
        let tmp = TempDir::new().unwrap();
        let mgr = make_manager(&tmp);
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        mgr.add_profile_from("remove", &source).unwrap();
        mgr.add_profile_from("refresh", &source).unwrap();
        fs::create_dir_all(mgr.base_dir.join("keys/remove.gateway")).unwrap();
        fs::create_dir_all(mgr.base_dir.join("keys/refresh.gateway")).unwrap();
        let registry = fs::read(mgr.base_dir.join("registry.json")).unwrap();
        assert!(mgr.remove_profile("remove").is_err());
        assert!(mgr.add_profile_from_force("refresh", &source).is_err());
        assert_eq!(
            fs::read(mgr.base_dir.join("registry.json")).unwrap(),
            registry
        );
        assert!(mgr.profile_dir("remove").join("settings.json").exists());
        assert!(mgr.profile_dir("refresh").join("settings.json").exists());
    }
}

#[cfg(all(test, unix))]
mod stage_c_tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn manager(temp: &TempDir) -> ProfileManager {
        ProfileManager::with_base_dir(temp.path().join(".claude-switch")).unwrap()
    }

    fn register(manager: &ProfileManager, name: &str) {
        let mut registry = manager.load_registry().unwrap();
        registry.profiles.insert(
            name.into(),
            Profile {
                name: name.into(),
                tool: Tool::Antigravity,
                email: None,
                added: Utc::now(),
                last_used: None,
            },
        );
        manager.save_registry(&registry).unwrap();
    }

    #[test]
    fn antigravity_registry_roundtrip_and_alias() {
        // Known-bad: antigravity remains Unknown and loses its agy- alias on save.
        let temp = TempDir::new().unwrap();
        let manager = manager(&temp);
        let raw = r#"{"profiles":{"g":{"name":"g","tool":"antigravity","email":"g@example.com","added":"2030-01-01T00:00:00Z","last_used":null}}}"#;
        fs::write(&manager.registry_path, raw).unwrap();
        assert_eq!(manager.get_profile("g").unwrap().tool, Tool::Antigravity);
        manager
            .save_registry(&manager.load_registry().unwrap())
            .unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&manager.registry_path).unwrap()).unwrap();
        assert_eq!(saved["profiles"]["g"]["tool"], "antigravity");
        let shell = manager
            .generate_shell_aliases(&manager.list_profiles().unwrap())
            .unwrap();
        let powershell = manager
            .generate_powershell_aliases(&manager.list_profiles().unwrap())
            .unwrap();
        assert!(shell.contains("agy-g="), "{shell}");
        assert!(powershell.contains("function agy-g"), "{powershell}");
    }

    #[test]
    fn agy_launch_spec_sets_only_fake_home() {
        // Known-bad: a Codex-or-else-Claude branch launches agy with CLAUDE_CONFIG_DIR.
        let dir = PathBuf::from("/synthetic/profile");
        assert_eq!(
            launch_spec(Tool::Antigravity, dir.clone()).unwrap(),
            LaunchSpec {
                program: agy::AGY_PROGRAM,
                env_key: "HOME",
                env_value: dir.join("home"),
            }
        );
    }

    #[test]
    fn agy_missing_token_refuses_registration() {
        // Known-bad: trusting sign-in's exit code alone registers a profile with no token.
        assert_eq!(
            agy_login_verdict(TokenState::Missing, false)
                .unwrap_err()
                .message(),
            "Antigravity did not leave a login token. Nothing was registered."
        );
    }

    #[test]
    fn agy_empty_token_refuses_registration() {
        // Known-bad: mere presence of an empty token passes login verification.
        assert_eq!(
            agy_login_verdict(TokenState::Empty, false)
                .unwrap_err()
                .message(),
            "Antigravity left an empty login token. Nothing was registered."
        );
    }

    #[test]
    fn agy_models_failure_refuses_registration() {
        // Known-bad: a populated token is trusted even when agy models rejects it.
        assert_eq!(
            agy_login_verdict(TokenState::NonEmpty, false)
                .unwrap_err()
                .message(),
            "Antigravity models check failed. Nothing was registered."
        );
    }

    #[test]
    fn agy_login_verdict_accepts_wired_success() {
        // Known-bad: valid sign-in, token and models check still refuse registration.
        assert_eq!(agy_login_verdict(TokenState::NonEmpty, true), Ok(()));
    }

    #[test]
    fn agy_activity_walk_does_not_follow_farm_link() {
        // Known-bad: metadata() follows a linked activity directory and reports outside writes.
        let temp = TempDir::new().unwrap();
        let manager = manager(&temp);
        let profile = manager.profile_dir("g");
        let cli = profile.join("home/.gemini/antigravity-cli");
        fs::create_dir_all(&cli).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("fresh"), b"new write").unwrap();
        symlink(&outside, cli.join("log")).unwrap();
        register(&manager, "g");
        assert_eq!(newest_write_tree(&cli.join("log")), None);
        assert_eq!(manager.maybe_in_use("g"), None);
        assert_eq!(fs::read(outside.join("fresh")).unwrap(), b"new write");
    }

    #[test]
    fn agy_activity_refuses_linked_home_ancestor() {
        // Known-bad: checking only the activity leaf walks through a linked HOME ancestor.
        let temp = TempDir::new().unwrap();
        let manager = manager(&temp);
        let profile = manager.profile_dir("g");
        fs::create_dir_all(&profile).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(outside.join(".gemini/antigravity-cli/log")).unwrap();
        fs::write(
            outside.join(".gemini/antigravity-cli/log/fresh"),
            b"outside",
        )
        .unwrap();
        symlink(&outside, profile.join("home")).unwrap();
        register(&manager, "g");
        assert_eq!(manager.maybe_in_use("g"), None);
        assert!(manager.agy_farm_health("g").is_err());
        assert_eq!(
            fs::read(outside.join(".gemini/antigravity-cli/log/fresh")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn agy_claude_only_guards_refuse_valid_claude_fixtures() {
        // Known-bad: a Codex-only guard lets an agy profile use Claude key or skills paths.
        let temp = TempDir::new().unwrap();
        let manager = manager(&temp);
        let profile = manager.profile_dir("g");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("settings.json"), br#"{"theme":"dark"}"#).unwrap();
        fs::write(
            profile.join(".claude.json"),
            br#"{"primaryApiKey":"synthetic"}"#,
        )
        .unwrap();
        fs::create_dir_all(temp.path().join(".claude/skills")).unwrap();
        fs::write(temp.path().join(".claude/skills/entry"), b"warm skill").unwrap();
        register(&manager, "g");
        assert_eq!(
            crate::key::precheck_set_key(&manager, "g", false)
                .unwrap_err()
                .to_string(),
            "API keys are Claude-only."
        );
        assert_eq!(
            crate::key::clear_key(&manager, "g", Utc::now())
                .err()
                .unwrap()
                .to_string(),
            "API keys are Claude-only."
        );
        assert_eq!(
            manager
                .sync_skills(
                    "g",
                    &SyncOptions {
                        dry_run: false,
                        adopt: Vec::new(),
                    }
                )
                .unwrap_err()
                .to_string(),
            "Skills sync is Claude-only."
        );
        assert!(!profile.join("skills").exists());
        assert_eq!(
            fs::read(profile.join("settings.json")).unwrap(),
            br#"{"theme":"dark"}"#
        );
    }
}
