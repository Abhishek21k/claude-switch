//! Local Claude API keys. Key bytes only enter the private store or helper stdout.

use crate::atomic;
use crate::limits::read_claude_json;
use crate::profile::{ProfileManager, shell_quote};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey(Option<&'static str>),
    BrokenKey,
    ForeignHelper,
    Console,
    Subscription,
    NotLoggedIn,
    Unreadable,
}

impl AuthMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ApiKey(Some("console")) => "API key (cswitch), overrides the Console key",
            Self::ApiKey(Some("subscription")) => "API key (cswitch), overrides the subscription",
            Self::ApiKey(_) => "API key (cswitch)",
            Self::BrokenKey => "broken: key file missing",
            Self::ForeignHelper => "apiKeyHelper (not cswitch)",
            Self::Console => "Console (API billing)",
            Self::Subscription => "Claude subscription",
            Self::NotLoggedIn => "not logged in",
            Self::Unreadable => "unreadable",
        }
    }

    pub fn api_billed(&self) -> bool {
        matches!(self, Self::ApiKey(_) | Self::Console)
    }
}

/// The only fields inspected are the presence of identity markers, never values.
pub fn derive_auth_mode(
    name: &str,
    claude: Option<&Value>,
    helper: Option<&str>,
    has_key_file: bool,
) -> AuthMode {
    let console =
        claude.is_some_and(|value| value.get("primaryApiKey").is_some_and(|v| !v.is_null()));
    let subscription =
        claude.is_some_and(|value| value.get("oauthAccount").is_some_and(|v| !v.is_null()));
    if let Some(command) = helper {
        if managed_helper_name(command).as_deref() == Some(name) {
            if !has_key_file {
                return AuthMode::BrokenKey;
            }
            return AuthMode::ApiKey(if console {
                Some("console")
            } else if subscription {
                Some("subscription")
            } else {
                None
            });
        }
        return AuthMode::ForeignHelper;
    }
    if console {
        AuthMode::Console
    } else if subscription {
        AuthMode::Subscription
    } else {
        AuthMode::NotLoggedIn
    }
}

pub fn read_auth_mode(
    manager: &ProfileManager,
    name: &str,
    claude: Result<Option<Value>, ()>,
) -> AuthMode {
    let Ok(claude) = claude else {
        return AuthMode::Unreadable;
    };
    let path = manager.profile_dir(name).join("settings.json");
    let Ok(helper) = read_helper(&path) else {
        return AuthMode::Unreadable;
    };
    derive_auth_mode(
        name,
        claude.as_ref(),
        helper.as_deref(),
        key_path(&manager.base_dir, name).exists(),
    )
}

fn read_helper(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let json: Value = serde_json::from_slice(&fs::read(path)?)?;
    let object = json.as_object().context("settings is not an object")?;
    if object.contains_key("apiKeyHelper")
        && !object.get("apiKeyHelper").is_some_and(Value::is_string)
    {
        bail!("settings has an invalid helper");
    }
    Ok(object
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .map(str::to_string))
}

pub fn key_path(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("keys").join(format!("{name}.key"))
}

pub fn has_key(manager: &ProfileManager, name: &str) -> bool {
    key_path(&manager.base_dir, name).exists()
}

