//! Local Claude API keys. Key bytes only enter the private store or helper stdout.

use crate::atomic;
use crate::gateway::{self, GatewayInput};
use crate::limits::read_claude_json;
use crate::profile::{ProfileManager, shell_quote};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
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
    Via(Box<AuthMode>, String),
}

impl AuthMode {
    pub fn label(&self) -> String {
        if let Self::Via(mode, host) = self {
            let label = mode.label();
            return if let Some((first, rest)) = label.split_once(", overrides") {
                format!("{first} via {host}, overrides{rest}")
            } else {
                format!("{label} via {host}")
            };
        }
        let label = match self {
            Self::ApiKey(Some("console")) => "API key (cswitch), overrides the Console key",
            Self::ApiKey(Some("subscription")) => "API key (cswitch), overrides the subscription",
            Self::ApiKey(_) => "API key (cswitch)",
            Self::BrokenKey => "broken: key file missing",
            Self::ForeignHelper => "apiKeyHelper (not cswitch)",
            Self::Console => "Console (API billing)",
            Self::Subscription => "Claude subscription",
            Self::NotLoggedIn => "not logged in",
            Self::Unreadable => "unreadable",
            Self::Via(_, _) => unreachable!(),
        };
        label.to_string()
    }

    pub fn api_billed(&self) -> bool {
        match self {
            Self::Via(mode, _) => mode.api_billed(),
            _ => matches!(self, Self::ApiKey(_) | Self::Console),
        }
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
    let path = manager.profile_dir(name).join("settings.json");
    let mode = match (claude, read_helper(&path)) {
        (Ok(claude), Ok(helper)) => derive_auth_mode(
            name,
            claude.as_ref(),
            helper.as_deref(),
            key_path(&manager.base_dir, name).exists(),
        ),
        _ => AuthMode::Unreadable,
    };
    match gateway_host(&path) {
        Some(host) => AuthMode::Via(Box::new(mode), host),
        None => mode,
    }
}

fn gateway_host(path: &Path) -> Option<String> {
    let json: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let value = json.get("env")?.get("ANTHROPIC_BASE_URL")?;
    Some(
        value
            .as_str()
            .and_then(|raw| gateway::normalize_base_url(raw).ok())
            .map_or_else(
                || "(unrecognized base URL)".into(),
                |url| gateway::host(&url).to_string(),
            ),
    )
}

pub fn gateway_display(manager: &ProfileManager, name: &str) -> String {
    let path = manager.profile_dir(name).join("settings.json");
    let Ok(bytes) = fs::read(path) else {
        return "the Anthropic API".into();
    };
    let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
        return "(unrecognized base URL)".into();
    };
    let Some(raw) = json.get("env").and_then(|v| v.get("ANTHROPIC_BASE_URL")) else {
        return "the Anthropic API".into();
    };
    raw.as_str()
        .and_then(|s| gateway::normalize_base_url(s).ok())
        .unwrap_or_else(|| "(unrecognized base URL)".into())
}

pub fn gateway_info(manager: &ProfileManager, name: &str) -> String {
    let url = gateway_display(manager, name);
    if url == "the Anthropic API" {
        return url;
    }
    let manifest = read_manifest(&manager.base_dir, name).ok().flatten();
    match manifest {
        Some(names) if !names.is_empty() => format!("{url} (cswitch, {} settings)", names.len()),
        _ => format!("{url} (set outside cswitch)"),
    }
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
    // Remove sidecars before callers touch the profile directory or registry.
    let _ = read_manifest(base_dir, name)?;
    remove_manifest(base_dir, name)?;
    remove_key_file(base_dir, name)
}

