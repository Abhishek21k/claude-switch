use anyhow::Result;
use chrono::{DateTime, Local, Offset, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
        Wrap,
    },
};

use crate::gateway::{self, GatewayInput};
use crate::key::{self, AuthMode, InputStep};
use crate::limits::{Limits, Window, parse_limits, read_claude_json};
use crate::profile::{
    LoginMethod, LoginOutcome, Profile, ProfileManager, Tool, describe_age, detect_current_account,
};
use crate::usage::{self, metrics as m};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

// ── Palette ───────────────────────────────────────────────────────────────────
const ACCENT: Color = Color::Rgb(255, 149, 0);
const DIM: Color = Color::Rgb(100, 100, 110);
const SUCCESS: Color = Color::Rgb(80, 200, 120);
const DANGER: Color = Color::Rgb(220, 80, 80);
const BG: Color = Color::Rgb(14, 14, 18);
const PANEL: Color = Color::Rgb(22, 22, 28);
const BORDER: Color = Color::Rgb(50, 50, 60);
const TEXT: Color = Color::Rgb(220, 220, 230);
const MUTED: Color = Color::Rgb(140, 140, 155);
const SEARCH_HL: Color = Color::Rgb(255, 230, 140);

// ── Mode ──────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    FirstRun,
    Normal,
    Search,
    Help,
    ConfirmDelete,
    ConfirmRefresh,
    ConfirmKeyClear,
    KeyEntry,
    GatewayEntry,
    ConfirmGatewayDefaults,
    AddName,
    /// Name accepted; now choosing *how* the profile gets its account.
    /// Split from `AddName` so Esc backs out one step at a time and the
    /// side effect is named before it happens.
    AddChoice,
    LoginName,
    Message(String, bool),
}

/// Work that has to happen with the TUI torn down — it spawns `claude`, which
/// needs the real terminal. Queued by a key handler, drained by the event loop,
/// which restores the terminal around it and rebuilds it afterwards.
#[derive(Debug, Clone, PartialEq)]
enum PendingAction {
    /// Authenticate a brand-new profile as a different Claude account.
    Login {
        name: String,
        method: LoginMethod,
    },
    CodexLogin {
        name: String,
    },
}

// ── App ───────────────────────────────────────────────────────────────────────
pub struct App {
    manager: ProfileManager,
    profiles: Vec<Profile>,
    list_state: ListState,
    mode: Mode,
    input_buffer: String,
    key_buffer: String,
    gateway_buffer: String,
    pending_gateway: Option<GatewayInput>,
    gateway_replaces_defaults: bool,
    executable: PathBuf,
    search_query: String,
    /// Indices into `profiles` matching the current search.
    filtered_indices: Vec<usize>,
    detected_email: Option<String>,
    claude_dir_found: bool,
    /// Account currently logged in under `~/.claude`, resolved when the add
    /// flow starts so "Copy" can name what it is about to copy.
    current_account: Option<String>,
    /// Seconds since a Claude session last wrote to the selected profile, when
    /// that was recent enough to matter. Resolved as a destructive confirmation
    /// opens, because profiles run concurrently and the one being overwritten
    /// may be open in another terminal.
    selected_in_use: Option<u64>,
    /// Whether the selected profile holds conversation content that a refresh
    /// would destroy. Resolved alongside `selected_in_use`, for the same
    /// reason: the confirmation should name every consequence, not just the
    /// credential one.
    selected_has_history: bool,
    selected_has_key: bool,
    pending: Option<PendingAction>,
    /// How the live account gets resolved. Swapped in tests so the choice
    /// screen can be exercised without a real `~/.claude` behind it.
    account_probe: AccountProbe,
    limits: HashMap<String, Limits>,
    auth_modes: HashMap<String, AuthMode>,
    codex_plans: HashMap<String, Option<String>>,
    limits_selection: Option<String>,
    last_limits_refresh: Option<Instant>,
    limits_now: DateTime<Utc>,
    usage_dir: PathBuf,
    usage_cache: UsageCache,
    ingest_rx: Option<Receiver<IngestOutcome>>,
    ingest_status: Option<IngestStatus>,
}

type AccountProbe = fn() -> Option<String>;

struct UsageCache {
    hourly: Option<m::Hourly>,
    settings: crate::UsageSettings,
    history: Vec<m::LimitRow>,
    cells: HashMap<String, String>,
}

impl UsageCache {
    fn load(
        manager: &ProfileManager,
        profiles: &[Profile],
        dir: &Path,
        now: DateTime<Utc>,
    ) -> Self {
        let hourly = m::read(dir);
        let settings = m::load_settings(dir, now);
        let history = m::history(dir);
        let cells = profiles
            .iter()
            .map(|profile| {
                let cell = if profile.tool == Tool::Claude {
                    let auth = key::read_auth_mode(
                        manager,
                        &profile.name,
                        read_claude_json(&manager.profile_dir(&profile.name)),
                    );
                    crate::usage_30d_cell(
                        &profile.name,
                        &auth,
                        hourly.as_ref(),
                        &settings,
                        now,
                        Local::now().offset().fix(),
                    )
                } else {
                    "—".into()
                };
                (profile.name.clone(), cell)
            })
            .collect();
        Self {
            hourly,
            settings,
            history,
            cells,
        }
    }
}

enum IngestOutcome {
    Updated,
    Busy,
    Failed(String),
}

enum IngestStatus {
    Busy,
    Failed(String),
}

fn live_account_email() -> Option<String> {
    detect_current_account().and_then(|a| a.email)
}

fn profile_name_line(name: &str, cell: &str, area_width: u16) -> Line<'static> {
    // Two border cells and two highlight-symbol cells are reserved on every row.
    let available = area_width.saturating_sub(4) as usize;
    if available == 0 {
        return Line::from("");
    }
    let cell_width = cell.chars().count().min(available.saturating_sub(2));
    let cell: String = cell.chars().take(cell_width).collect();
    let name_budget = available.saturating_sub(cell_width + 2);
    let name: String = name.chars().take(name_budget).collect();
    let padding = available.saturating_sub(1 + name.chars().count() + cell_width);
    Line::from(vec![
        Span::styled(
            format!(" {name}{}", " ".repeat(padding)),
            Style::default().fg(TEXT).bold(),
        ),
        Span::styled(cell, Style::default().fg(ACCENT)),
    ])
}

impl App {
    pub fn new(manager: ProfileManager) -> Result<Self> {
        let profiles = manager.list_profiles()?;
        let usage_dir = crate::usage_directory(&manager);
        let usage_cache = UsageCache::load(&manager, &profiles, &usage_dir, Utc::now());
        let filtered_indices: Vec<usize> = (0..profiles.len()).collect();
        let mut list_state = ListState::default();
        if !profiles.is_empty() {
            list_state.select(Some(0));
        }

        let (mode, detected_email, claude_dir_found, input_buffer) = if profiles.is_empty() {
            match detect_current_account() {
                Some(acct) => (Mode::FirstRun, acct.email, true, "default".to_string()),
                None => (Mode::FirstRun, None, false, String::new()),
            }
        } else {
            (Mode::Normal, None, false, String::new())
        };

        Ok(Self {
            manager,
            profiles,
            list_state,
            mode,
            input_buffer,
            key_buffer: String::new(),
            gateway_buffer: String::new(),
            pending_gateway: None,
            gateway_replaces_defaults: false,
            executable: PathBuf::new(),
            search_query: String::new(),
            filtered_indices,
            detected_email,
            claude_dir_found,
            current_account: None,
            selected_in_use: None,
            selected_has_history: false,
            selected_has_key: false,
            pending: None,
            account_probe: live_account_email,
            limits: HashMap::new(),
            auth_modes: HashMap::new(),
            codex_plans: HashMap::new(),
            limits_selection: None,
            last_limits_refresh: None,
            limits_now: Utc::now(),
            usage_dir,
            usage_cache,
            ingest_rx: None,
            ingest_status: None,
        })
    }

    pub fn with_executable(mut self, executable: PathBuf) -> Self {
        self.executable = executable;
        self
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn refresh(&mut self) -> Result<()> {
        self.profiles = self.manager.list_profiles()?;
        self.reload_usage_data();
        self.apply_filter();
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
        } else {
            let idx = self.list_state.selected().unwrap_or(0);
            self.list_state
                .select(Some(idx.min(self.filtered_indices.len() - 1)));
        }
        self.last_limits_refresh = None;
        Ok(())
    }

    fn reload_usage_data(&mut self) {
        self.usage_cache =
            UsageCache::load(&self.manager, &self.profiles, &self.usage_dir, Utc::now());
    }

    fn start_background_ingest(&mut self) {
        match usage::store(&self.manager, Some(self.usage_dir.clone())) {
            Ok(store) => self.start_ingest_with(move || match store.ingest() {
                Ok(report) if report.skipped_lock => IngestOutcome::Busy,
                Ok(_) => IngestOutcome::Updated,
                Err(error) => IngestOutcome::Failed(error.to_string()),
            }),
            Err(error) => self.ingest_status = Some(IngestStatus::Failed(error.to_string())),
        }
    }