pub fn remove_key(base_dir: &Path, name: &str) -> Result<()> {
    let path = key_path(base_dir, name);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn store_key(base_dir: &Path, name: &str, key: &str) -> Result<()> {
    let directory = base_dir.join("keys");
    if let Ok(metadata) = fs::symlink_metadata(&directory)
        && !metadata.file_type().is_dir()
    {
        bail!("Key directory is not a regular directory.");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(&directory)?;
    let mut bytes = key.as_bytes().to_vec();
    bytes.push(b'\n');
    atomic::write_private(&key_path(base_dir, name), &bytes)
}

pub struct SetOutcome {
    pub running_session: bool,
    pub overrides_subscription: bool,
    pub build_path: bool,
}

pub fn set_key(
    manager: &ProfileManager,
    name: &str,
    input: &str,
    executable: &Path,
    replace_helper: bool,
    now: DateTime<Utc>,
) -> Result<SetOutcome> {
    if !valid_name(name) {
        bail!("Invalid profile name.");
    }
    manager.get_profile(name)?;
    let key = input.trim();
    if !valid_key(key) {
        bail!("API key must contain only alphanumeric characters, dashes, underscores.");
    }
    let executable = fs::canonicalize(executable).context("Cannot resolve cswitch executable")?;
    let helper = format!(
        "{} key print {name}",
        shell_quote(&executable.to_string_lossy())
    );
    let settings = manager.profile_dir(name).join("settings.json");
    // Refuse a foreign or malformed settings file before storing the secret.
    let _ = merge_settings(
        &read_settings(&settings)?.json,
        name,
        Edit::Set(&helper, replace_helper),
    )?;
    store_key(&manager.base_dir, name, key)?;
    edit_settings(
        &settings,
        &manager.base_dir,
        name,
        Edit::Set(&helper, replace_helper),
        now,
        |_| {},
    )?;
    let claude = read_claude_json(&manager.profile_dir(name)).ok().flatten();
    let overrides_subscription = claude
        .as_ref()
        .is_some_and(|v| v.get("oauthAccount").is_some() && v.get("primaryApiKey").is_none());
    Ok(SetOutcome {
        running_session: manager.maybe_in_use(name).is_some(),
        overrides_subscription,
        build_path: executable
            .components()
            .any(|component| component.as_os_str() == "target"),
    })
}

pub struct ClearOutcome {
    pub foreign_helper: bool,
    pub fallback: &'static str,
}

pub fn clear_key(manager: &ProfileManager, name: &str, now: DateTime<Utc>) -> Result<ClearOutcome> {
    if !valid_name(name) {
        bail!("Invalid profile name.");
    }
    manager.get_profile(name)?;
    let settings = manager.profile_dir(name).join("settings.json");
    let result = edit_settings(&settings, &manager.base_dir, name, Edit::Clear, now, |_| {})?;
    remove_key(&manager.base_dir, name)?;
    let claude = read_claude_json(&manager.profile_dir(name)).ok().flatten();
    let fallback = if claude
        .as_ref()
        .is_some_and(|v| v.get("primaryApiKey").is_some())
    {
        "Console key"
    } else if claude
        .as_ref()
        .is_some_and(|v| v.get("oauthAccount").is_some())
    {
        "Claude subscription"
    } else {
        "not logged in"
    };
    Ok(ClearOutcome {
        foreign_helper: result.foreign_helper,
        fallback,
    })
}

/// A strict shell-word parser for cswitch's generated command. Shell operators
/// and expansions make a command foreign even when it contains `key print`.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('"') if c == '"' => quote = None,
            Some('\'') => word.push(c),
            Some('"') if c == '\\' => word.push(chars.next()?),
            Some('"') if matches!(c, '$' | '`') => return None,
            Some('"') => word.push(c),
            Some(_) => return None,
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\' => {
                word.push(chars.next()?);
                started = true;
            }
            None if matches!(
                c,
                ';' | '|' | '&' | '$' | '`' | '<' | '>' | '(' | ')' | '\n'
            ) =>
            {
                return None;
            }
            None => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

pub fn managed_helper_name(command: &str) -> Option<String> {
    let words = shell_words(command)?;
    if words.len() != 4 || words[1] != "key" || words[2] != "print" || !valid_name(&words[3]) {
        return None;
    }
    let file = Path::new(&words[0]).file_name()?.to_str()?;
    if file != "cswitch" && file != "cswitch.exe" {
        return None;
    }
    Some(words[3].clone())
}

#[derive(Clone, Copy)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    mode: u32,
}

impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.modified == other.modified
    }
}

struct SettingsState {
    json: Value,
    bytes: Option<Vec<u8>>,
    stamp: Option<Stamp>,
}

fn stamp(path: &Path) -> Result<Option<Stamp>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        bail!("settings.json is a symlink.");
    }
    if !metadata.is_file() {
        bail!("settings.json is not a regular file.");
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    };
    #[cfg(not(unix))]
    let mode = 0o600;
    Ok(Some(Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        mode,
    }))
}

fn read_settings(path: &Path) -> Result<SettingsState> {
    let before = stamp(path)?;
    let Some(before) = before else {
        return Ok(SettingsState {
            json: Value::Object(Map::new()),
            bytes: None,
            stamp: None,
        });
    };
    let bytes = fs::read(path)?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("settings.json is not valid JSON."))?;
    if !json.is_object() {
        bail!("settings.json is not a JSON object.");
    }
    Ok(SettingsState {
        json,
        bytes: Some(bytes),
        stamp: Some(before),
    })
}