fn check_key_directory(base_dir: &Path) -> Result<()> {
    let directory = base_dir.join("keys");
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            bail!("Key directory is not a regular directory.")
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_key_file(base_dir: &Path, name: &str) -> Result<()> {
    check_key_directory(base_dir)?;
    let path = key_path(base_dir, name);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn manifest_path(base_dir: &Path, name: &str) -> PathBuf {
    base_dir.join("keys").join(format!("{name}.gateway"))
}

fn read_manifest(base_dir: &Path, name: &str) -> Result<Option<BTreeSet<String>>> {
    check_key_directory(base_dir)?;
    let path = manifest_path(base_dir, name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("Gateway manifest is unsafe or invalid."),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Gateway manifest is unsafe or invalid.");
    }
    let bytes =
        fs::read(path).map_err(|_| anyhow::anyhow!("Gateway manifest is unsafe or invalid."))?;
    let names: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Gateway manifest is unsafe or invalid."))?;
    if names
        .iter()
        .any(|name| !gateway::valid_env_name(name) || gateway::credential_name(name))
        || names.windows(2).any(|pair| pair[0] >= pair[1])
    {
        bail!("Gateway manifest is unsafe or invalid.");
    }
    Ok(Some(names.into_iter().collect()))
}

fn write_manifest(base_dir: &Path, name: &str, names: &BTreeSet<String>) -> Result<()> {
    check_key_directory(base_dir)?;
    atomic::write_private(
        &manifest_path(base_dir, name),
        &serde_json::to_vec(&names.iter().collect::<Vec<_>>())?,
    )
}

fn remove_manifest(base_dir: &Path, name: &str) -> Result<()> {
    check_key_directory(base_dir)?;
    match fs::remove_file(manifest_path(base_dir, name)) {
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

pub fn validate_key_input(input: &str) -> Result<&str> {
    let key = input.trim();
    if !valid_key(key) {
        let lower = key.to_ascii_lowercase();
        if key.starts_with('{') || lower.starts_with("http://") || lower.starts_with("https://") {
            bail!(
                "That looks like a URL or JSON: paste the key first; the gateway step comes next."
            );
        }
        bail!("API key must contain only alphanumeric characters, dashes, underscores.");
    }
    Ok(key)
}

fn store_key(base_dir: &Path, name: &str, key: &str) -> Result<()> {
    let directory = base_dir.join("keys");
    check_key_directory(base_dir)?;
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
    pub gateway_lines: Vec<String>,
}

pub fn precheck_set_key(manager: &ProfileManager, name: &str, replace_helper: bool) -> Result<()> {
    if !valid_name(name) {
        bail!("Invalid profile name.");
    }
    manager.get_profile(name)?;
    let settings = manager.profile_dir(name).join("settings.json");
    let state = read_settings(&settings)?;
    let _ = read_manifest(&manager.base_dir, name)?;
    let object = state
        .json
        .as_object()
        .expect("read_settings returns an object");
    if object.get("env").is_some_and(|value| !value.is_object()) {
        bail!("settings.json env is not an object.");
    }
    if object.contains_key("apiKeyHelper")
        && object
            .get("apiKeyHelper")
            .and_then(Value::as_str)
            .and_then(managed_helper_name)
            .as_deref()
            != Some(name)
        && !replace_helper
    {
        bail!("settings.json has a foreign apiKeyHelper; use --replace-helper to replace it.");
    }
    Ok(())
}

#[cfg(test)]
pub fn set_key(
    manager: &ProfileManager,
    name: &str,
    input: &str,
    executable: &Path,
    replace_helper: bool,
    now: DateTime<Utc>,
) -> Result<SetOutcome> {
    set_key_with_gateway(
        manager,
        name,
        input,
        executable,
        replace_helper,
        &GatewayInput::Keep,
        false,
        now,
    )
}

#[allow(clippy::too_many_arguments)] // CLI and TUI share this complete, atomic key operation.
pub fn set_key_with_gateway(
    manager: &ProfileManager,
    name: &str,
    input: &str,
    executable: &Path,
    replace_helper: bool,
    gateway_input: &GatewayInput,
    save_defaults: bool,
    now: DateTime<Utc>,
) -> Result<SetOutcome> {
    let key = validate_key_input(input)?;
    precheck_set_key(manager, name, replace_helper)?;
    let executable = fs::canonicalize(executable).context("Cannot resolve cswitch executable")?;
    let helper = format!(
        "{} key print {name}",
        shell_quote(&executable.to_string_lossy())
    );
    let settings = manager.profile_dir(name).join("settings.json");
    let old_names = read_manifest(&manager.base_dir, name)?.unwrap_or_default();
    let mut defaults = if matches!(gateway_input, GatewayInput::Url(_)) || save_defaults {
        Some(gateway::read_defaults(&manager.base_dir)?)
    } else {
        None
    };
    let new_settings: BTreeMap<String, String> = match gateway_input {
        GatewayInput::Keep | GatewayInput::Remove => BTreeMap::new(),
        GatewayInput::Url(url) => defaults
            .as_ref()
            .and_then(|saved| saved.get(url))
            .cloned()
            .unwrap_or_else(|| BTreeMap::from([("ANTHROPIC_BASE_URL".into(), url.clone())])),
        GatewayInput::Json { settings, .. } => settings.clone(),
    };
    let new_names: BTreeSet<String> = match gateway_input {
        GatewayInput::Keep => old_names.clone(),
        _ => new_settings.keys().cloned().collect(),
    };
    let had_saved_defaults = match gateway_input {
        GatewayInput::Url(url) => defaults
            .as_ref()
            .is_some_and(|saved| saved.contains_key(url)),
        _ => false,
    };
    let gateway_edit = match gateway_input {
        GatewayInput::Keep => GatewayEdit::Keep,
        GatewayInput::Remove => GatewayEdit::Remove,
        _ => GatewayEdit::Set(&new_settings),
    };
    let edit = Edit::Set {
        helper: &helper,
        replace: replace_helper,
        gateway: gateway_edit,
        owned: &old_names,
    };
    // Dry-run the complete merge before either sidecar or settings is written.
    let _ = merge_settings(&read_settings(&settings)?.json, name, edit)?;
    store_key(&manager.base_dir, name, key)?;
    if !matches!(gateway_input, GatewayInput::Keep) {
        let union = old_names.union(&new_names).cloned().collect();
        write_manifest(&manager.base_dir, name, &union)?;
    }
    let edited = edit_settings(&settings, &manager.base_dir, name, edit, now, |_| {})?;
    if !matches!(gateway_input, GatewayInput::Keep) {
        if new_names.is_empty() {
            remove_manifest(&manager.base_dir, name)?;
        } else {
            write_manifest(&manager.base_dir, name, &new_names)?;
        }
    }
    if save_defaults && let GatewayInput::Json { url, .. } = gateway_input {
        let saved = defaults.get_or_insert_with(BTreeMap::new);
        saved.insert(url.clone(), new_settings.clone());
        gateway::write_defaults(&manager.base_dir, saved)?;
    }
    let mut gateway_lines = match gateway_input {
        GatewayInput::Keep => vec![format!(
            "Gateway unchanged: {}.",
            gateway_display(manager, name)
        )],
        GatewayInput::Remove => vec!["Gateway removed: the key goes to the Anthropic API.".into()],
        GatewayInput::Url(url) => {
            if had_saved_defaults {
                vec![format!(
                    "Using the {} saved settings for {url}.",
                    new_settings.len()
                )]
            } else {
                vec![format!(
                    "No saved defaults for {url}: only the base URL is set. Paste the provider's JSON to save some."
                )]
            }
        }
        GatewayInput::Json { url, dropped, .. } => {
            let mut lines = vec![format!("Gateway: {url} ({} settings).", new_settings.len())];
            for name in dropped {
                lines.push(format!(
                    "Ignored {name}: the key comes only from the key prompt."
                ));
            }
            if save_defaults {
                lines.push(format!("Saved as the defaults for {url}."));
            }
            lines
        }
    };
    for name in edited.overwritten {
        gateway_lines.push(format!("Replaced {name}, which cswitch didn't set."));
    }
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
        gateway_lines,
    })
}

pub struct ClearOutcome {
    pub foreign_helper: bool,
    pub fallback: &'static str,
    pub gateway_line: Option<String>,
}

pub fn clear_key(manager: &ProfileManager, name: &str, now: DateTime<Utc>) -> Result<ClearOutcome> {
    if !valid_name(name) {
        bail!("Invalid profile name.");
    }
    manager.get_profile(name)?;
    let settings = manager.profile_dir(name).join("settings.json");
    let old_names = read_manifest(&manager.base_dir, name)?.unwrap_or_default();
    let old_gateway = gateway_display(manager, name);
    let result = edit_settings(
        &settings,
        &manager.base_dir,
        name,
        Edit::Clear { owned: &old_names },
        now,
        |_| {},
    )?;
    remove_key_file(&manager.base_dir, name)?;
    if !result.foreign_helper {
        remove_manifest(&manager.base_dir, name)?;
    }
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
        gateway_line: if result.removed_gateway {
            let host =
                if old_gateway == "the Anthropic API" || old_gateway == "(unrecognized base URL)" {
                    old_gateway
                } else {
                    gateway::host(&old_gateway).to_string()
                };
            Some(format!(
                "Removed the gateway ({host}, {} settings).",
                old_names.len().max(1)
            ))
        } else {
            None
        },
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
    Set {
        helper: &'a str,
        replace: bool,
        gateway: GatewayEdit<'a>,
        owned: &'a BTreeSet<String>,
    },
    Clear {
        owned: &'a BTreeSet<String>,
    },
}

#[derive(Clone, Copy)]
enum GatewayEdit<'a> {
    Keep,
    Remove,
    Set(&'a BTreeMap<String, String>),
}

struct Merge {
    json: Value,
    changed: bool,
    foreign_helper: bool,
    overwritten: Vec<String>,
    removed_gateway: bool,
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
        Edit::Set {
            helper,
            replace,
            gateway,
            owned,
        } => {
            if present && managed.as_deref() != Some(name) && !replace {
                bail!(
                    "settings.json has a foreign apiKeyHelper; use --replace-helper to replace it."
                );
            }
            // Every key set makes one settings edit and backup, including a piped key rotation.
            let mut changed = true;
            object.insert(
                "apiKeyHelper".to_string(),
                Value::String(helper.to_string()),
            );
            let mut overwritten = Vec::new();
            if !matches!(gateway, GatewayEdit::Keep) {
                let env = match object.get_mut("env") {
                    Some(value) => value
                        .as_object_mut()
                        .ok_or_else(|| anyhow::anyhow!("settings.json env is not an object."))?,
                    None => {
                        object.insert("env".into(), Value::Object(Map::new()));
                        object
                            .get_mut("env")
                            .and_then(Value::as_object_mut)
                            .expect("inserted object")
                    }
                };
                let before = env.clone();
                match gateway {
                    GatewayEdit::Keep => unreachable!(),
                    GatewayEdit::Remove => {
                        env.remove("ANTHROPIC_BASE_URL");
                        for name in owned {
                            env.remove(name);
                        }
                    }
                    GatewayEdit::Set(settings) => {
                        env.remove("ANTHROPIC_BASE_URL");
                        for name in owned {
                            env.remove(name);
                        }
                        for (name, value) in settings {
                            if before.contains_key(name)
                                && !owned.contains(name)
                                && before.get(name) != Some(&Value::String(value.clone()))
                            {
                                overwritten.push(name.clone());
                            }
                            env.insert(name.clone(), Value::String(value.clone()));
                        }
                    }
                }
                changed |= *env != before;
                if env.is_empty() {
                    changed = true;
                    object.remove("env");
                }
            } else if object.get("env").is_some_and(|value| !value.is_object()) {
                bail!("settings.json env is not an object.");
            }
            Ok(Merge {
                json,
                changed,
                foreign_helper: false,
                overwritten,
                removed_gateway: false,
            })
        }
        Edit::Clear { owned } => {
            if managed.as_deref() == Some(name) {
                object.remove("apiKeyHelper");
                let mut removed_gateway = false;
                if let Some(env) = object.get_mut("env") {
                    let env = env
                        .as_object_mut()
                        .ok_or_else(|| anyhow::anyhow!("settings.json env is not an object."))?;
                    removed_gateway |= env.remove("ANTHROPIC_BASE_URL").is_some();
                    for entry in owned {
                        removed_gateway |= env.remove(entry).is_some();
                    }
                    if env.is_empty() {
                        object.remove("env");
                    }
                }
                Ok(Merge {
                    json,
                    changed: true,
                    foreign_helper: false,
                    overwritten: Vec::new(),
                    removed_gateway,
                })
            } else if !present && !owned.is_empty() {
                let mut removed_gateway = false;
                if let Some(env) = object.get_mut("env") {
                    let env = env
                        .as_object_mut()
                        .ok_or_else(|| anyhow::anyhow!("settings.json env is not an object."))?;
                    for entry in owned {
                        removed_gateway |= env.remove(entry).is_some();
                    }
                    if env.is_empty() {
                        object.remove("env");
                    }
                }
                Ok(Merge {
                    json,
                    changed: removed_gateway,
                    foreign_helper: false,
                    overwritten: Vec::new(),
                    removed_gateway,
                })
            } else {
                Ok(Merge {
                    json,
                    changed: false,
                    foreign_helper: foreign,
                    overwritten: Vec::new(),
                    removed_gateway: false,
                })
            }
        }
    }
}

struct EditOutcome {
    foreign_helper: bool,
    removed_gateway: bool,
    overwritten: Vec<String>,
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
                removed_gateway: merged.removed_gateway,
                overwritten: merged.overwritten,
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
                removed_gateway: merged.removed_gateway,
                overwritten: merged.overwritten,
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
pub fn strip_copied_helper(path: &Path, base_dir: &Path) -> Result<()> {
    let Some(metadata) = fs::symlink_metadata(path).ok() else {
        return Ok(());
    };
    let bytes = fs::read(path)?;
    let mut json: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Copied settings.json is not valid JSON."))?;
    let object = json
        .as_object_mut()
        .context("Copied settings.json is not a JSON object.")?;
    if let Some(source_name) = object
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .and_then(managed_helper_name)
    {
        object.remove("apiKeyHelper");
        if object.get("env").is_some_and(|value| !value.is_object()) {
            bail!("Copied settings.json env is not an object.");
        }
        if let Some(env) = object.get_mut("env").and_then(Value::as_object_mut) {
            env.remove("ANTHROPIC_BASE_URL");
            if let Some(names) = read_manifest(base_dir, &source_name).ok().flatten() {
                for name in names {
                    env.remove(&name);
                }
            }
            if env.is_empty() {
                object.remove("env");
            }
        }
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
    TooLong,
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

pub fn apply_gateway_event(buffer: &mut String, key: KeyEvent) -> InputStep {
    if key.kind != KeyEventKind::Press {
        return InputStep::Continue;
    }
    if key.code == KeyCode::Enter {
        let trimmed = buffer.trim();
        if trimmed.is_empty()
            || !trimmed.starts_with('{')
            || serde_json::from_str::<Value>(trimmed).is_ok()
        {
            return InputStep::Complete;
        }
        buffer.push('\n');
    } else {
        match apply_key_event(buffer, key) {
            InputStep::Continue => {}
            step => return step,
        }
    }
    if buffer.len() > 65_536 {
        buffer.clear();
        InputStep::TooLong
    } else {
        InputStep::Continue
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
                InputStep::TooLong => unreachable!(),
            }
        }
    };
    drop(guard);
    println!();
    result
}

pub fn read_gateway_input(current: &str) -> Result<GatewayInput> {
    println!("Gateway now: {current}");
    println!(
        "Base URL, or the provider's settings JSON (hidden; Enter keeps it, \"none\" for the Anthropic API):"
    );
    io::stdout().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let guard = RawModeGuard;
    let mut buffer = String::new();
    let result = loop {
        if let Event::Key(key) = event::read()? {
            match apply_gateway_event(&mut buffer, key) {
                InputStep::Continue => {}
                InputStep::Complete => break parse_gateway_input(&buffer),
                InputStep::Abort => break Err(anyhow::anyhow!("Gateway entry cancelled.")),
                InputStep::TooLong => break Err(anyhow::anyhow!("Gateway input is too long.")),
            }
        }
    };
    buffer.clear();
    drop(guard);
    println!();
    result
}

pub fn parse_gateway_input(input: &str) -> Result<GatewayInput> {
    gateway::parse_gateway_input(input)
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

    static EMPTY_NAMES: BTreeSet<String> = BTreeSet::new();
    fn test_set(helper: &str, replace: bool) -> Edit<'_> {
        Edit::Set {
            helper,
            replace,
            gateway: GatewayEdit::Keep,
            owned: &EMPTY_NAMES,
        }
    }
    fn test_clear() -> Edit<'static> {
        Edit::Clear {
            owned: &EMPTY_NAMES,
        }
    }

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
        assert!(merge_settings(&source, "n", test_set("cswitch key print n", false)).is_err());
        let merged = merge_settings(&source, "n", test_set("cswitch key print n", true)).unwrap();
        assert_eq!(merged.json["theme"], "dark");
        assert_eq!(merged.json["permissions"]["allow"][0], "Read");
        let foreign = merge_settings(&source, "n", test_clear()).unwrap();
        assert!(!foreign.changed);
        assert_eq!(foreign.json, source);
        let other = json!({"apiKeyHelper":"cswitch key print other"});
        assert!(!merge_settings(&other, "n", test_clear()).unwrap().changed);
        let owned = json!({"theme":"dark", "apiKeyHelper":"cswitch key print n"});
        assert_eq!(
            merge_settings(&owned, "n", test_clear()).unwrap().json,
            json!({"theme":"dark"})
        );
        assert!(
            merge_settings(
                &json!({"apiKeyHelper":42}),
                "n",
                test_set("cswitch key print n", false)
            )
            .is_err()
        );
        assert_eq!(
            merge_settings(&json!({}), "n", test_set("cswitch key print n", false))
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

    #[cfg(unix)]
    #[test]
    fn valid_settings_symlink_is_refused_by_reader() {
        // Known-bad: stamp() follows a symlink to valid JSON and treats it as a normal file.
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("shared-settings.json");
        fs::write(&target, br#"{"theme":"dark"}"#).unwrap();
        let link = tmp.path().join("settings.json");
        symlink(&target, &link).unwrap();
        assert_eq!(
            read_settings(&link).err().unwrap().to_string(),
            "settings.json is a symlink."
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn valid_settings_symlink_is_refused_before_storing_key() {
        // Known-bad: stamp() follows symlinks, so set_key stores a key through unsafe settings.
        use std::os::unix::fs::symlink;

        let (tmp, manager, executable) = manager_with_profile();
        let target = tmp.path().join("shared-settings.json");
        let original = br#"{"theme":"dark"}"#;
        fs::write(&target, original).unwrap();
        let link = manager.profile_dir("n").join("settings.json");
        symlink(&target, &link).unwrap();

        let error = set_key(
            &manager,
            "n",
            "sk-ant-api03-TESTKEY000",
            &executable,
            false,
            now(),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "settings.json is a symlink.");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), original);
        assert!(!has_key(&manager, "n"));
    }

    #[cfg(unix)]
    #[test]
    fn key_directory_symlink_is_refused_without_writing_target() {
        // Known-bad: the store_key directory check follows a link to a valid 0700 directory.
        use std::os::unix::fs::{PermissionsExt, symlink};

        let (tmp, manager, executable) = manager_with_profile();
        let target = tmp.path().join("external-keys");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = manager.base_dir.join("keys");
        symlink(&target, &link).unwrap();

        let error = set_key(
            &manager,
            "n",
            "sk-ant-api03-TESTKEY000",
            &executable,
            false,
            now(),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            "Key directory is not a regular directory."
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert!(!has_key(&manager, "n"));
    }

    #[cfg(unix)]
    #[test]
    fn key_file_symlink_is_refused_without_output() {
        // Known-bad: print_key uses metadata() and follows a valid 0600 key file link.
        use std::os::unix::fs::{PermissionsExt, symlink};

        let (tmp, manager, _) = manager_with_profile();
        let target = tmp.path().join("external-key");
        let original = b"sk-ant-api03-TESTKEY000\n";
        fs::write(&target, original).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir_all(manager.base_dir.join("keys")).unwrap();
        let link = key_path(&manager.base_dir, "n");
        symlink(&target, &link).unwrap();

        let mut output = Vec::new();
        assert_eq!(
            print_key(&manager.base_dir, "n", false, &mut output),
            Err((1, "cswitch: key unavailable"))
        );
        assert!(output.is_empty());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), original);
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
            test_set("cswitch key print n", false),
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
                test_set("cswitch key print n", false),
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
            test_set("cswitch key print n", false),
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
            test_set("cswitch key print n", false),
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
            edit_settings(&path, tmp.path(), "n", test_clear(), now(), |_| {}).unwrap();
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

    #[test]
    fn g4_gateway_enter_waits_for_complete_json_and_aborts_cleanly() {
        // Known-bad: the first Enter truncates a multi-line JSON paste.
        let mut buffer = String::new();
        for ch in "{\"env\": {".chars() {
            assert_eq!(
                apply_gateway_event(
                    &mut buffer,
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
                ),
                InputStep::Continue
            );
        }
        assert_eq!(
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputStep::Continue
        );
        for ch in "\"ANTHROPIC_BASE_URL\":\"https://gateway.example.com\"}}".chars() {
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            );
        }
        assert_eq!(
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputStep::Complete
        );
        assert!(matches!(
            parse_gateway_input(&buffer).unwrap(),
            GatewayInput::Json { .. }
        ));
        buffer.clear();
        assert_eq!(
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputStep::Complete
        );
        buffer.push_str("https://gateway.example.com");
        assert_eq!(
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            InputStep::Complete
        );
        assert_eq!(
            apply_gateway_event(&mut buffer, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            InputStep::Abort
        );
        assert!(buffer.is_empty());
        buffer.push('x');
        assert_eq!(
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            InputStep::Abort
        );
        assert!(buffer.is_empty());
        buffer = "x".repeat(65_536);
        assert_eq!(
            apply_gateway_event(
                &mut buffer,
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            InputStep::TooLong
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn g5_json_set_uses_one_settings_backup_and_exact_manifest() {
        // Known-bad: writing helper and gateway separately makes two backups or loses ownership names.
        let (_tmp, manager, executable) = manager_with_profile();
        let path = manager.profile_dir("n").join("settings.json");
        fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        let input = parse_gateway_input(r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com/a","ANTHROPIC_MODEL":"vendor/claude-model","ANTHROPIC_AUTH_TOKEN":"TOKEN-CANARY"}}"#).unwrap();
        let outcome = set_key_with_gateway(
            &manager,
            "n",
            "TESTKEY",
            &executable,
            false,
            &input,
            true,
            now(),
        )
        .unwrap();
        let json: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(
            json["apiKeyHelper"]
                .as_str()
                .unwrap()
                .contains("key print n")
        );
        assert_eq!(
            json["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example.com/a"
        );
        assert_eq!(json["env"]["ANTHROPIC_MODEL"], "vendor/claude-model");
        assert!(json["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert_eq!(
            read_manifest(&manager.base_dir, "n").unwrap().unwrap(),
            BTreeSet::from(["ANTHROPIC_BASE_URL".into(), "ANTHROPIC_MODEL".into()])
        );
        assert_eq!(
            fs::read_dir(manager.base_dir.join("backups/settings/n"))
                .unwrap()
                .count(),
            1
        );
        assert!(
            outcome
                .gateway_lines
                .iter()
                .any(|line| line.contains("Ignored ANTHROPIC_AUTH_TOKEN"))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(manifest_path(&manager.base_dir, "n"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(
            !fs::read(manager.base_dir.join("gateways.json"))
                .unwrap()
                .windows(12)
                .any(|window| window == b"TOKEN-CANARY")
        );
    }

    #[test]
    fn g6_switch_removes_old_names_and_reports_foreign_overwrite() {
        // Known-bad: old manifest names survive a gateway switch, or a foreign override stays silent.
        let (_tmp, manager, executable) = manager_with_profile();
        let first = parse_gateway_input(
            r#"{"ANTHROPIC_BASE_URL":"https://gateway.example.com/first","A":"a","B":"b"}"#,
        )
        .unwrap();
        set_key_with_gateway(
            &manager,
            "n",
            "TESTKEY",
            &executable,
            false,
            &first,
            false,
            now(),
        )
        .unwrap();
        let path = manager.profile_dir("n").join("settings.json");
        let mut json: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["env"]["C"] = Value::String("foreign".into());
        json["env"]["OTHER"] = Value::String("preserve".into());
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let second = parse_gateway_input(
            r#"{"ANTHROPIC_BASE_URL":"https://gateway.example.com/second","C":"new"}"#,
        )
        .unwrap();
        let outcome = set_key_with_gateway(
            &manager,
            "n",
            "NEWKEY",
            &executable,
            false,
            &second,
            false,
            now(),
        )
        .unwrap();
        let result: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            result["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example.com/second"
        );
        assert_eq!(result["env"]["C"], "new");
        assert_eq!(result["env"]["OTHER"], "preserve");
        assert!(result["env"].get("A").is_none() && result["env"].get("B").is_none());
        assert!(
            outcome
                .gateway_lines
                .iter()
                .any(|line| line == "Replaced C, which cswitch didn't set.")
        );
    }

    #[test]
    fn g7_keep_preserves_env_for_managed_and_foreign_gateway() {
        // Known-bad: Enter at the gateway step wipes or reserializes env settings.
        let (_tmp, manager, executable) = manager_with_profile();
        let path = manager.profile_dir("n").join("settings.json");
        let original = json!({"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com","ANTHROPIC_MODEL":"vendor/claude-model"},"apiKeyHelper":"foreign helper"});
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        set_key_with_gateway(
            &manager,
            "n",
            "TESTKEY",
            &executable,
            true,
            &GatewayInput::Keep,
            false,
            now(),
        )
        .unwrap();
        let first: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(first["env"], original["env"]);
        set_key_with_gateway(
            &manager,
            "n",
            "NEWKEY",
            &executable,
            false,
            &GatewayInput::Keep,
            false,
            now(),
        )
        .unwrap();
        let second: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(second["env"], original["env"]);
        assert_eq!(
            fs::read_dir(manager.base_dir.join("backups/settings/n"))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn g8_clear_removes_base_even_without_manifest() {
        // Known-bad: clear leaves ANTHROPIC_BASE_URL and sends the fallback login to the gateway.
        let (_tmp, manager, executable) = manager_with_profile();
        set_key(&manager, "n", "TESTKEY", &executable, false, now()).unwrap();
        let path = manager.profile_dir("n").join("settings.json");
        let mut json: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["env"] = json!({"ANTHROPIC_BASE_URL":"https://gateway.example.com","OTHER":"keep"});
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let result = clear_key(&manager, "n", now()).unwrap();
        let cleared: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(cleared["env"], json!({"OTHER":"keep"}));
        assert!(cleared.get("apiKeyHelper").is_none());
        assert!(result.gateway_line.unwrap().contains("gateway.example.com"));
        assert!(!has_key(&manager, "n"));
    }

    #[test]
    fn g8_clear_removes_manifest_names_with_the_helper_in_one_backup() {
        // Known-bad: clear removes the helper first and leaves a managed model or base URL behind.
        let (_tmp, manager, executable) = manager_with_profile();
        let path = manager.profile_dir("n").join("settings.json");
        fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        let gateway = parse_gateway_input(r#"{"ANTHROPIC_BASE_URL":"https://gateway.example.com","ANTHROPIC_MODEL":"vendor/claude-model"}"#).unwrap();
        set_key_with_gateway(
            &manager,
            "n",
            "TESTKEY",
            &executable,
            false,
            &gateway,
            false,
            now(),
        )
        .unwrap();
        let before = fs::read_dir(manager.base_dir.join("backups/settings/n"))
            .unwrap()
            .count();
        clear_key(&manager, "n", now()).unwrap();
        let after = fs::read_dir(manager.base_dir.join("backups/settings/n"))
            .unwrap()
            .count();
        assert_eq!(after, before + 1);
        let cleared: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(cleared, json!({"theme":"dark"}));
        assert!(!manifest_path(&manager.base_dir, "n").exists());
        assert!(!has_key(&manager, "n"));
    }

    #[test]
    fn g9_foreign_helper_clear_keeps_settings_and_manifest() {
        // Known-bad: clear removes a foreign helper's gateway settings.
        let (_tmp, manager, _) = manager_with_profile();
        let path = manager.profile_dir("n").join("settings.json");
        let bytes = br#"{"apiKeyHelper":"foreign helper","env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com"}}"#;
        fs::write(&path, bytes).unwrap();
        write_manifest(
            &manager.base_dir,
            "n",
            &BTreeSet::from(["ANTHROPIC_BASE_URL".into()]),
        )
        .unwrap();
        store_key(&manager.base_dir, "n", "TESTKEY").unwrap();
        let outcome = clear_key(&manager, "n", now()).unwrap();
        assert!(outcome.foreign_helper);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(manifest_path(&manager.base_dir, "n").exists());
        assert!(!has_key(&manager, "n"));
    }

    #[test]
    fn invalid_manifest_refuses_set_before_storing_key() {
        // Known-bad: guessing owned env names from an invalid manifest can erase foreign settings.
        let (_tmp, manager, executable) = manager_with_profile();
        fs::create_dir_all(manager.base_dir.join("keys")).unwrap();
        fs::write(manifest_path(&manager.base_dir, "n"), "bad").unwrap();
        let gateway = parse_gateway_input("https://gateway.example.com").unwrap();
        let error = set_key_with_gateway(
            &manager,
            "n",
            "TESTKEY",
            &executable,
            false,
            &gateway,
            false,
            now(),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "Gateway manifest is unsafe or invalid.");
        assert!(!has_key(&manager, "n"));
        assert!(!manager.profile_dir("n").join("settings.json").exists());
    }

    #[test]
    fn g12_url_applies_normalized_defaults_and_unknown_sets_base_only() {
        // Known-bad: defaults are keyed by the raw pasted URL, or an unknown URL inherits stale names.
        let (_tmp, manager, executable) = manager_with_profile();
        let json = parse_gateway_input(r#"{"ANTHROPIC_BASE_URL":"https://gateway.example.com/a","ANTHROPIC_MODEL":"vendor/claude-model"}"#).unwrap();
        set_key_with_gateway(
            &manager,
            "n",
            "TESTKEY",
            &executable,
            false,
            &json,
            true,
            now(),
        )
        .unwrap();
        let url = parse_gateway_input("HTTPS://GATEWAY.EXAMPLE.COM/a/").unwrap();
        let result = set_key_with_gateway(
            &manager,
            "n",
            "NEWKEY",
            &executable,
            false,
            &url,
            false,
            now(),
        )
        .unwrap();
        assert!(result.gateway_lines[0].contains("Using the 2 saved settings"));
        let unknown = parse_gateway_input("https://gateway.example.com/b").unwrap();
        set_key_with_gateway(
            &manager,
            "n",
            "NEWKEY",
            &executable,
            false,
            &unknown,
            false,
            now(),
        )
        .unwrap();
        let settings: Value = serde_json::from_slice(
            &fs::read(manager.profile_dir("n").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["env"],
            json!({"ANTHROPIC_BASE_URL":"https://gateway.example.com/b"})
        );
    }

    #[test]
    fn g14_display_uses_normalized_or_masked_url() {
        // Known-bad: info or Auth prints an untrusted raw URL with a token query.
        let (_tmp, manager, _) = manager_with_profile();
        let path = manager.profile_dir("n").join("settings.json");
        fs::write(
            &path,
            r#"{"env":{"ANTHROPIC_BASE_URL":"HTTPS://GATEWAY.EXAMPLE.COM/a/"}}"#,
        )
        .unwrap();
        assert_eq!(
            gateway_info(&manager, "n"),
            "https://gateway.example.com/a (set outside cswitch)"
        );
        assert!(
            read_auth_mode(&manager, "n", Ok(None))
                .label()
                .contains("via gateway.example.com")
        );
        fs::write(
            &path,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com/?token=TOKEN-CANARY"}}"#,
        )
        .unwrap();
        assert!(gateway_info(&manager, "n").contains("(unrecognized base URL)"));
        assert!(
            !read_auth_mode(&manager, "n", Ok(None))
                .label()
                .contains("TOKEN-CANARY")
        );
    }

    #[test]
    fn g14_every_auth_mode_shows_gateway_host() {
        // Known-bad: via <host> appears only for a cswitch-managed key.
        let (_tmp, manager, _) = manager_with_profile();
        let path = manager.profile_dir("n").join("settings.json");
        for helper in [None, Some("foreign helper"), Some("cswitch key print n")] {
            let mut settings =
                json!({"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com/a"}});
            if let Some(helper) = helper {
                settings["apiKeyHelper"] = json!(helper);
            }
            fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
            for claude in [
                Ok(None),
                Ok(Some(json!({"primaryApiKey":"synthetic"}))),
                Ok(Some(json!({"oauthAccount":{}}))),
                Err(()),
            ] {
                let label = read_auth_mode(&manager, "n", claude).label();
                assert!(label.contains("via gateway.example.com"));
            }
        }
    }

    #[test]
    fn g15_mistaken_key_paste_fails_before_writing() {
        // Known-bad: a URL or JSON at the first prompt is stored as the API key.
        let (_tmp, manager, executable) = manager_with_profile();
        for input in ["https://gateway.example.com", "{\"env\":{}}"] {
            let error = set_key(&manager, "n", input, &executable, false, now())
                .err()
                .unwrap();
            assert_eq!(
                error.to_string(),
                "That looks like a URL or JSON: paste the key first; the gateway step comes next."
            );
            assert!(!has_key(&manager, "n"));
        }
    }
}
