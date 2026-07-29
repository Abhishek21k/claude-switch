use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};

use crate::profile::{describe_age, detect_current_account, Profile, ProfileManager};

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
    Login { name: String },
}

// ── App ───────────────────────────────────────────────────────────────────────
pub struct App {
    manager: ProfileManager,
    profiles: Vec<Profile>,
    list_state: ListState,
    mode: Mode,
    input_buffer: String,
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
    pending: Option<PendingAction>,
    /// How the live account gets resolved. Swapped in tests so the choice
    /// screen can be exercised without a real `~/.claude` behind it.
    account_probe: AccountProbe,
}

type AccountProbe = fn() -> Option<String>;

fn live_account_email() -> Option<String> {
    detect_current_account().and_then(|a| a.email)
}

impl App {
    pub fn new(manager: ProfileManager) -> Result<Self> {
        let profiles = manager.list_profiles()?;
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
            search_query: String::new(),
            filtered_indices,
            detected_email,
            claude_dir_found,
            current_account: None,
            selected_in_use: None,
            pending: None,
            account_probe: live_account_email,
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn refresh(&mut self) -> Result<()> {
        self.profiles = self.manager.list_profiles()?;
        self.apply_filter();
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
        } else {
            let idx = self.list_state.selected().unwrap_or(0);
            self.list_state
                .select(Some(idx.min(self.filtered_indices.len() - 1)));
        }
        Ok(())
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
                        || p.email
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&q)
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

    /// How recently a Claude session wrote to the selected profile, if that was
    /// recent enough that another terminal may still have it open.
    fn selected_session_age(&self) -> Option<u64> {
        let name = self.selected_profile()?.name.clone();
        self.manager.maybe_in_use(&name)
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
        let result = self.event_loop(&mut terminal);
        ratatui::restore();
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;

            if let Event::Key(key) = event::read()? {
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
    fn run_pending(
        &mut self,
        action: PendingAction,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
        ratatui::restore();

        // `select` is only set when a profile actually landed in the registry,
        // so a failed attempt leaves the current selection alone.
        let (select, message) = match action {
            PendingAction::Login { name } => {
                match self.manager.login_profile(&name, false, None) {
                    Ok(result) => {
                        let others: Vec<&str> = result
                            .same_account_as
                            .iter()
                            .map(String::as_str)
                            .filter(|n| *n != name)
                            .collect();

                        let msg = if others.is_empty() {
                            format!("Profile '{}' logged in as {}.", name, result.display_email())
                        } else {
                            // Not an error: two profiles for one account is a
                            // valid setup. But say it, or it reads as a new one.
                            format!(
                                "Profile '{}' is {} — the same account as {}. \
                                 Sign out of claude.ai and retry if you wanted a different one.",
                                name,
                                result.display_email(),
                                others.join(", ")
                            )
                        };
                        (Some(name), Mode::Message(msg, false))
                    }
                    Err(e) => (None, Mode::Message(e.to_string(), true)),
                }
            }
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

    fn handle_first_run_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        if !self.claude_dir_found {
            match code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
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
                self.pending = Some(PendingAction::Login { name });
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

    fn handle_normal_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
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
                    let name = p.name.clone();
                    ratatui::restore();
                    println!("Launching Claude with profile '{}'…", name);
                    self.manager.launch_claude(&name, &[])?;
                }
            }

            KeyCode::Char('l') => {
                self.mode = Mode::LoginName;
                self.input_buffer.clear();
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
                self.current_account = (self.account_probe)();
                self.selected_in_use = self.selected_session_age();
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

    fn handle_search_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
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
                self.pending = Some(PendingAction::Login { name });
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
                self.pending = Some(PendingAction::Login { name });
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
            Line::from(Span::styled("  ─────────────────────────────────────────────────────────", Style::default().fg(BORDER))),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ✓ ", Style::default().fg(SUCCESS).bold()),
                Span::styled("Claude Code installation detected", Style::default().fg(TEXT).bold()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Current account   ", Style::default().fg(DIM)),
                Span::styled(email, Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(""),
            Line::from(Span::styled("  ─────────────────────────────────────────────────────────", Style::default().fg(BORDER))),
            Line::from(""),
            Line::from(Span::styled("  Set up your first profile:", Style::default().fg(TEXT))),
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
            Line::from(Span::styled("  ─────────────────────────────────────────────────────────", Style::default().fg(BORDER))),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [1] ", Style::default().fg(ACCENT).bold()),
                Span::styled("Copy active session as this profile", Style::default().fg(TEXT)),
            ]),
            Line::from(Span::styled("      Uses your existing credentials — no re-login needed", Style::default().fg(DIM))),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [2] ", Style::default().fg(ACCENT).bold()),
                Span::styled("Login to a different account for this profile", Style::default().fg(TEXT)),
            ]),
            Line::from(Span::styled("      Opens Claude for you to authenticate a new account", Style::default().fg(DIM))),
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
            Line::from(Span::styled("  ─────────────────────────────────────────────────────────", Style::default().fg(BORDER))),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ✗ ", Style::default().fg(DANGER).bold()),
                Span::styled("No Claude Code installation found at ~/.claude", Style::default().fg(TEXT).bold()),
            ]),
            Line::from(""),
            Line::from(Span::styled("  You need to install and log in to Claude Code before adding profiles.", Style::default().fg(DIM))),
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
            Line::from(Span::styled("  Then re-run cswitch to set up your first profile.", Style::default().fg(DIM))),
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