#[derive(Clone, Copy)]
enum Edit<'a> {
    Set(&'a str, bool),
    Clear,
}

struct Merge {
    json: Value,
    changed: bool,
    foreign_helper: bool,
}

fn merge_settings(source: &Value, name: &str, edit: Edit<'_>) -> Result<Merge> {
    let mut json = source.clone();
    let object = json
        .as_object_mut()
        .context("settings.json is not a JSON object.")?;
    let present = object.contains_key("apiKeyHelper");
    let current = object.get("apiKeyHelper").and_then(Value::as_str);
    let foreign = present;
    let managed = current.and_then(managed_helper_name);
    match edit {
        Edit::Set(helper, replace) => {
            if present && managed.as_deref() != Some(name) && !replace {
                bail!(
                    "settings.json has a foreign apiKeyHelper; use --replace-helper to replace it."
                );
            }
            let changed = current != Some(helper);
            object.insert(
                "apiKeyHelper".to_string(),
                Value::String(helper.to_string()),
            );
            Ok(Merge {
                json,
                changed,
                foreign_helper: false,
            })
        }
        Edit::Clear => {
            if managed.as_deref() == Some(name) {
                object.remove("apiKeyHelper");
                Ok(Merge {
                    json,
                    changed: true,
                    foreign_helper: false,
                })
            } else {
                Ok(Merge {
                    json,
                    changed: false,
                    foreign_helper: foreign,
                })
            }
        }
    }
}

struct EditOutcome {
    foreign_helper: bool,
}

fn edit_settings<F: FnMut(usize)>(
    path: &Path,
    base_dir: &Path,
    name: &str,
    edit: Edit<'_>,
    now: DateTime<Utc>,
    mut before_check: F,
) -> Result<EditOutcome> {
    for attempt in 0..2 {
        let state = read_settings(path)?;
        let merged = merge_settings(&state.json, name, edit)?;
        if !merged.changed {
            return Ok(EditOutcome {
                foreign_helper: merged.foreign_helper,
            });
        }
        let bytes = serde_json::to_vec_pretty(&merged.json)?;
        let parent = path.parent().context("settings.json has no parent")?;
        let (temporary, mut file) = atomic::create_private_temp(parent)?;
        let result = (|| -> Result<bool> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(unix)]
            if let Some(old) = state.stamp {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(old.mode))?;
            }
            drop(file);
            before_check(attempt);
            if stamp(path)? != state.stamp {
                return Ok(false);
            }
            if let Some(original) = &state.bytes {
                backup_settings(base_dir, name, original, now)?;
            }
            // The backup itself takes time: compare again immediately before rename.
            if stamp(path)? != state.stamp {
                return Ok(false);
            }
            fs::rename(&temporary, path)?;
            Ok(true)
        })();
        let _ = fs::remove_file(&temporary);
        if result? {
            return Ok(EditOutcome {
                foreign_helper: false,
            });
        }
    }
    bail!("settings.json is changing under us; close Claude sessions of '{name}' and retry")
}

fn backup_settings(base_dir: &Path, name: &str, original: &[u8], now: DateTime<Utc>) -> Result<()> {
    let directory = base_dir.join("backups/settings").join(name);
    fs::create_dir_all(&directory)?;
    let stem = format!("settings-{}", now.format("%Y%m%dT%H%M%S"));
    for index in 0.. {
        let suffix = if index == 0 {
            String::new()
        } else {
            format!("-{index}")
        };
        let path = directory.join(format!("{stem}{suffix}.json"));
        if !path.exists() {
            return atomic::write_private(&path, original);
        }
    }
    unreachable!()
}