    fn start_ingest_with<F>(&mut self, ingest: F)
    where
        F: FnOnce() -> IngestOutcome + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        // The worker only touches the usage store and this channel, never the terminal.
        std::thread::spawn(move || {
            let _ = tx.send(ingest());
        });
        self.ingest_rx = Some(rx);
        self.ingest_status = None;
    }

    fn poll_ingest(&mut self) {
        let Some(rx) = &self.ingest_rx else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                IngestOutcome::Failed("background ingest stopped".into())
            }
        };
        self.ingest_rx = None;
        match outcome {
            IngestOutcome::Updated => {
                self.reload_usage_data();
                self.last_limits_refresh = None;
                self.ingest_status = None;
            }
            IngestOutcome::Busy => self.ingest_status = Some(IngestStatus::Busy),
            IngestOutcome::Failed(error) => self.ingest_status = Some(IngestStatus::Failed(error)),
        }
    }

    fn refresh_limits_if_due(&mut self, instant: Instant) {
        let selected = self.selected_profile().map(|profile| profile.name.clone());
        let changed = selected != self.limits_selection;
        let due = self
            .last_limits_refresh
            .is_none_or(|last| instant.duration_since(last) >= Duration::from_secs(30));
        if let Some(name) = &selected
            && (changed || due)
        {
            if let Some(tool) = self.selected_profile().map(|profile| profile.tool.clone())
                && tool != Tool::Claude
            {
                self.limits.remove(name);
                self.auth_modes.remove(name);
                if tool == Tool::Codex {
                    self.codex_plans.insert(
                        name.clone(),
                        self.manager
                            .codex_identity(name)
                            .and_then(|identity| identity.plan_type),
                    );
                }
                self.limits_selection = selected;
                self.last_limits_refresh = Some(instant);
                return;
            }
            let claude = read_claude_json(&self.manager.profile_dir(name));
            self.auth_modes.insert(
                name.clone(),
                key::read_auth_mode(&self.manager, name, claude.clone()),
            );
            self.limits.insert(
                name.clone(),
                claude
                    .map(|json| json.map_or(Limits::Unreadable, |value| parse_limits(&value)))
                    .unwrap_or(Limits::Unreadable),
            );
            if let Some(auth) = self.auth_modes.get(name) {
                self.usage_cache.cells.insert(
                    name.clone(),
                    crate::usage_30d_cell(
                        name,
                        auth,
                        self.usage_cache.hourly.as_ref(),
                        &self.usage_cache.settings,
                        self.limits_now,
                        Local::now().offset().fix(),
                    ),
                );
            }
            self.last_limits_refresh = Some(instant);
        }
        self.limits_selection = selected;
    }

    fn apply_filter(&mut self) {
        let q = self.search_query.to_lowercase();
        if q.is_empty() {
            self.filtered_indices = (0..self.profiles.len()).collect();
        } else {
            self.filtered_indices = self
                .profiles
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.name.to_lowercase().contains(&q)
                        || p.email.as_deref().unwrap_or("").to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();
        }
        // Keep selection in bounds
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
        } else {
            let sel = self.list_state.selected().unwrap_or(0);
            self.list_state
                .select(Some(sel.min(self.filtered_indices.len() - 1)));
        }
    }

    fn select_by_name(&mut self, name: &str) {
        if let Some(fi) = self
            .filtered_indices
            .iter()
            .position(|&i| self.profiles[i].name == name)
        {
            self.list_state.select(Some(fi));
        }
    }

    fn selected_profile(&self) -> Option<&Profile> {
        self.list_state
            .selected()
            .and_then(|fi| self.filtered_indices.get(fi))
            .and_then(|&i| self.profiles.get(i))
    }

    fn usage_panel_text(&self, name: &str) -> String {
        let auth = self.auth_modes.get(name).unwrap_or(&AuthMode::NotLoggedIn);
        let limits = self.limits.get(name).unwrap_or(&Limits::NoSnapshot);
        let mut text = crate::usage_info_cached(
            crate::UsageInfoData {
                hourly: self.usage_cache.hourly.as_ref(),
                settings: &self.usage_cache.settings,
                history: &self.usage_cache.history,
            },
            name,
            auth,
            self.limits_now,
            Local::now().offset().fix(),
            limits,
        );
        match &self.ingest_status {
            Some(IngestStatus::Busy) if text.starts_with("Usage (ledger as of ") => {
                text = text.replacen("):\n", "; ledger busy):\n", 1);
            }
            Some(IngestStatus::Busy) => {
                text = format!("Usage (ledger busy):\n{text}");
            }
            Some(IngestStatus::Failed(error)) => {
                text = format!("Usage (ingest error: {error}):\n{text}");
            }
            None => {}
        }
        text
    }

    /// How recently a Claude session wrote to the selected profile, if that was
    /// recent enough that another terminal may still have it open.
    fn selected_session_age(&self) -> Option<u64> {
        let name = self.selected_profile()?.name.clone();
        self.manager.maybe_in_use(&name)
    }

    /// Whether the selected profile has conversation content a refresh would
    /// destroy.
    fn selected_holds_history(&self) -> bool {
        match self.selected_profile() {
            Some(p) => self.manager.has_local_history(&p.name),
            None => false,
        }
    }

    fn move_up(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(0) | None => self.filtered_indices.len() - 1,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(i));
    }

    fn move_down(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1) % self.filtered_indices.len(),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    // ── Run ───────────────────────────────────────────────────────────────────

    pub fn run(mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        terminal.clear()?;
        self.start_background_ingest();
        let result = self.event_loop(&mut terminal);
        ratatui::restore();
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.limits_now = Utc::now();
            self.poll_ingest();
            self.refresh_limits_if_due(Instant::now());
            terminal.draw(|f| self.render(f))?;

            let wait = if self.ingest_rx.is_some() {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(30)
            };
            if event::poll(wait)?
                && let Event::Key(key) = event::read()?
            {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match &self.mode.clone() {
                    Mode::FirstRun => {
                        if self.handle_first_run_key(key.code, key.modifiers)? {
                            return Ok(());
                        }
                    }
                    Mode::Normal => {
                        if self.handle_normal_key(key.code, key.modifiers)? {
                            return Ok(());
                        }
                    }
                    Mode::Search => {
                        if self.handle_search_key(key.code, key.modifiers)? {
                            return Ok(());
                        }
                    }
                    Mode::Help => {
                        // Any key dismisses help
                        self.mode = Mode::Normal;
                    }
                    Mode::ConfirmDelete => {
                        self.handle_confirm_delete(key.code)?;
                    }
                    Mode::ConfirmRefresh => {
                        self.handle_confirm_refresh(key.code)?;
                    }
                    Mode::ConfirmKeyClear => self.handle_confirm_key_clear(key.code)?,
                    Mode::KeyEntry => self.handle_key_entry(key)?,
                    Mode::GatewayEntry => self.handle_gateway_entry(key)?,
                    Mode::ConfirmGatewayDefaults => self.handle_confirm_gateway_defaults(key)?,
                    Mode::AddName => {
                        if self.handle_add_name(key.code)? {
                            return Ok(());
                        }
                    }
                    Mode::AddChoice => {
                        if self.handle_add_choice(key.code)? {
                            return Ok(());
                        }
                    }
                    Mode::LoginName => {
                        if self.handle_login_name(key.code)? {
                            return Ok(());
                        }
                    }
                    Mode::Message(_, _) => {
                        self.mode = Mode::Normal;
                    }
                }
            }

            // Anything that needs the bare terminal runs here, between frames,
            // so the TUI is fully torn down before `claude` takes over stdin.
            if let Some(action) = self.pending.take() {
                self.run_pending(action, terminal)?;
            }
        }
    }

    /// Tear the TUI down, run `action` against the real terminal, then rebuild
    /// the TUI and carry on. Errors are shown in-app rather than propagated —
    /// a cancelled or failed login must not take the whole program down with it.
    fn run_pending(&mut self, action: PendingAction, terminal: &mut DefaultTerminal) -> Result<()> {
        ratatui::restore();

        // `select` is only set when a profile actually landed in the registry,
        // so a failed attempt leaves the current selection alone.
        let (name, result, method) = match action {
            PendingAction::Login { name, method } => {
                let result = self.manager.login_profile(&name, false, None, method);
                (name, result, Some(method))
            }
            PendingAction::CodexLogin { name } => {
                let result = self.manager.login_codex_profile(&name);
                (name, result, None)
            }
        };
        let (select, message) = match result {
            Ok(result) => {
                let msg = pending_login_message(&name, &result, method);
                (Some(name), Mode::Message(msg, false))
            }
            Err(e) => (None, Mode::Message(e.to_string(), true)),
        };

        // Rebuild the terminal the loop is about to draw into.
        *terminal = ratatui::init();
        terminal.clear()?;

        self.refresh()?;
        if let Some(name) = select {
            self.select_by_name(&name);
        }
        self.mode = message;
        Ok(())
    }

    // ── Key handlers ──────────────────────────────────────────────────────────

    fn handle_first_run_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        if !self.claude_dir_found {
            match code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Char('a') => {
                    self.input_buffer.clear();
                    self.mode = Mode::AddName;
                }
                _ => self.mode = Mode::Normal,
            }
            return Ok(false);
        }

        match code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('1') => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    return Ok(false);
                }
                match self.manager.add_profile(&name, false) {
                    Ok(_) => {
                        self.refresh()?;
                        self.select_by_name(&name);
                        self.detected_email = None;
                        self.claude_dir_found = false;
                        self.mode = Mode::Message(
                            format!(
                                "Profile '{}' saved from active session. Press Enter to launch.",
                                name
                            ),
                            false,
                        );
                    }
                    Err(e) => self.mode = Mode::Message(e.to_string(), true),
                }
            }

            KeyCode::Char('2') => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    return Ok(false);
                }
                self.detected_email = None;
                self.claude_dir_found = false;
                self.mode = Mode::Normal;
                self.pending = Some(PendingAction::Login {
                    name,
                    method: LoginMethod::ClaudeAi,
                });
            }

            KeyCode::Char('3') => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    return Ok(false);
                }
                self.detected_email = None;
                self.claude_dir_found = false;
                self.mode = Mode::Normal;
                self.pending = Some(PendingAction::CodexLogin { name });
            }

            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) if c.is_alphanumeric() || c == '-' || c == '_' => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_normal_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),

            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),

            KeyCode::Char('/') => {
                self.search_query.clear();
                self.apply_filter();
                self.mode = Mode::Search;
            }

            KeyCode::Char('?') => {
                self.mode = Mode::Help;
            }

            KeyCode::Enter => {
                if let Some(p) = self.selected_profile() {
                    if matches!(p.tool, Tool::Unknown(_)) {
                        self.mode = Mode::Message(
                            "Profile has an unknown tool; cannot use or log in.".into(),
                            true,
                        );
                        return Ok(false);
                    }
                    let name = p.name.clone();
                    let tool = p.tool.clone();
                    ratatui::restore();
                    println!("Launching {} with profile '{}'…", tool.label(), name);
                    self.manager.launch_profile(&name, &[])?;
                }
            }

            KeyCode::Char('l') => {
                self.mode = Mode::LoginName;
                self.input_buffer.clear();
            }

            KeyCode::Char('p') if self.selected_profile().is_some() => {
                if self
                    .selected_profile()
                    .is_some_and(|p| p.tool != Tool::Claude)
                {
                    self.mode = Mode::Message("API keys are Claude-only.".into(), true);
                    return Ok(false);
                }
                self.key_buffer.clear();
                self.gateway_buffer.clear();
                self.pending_gateway = None;
                self.gateway_replaces_defaults = false;
                self.mode = Mode::KeyEntry;
            }
            KeyCode::Char('P') if self.selected_profile().is_some() => {
                self.mode = if self
                    .selected_profile()
                    .is_some_and(|p| p.tool == Tool::Claude)
                {
                    Mode::ConfirmKeyClear
                } else {
                    Mode::Message("API keys are Claude-only.".into(), true)
                };
            }

            KeyCode::Char('a') => {
                self.mode = Mode::AddName;
                self.input_buffer.clear();
            }

            KeyCode::Char('d') | KeyCode::Delete => {
                if self.selected_profile().is_some() {
                    self.selected_in_use = self.selected_session_age();
                    self.mode = Mode::ConfirmDelete;
                }
            }

            // `r` replaces the profile's credentials with whatever ~/.claude
            // currently holds. Harmless when both are the same account, silent
            // account theft when they are not — so it is confirmed against the
            // identities involved.
            KeyCode::Char('r') if self.selected_profile().is_some() => {
                if self
                    .selected_profile()
                    .is_some_and(|p| p.tool != Tool::Claude)
                {
                    self.mode = Mode::Message("Refresh is Claude-only".into(), true);
                    return Ok(false);
                }
                self.current_account = (self.account_probe)();
                self.selected_in_use = self.selected_session_age();
                self.selected_has_history = self.selected_holds_history();
                self.selected_has_key = self
                    .selected_profile()
                    .is_some_and(|p| key::has_key(&self.manager, &p.name));
                self.mode = Mode::ConfirmRefresh;
            }

            _ => {}
        }
        Ok(false)
    }

    fn handle_confirm_refresh(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(p) = self.selected_profile() {
                    if p.tool != Tool::Claude {
                        self.mode = Mode::Message("Refresh is Claude-only".into(), true);
                        return Ok(());
                    }
                    let name = p.name.clone();
                    match self.manager.add_profile_force(&name, false) {
                        Ok(p) => {
                            self.refresh()?;
                            self.select_by_name(&name);
                            self.mode = Mode::Message(
                                format!(
                                    "Profile '{}' refreshed from the current session ({}).",
                                    name,
                                    p.email.as_deref().unwrap_or("unknown account")
                                ),
                                false,
                            );
                        }
                        Err(e) => self.mode = Mode::Message(e.to_string(), true),
                    }
                } else {
                    self.mode = Mode::Normal;
                }
            }
            _ => self.mode = Mode::Normal,
        }
        Ok(())
    }

    fn handle_key_entry(&mut self, event: KeyEvent) -> Result<()> {
        match key::apply_key_event(&mut self.key_buffer, event) {
            InputStep::Continue => {}
            InputStep::Abort => {
                self.gateway_buffer.clear();
                self.pending_gateway = None;
                self.mode = Mode::Normal;
            }
            InputStep::TooLong => unreachable!(),
            InputStep::Complete => {
                if let Some(profile) = self.selected_profile() {
                    let name = profile.name.clone();
                    let check = key::validate_key_input(&self.key_buffer)
                        .and_then(|_| key::precheck_set_key(&self.manager, &name, false));
                    self.mode = match check {
                        Ok(()) => Mode::GatewayEntry,
                        Err(error) => {
                            self.key_buffer.clear();
                            Mode::Message(error.to_string(), true)
                        }
                    };
                } else {
                    self.key_buffer.clear();
                    self.mode = Mode::Normal;
                }
            }
        }
        Ok(())
    }

    fn cancel_gateway_entry(&mut self) {
        self.key_buffer.clear();
        self.gateway_buffer.clear();
        self.pending_gateway = None;
        self.gateway_replaces_defaults = false;
        self.mode = Mode::Normal;
    }

    fn handle_gateway_entry(&mut self, event: KeyEvent) -> Result<()> {
        match key::apply_gateway_event(&mut self.gateway_buffer, event) {
            InputStep::Continue => {}
            InputStep::Abort => self.cancel_gateway_entry(),
            InputStep::TooLong => {
                self.cancel_gateway_entry();
                self.mode = Mode::Message("Gateway input is too long.".into(), true);
            }
            InputStep::Complete => {
                let mut input = std::mem::take(&mut self.gateway_buffer);
                let parsed = key::parse_gateway_input(&input);
                input.clear();
                self.mode = match parsed {
                    Ok(gateway) => {
                        let confirm = matches!(gateway, GatewayInput::Json { .. });
                        if let GatewayInput::Json { url, settings, .. } = &gateway {
                            match gateway::read_defaults(&self.manager.base_dir) {
                                Ok(defaults) => {
                                    self.gateway_replaces_defaults =
                                        defaults.get(url).is_some_and(|old| old != settings)
                                }
                                Err(error) => {
                                    self.key_buffer.clear();
                                    self.mode = Mode::Message(error.to_string(), true);
                                    return Ok(());
                                }
                            }
                        }
                        self.pending_gateway = Some(gateway);
                        if confirm {
                            Mode::ConfirmGatewayDefaults
                        } else {
                            self.finish_key_set(false)?;
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        self.key_buffer.clear();
                        Mode::Message(error.to_string(), true)
                    }
                };
            }
        }
        Ok(())
    }

    fn handle_confirm_gateway_defaults(&mut self, event: KeyEvent) -> Result<()> {
        if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
            self.cancel_gateway_entry();
            return Ok(());
        }
        match event.code {
            KeyCode::Esc => self.cancel_gateway_entry(),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.finish_key_set(true)?
            }
            KeyCode::Char('n') | KeyCode::Char('N') => self.finish_key_set(false)?,
            _ => {}
        }
        Ok(())
    }

    fn finish_key_set(&mut self, save_defaults: bool) -> Result<()> {
        let gateway = self.pending_gateway.take().unwrap_or(GatewayInput::Keep);
        let mut input = std::mem::take(&mut self.key_buffer);
        self.gateway_buffer.clear();
        let Some(profile) = self.selected_profile() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        let name = profile.name.clone();
        self.mode = match key::set_key_with_gateway(
            &self.manager,
            &name,
            &input,
            &self.executable,
            false,
            &gateway,
            save_defaults,
            Utc::now(),
        ) {
            Ok(outcome) => {
                self.last_limits_refresh = None;
                let mut lines = vec![format!("API key saved for profile '{name}'.")];
                lines.extend(outcome.gateway_lines);
                if outcome.running_session {
                    lines.push("Restart running sessions.".into());
                }
                if outcome.overrides_subscription {
                    lines.push("Billing moves to the API.".into());
                }
                if outcome.build_path {
                    lines.push(
                        "Helper uses a build directory; install cswitch and rerun key set.".into(),
                    );
                }
                Mode::Message(lines.join("\n"), false)
            }
            Err(error) => Mode::Message(error.to_string(), true),
        };
        input.clear();
        Ok(())
    }

    fn handle_confirm_key_clear(&mut self, code: KeyCode) -> Result<()> {
        if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y'))
            && let Some(profile) = self.selected_profile()
        {
            let name = profile.name.clone();
            self.mode = match key::clear_key(&self.manager, &name, Utc::now()) {
                Ok(result) => {
                    self.last_limits_refresh = None;
                    let mut message = format!(
                        "API key removed for '{name}'. Fallback: {}.",
                        result.fallback
                    );
                    if result.foreign_helper {
                        message.push_str(" A foreign helper remains active.");
                    }
                    if let Some(line) = result.gateway_line {
                        message.push_str(&format!("\n{line}"));
                    }
                    Mode::Message(message, false)
                }
                Err(error) => Mode::Message(error.to_string(), true),
            };
            return Ok(());
        }
        self.mode = Mode::Normal;
        Ok(())
    }

    fn handle_search_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        match code {
            KeyCode::Esc => {
                self.search_query.clear();
                self.apply_filter();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                // Keep filter, go back to normal mode (so user can press Enter to launch)
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.apply_filter();
            }
            KeyCode::Up | KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up();
            }
            KeyCode::Down | KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down();
            }
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.apply_filter();
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_confirm_delete(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(p) = self.selected_profile() {
                    let name = p.name.clone();
                    match self.manager.remove_profile(&name) {
                        Ok(_) => {
                            self.refresh()?;
                            self.mode =
                                Mode::Message(format!("Profile '{}' removed.", name), false);
                        }
                        Err(e) => self.mode = Mode::Message(e.to_string(), true),
                    }
                }
            }
            _ => self.mode = Mode::Normal,
        }
        Ok(())
    }

    /// Name entry for the unified add flow. Enter advances to the operation
    /// choice; it never creates anything, because at this point we still do not
    /// know whether the user wants this account or a different one.
    fn handle_add_name(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Enter => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    self.mode = Mode::Normal;
                    return Ok(false);
                }
                if let Some(existing) = self.existing_profile_error(&name) {
                    self.mode = Mode::Message(existing, true);
                    return Ok(false);
                }
                // Resolve the live account now so the Copy option can name it.
                self.current_account = (self.account_probe)();
                self.mode = Mode::AddChoice;
            }
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) if c.is_alphanumeric() || c == '-' || c == '_' => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
        Ok(false)
    }

    /// The decision that used to be implicit: copy the account we already have,
    /// or authenticate a different one. Each key maps to exactly one backend
    /// operation — no shared path that could drift into the wrong side effect.
    fn handle_add_choice(&mut self, code: KeyCode) -> Result<bool> {
        let name = self.input_buffer.trim().to_string();
        if name.is_empty() {
            self.mode = Mode::Normal;
            return Ok(false);
        }

        match code {
            KeyCode::Char('c') | KeyCode::Char('C') => {
                match self.manager.add_profile(&name, false) {
                    Ok(p) => {
                        self.refresh()?;
                        self.select_by_name(&name);
                        self.mode = Mode::Message(
                            format!(
                                "Profile '{}' created from the current session ({}).",
                                name,
                                p.email.as_deref().unwrap_or("unknown account")
                            ),
                            false,
                        );
                    }
                    Err(e) => self.mode = Mode::Message(e.to_string(), true),
                }
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.pending = Some(PendingAction::Login {
                    name,
                    method: LoginMethod::ClaudeAi,
                });
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                self.pending = Some(PendingAction::Login {
                    name,
                    method: LoginMethod::Console,
                });
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.pending = Some(PendingAction::CodexLogin { name });
            }
            // Esc steps back to the name, not out of the flow — a typo in the
            // name should not cost the whole interaction.
            KeyCode::Esc | KeyCode::Backspace => self.mode = Mode::AddName,
            KeyCode::Char('q') => self.mode = Mode::Normal,
            _ => {}
        }
        Ok(false)
    }

    /// Fast path for users who already know they want a different account.
    /// Same backend operation as `AddChoice`'s `l`.
    fn handle_login_name(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Enter => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    self.mode = Mode::Normal;
                    return Ok(false);
                }
                if let Some(existing) = self.existing_profile_error(&name) {
                    self.mode = Mode::Message(existing, true);
                    return Ok(false);
                }
                self.pending = Some(PendingAction::Login {
                    name,
                    method: LoginMethod::ClaudeAi,
                });
            }
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) if c.is_alphanumeric() || c == '-' || c == '_' => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Reject a taken name up front, in the popup, instead of letting the
    /// backend bail after the terminal has already been torn down.
    fn existing_profile_error(&self, name: &str) -> Option<String> {
        self.profiles.iter().find(|p| p.name == name).map(|p| {
            format!(
                "Profile '{}' already exists ({}). Delete it first with 'd', or pick another name.",
                name,
                p.email.as_deref().unwrap_or("unknown account")
            )
        })
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Rendering
    // ══════════════════════════════════════════════════════════════════════════

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        if self.mode == Mode::FirstRun {
            self.render_first_run(f, area);
            return;
        }

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_header(f, layout[0]);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(layout[1]);

        self.render_profile_list(f, cols[0]);
        self.render_detail_panel(f, cols[1]);
        self.render_footer(f, layout[2]);

        // Overlays
        match &self.mode.clone() {
            Mode::Help => self.render_help(f),
            Mode::ConfirmDelete => self.render_confirm_delete_popup(f),
            Mode::ConfirmRefresh => self.render_confirm_refresh_popup(f),
            Mode::ConfirmKeyClear => self.render_confirm_key_clear_popup(f),
            Mode::KeyEntry => self.render_key_entry_popup(f),
            Mode::GatewayEntry => self.render_gateway_entry_popup(f),
            Mode::ConfirmGatewayDefaults => self.render_confirm_gateway_defaults_popup(f),
            Mode::AddName => self.render_add_name_popup(f),
            Mode::AddChoice => self.render_add_choice_popup(f),
            Mode::LoginName => self.render_login_name_popup(f),
            Mode::Message(msg, is_err) => self.render_message(f, msg, *is_err),
            _ => {}
        }
    }

    // ── First-run screen ──────────────────────────────────────────────────────

    fn render_first_run(&self, f: &mut Frame, area: Rect) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area);

        let header_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ◆ ", Style::default().fg(ACCENT).bold()),
                Span::styled("claude-switch", Style::default().fg(TEXT).bold()),
                Span::styled("  first run setup", Style::default().fg(DIM)),
            ]))
            .block(header_block),
            layout[0],
        );

        let body_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let inner = body_block.inner(layout[1]);
        f.render_widget(body_block, layout[1]);

        let content: Vec<Line> = if self.claude_dir_found {
            self.render_first_run_detected()
        } else {
            self.render_first_run_no_claude()
        };
        f.render_widget(Paragraph::new(content).wrap(Wrap { trim: false }), inner);

        let footer_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let footer_spans: Vec<Span> = if self.claude_dir_found {
            vec![
                Span::styled(" 1 ", Style::default().fg(ACCENT).bold()),
                Span::styled("copy session  ", Style::default().fg(DIM)),
                Span::styled(" 2 ", Style::default().fg(ACCENT).bold()),
                Span::styled("login new  ", Style::default().fg(DIM)),
                Span::styled(" 3 ", Style::default().fg(ACCENT).bold()),
                Span::styled("codex login  ", Style::default().fg(DIM)),
                Span::styled(" esc ", Style::default().fg(ACCENT).bold()),
                Span::styled("skip  ", Style::default().fg(DIM)),
                Span::styled(" q ", Style::default().fg(ACCENT).bold()),
                Span::styled("quit", Style::default().fg(DIM)),
            ]
        } else {
            vec![
                Span::styled(" any key ", Style::default().fg(ACCENT).bold()),
                Span::styled("open main view  ", Style::default().fg(DIM)),
                Span::styled(" q ", Style::default().fg(ACCENT).bold()),
                Span::styled("quit", Style::default().fg(DIM)),
            ]
        };

        f.render_widget(
            Paragraph::new(Line::from(footer_spans)).block(footer_block),
            layout[2],
        );
    }

    fn render_first_run_detected(&self) -> Vec<Line<'static>> {
        let email = self
            .detected_email
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let name = if self.input_buffer.trim().is_empty() {
            "default"
        } else {
            self.input_buffer.trim()
        };
        let dest = format!("~/.claude-switch/profiles/{}/", name);
        let name_display = if self.input_buffer.trim().is_empty() {
            "█".to_string()
        } else {
            format!("{}█", self.input_buffer.trim())
        };

        vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Welcome to ", Style::default().fg(TEXT)),
                Span::styled("claude-switch", Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(Span::styled(
                "  Manage multiple Claude Code accounts using isolated profile directories.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ✓ ", Style::default().fg(SUCCESS).bold()),
                Span::styled(
                    "Claude Code installation detected",
                    Style::default().fg(TEXT).bold(),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Current account   ", Style::default().fg(DIM)),
                Span::styled(email, Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Set up your first profile:",
                Style::default().fg(TEXT),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Profile name   ", Style::default().fg(DIM)),
                Span::styled(name_display, Style::default().fg(TEXT).bold()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Saves to  ", Style::default().fg(DIM)),
                Span::styled(dest, Style::default().fg(Color::Rgb(140, 200, 140))),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [1] ", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    "Copy active session as this profile",
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(Span::styled(
                "      Uses your existing credentials — no re-login needed",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [2] ", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    "Login to a different account for this profile",
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(Span::styled(
                "      Opens Claude for you to authenticate a new account",
                Style::default().fg(DIM),
            )),
            Line::from(vec![
                Span::styled("  [3] ", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    "Log in to a Codex (ChatGPT) account",
                    Style::default().fg(TEXT),
                ),
            ]),
        ]
    }

    fn render_first_run_no_claude(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Welcome to ", Style::default().fg(TEXT)),
                Span::styled("claude-switch", Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ✗ ", Style::default().fg(DANGER).bold()),
                Span::styled(
                    "No Claude Code installation found at ~/.claude",
                    Style::default().fg(TEXT).bold(),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Claude Code is not set up here. You can still add a Codex profile.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Install   ", Style::default().fg(DIM)),
                Span::styled(
                    if cfg!(target_os = "windows") {
                        "npm install -g @anthropic-ai/claude-code   (in PowerShell/cmd)"
                    } else {
                        "npm install -g @anthropic-ai/claude-code"
                    },
                    Style::default().fg(Color::Rgb(140, 200, 140)),
                ),
            ]),
            Line::from(vec![
                Span::styled("    Log in    ", Style::default().fg(DIM)),
                Span::styled("claude", Style::default().fg(Color::Rgb(140, 200, 140))),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Press [a] to choose Codex login, or install Claude Code first.",
                Style::default().fg(DIM),
            )),
        ]
    }

    // ── Normal view widgets ───────────────────────────────────────────────────

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ◆ ", Style::default().fg(ACCENT).bold()),
                Span::styled("claude-switch", Style::default().fg(TEXT).bold()),
                Span::styled("  profile manager", Style::default().fg(DIM)),
            ]))
            .block(block),
            area,
        );

        let count = self.filtered_indices.len();
        let total = self.profiles.len();
        let label = if count == total {
            format!(" {} profile{} ", total, if total == 1 { "" } else { "s" })
        } else {
            format!(" {}/{} ", count, total)
        };

        let count_width = (label.len() as u16 + 1).min(area.width.saturating_sub(1));
        if count_width > 0 {
            let count_area = Rect {
                x: area.x + area.width.saturating_sub(count_width + 1),
                y: area.y + 1,
                width: count_width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(label, Style::default().fg(DIM)))
                    .alignment(Alignment::Right),
                count_area,
            );
        }
    }

    fn render_profile_list(&mut self, f: &mut Frame, area: Rect) {
        let title_line: Line = if self.mode == Mode::Search {
            Line::from(vec![
                Span::styled(" /", Style::default().fg(SEARCH_HL).bold()),
                Span::styled(
                    self.search_query.clone(),
                    Style::default().fg(SEARCH_HL).bold(),
                ),
                Span::styled("█ ", Style::default().fg(SEARCH_HL)),
            ])
        } else if !self.search_query.is_empty() {
            Line::from(vec![
                Span::styled(" Search: ", Style::default().fg(DIM)),
                Span::styled(self.search_query.clone(), Style::default().fg(SEARCH_HL)),
                Span::styled(" ", Style::default()),
            ])
        } else {
            Line::from(Span::styled(
                " Profiles ",
                Style::default().fg(ACCENT).bold(),
            ))
        };

        let block = Block::default()
            .title(title_line)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if self.mode == Mode::Search {
                Style::default().fg(SEARCH_HL)
            } else {
                Style::default().fg(BORDER)
            })
            .style(Style::default().bg(PANEL));

        let items: Vec<ListItem> = self
            .filtered_indices
            .iter()
            .map(|&i| {
                let p = &self.profiles[i];
                let email = p.email.as_deref().unwrap_or("no email");
                let cell = self
                    .usage_cache
                    .cells
                    .get(&p.name)
                    .map_or("—", String::as_str);
                ListItem::new(vec![
                    profile_name_line(&p.name, cell, area.width),
                    Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(
                            format!("{} · {email}", p.tool.label()),
                            Style::default().fg(DIM),
                        ),
                    ]),
                ])
            })
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(35, 35, 45))
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ")
            .highlight_spacing(HighlightSpacing::Always);

        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_detail_panel(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .title(Line::from(Span::styled(
                " Details ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let inner = block.inner(area);
        f.render_widget(block, area);

        let Some(profile) = self.selected_profile() else {
            let hint = if self.search_query.is_empty() {
                "  No profiles yet. Press 'l' to login, 'a' to add."
            } else {
                "  No profiles match your search."
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(DIM)))),
                inner,
            );
            return;
        };

        let profile_dir = self.manager.profile_dir(&profile.name);

        let mut lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Name         ", Style::default().fg(DIM)),
                Span::styled(profile.name.clone(), Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(vec![
                Span::styled("  Tool         ", Style::default().fg(DIM)),
                Span::styled(profile.tool.label(), Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("  Email        ", Style::default().fg(DIM)),
                Span::styled(
                    profile.email.clone().unwrap_or("unknown".into()),
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Auth         ", Style::default().fg(DIM)),
                Span::styled(
                    if profile.tool == Tool::Claude {
                        self.auth_modes
                            .get(&profile.name)
                            .map(AuthMode::label)
                            .unwrap_or_else(|| "unreadable".into())
                    } else {
                        "—".into()
                    },
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Gateway      ", Style::default().fg(DIM)),
                Span::styled(
                    if profile.tool == Tool::Claude {
                        key::gateway_info(&self.manager, &profile.name)
                    } else {
                        "—".into()
                    },
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Added        ", Style::default().fg(DIM)),
                Span::styled(
                    profile.added.format("%Y-%m-%d %H:%M UTC").to_string(),
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Last used    ", Style::default().fg(DIM)),
                Span::styled(
                    profile
                        .last_used
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or("never".into()),
                    Style::default().fg(TEXT),
                ),
            ]),
        ];
        if profile.tool == Tool::Codex {
            lines.push(Line::from(vec![
                Span::styled("  Plan         ", Style::default().fg(DIM)),
                Span::styled(
                    self.codex_plans
                        .get(&profile.name)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| "—".into()),
                    Style::default().fg(TEXT),
                ),
            ]));
        }
        lines.push(Line::from(""));
        if profile.tool != Tool::Claude {
            lines.push(Line::from("  Plan limits  —"));
        } else {
            match self.limits.get(&profile.name) {
                Some(Limits::Snapshot(snapshot)) => {
                    lines.push(Line::from(vec![
                        Span::styled("  Plan limits  ", Style::default().fg(DIM)),
                        Span::styled(
                            format!("as of {}", snapshot.age(self.limits_now)),
                            Style::default().fg(TEXT),
                        ),
                    ]));
                    if snapshot.windows.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "    No limit windows in this snapshot.",
                            Style::default().fg(MUTED),
                        )));
                    }
                    for window in &snapshot.windows {
                        lines.push(limit_line(window, self.limits_now));
                    }
                }
                Some(Limits::AccountMismatch) => lines.push(Line::from(Span::styled(
                    "  Plan limits  mismatch: another account's snapshot",
                    Style::default().fg(MUTED),
                ))),
                Some(Limits::Unreadable) => lines.push(Line::from(Span::styled(
                    "  Plan limits  unreadable: file couldn't be read",
                    Style::default().fg(MUTED),
                ))),
                Some(Limits::NoSnapshot) | None => lines.push(Line::from(Span::styled(
                    "  Plan limits  no data: no cached snapshot yet",
                    Style::default().fg(MUTED),
                ))),
            }
            lines.push(Line::from(""));
            for line in self.usage_panel_text(&profile.name).lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(TEXT),
                )));
            }
        }
        lines.extend([
            Line::from(""),
            Line::from(vec![
                Span::styled("  Config dir   ", Style::default().fg(DIM)),
                Span::styled(
                    profile_dir.display().to_string(),
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  ─────────────────────────────────────────",
                Style::default().fg(BORDER),
            )),
            Line::from(""),
            Line::from(Span::styled("  Launch command", Style::default().fg(DIM))),
            Line::from(Span::styled(
                if cfg!(target_os = "windows") {
                    format!(
                        "  $env:{}='{}'; {}",
                        if profile.tool == Tool::Codex {
                            "CODEX_HOME"
                        } else {
                            "CLAUDE_CONFIG_DIR"
                        },
                        profile_dir.display(),
                        if profile.tool == Tool::Codex {
                            "codex"
                        } else {
                            "claude"
                        }
                    )
                } else {
                    format!(
                        "  {}='{}' {}",
                        if profile.tool == Tool::Codex {
                            "CODEX_HOME"
                        } else {
                            "CLAUDE_CONFIG_DIR"
                        },
                        profile_dir.display(),
                        if profile.tool == Tool::Codex {
                            "codex"
                        } else {
                            "claude"
                        }
                    )
                },
                Style::default().fg(Color::Rgb(140, 200, 140)),
            )),
        ]);

        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let keys: Vec<(&str, &str)> = if self.mode == Mode::Search {
            vec![("↑/↓", "navigate"), ("enter", "confirm"), ("esc", "clear")]
        } else {
            vec![
                ("↑↓/jk", "nav"),
                ("enter", "launch"),
                ("/", "search"),
                ("a", "add account"),
                ("l", "login"),
                ("p", "set key"),
                ("P", "clear key"),
                ("r", "refresh"),
                ("d", "delete"),
                ("?", "help"),
                ("q", "quit"),
            ]
        };

        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, v)| {
                vec![
                    Span::styled(format!(" {} ", k), Style::default().fg(ACCENT).bold()),
                    Span::styled(*v, Style::default().fg(DIM)),
                    Span::styled(" ", Style::default()),
                ]
            })
            .collect();

        f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    // ── Overlay popups ────────────────────────────────────────────────────────

    fn render_help(&self, f: &mut Frame) {
        let area = centered_rect(60, 20, f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Help — Keybindings ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        let help_entries: Vec<(&str, &str)> = vec![
            ("↑/↓  j/k", "Navigate profiles"),
            ("Enter", "Launch selected profile's tool"),
            ("/", "Search profiles by name or email"),
            ("a", "Add account — choose Claude or Codex login"),
            ("l", "Login — straight to a different account"),
            ("p", "Set API key for selected profile"),
            ("P", "Clear API key after confirmation"),
            ("r", "Refresh — overwrite with current session"),
            ("d / Del", "Delete selected profile"),
            ("?", "Toggle this help dialog"),
            ("q / Esc", "Quit"),
        ];

        let mut lines: Vec<Line> = vec![Line::from("")];

        for (key, desc) in &help_entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<14}", key), Style::default().fg(ACCENT).bold()),
                Span::styled(*desc, Style::default().fg(TEXT)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ───────────────────────────────────────",
            Style::default().fg(BORDER),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  A profile is a config environment, not an identity.",
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(Span::styled(
            "  Its account comes from logging in — never from the name.",
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Press any key to close",
            Style::default().fg(DIM),
        )));

        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_confirm_delete_popup(&self, f: &mut Frame) {
        let name = self
            .selected_profile()
            .map(|p| p.name.as_str())
            .unwrap_or("?");

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Confirm Delete ",
                Style::default().fg(DANGER).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(DANGER))
            .style(Style::default().bg(PANEL));

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Delete profile ", Style::default().fg(TEXT)),
                Span::styled(name.to_string(), Style::default().fg(DANGER).bold()),
                Span::styled("? This cannot be undone.", Style::default().fg(TEXT)),
            ]),
            Line::from(""),
        ];

        if let Some(secs) = self.selected_in_use {
            lines.push(Line::from(Span::styled(
                format!(
                    "  In use? Written {} by a Claude session.",
                    describe_age(secs)
                ),
                Style::default().fg(DANGER).bold(),
            )));
            lines.push(Line::from(Span::styled(
                "  Deleting pulls the config out from under that terminal.",
                Style::default().fg(DANGER),
            )));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("y", Style::default().fg(DANGER).bold()),
            Span::styled(" confirm   ", Style::default().fg(DIM)),
            Span::styled("any other key", Style::default().fg(ACCENT).bold()),
            Span::styled(" cancel", Style::default().fg(DIM)),
        ]));

        let area = centered_rect(56, lines.len() as u16 + 2, f.area());
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    }

    fn render_add_name_popup(&self, f: &mut Frame) {
        let area = centered_rect(52, 7, f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Add Account ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Profile name: ", Style::default().fg(DIM)),
                    Span::styled(self.input_buffer.clone(), Style::default().fg(TEXT).bold()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "  A local label. You pick the account on the next step.",
                    Style::default().fg(DIM),
                )),
            ]))
            .block(block),
            area,
        );
    }

    /// The step that makes the side effect explicit before it happens.
    fn render_add_choice_popup(&self, f: &mut Frame) {
        let area = centered_rect(66, 17, f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Add Account — which account? ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        let current = self.current_account.as_deref().unwrap_or("unknown account");

        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Profile: ", Style::default().fg(DIM)),
                    Span::styled(self.input_buffer.clone(), Style::default().fg(TEXT).bold()),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  [c] ", Style::default().fg(ACCENT).bold()),
                    Span::styled("Copy current session  ", Style::default().fg(TEXT)),
                    Span::styled(current.to_string(), Style::default().fg(SUCCESS)),
                ]),
                Line::from(Span::styled(
                    "      Same account, separate settings and history.",
                    Style::default().fg(DIM),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  [l] ", Style::default().fg(ACCENT).bold()),
                    Span::styled(
                        "Log in to a different Claude account",
                        Style::default().fg(TEXT),
                    ),
                ]),
                Line::from(Span::styled(
                    "      Opens Claude's login. Sign out of claude.ai first,",
                    Style::default().fg(DIM),
                )),
                Line::from(Span::styled(
                    "      or it will grant the account already signed in there.",
                    Style::default().fg(DIM),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  [p] ", Style::default().fg(ACCENT).bold()),
                    Span::styled(
                        "Log in with the Anthropic Console (API billing)",
                        Style::default().fg(TEXT),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  [o] ", Style::default().fg(ACCENT).bold()),
                    Span::styled(
                        "Log in to a Codex (ChatGPT) account",
                        Style::default().fg(TEXT),
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "  Esc back · q cancel",
                    Style::default().fg(MUTED),
                )),
            ]))
            .block(block),
            area,
        );
    }

    fn render_confirm_refresh_popup(&self, f: &mut Frame) {
        let (name, profile_email) = match self.selected_profile() {
            Some(p) => (
                p.name.clone(),
                p.email.as_deref().unwrap_or("unknown account").to_string(),
            ),
            None => return,
        };
        let current = self.current_account.as_deref().unwrap_or("unknown account");
        // Only cross-account refreshes actually destroy anything.
        let replaces_account = self
            .current_account
            .as_deref()
            .map(|c| !c.eq_ignore_ascii_case(&profile_email))
            .unwrap_or(true);
        // Overwriting a profile another terminal is using is its own hazard,
        // independent of which account it holds — as is destroying history,
        // which happens even on a same-account refresh.
        let color =
            if replaces_account || self.selected_in_use.is_some() || self.selected_has_history {
                DANGER
            } else {
                ACCENT
            };

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Refresh profile ",
                Style::default().fg(color).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color))
            .style(Style::default().bg(PANEL));

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Overwrite '", Style::default().fg(TEXT)),
                Span::styled(name, Style::default().fg(TEXT).bold()),
                Span::styled("' with the current session.", Style::default().fg(TEXT)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Profile holds: ", Style::default().fg(DIM)),
                Span::styled(profile_email, Style::default().fg(MUTED)),
            ]),
            Line::from(vec![
                Span::styled("  Will become:   ", Style::default().fg(DIM)),
                Span::styled(current.to_string(), Style::default().fg(SUCCESS)),
            ]),
            Line::from(""),
        ];

        if replaces_account {
            lines.push(Line::from(Span::styled(
                "  This replaces a DIFFERENT account's credentials.",
                Style::default().fg(DANGER).bold(),
            )));
            lines.push(Line::from(Span::styled(
                "  You will have to log that account in again.",
                Style::default().fg(DANGER),
            )));
            lines.push(Line::from(""));
        }

        if let Some(secs) = self.selected_in_use {
            lines.push(Line::from(Span::styled(
                format!(
                    "  In use? Written {} by a Claude session.",
                    describe_age(secs)
                ),
                Style::default().fg(DANGER).bold(),
            )));
            lines.push(Line::from(Span::styled(
                "  Another terminal may have this profile open right now.",
                Style::default().fg(DANGER),
            )));
            lines.push(Line::from(""));
        }

        // A refresh reseeds without history, so whatever this profile has
        // accumulated goes with it. Credentials can be logged in again;
        // transcripts cannot be recovered at all.
        if self.selected_has_history {
            lines.push(Line::from(Span::styled(
                "  This profile's conversation history will be deleted.",
                Style::default().fg(DANGER).bold(),
            )));
            lines.push(Line::from(Span::styled(
                "  Transcripts and prompt history cannot be recovered.",
                Style::default().fg(DANGER),
            )));
            lines.push(Line::from(""));
        }

        if self.selected_has_key {
            lines.push(Line::from(Span::styled(
                "  Its saved API key will be deleted.",
                Style::default().fg(DANGER).bold(),
            )));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(Span::styled(
            "  [y] Confirm   ·   any other key cancels",
            Style::default().fg(MUTED),
        )));

        let area = centered_rect(66, lines.len() as u16 + 2, f.area());
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    }

    fn render_login_name_popup(&self, f: &mut Frame) {
        let area = centered_rect(55, 8, f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Login New Account ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Profile name: ", Style::default().fg(DIM)),
                    Span::styled(self.input_buffer.clone(), Style::default().fg(TEXT).bold()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "  Claude will open for you to log in with a new account.",
                    Style::default().fg(DIM),
                )),
                Line::from(Span::styled(
                    "  Exit Claude after login to finish setup.",
                    Style::default().fg(DIM),
                )),
            ]))
            .block(block),
            area,
        );
    }

    fn render_key_entry_popup(&self, f: &mut Frame) {
        let area = centered_rect(60, 7, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Set API key ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));
        let lines = vec![
            Line::from(""),
            Line::from(format!(
                "  API key: {}",
                "•".repeat(self.key_buffer.chars().count())
            )),
            Line::from(""),
            Line::from("  Enter continues · Esc or Ctrl-C cancels"),
        ];
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_gateway_entry_popup(&self, f: &mut Frame) {
        let area = centered_rect(82, 9, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Gateway ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));
        let current = self.selected_profile().map_or_else(
            || "the Anthropic API".into(),
            |profile| key::gateway_display(&self.manager, &profile.name),
        );
        let lines = vec![
            Line::from(""),
            Line::from(format!("  Gateway now: {current}")),
            Line::from(format!(
                "  Input: {} characters (hidden)",
                self.gateway_buffer.chars().count()
            )),
            Line::from(""),
            Line::from("  Enter keeps it · \"none\" for the Anthropic API"),
            Line::from("  Paste a URL or the provider's JSON · a blank line ends a JSON paste"),
            Line::from("  Esc cancels"),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_confirm_gateway_defaults_popup(&self, f: &mut Frame) {
        let area = centered_rect(82, 12, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Save gateway defaults ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));
        let mut lines = vec![Line::from("")];
        if let Some(GatewayInput::Json {
            url,
            settings,
            dropped,
        }) = &self.pending_gateway
        {
            lines.push(Line::from(format!(
                "  Read: {}",
                settings.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
            if !dropped.is_empty() {
                lines.push(Line::from(format!("  Ignored: {}", dropped.join(", "))));
            }
            let suffix = if self.gateway_replaces_defaults {
                " (replaces the saved ones)"
            } else {
                ""
            };
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "  Save these {} settings as the defaults for {url}? [Y/n]{suffix}",
                settings.len()
            )));
        }
        lines.push(Line::from("  Enter/y saves · n skips · Esc cancels"));
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_confirm_key_clear_popup(&self, f: &mut Frame) {
        let area = centered_rect(60, 6, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Clear API key ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DANGER))
            .style(Style::default().bg(PANEL));
        f.render_widget(
            Paragraph::new(
                "\n  Delete this profile's saved API key?\n\n  [y] Confirm · any other key cancels",
            )
            .block(block),
            area,
        );
    }

    fn render_message(&self, f: &mut Frame, msg: &str, is_err: bool) {
        let area = centered_rect(
            78,
            (msg.lines().count() as u16 + 5).min(f.area().height),
            f.area(),
        );
        f.render_widget(Clear, area);

        let color = if is_err { DANGER } else { SUCCESS };
        let title = if is_err { " Error " } else { " Done " };

        let block = Block::default()
            .title(Line::from(Span::styled(
                title,
                Style::default().fg(color).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color))
            .style(Style::default().bg(PANEL));

        f.render_widget(
            Paragraph::new(Text::from(
                std::iter::once(Line::from(""))
                    .chain(msg.lines().map(|line| {
                        Line::from(Span::styled(format!("  {line}"), Style::default().fg(TEXT)))
                    }))
                    .chain([
                        Line::from(""),
                        Line::from(Span::styled(
                            "  Press any key to continue",
                            Style::default().fg(DIM),
                        )),
                    ])
                    .collect::<Vec<_>>(),
            ))
            .block(block)
            .wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn pending_login_message(name: &str, result: &LoginOutcome, method: Option<LoginMethod>) -> String {
    if result.email.is_none() {
        if result.tool == Tool::Codex {
            return format!("Codex login completed for profile '{name}' (email unavailable).");
        }
        if method == Some(LoginMethod::Console) {
            return format!("Console login completed for profile '{name}' (email unavailable).");
        }
    }
    let others: Vec<&str> = result
        .same_account_as
        .iter()
        .map(String::as_str)
        .filter(|other| *other != name)
        .collect();
    if others.is_empty() {
        format!(
            "Profile '{}' logged in as {}.",
            name,
            result.display_email()
        )
    } else {
        format!(
            "Profile '{}' is {} — the same {} account as {}.",
            name,
            result.display_email(),
            result.tool.label(),
            others.join(", ")
        )
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn limit_line(window: &Window, now: DateTime<Utc>) -> Line<'static> {
    let reset = window.rolled_over(now);
    let color = if reset {
        MUTED
    } else {
        match window.severity.as_deref() {
            Some("normal") => SUCCESS,
            Some("warning") => ACCENT,
            Some("critical") => DANGER,
            _ => MUTED,
        }
    };
    let value = if reset {
        format!("reset (was {:.0}%)", window.percent.round())
    } else {
        window.percent_label(now)
    };
    let bar = if reset {
        "░".repeat(10)
    } else {
        window.bar()
    };
    let reset_time = window
        .resets_at
        .map(|time| time.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string());
    Line::from(Span::styled(
        format!(
            "  {:<14} {} {:<5} {}",
            window.label(),
            bar,
            value,
            reset_time
        ),
        Style::default().fg(color),
    ))
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let w = area.width * percent_x / 100;
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: w,
        height: height.min(area.height),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};
    use std::fs;
    use tempfile::TempDir;

    const STUB_EMAIL: &str = "current@example.com";

    fn stub_account() -> Option<String> {
        Some(STUB_EMAIL.to_string())
    }

    fn no_account() -> Option<String> {
        None
    }

    /// An App wired to a manager that cannot reach the real `~/.claude-switch`,
    /// with the live-account probe stubbed out.
    fn make_app(tmp: &TempDir, existing: &[(&str, Option<&str>)]) -> App {
        let manager = ProfileManager::with_base_dir(tmp.path().join(".claude-switch")).unwrap();

        for (name, email) in existing {
            let mut registry = manager.load_registry().unwrap();
            registry.profiles.insert(
                name.to_string(),
                Profile {
                    name: name.to_string(),
                    tool: Tool::Claude,
                    email: email.map(String::from),
                    added: Utc::now(),
                    last_used: None,
                },
            );
            let content = serde_json::to_string_pretty(&registry).unwrap();
            std::fs::write(tmp.path().join(".claude-switch/registry.json"), content).unwrap();
        }

        let mut app = App::new(manager).unwrap();
        app.account_probe = stub_account;
        // `App::new` starts in FirstRun when the registry is empty, which also
        // pre-seeds the buffer with "default". These tests are about the normal
        // TUI — the case the bug lived in — where `a`/`l` clear the buffer.
        app.mode = Mode::Normal;
        app.input_buffer.clear();
        app
    }

    fn render_text_at(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn render_text(app: &mut App) -> String {
        render_text_at(app, 120, 40)
    }

    #[test]
    fn console_choice_routes_through_pending_action() {
        // Known-bad: [p] falling through to the subscription login.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "api".to_string();
        app.handle_add_choice(KeyCode::Char('p')).unwrap();
        assert_eq!(
            app.pending,
            Some(PendingAction::Login {
                name: "api".to_string(),
                method: LoginMethod::Console
            })
        );
        assert!(app.manager.load_registry().unwrap().profiles.is_empty());
    }

    #[test]
    fn key_popup_masks_secret_and_escape_clears_it() {
        // Known-bad: the entered key appears in a TUI frame or survives Escape.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("api", Some("user@example.com"))]);
        app.handle_normal_key(KeyCode::Char('p'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.mode, Mode::KeyEntry);
        app.key_buffer = "sk-ant-api03-TESTKEY000".to_string();
        let frame = render_text(&mut app);
        assert!(!frame.contains("sk-ant-api03-TESTKEY000"));
        assert!(frame.contains('•'));
        app.handle_key_entry(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.key_buffer.is_empty());
    }

    #[test]
    fn t4_foreign_helper_refuses_before_gateway_entry() {
        // Known-bad: bypassing the TUI pre-check opens GatewayEntry over a foreign helper.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("api", Some("user@example.com"))]);
        let profile_dir = app.manager.profile_dir("api");
        fs::create_dir_all(&profile_dir).unwrap();
        let settings = br#"{"apiKeyHelper":"foreign helper","env":{"OTHER":"keep"}}"#;
        let path = profile_dir.join("settings.json");
        fs::write(&path, settings).unwrap();
        app.handle_normal_key(KeyCode::Char('p'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.mode, Mode::KeyEntry);
        for ch in "TESTKEY".chars() {
            app.handle_key_entry(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key_entry(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(
            matches!(&app.mode, Mode::Message(message, true) if message.contains("foreign apiKeyHelper"))
        );
        assert!(app.key_buffer.is_empty());
        assert!(!key::key_path(&app.manager.base_dir, "api").exists());
        assert!(!key::manifest_path(&app.manager.base_dir, "api").exists());
        assert_eq!(fs::read(&path).unwrap(), settings);
    }

    #[test]
    fn g13_gateway_modes_hide_pasted_token_and_abort_clears_both_buffers() {
        // Known-bad: a pasted JSON token reaches a TUI frame or survives cancellation.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("api", Some("user@example.com"))]);
        let json = r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com","ANTHROPIC_AUTH_TOKEN":"TOKEN-CANARY"}}"#;
        let executable = tmp.path().join("cswitch");
        fs::write(&executable, "synthetic executable").unwrap();
        let parsed = key::parse_gateway_input(json).unwrap();
        let set = key::set_key_with_gateway(
            &app.manager,
            "api",
            "TESTKEY",
            &executable,
            false,
            &parsed,
            true,
            Utc::now(),
        )
        .unwrap();
        let info = format!(
            "{} {}",
            key::read_auth_mode(&app.manager, "api", Ok(None)).label(),
            key::gateway_info(&app.manager, "api")
        );
        let list = crate::list_output(&app.manager, Utc::now()).unwrap();
        let defaults = gateway::list(&app.manager.base_dir).unwrap();
        for output in [set.gateway_lines.join("\n"), info, list, defaults] {
            assert!(!output.contains("TOKEN-CANARY") && !output.contains("TESTKEY"));
        }
        fn collect(root: &std::path::Path, output: &mut Vec<u8>, skip_key: bool) {
            for entry in fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if skip_key && path.file_name().is_some_and(|name| name == "api.key") {
                    continue;
                }
                if fs::symlink_metadata(&path).unwrap().file_type().is_dir() {
                    collect(&path, output, skip_key);
                } else {
                    output.extend(fs::read(&path).unwrap());
                }
            }
        }
        let mut files = Vec::new();
        collect(&app.manager.base_dir, &mut files, true);
        assert!(!files.windows(7).any(|window| window == b"TESTKEY"));
        assert!(!files.windows(12).any(|window| window == b"TOKEN-CANARY"));
        app.handle_normal_key(KeyCode::Char('p'), KeyModifiers::NONE)
            .unwrap();
        app.key_buffer = "TESTKEY".into();
        app.handle_key_entry(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::GatewayEntry);
        app.gateway_buffer = json.into();
        assert!(!render_text(&mut app).contains("TOKEN-CANARY"));
        app.handle_gateway_entry(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::ConfirmGatewayDefaults);
        let frame = render_text(&mut app);
        assert!(frame.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!frame.contains("TOKEN-CANARY"));
        app.handle_confirm_gateway_defaults(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            app.key_buffer.is_empty()
                && app.gateway_buffer.is_empty()
                && app.pending_gateway.is_none()
        );
        key::clear_key(&app.manager, "api", Utc::now()).unwrap();
        assert!(!key::key_path(&app.manager.base_dir, "api").exists());
        files.clear();
        collect(&app.manager.base_dir, &mut files, false);
        assert!(!files.windows(7).any(|window| window == b"TESTKEY"));
        assert!(!files.windows(12).any(|window| window == b"TOKEN-CANARY"));
    }

    #[test]
    fn k_moves_up_in_normal_mode_without_opening_key_entry() {
        // Known-bad: binding k to KeyEntry removes vim-style up navigation.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(
            &tmp,
            &[
                ("alpha", Some("alpha@example.com")),
                ("beta", Some("beta@example.com")),
            ],
        );
        assert_eq!(app.selected_profile().unwrap().name, "alpha");
        app.handle_normal_key(KeyCode::Char('k'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.selected_profile().unwrap().name, "beta");
        assert_eq!(app.mode, Mode::Normal);
        app.handle_normal_key(KeyCode::Char('j'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.selected_profile().unwrap().name, "alpha");
        app.handle_normal_key(KeyCode::Char('K'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        app.handle_normal_key(KeyCode::Char('P'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.mode, Mode::ConfirmKeyClear);
    }

    #[test]
    fn refresh_confirmation_names_saved_key_loss() {
        // Known-bad: refresh deletes a key without warning in the confirmation.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("api", Some("user@example.com"))]);
        fs::create_dir_all(app.manager.base_dir.join("keys")).unwrap();
        fs::write(key::key_path(&app.manager.base_dir, "api"), "synthetic\n").unwrap();
        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.mode, Mode::ConfirmRefresh);
        assert!(render_text(&mut app).contains("Its saved API key will be deleted."));
    }

    fn type_name(app: &mut App, name: &str) {
        for c in name.chars() {
            app.handle_add_name(KeyCode::Char(c)).unwrap();
        }
    }

    #[test]
    fn limits_cache_refreshes_on_selection_or_after_thirty_seconds() {
        // Known-bad: reading during render rereads every frame, while never refreshing leaves a stale panel.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(
            &tmp,
            &[
                ("alpha", Some("a@example.com")),
                ("beta", Some("b@example.com")),
            ],
        );
        let path = app.manager.profile_dir("alpha");
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join(".claude.json");
        let cache = serde_json::json!({
            "cachedUsageUtilization": {
                "fetchedAtMs": 1_894_021_200_000_i64,
                "utilization": {"limits": [{"kind":"session", "group":"session", "percent":12}]}
            }
        });
        std::fs::write(&file, serde_json::to_vec(&cache).unwrap()).unwrap();
        std::fs::create_dir_all(app.manager.profile_dir("beta")).unwrap();
        std::fs::write(app.manager.profile_dir("beta").join(".claude.json"), b"{}").unwrap();
        let start = Instant::now();
        app.refresh_limits_if_due(start);
        assert!(matches!(app.limits.get("alpha"), Some(Limits::Snapshot(_))));
        std::fs::write(&file, b"{}").unwrap();
        app.refresh_limits_if_due(start + Duration::from_secs(29));
        assert!(matches!(app.limits.get("alpha"), Some(Limits::Snapshot(_))));
        app.refresh_limits_if_due(start + Duration::from_secs(30));
        assert_eq!(app.limits.get("alpha"), Some(&Limits::NoSnapshot));
        app.move_down();
        app.refresh_limits_if_due(start + Duration::from_secs(31));
        assert_eq!(app.limits.get("beta"), Some(&Limits::NoSnapshot));
    }

    #[test]
    fn limit_lines_use_severity_palette_and_mute_reset() {
        // Known-bad: a reset retains its old critical colour or unknown severity looks normal.
        let now = DateTime::parse_from_rfc3339("2030-01-07T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut window = Window {
            group: crate::limits::WindowGroup::Session,
            kind: "session".to_string(),
            percent: 40.0,
            severity: Some("normal".to_string()),
            resets_at: None,
            is_active: None,
        };
        assert_eq!(limit_line(&window, now).spans[0].style.fg, Some(SUCCESS));
        window.resets_at = Some(now + chrono::Duration::hours(1));
        assert!(limit_line(&window, now).spans[0].content.chars().count() <= 46);
        window.severity = Some("warning".to_string());
        assert_eq!(limit_line(&window, now).spans[0].style.fg, Some(ACCENT));
        window.severity = Some("critical".to_string());
        assert_eq!(limit_line(&window, now).spans[0].style.fg, Some(DANGER));
        window.severity = None;
        assert_eq!(limit_line(&window, now).spans[0].style.fg, Some(MUTED));
        window.severity = Some("critical".to_string());
        window.resets_at = Some(now - chrono::Duration::hours(1));
        let line = limit_line(&window, now);
        assert_eq!(line.spans[0].style.fg, Some(MUTED));
        assert!(line.spans[0].content.contains("reset (was 40%)"));
    }

    // ── `a` → name → choice ───────────────────────────────────────────────────

    #[test]
    fn add_key_opens_name_entry_not_a_copy() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("default", Some("me@example.com"))]);

        app.handle_normal_key(KeyCode::Char('a'), KeyModifiers::NONE)
            .unwrap();

        assert_eq!(app.mode, Mode::AddName);
        assert!(app.input_buffer.is_empty());
        // Nothing may be created before the user has said which account.
        assert!(app.pending.is_none());
        assert_eq!(app.profiles.len(), 1);
    }

    #[test]
    fn name_entry_advances_to_choice_and_resolves_current_account() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddName;

        type_name(&mut app, "business");
        app.handle_add_name(KeyCode::Enter).unwrap();

        assert_eq!(app.mode, Mode::AddChoice);
        assert_eq!(app.current_account.as_deref(), Some(STUB_EMAIL));
        // Still nothing created — the choice has not been made yet.
        assert!(app.pending.is_none());
        assert!(app.manager.load_registry().unwrap().profiles.is_empty());
    }

    #[test]
    fn choice_screen_falls_back_when_account_is_unknown() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.account_probe = no_account;
        app.mode = Mode::AddName;

        type_name(&mut app, "business");
        app.handle_add_name(KeyCode::Enter).unwrap();

        assert_eq!(app.mode, Mode::AddChoice);
        assert!(app.current_account.is_none());
    }

    #[test]
    fn empty_name_cancels_instead_of_advancing() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddName;

        app.handle_add_name(KeyCode::Enter).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending.is_none());
    }

    #[test]
    fn name_entry_ignores_characters_invalid_in_a_directory_name() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddName;

        for c in "a/b .c*".chars() {
            app.handle_add_name(KeyCode::Char(c)).unwrap();
        }

        assert_eq!(app.input_buffer, "abc");
    }

    #[test]
    fn duplicate_name_is_rejected_before_the_terminal_is_torn_down() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some("me@example.com"))]);
        app.mode = Mode::AddName;

        type_name(&mut app, "business");
        app.handle_add_name(KeyCode::Enter).unwrap();

        match &app.mode {
            Mode::Message(msg, is_err) => {
                assert!(*is_err, "duplicate name should be an error");
                assert!(msg.contains("already exists"), "{msg}");
                assert!(msg.contains("me@example.com"), "{msg}");
            }
            other => panic!("expected an error message, got {other:?}"),
        }
        assert!(app.pending.is_none());
    }

    // ── Routing: Copy and Login cannot be confused ────────────────────────────

    #[test]
    fn choice_l_routes_to_login_and_creates_nothing_yet() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "business".to_string();

        app.handle_add_choice(KeyCode::Char('l')).unwrap();

        assert_eq!(
            app.pending,
            Some(PendingAction::Login {
                name: "business".to_string(),
                method: LoginMethod::ClaudeAi,
            })
        );
        // Login must not register anything until Claude has authenticated.
        assert!(app.manager.load_registry().unwrap().profiles.is_empty());
    }

    #[test]
    fn choice_c_never_routes_to_login() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "copy-target".to_string();

        // Copy runs against the real `~/.claude`, so it may succeed or fail
        // depending on the machine. Either way it must resolve inline and must
        // never queue an authentication.
        app.handle_add_choice(KeyCode::Char('c')).unwrap();

        assert!(app.pending.is_none(), "Copy must not queue a login");
        assert!(matches!(app.mode, Mode::Message(_, _)));
    }

    #[test]
    fn choice_accepts_uppercase_keys() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "business".to_string();

        app.handle_add_choice(KeyCode::Char('L')).unwrap();

        assert_eq!(
            app.pending,
            Some(PendingAction::Login {
                name: "business".to_string(),
                method: LoginMethod::ClaudeAi,
            })
        );
    }

    // ── Back / cancel ────────────────────────────────────────────────────────

    #[test]
    fn esc_from_choice_steps_back_to_the_name_it_came_from() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "business".to_string();

        app.handle_add_choice(KeyCode::Esc).unwrap();

        assert_eq!(app.mode, Mode::AddName);
        assert_eq!(
            app.input_buffer, "business",
            "the typed name should survive"
        );
        assert!(app.pending.is_none());
    }

    #[test]
    fn q_from_choice_cancels_the_whole_flow() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "business".to_string();

        app.handle_add_choice(KeyCode::Char('q')).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending.is_none());
        assert!(app.manager.load_registry().unwrap().profiles.is_empty());
    }

    #[test]
    fn esc_from_name_entry_creates_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddName;
        type_name(&mut app, "business");

        app.handle_add_name(KeyCode::Esc).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending.is_none());
        assert!(app.manager.load_registry().unwrap().profiles.is_empty());
    }

    #[test]
    fn unknown_key_on_choice_screen_does_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "business".to_string();

        app.handle_add_choice(KeyCode::Char('x')).unwrap();

        assert_eq!(app.mode, Mode::AddChoice);
        assert!(app.pending.is_none());
    }

    // ── `l` fast path ────────────────────────────────────────────────────────

    #[test]
    fn l_key_still_goes_straight_to_login_name_entry() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("default", Some("me@example.com"))]);

        app.handle_normal_key(KeyCode::Char('l'), KeyModifiers::NONE)
            .unwrap();

        assert_eq!(app.mode, Mode::LoginName);
    }

    #[test]
    fn login_name_queues_the_same_action_as_the_choice_screen() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::LoginName;
        for c in "business".chars() {
            app.handle_login_name(KeyCode::Char(c)).unwrap();
        }

        app.handle_login_name(KeyCode::Enter).unwrap();

        assert_eq!(
            app.pending,
            Some(PendingAction::Login {
                name: "business".to_string(),
                method: LoginMethod::ClaudeAi,
            })
        );
    }

    #[test]
    fn login_on_an_existing_name_reports_instead_of_exiting() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some("me@example.com"))]);
        app.mode = Mode::LoginName;
        for c in "business".chars() {
            app.handle_login_name(KeyCode::Char(c)).unwrap();
        }

        // The old code propagated this error out of the event loop, killing
        // the TUI. It has to surface as an in-app message instead.
        let should_exit = app.handle_login_name(KeyCode::Enter).unwrap();

        assert!(!should_exit, "a taken name must not quit the TUI");
        assert!(matches!(app.mode, Mode::Message(_, true)));
        assert!(app.pending.is_none());
    }

    // ── Destructive refresh ──────────────────────────────────────────────────

    #[test]
    fn r_key_asks_before_overwriting_credentials() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some("other@example.com"))]);

        let should_exit = app
            .handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();

        assert!(!should_exit);
        assert_eq!(app.mode, Mode::ConfirmRefresh);
        assert_eq!(app.current_account.as_deref(), Some(STUB_EMAIL));
    }

    #[test]
    fn declining_the_refresh_leaves_the_profile_alone() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some("other@example.com"))]);
        app.mode = Mode::ConfirmRefresh;

        app.handle_confirm_refresh(KeyCode::Char('n')).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        let registry = app.manager.load_registry().unwrap();
        assert_eq!(
            registry.profiles["business"].email.as_deref(),
            Some("other@example.com")
        );
    }

    // ── Concurrent sessions ──────────────────────────────────────────────────

    /// Give a profile the on-disk trace a running Claude session leaves.
    fn mark_live(app: &App, name: &str) {
        let dir = app.manager.profile_dir(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session-env"), "live").unwrap();
    }

    /// Give a profile the conversation content a refresh would destroy.
    fn mark_has_history(app: &App, name: &str) {
        let dir = app.manager.profile_dir(name).join("transcripts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ses_abc.jsonl"), "{}").unwrap();
    }

    #[test]
    fn refreshing_a_profile_holding_history_says_it_will_be_destroyed() {
        // A refresh reseeds without history, so this is a one-way loss — and
        // it happens even when the account is unchanged, which is exactly the
        // case the account comparison alone reports as safe.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);
        mark_has_history(&app, "business");

        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();

        assert_eq!(app.mode, Mode::ConfirmRefresh);
        assert!(
            app.selected_has_history,
            "history that the refresh will delete must be reported"
        );
    }

    #[test]
    fn refreshing_a_profile_without_history_reports_nothing_to_lose() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);

        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();

        assert!(
            !app.selected_has_history,
            "a profile with no transcripts must not claim a loss"
        );
    }

    #[test]
    fn the_history_warning_does_not_change_the_confirmation_keys() {
        // Advises, does not block — same contract as the in-use warning.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);
        mark_has_history(&app, "business");

        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();
        app.handle_confirm_refresh(KeyCode::Esc).unwrap();

        assert_eq!(app.mode, Mode::Normal, "Esc must still cancel");
    }

    #[test]
    fn refreshing_a_profile_another_session_is_using_warns_first() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);
        mark_live(&app, "business");

        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();

        assert_eq!(app.mode, Mode::ConfirmRefresh);
        assert!(
            app.selected_in_use.is_some(),
            "a profile written to seconds ago must be reported as possibly open"
        );
    }

    #[test]
    fn deleting_a_profile_another_session_is_using_warns_first() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);
        mark_live(&app, "business");

        app.handle_normal_key(KeyCode::Char('d'), KeyModifiers::NONE)
            .unwrap();

        assert_eq!(app.mode, Mode::ConfirmDelete);
        assert!(app.selected_in_use.is_some());
    }

    #[test]
    fn an_untouched_profile_carries_no_in_use_warning() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);

        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();

        assert_eq!(app.selected_in_use, None);
    }

    #[test]
    fn the_in_use_warning_advises_but_does_not_block() {
        // mtime is evidence, not proof. A user who knows the other terminal is
        // closed must still be able to proceed, so `y` keeps its meaning.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("business", Some(STUB_EMAIL))]);
        mark_live(&app, "business");
        app.handle_normal_key(KeyCode::Char('d'), KeyModifiers::NONE)
            .unwrap();

        app.handle_confirm_delete(KeyCode::Char('y')).unwrap();

        assert!(
            !app.manager
                .load_registry()
                .unwrap()
                .profiles
                .contains_key("business"),
            "confirming must still delete"
        );
    }

    #[test]
    fn codex_add_choice_routes_to_codex_login() {
        // Known-bad: [o] routes to a Claude login instead of Codex.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("o", Some("o@example.com"))]);
        app.mode = Mode::AddChoice;
        app.input_buffer = "new".into();
        app.handle_add_choice(KeyCode::Char('o')).unwrap();
        assert_eq!(
            app.pending,
            Some(PendingAction::CodexLogin { name: "new".into() })
        );
    }

    #[test]
    fn codex_refresh_hotkey_refuses_before_confirmation() {
        // Known-bad: the r key offers to replace a Codex home with a Claude session.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("o", Some("o@example.com"))]);
        app.mode = Mode::Normal;
        app.profiles[0].tool = Tool::Codex;
        app.handle_normal_key(KeyCode::Char('r'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(
            app.mode,
            Mode::Message("Refresh is Claude-only".into(), true)
        );
    }

    #[test]
    fn codex_refresh_confirmation_refuses_even_if_reached() {
        // Known-bad: a stale confirmation deletes the Codex home on y.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("o", Some("o@example.com"))]);
        app.mode = Mode::ConfirmRefresh;
        app.profiles[0].tool = Tool::Codex;
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let marker = app.manager.profile_dir("o").join("auth.json");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"synthetic untouched").unwrap();
        app.handle_confirm_refresh(KeyCode::Char('y')).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"synthetic untouched");
    }

    #[test]
    fn enter_on_unknown_profile_stays_in_tui_with_error_message() {
        // Known-bad: without the TUI guard, Enter returns a launch error and exits the TUI.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("alien", Some("alien@example.com"))]);
        app.profiles[0].tool = Tool::Unknown("martian".into());
        let mut registry = app.manager.load_registry().unwrap();
        registry.profiles.get_mut("alien").unwrap().tool = Tool::Unknown("martian".into());
        std::fs::write(
            app.manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        assert!(
            !app.handle_normal_key(KeyCode::Enter, KeyModifiers::NONE)
                .unwrap()
        );
        assert_eq!(
            app.mode,
            Mode::Message(
                "Profile has an unknown tool; cannot use or log in.".into(),
                true
            )
        );
    }

    #[test]
    fn codex_api_key_hotkeys_refuse_before_entry_or_clear() {
        // Known-bad: removing either p/P tool guard opens a Claude-only key flow.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("work", Some("work@example.com"))]);
        app.profiles[0].tool = Tool::Codex;
        for key in ['p', 'P'] {
            app.mode = Mode::Normal;
            assert!(
                !app.handle_normal_key(KeyCode::Char(key), KeyModifiers::NONE)
                    .unwrap()
            );
            assert_eq!(
                app.mode,
                Mode::Message("API keys are Claude-only.".into(), true),
                "{key}"
            );
        }
    }

    #[test]
    fn first_run_without_claude_can_enter_codex_add_flow() {
        // Known-bad: the first-run screen requires Claude before Codex can be added.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::FirstRun;
        app.claude_dir_found = false;
        app.handle_first_run_key(KeyCode::Char('a'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(app.mode, Mode::AddName);
    }

    #[test]
    fn first_run_detected_name_keeps_a_and_uses_three_for_codex() {
        // Known-bad: `a` opens AddName and discards the detected screen's name.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::FirstRun;
        app.claude_dir_found = true;
        app.input_buffer = "default".into();
        for key in ['-', 'a'] {
            app.handle_first_run_key(KeyCode::Char(key), KeyModifiers::NONE)
                .unwrap();
        }
        assert_eq!(app.input_buffer, "default-a");
        assert_eq!(app.mode, Mode::FirstRun);

        app.input_buffer = "work".into();
        app.handle_first_run_key(KeyCode::Char('3'), KeyModifiers::NONE)
            .unwrap();
        assert!(matches!(
            app.pending,
            Some(PendingAction::CodexLogin { ref name }) if name == "work"
        ));
    }

    #[test]
    fn first_run_empty_name_does_not_queue_codex_login() {
        // Known-bad: removing the empty-name guard from the `3` arm queues a blank login.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[]);
        app.mode = Mode::FirstRun;
        app.claude_dir_found = true;
        app.input_buffer.clear();
        assert!(app.pending.is_none());

        app.handle_first_run_key(KeyCode::Char('3'), KeyModifiers::NONE)
            .unwrap();

        assert!(app.pending.is_none());
        assert_eq!(app.mode, Mode::FirstRun);
    }

    #[test]
    fn codex_login_without_identity_has_explicit_tui_confirmation() {
        // Known-bad: an unreadable Codex identity is refused before the TUI can confirm it.
        let outcome = LoginOutcome {
            email: None,
            same_account_as: Vec::new(),
            tool: Tool::Codex,
        };
        assert_eq!(
            pending_login_message("work", &outcome, None),
            "Codex login completed for profile 'work' (email unavailable)."
        );
    }

    #[test]
    fn codex_detail_shows_tool_and_plan_without_claude_limits() {
        // Known-bad: Codex details display Claude auth or limit data, or omit JWT plan type.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("o", Some("o@example.com"))]);
        let mut registry = app.manager.load_registry().unwrap();
        registry.profiles.get_mut("o").unwrap().tool = Tool::Codex;
        std::fs::write(
            app.manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        app.profiles[0].tool = Tool::Codex;
        let auth = br#"{"tokens":{"id_token":"h.eyJlbWFpbCI6Im9AZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIn19.s"}}"#;
        std::fs::create_dir_all(app.manager.profile_dir("o")).unwrap();
        std::fs::write(app.manager.profile_dir("o").join("auth.json"), auth).unwrap();
        app.refresh_limits_if_due(Instant::now());
        let display = render_text(&mut app);
        assert!(display.contains("Tool"));
        assert!(display.contains("codex"));
        assert!(display.contains("plus"));
        assert!(display.contains("Plan limits  —"));
        assert!(display.contains("CODEX_HOME"));
        assert!(!display.contains("no data: no cached snapshot"));
    }

    #[test]
    fn usage_panel_reuses_info_lines_for_plan_and_per_token_profiles() {
        // Known-bad: a TUI-only formatter diverging from info's fee, spend, Effective or capacity lines.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(
            &tmp,
            &[
                ("plan", Some("plan@example.com")),
                ("api", Some("api@example.com")),
            ],
        );
        for (name, auth) in [
            ("plan", r#"{"oauthAccount":{}}"#),
            ("api", r#"{"primaryApiKey":"synthetic"}"#),
        ] {
            let dir = app.manager.profile_dir(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".claude.json"), auth).unwrap();
        }
        let now = Utc::now();
        let bucket = |name: &str, model: &str| m::Bucket {
            profile: name.into(),
            hour: now - chrono::Duration::hours(1),
            model: model.into(),
            speed: None,
            requests: 1,
            input: 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        };
        fs::create_dir_all(&app.usage_dir).unwrap();
        fs::write(
            app.usage_dir.join("hourly.json"),
            serde_json::to_vec(&m::Hourly {
                version: 1,
                generated_at: now,
                rows: vec![bucket("plan", "claude-opus-5"), bucket("api", "acme/model")],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            app.usage_dir.join("billing.json"),
            serde_json::json!({"version":1,"profiles":{
                "plan":{"plan":{"label":"Pro","fee_usd":20.0}},
                "api":{"rate":{"flat":2.0}}
            }})
            .to_string(),
        )
        .unwrap();
        app.limits_now = now;
        app.reload_usage_data();
        app.select_by_name("plan");
        app.refresh_limits_if_due(Instant::now());
        let plan = app.usage_panel_text("plan");
        assert!(plan.contains("$5.00 list price"), "{plan}");
        assert!(plan.contains("Plan:      Pro · $20.00/mo"), "{plan}");
        assert!(plan.contains("Effective: $20.00 per 1M tokens"), "{plan}");
        assert!(plan.contains("Capacity:  not enough data yet"), "{plan}");
        assert_eq!(app.usage_cache.cells["plan"], "~$5.00");
        app.select_by_name("api");
        app.refresh_limits_if_due(Instant::now());
        let api = app.usage_panel_text("api");
        assert!(api.contains("$2.00 spent · $* list price"), "{api}");
        assert!(
            api.contains("Rate:      $2.00 per 1M tokens, flat"),
            "{api}"
        );
        assert!(api.contains("Effective: $2.00 per 1M tokens"), "{api}");
        assert_eq!(app.usage_cache.cells["api"], "$2.00");
        let frame = render_text(&mut app);
        assert!(frame.contains("$2.00"));
        assert!(frame.contains("Effective:"));
        fs::write(app.usage_dir.join("billing.json"), b"{bad").unwrap();
        app.reload_usage_data();
        assert!(
            app.usage_panel_text("api")
                .contains("usage settings unreadable: billing.json")
        );
    }

    #[test]
    fn list_usage_cell_is_right_aligned_and_tiny_frames_do_not_panic() {
        // Known-bads: appending the cell immediately after the name or underflow below 20 columns.
        let row = profile_name_line("plan", "~$5.00", 18);
        let text = row
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text.chars().count(), 14);
        assert!(text.ends_with("~$5.00"), "{text}");
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("plan", Some("plan@example.com"))]);
        app.usage_cache.cells.insert("plan".into(), "~$5.00".into());
        let mut terminal = Terminal::new(TestBackend::new(50, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..50).map(|x| buffer[(x, 4)].symbol()).collect::<Vec<_>>();
        let money = row.iter().position(|symbol| *symbol == "~").unwrap();
        assert_eq!(&row[money..money + 6], ["~", "$", "5", ".", "0", "0"]);
        assert_eq!(row[money + 6], "│");
        for width in 1..20 {
            let _ = render_text_at(&mut app, width, 20);
        }
    }

    #[test]
    fn drawing_does_not_wait_for_background_ingest() {
        // Known-bad: running a slow ingest synchronously on the draw path.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("plan", Some("plan@example.com"))]);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        app.start_ingest_with(move || {
            release_rx.recv().unwrap();
            IngestOutcome::Updated
        });
        let (draw_tx, draw_rx) = mpsc::channel();
        let drawing = std::thread::spawn(move || {
            let _ = render_text(&mut app);
            draw_tx.send(app).unwrap();
        });
        let early = draw_rx.recv_timeout(Duration::from_secs(1));
        let drew_before_ingest = early.is_ok();
        release_tx.send(()).unwrap();
        let mut app = match early {
            Ok(app) => app,
            Err(_) => draw_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        };
        drawing.join().unwrap();
        assert!(drew_before_ingest);
        assert!(app.ingest_rx.is_some());
        // A completed worker is handled on the next event-loop pass, not by drawing.
        let deadline = Instant::now() + Duration::from_secs(1);
        while app.ingest_rx.is_some() && Instant::now() < deadline {
            app.poll_ingest();
            std::thread::yield_now();
        }
        assert!(app.ingest_rx.is_none());
    }

    #[test]
    fn ingest_completion_reloads_usage_and_busy_or_error_stays_in_panel() {
        // Known-bads: stale rollup/history after ingest, a busy lock clearing data, or errors aborting the UI.
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("plan", Some("plan@example.com"))]);
        let old_cell = app.usage_cache.cells["plan"].clone();
        app.auth_modes.insert("plan".into(), AuthMode::Subscription);
        let now = Utc::now();
        fs::create_dir_all(&app.usage_dir).unwrap();
        fs::write(
            app.usage_dir.join("hourly.json"),
            serde_json::to_vec(&m::Hourly {
                version: 1,
                generated_at: now,
                rows: vec![m::Bucket {
                    profile: "plan".into(),
                    hour: now - chrono::Duration::hours(1),
                    model: "claude-opus-5".into(),
                    speed: None,
                    requests: 1,
                    input: 1_000_000,
                    output: 0,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    cache_read: 0,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            app.usage_dir.join("limits.jsonl"),
            serde_json::json!({
                "profile":"plan","fetched_at":now.to_rfc3339(),
                "weekly":{"percent":50,"resets_at":(now+chrono::Duration::days(2)).to_rfc3339()}
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        app.ingest_rx = Some(rx);
        tx.send(IngestOutcome::Updated).unwrap();
        app.poll_ingest();
        assert_ne!(app.usage_cache.cells["plan"], old_cell);
        assert_eq!(app.usage_cache.history.len(), 1);
        assert!(app.usage_panel_text("plan").contains("weekly limit ≈ $10"));
        let loaded = app.usage_cache.cells["plan"].clone();
        let (tx, rx) = mpsc::channel();
        app.ingest_rx = Some(rx);
        tx.send(IngestOutcome::Busy).unwrap();
        app.poll_ingest();
        assert_eq!(app.usage_cache.cells["plan"], loaded);
        assert!(app.usage_panel_text("plan").contains("ledger busy"));
        let (tx, rx) = mpsc::channel();
        app.ingest_rx = Some(rx);
        tx.send(IngestOutcome::Failed("synthetic read error".into()))
            .unwrap();
        app.poll_ingest();
        assert!(
            app.usage_panel_text("plan")
                .contains("synthetic read error")
        );
    }
}