        let count_area = Rect {
            x: area.x + area.width.saturating_sub(label.len() as u16 + 2),
            y: area.y + 1,
            width: label.len() as u16 + 1,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(label, Style::default().fg(DIM)))
                .alignment(Alignment::Right),
            count_area,
        );
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
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(" ", Style::default()),
                        Span::styled(p.name.clone(), Style::default().fg(TEXT).bold()),
                    ]),
                    Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(email.to_string(), Style::default().fg(DIM)),
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
            .highlight_symbol("▶ ");

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

        let lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Name         ", Style::default().fg(DIM)),
                Span::styled(profile.name.clone(), Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Email        ", Style::default().fg(DIM)),
                Span::styled(
                    profile.email.clone().unwrap_or("unknown".into()),
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Added        ", Style::default().fg(DIM)),
                Span::styled(
                    profile.added.format("%Y-%m-%d %H:%M UTC").to_string(),
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(""),
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
            Line::from(""),
            Line::from(vec![
                Span::styled("  Config dir   ", Style::default().fg(DIM)),
                Span::styled(profile_dir.display().to_string(), Style::default().fg(MUTED)),
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
                    format!("  $env:CLAUDE_CONFIG_DIR='{}'; claude", profile_dir.display())
                } else {
                    format!("  CLAUDE_CONFIG_DIR='{}' claude", profile_dir.display())
                },
                Style::default().fg(Color::Rgb(140, 200, 140)),
            )),
        ];

        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let keys: Vec<(&str, &str)> = if self.mode == Mode::Search {
            vec![
                ("↑/↓", "navigate"),
                ("enter", "confirm"),
                ("esc", "clear"),
            ]
        } else {
            vec![
                ("↑↓/jk", "nav"),
                ("enter", "launch"),
                ("/", "search"),
                ("a", "add account"),
                ("l", "login"),
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

        f.render_widget(
            Paragraph::new(Line::from(spans)).block(block),
            area,
        );
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
            ("Enter", "Launch Claude with selected profile"),
            ("/", "Search profiles by name or email"),
            ("a", "Add account — then choose copy or login"),
            ("l", "Login — straight to a different account"),
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
                format!("  In use? Written {} by a Claude session.", describe_age(secs)),
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
        let area = centered_rect(66, 12, f.area());
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
                    Span::styled("Log in to a different Claude account", Style::default().fg(TEXT)),
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
        // independent of which account it holds.
        let color = if replaces_account || self.selected_in_use.is_some() {
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
                format!("  In use? Written {} by a Claude session.", describe_age(secs)),
                Style::default().fg(DANGER).bold(),
            )));
            lines.push(Line::from(Span::styled(
                "  Another terminal may have this profile open right now.",
                Style::default().fg(DANGER),
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

    fn render_message(&self, f: &mut Frame, msg: &str, is_err: bool) {
        let area = centered_rect(60, 6, f.area());
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
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {}", msg),
                    Style::default().fg(TEXT),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press any key to continue",
                    Style::default().fg(DIM),
                )),
            ]))
            .block(block)
            .wrap(Wrap { trim: false }),
            area,
        );
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

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
        let manager =
            ProfileManager::with_base_dir(tmp.path().join(".claude-switch")).unwrap();

        for (name, email) in existing {
            let mut registry = manager.load_registry().unwrap();
            registry.profiles.insert(
                name.to_string(),
                Profile {
                    name: name.to_string(),
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

    fn type_name(app: &mut App, name: &str) {
        for c in name.chars() {
            app.handle_add_name(KeyCode::Char(c)).unwrap();
        }
    }

    // ── `a` → name → choice ───────────────────────────────────────────────────

    #[test]
    fn add_key_opens_name_entry_not_a_copy() {
        let tmp = TempDir::new().unwrap();
        let mut app = make_app(&tmp, &[("default", Some("me@example.com"))]);

        app.handle_normal_key(KeyCode::Char('a'), KeyModifiers::NONE).unwrap();

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
            Some(PendingAction::Login { name: "business".to_string() })
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
            Some(PendingAction::Login { name: "business".to_string() })
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
        assert_eq!(app.input_buffer, "business", "the typed name should survive");
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

        app.handle_normal_key(KeyCode::Char('l'), KeyModifiers::NONE).unwrap();

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
            Some(PendingAction::Login { name: "business".to_string() })
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
            !app.manager.load_registry().unwrap().profiles.contains_key("business"),
            "confirming must still delete"
        );
    }
}