/// Remove a copied command that would bill a different profile's key.
pub fn strip_copied_helper(path: &Path) -> Result<()> {
    let Some(metadata) = fs::symlink_metadata(path).ok() else {
        return Ok(());
    };
    let bytes = fs::read(path)?;
    let mut json: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Copied settings.json is not valid JSON."))?;
    let object = json
        .as_object_mut()
        .context("Copied settings.json is not a JSON object.")?;
    if object
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .and_then(managed_helper_name)
        .is_some()
    {
        object.remove("apiKeyHelper");
        let output = serde_json::to_vec_pretty(&json)?;
        atomic::write_private(path, &output)?;
        #[cfg(unix)]
        if !metadata.file_type().is_symlink() {
            fs::set_permissions(path, metadata.permissions())?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputStep {
    Continue,
    Complete,
    Abort,
}

pub fn apply_key_event(buffer: &mut String, key: KeyEvent) -> InputStep {
    if key.kind != KeyEventKind::Press {
        return InputStep::Continue;
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            buffer.clear();
            InputStep::Abort
        }
        KeyCode::Esc => {
            buffer.clear();
            InputStep::Abort
        }
        KeyCode::Enter => InputStep::Complete,
        KeyCode::Backspace => {
            buffer.pop();
            InputStep::Continue
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            buffer.push(c);
            InputStep::Continue
        }
        _ => InputStep::Continue,
    }
}

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub fn read_key_input() -> Result<String> {
    if !io::stdin().is_terminal() {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        return Ok(line);
    }
    print!("API key: ");
    io::stdout().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let guard = RawModeGuard;
    let mut buffer = String::new();
    let result = loop {
        if let Event::Key(key) = event::read()? {
            match apply_key_event(&mut buffer, key) {
                InputStep::Continue => {}
                InputStep::Complete => break Ok(buffer),
                InputStep::Abort => break Err(anyhow::anyhow!("API key entry cancelled.")),
            }
        }
    };
    drop(guard);
    println!();
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintGate {
    Terminal,
    InsecureMode,
}

pub fn print_gate(is_terminal: bool, mode: Option<u32>) -> std::result::Result<(), PrintGate> {
    if is_terminal {
        return Err(PrintGate::Terminal);
    }
    #[cfg(unix)]
    if mode.is_some_and(|mode| mode & 0o077 != 0) {
        return Err(PrintGate::InsecureMode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

pub fn print_key(
    base_dir: &Path,
    name: &str,
    is_terminal: bool,
    output: &mut impl Write,
) -> std::result::Result<(), (i32, &'static str)> {
    if !valid_name(name) {
        return Err((1, "cswitch: key unavailable"));
    }
    if print_gate(is_terminal, None).is_err() {
        return Err((2, "cswitch: key output requires a pipe"));
    }
    let path = key_path(base_dir, name);
    let path_metadata = fs::symlink_metadata(&path).map_err(|_| (1, "cswitch: key unavailable"))?;
    if !path_metadata.is_file() {
        return Err((1, "cswitch: key unavailable"));
    }
    let mut file = File::open(&path).map_err(|_| (1, "cswitch: key unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| (1, "cswitch: key unavailable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != metadata.dev() || path_metadata.ino() != metadata.ino() {
            return Err((1, "cswitch: key unavailable"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if print_gate(false, Some(metadata.permissions().mode())).is_err() {
            return Err((2, "cswitch: key file permissions are unsafe"));
        }
    }
    let mut key = String::new();
    file.read_to_string(&mut key)
        .map_err(|_| (1, "cswitch: key unavailable"))?;
    let key = key.trim();
    if !valid_key(key) {
        return Err((1, "cswitch: key unavailable"));
    }
    writeln!(output, "{key}").map_err(|_| (1, "cswitch: key unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use tempfile::TempDir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }
    fn settings_path(tmp: &TempDir) -> PathBuf {
        tmp.path().join("settings.json")
    }
    fn manager_with_profile() -> (TempDir, ProfileManager, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir(&source).unwrap();
        let manager = ProfileManager::with_paths(tmp.path().join("base"), source.clone()).unwrap();
        manager.add_profile_from("n", &source).unwrap();
        let executable = tmp.path().join("cswitch");
        fs::write(&executable, "synthetic executable").unwrap();
        (tmp, manager, executable)
    }

    #[test]
    fn key_file_is_private_and_atomic() {
        // Known-bad: atomic::write creates a world-readable key temporary file.
        let (_tmp, manager, executable) = manager_with_profile();
        set_key(
            &manager,
            "n",
            "  sk-ant-api03-TESTKEY000  ",
            &executable,
            false,
            now(),
        )
        .unwrap();
        let key_dir = manager.base_dir.join("keys");
        let key_file = key_path(&manager.base_dir, "n");
        assert_eq!(
            fs::read_to_string(key_file).unwrap(),
            "sk-ant-api03-TESTKEY000\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&key_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(key_path(&manager.base_dir, "n"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(&key_dir).unwrap().count(), 1);
    }

    #[test]
    fn private_temp_has_mode_before_any_write() {
        // Known-bad: creating a 0644 temp file then chmod-ing it after writing.
        let tmp = TempDir::new().unwrap();
        let (path, file) = atomic::create_private_temp(tmp.path()).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        drop(file);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn print_gate_rejects_terminal_and_exposed_permissions() {
        // Known-bad: printing directly to a terminal or from a group-readable file.
        assert_eq!(print_gate(true, Some(0o600)), Err(PrintGate::Terminal));
        #[cfg(unix)]
        {
            assert_eq!(print_gate(false, Some(0o640)), Err(PrintGate::InsecureMode));
            assert_eq!(print_gate(false, Some(0o604)), Err(PrintGate::InsecureMode));
        }
        assert_eq!(print_gate(false, Some(0o600)), Ok(()));
    }

    #[test]
    fn settings_merge_preserves_other_fields_and_refuses_bad_inputs() {
        // Known-bad: a string replacement loses unrelated fields or clear removes a foreign helper.
        let source = json!({"theme":"dark", "permissions":{"allow":["Read"]}, "apiKeyHelper":"foreign helper"});
        assert!(merge_settings(&source, "n", Edit::Set("cswitch key print n", false)).is_err());
        let merged = merge_settings(&source, "n", Edit::Set("cswitch key print n", true)).unwrap();
        assert_eq!(merged.json["theme"], "dark");
        assert_eq!(merged.json["permissions"]["allow"][0], "Read");
        let foreign = merge_settings(&source, "n", Edit::Clear).unwrap();
        assert!(!foreign.changed);
        assert_eq!(foreign.json, source);
        let other = json!({"apiKeyHelper":"cswitch key print other"});
        assert!(!merge_settings(&other, "n", Edit::Clear).unwrap().changed);
        let owned = json!({"theme":"dark", "apiKeyHelper":"cswitch key print n"});
        assert_eq!(
            merge_settings(&owned, "n", Edit::Clear).unwrap().json,
            json!({"theme":"dark"})
        );
        assert!(
            merge_settings(
                &json!({"apiKeyHelper":42}),
                "n",
                Edit::Set("cswitch key print n", false)
            )
            .is_err()
        );
        assert_eq!(
            merge_settings(&json!({}), "n", Edit::Set("cswitch key print n", false))
                .unwrap()
                .json,
            json!({"apiKeyHelper":"cswitch key print n"})
        );
        let tmp = TempDir::new().unwrap();
        let path = settings_path(&tmp);
        fs::write(&path, "not json").unwrap();
        assert!(read_settings(&path).is_err());
        fs::write(&path, "[]").unwrap();
        assert!(read_settings(&path).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = tmp.path().join("linked.json");
            symlink(&path, &link).unwrap();
            assert!(read_settings(&link).is_err());
        }
    }

    #[test]
    fn helper_recognition_requires_exact_shell_words() {
        // Known-bad: contains("key print") treats another command as ours.
        assert_eq!(
            managed_helper_name("cswitch key print n"),
            Some("n".to_string())
        );
        assert_eq!(
            managed_helper_name("'/tmp/build dir/cswitch' key print n"),
            Some("n".to_string())
        );
        for foreign in [
            "cswitch key print other x",
            "cswitch-evil key print n",
            "echo x; cswitch key print n",
            "cswitch key print n; echo x",
            "echo 'key print' n",
        ] {
            assert_eq!(managed_helper_name(foreign), None);
        }
        assert_eq!(
            managed_helper_name("cswitch key print other"),
            Some("other".to_string())
        );
        assert_ne!(
            managed_helper_name("cswitch key print other").as_deref(),
            Some("n")
        );
    }

    #[test]
    fn concurrent_settings_change_is_reread_and_kept() {
        // Known-bad: blind overwrite discards a concurrent field.
        let tmp = TempDir::new().unwrap();
        let path = settings_path(&tmp);
        fs::write(&path, r#"{"theme":"old"}"#).unwrap();
        edit_settings(
            &path,
            tmp.path(),
            "n",
            Edit::Set("cswitch key print n", false),
            now(),
            |attempt| {
                if attempt == 0 {
                    fs::write(&path, r#"{"theme":"new","concurrent":true}"#).unwrap();
                }
            },
        )
        .unwrap();
        let json: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["theme"], "new");
        assert_eq!(json["concurrent"], true);
        assert_eq!(json["apiKeyHelper"], "cswitch key print n");
    }

    #[test]
    fn settings_backup_precedes_the_write() {
        // Known-bad: backing up after the settings replacement loses the old bytes.
        let tmp = TempDir::new().unwrap();
        let path = settings_path(&tmp);
        let original = br#"{"theme":"old"}"#;
        fs::write(&path, original).unwrap();
        let backup_parent = tmp.path().join("backups/settings");
        fs::create_dir_all(&backup_parent).unwrap();
        fs::write(backup_parent.join("n"), "blocked").unwrap();
        assert!(
            edit_settings(
                &path,
                tmp.path(),
                "n",
                Edit::Set("cswitch key print n", false),
                now(),
                |_| {}
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_file(backup_parent.join("n")).unwrap();
        edit_settings(
            &path,
            tmp.path(),
            "n",
            Edit::Set("cswitch key print n", false),
            now(),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            fs::read(
                tmp.path()
                    .join("backups/settings/n/settings-20300101T000000.json")
            )
            .unwrap(),
            original
        );
    }

    #[test]
    fn settings_write_keeps_existing_mode_and_new_file_is_private() {
        // Known-bad: replacing settings.json with a default-mode temporary file changes its permissions.
        let tmp = TempDir::new().unwrap();
        let path = settings_path(&tmp);
        edit_settings(
            &path,
            tmp.path(),
            "n",
            Edit::Set("cswitch key print n", false),
            now(),
            |_| {},
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            edit_settings(&path, tmp.path(), "n", Edit::Clear, now(), |_| {}).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn auth_mode_distinguishes_all_sources_without_values() {
        // Known-bad: a foreign helper is labelled as a cswitch key.
        let console = json!({"primaryApiKey":"synthetic"});
        let subscription = json!({"oauthAccount":{"emailAddress":"user@example.com"}});
        assert_eq!(
            derive_auth_mode("n", Some(&console), Some("cswitch key print n"), true),
            AuthMode::ApiKey(Some("console"))
        );
        assert_eq!(
            derive_auth_mode("n", Some(&subscription), Some("cswitch key print n"), true),
            AuthMode::ApiKey(Some("subscription"))
        );
        assert_eq!(
            derive_auth_mode("n", None, Some("cswitch key print n"), false),
            AuthMode::BrokenKey
        );
        assert_eq!(
            derive_auth_mode("n", Some(&console), Some("foreign helper"), true),
            AuthMode::ForeignHelper
        );
        assert_eq!(
            derive_auth_mode("n", Some(&console), None, false),
            AuthMode::Console
        );
        assert_eq!(
            derive_auth_mode("n", Some(&subscription), None, false),
            AuthMode::Subscription
        );
        assert_eq!(
            derive_auth_mode("n", None, None, false),
            AuthMode::NotLoggedIn
        );
        let (_tmp, manager, _) = manager_with_profile();
        fs::write(manager.profile_dir("n").join(".claude.json"), "broken").unwrap();
        assert_eq!(
            read_auth_mode(&manager, "n", read_claude_json(&manager.profile_dir("n"))),
            AuthMode::Unreadable
        );
    }

    #[test]
    fn hidden_input_aborts_without_retaining_partial_key() {
        // Known-bad: Ctrl-C in raw mode stores the partial key.
        let mut buffer = String::new();
        assert_eq!(
            apply_key_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
            ),
            InputStep::Continue
        );
        assert_eq!(
            apply_key_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
            ),
            InputStep::Continue
        );
        assert_eq!(
            apply_key_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            InputStep::Continue
        );
        assert_eq!(buffer, "a");
        assert_eq!(
            apply_key_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            InputStep::Abort
        );
        assert!(buffer.is_empty());
        buffer.push('x');
        assert_eq!(
            apply_key_event(&mut buffer, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            InputStep::Abort
        );
        assert!(buffer.is_empty());
        buffer.push('x');
        assert_eq!(
            apply_key_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputStep::Complete
        );
        assert_eq!(buffer, "x");
    }

    #[test]
    fn validation_failure_never_stores_a_key() {
        // Known-bad: saving before validation leaves a rejected secret on disk.
        let (_tmp, manager, executable) = manager_with_profile();
        assert!(set_key(&manager, "n", "bad key", &executable, false, now()).is_err());
        assert!(!has_key(&manager, "n"));
        assert!(!manager.profile_dir("n").join("settings.json").exists());
    }
}
