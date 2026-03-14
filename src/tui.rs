use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};

use crate::profile::{detect_current_account, Profile, ProfileManager};

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

// ── Mode ──────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Shown on very first launch when no profiles exist yet.
    FirstRun,
    Normal,
    ConfirmDelete,
    AddName,
    LoginName,
    Message(String, bool), // (text, is_error)
}

// ── App ───────────────────────────────────────────────────────────────────────
pub struct App {
    manager: ProfileManager,
    profiles: Vec<Profile>,
    list_state: ListState,
    mode: Mode,
    /// Shared text input buffer (used by FirstRun, AddName).
    input_buffer: String,
    /// Email detected from the live ~/.claude on startup (first-run only).
    detected_email: Option<String>,
    /// Whether ~/.claude exists at all (first-run only).
    claude_dir_found: bool,
}

impl App {
    pub fn new(manager: ProfileManager) -> Result<Self> {
        let profiles = manager.list_profiles()?;
        let mut list_state = ListState::default();
        if !profiles.is_empty() {
            list_state.select(Some(0));
        }

        // Detect whether this is a first run (no profiles saved yet).
        let (mode, detected_email, claude_dir_found, input_buffer) = if profiles.is_empty() {
            match detect_current_account() {
                Some(acct) => {
                    // Pre-fill the profile name with "default".
                    (Mode::FirstRun, acct.email, true, "default".to_string())
                }
                None => {
                    // ~/.claude doesn't exist — show first-run screen but warn.
                    (Mode::FirstRun, None, false, String::new())
                }
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
            detected_email,
            claude_dir_found,
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn refresh(&mut self) -> Result<()> {
        self.profiles = self.manager.list_profiles()?;
        if self.profiles.is_empty() {
            self.list_state.select(None);
        } else {
            let idx = self.list_state.selected().unwrap_or(0);
            self.list_state.select(Some(idx.min(self.profiles.len() - 1)));
        }
        Ok(())
    }

    fn select_by_name(&mut self, name: &str) {
        if let Some(idx) = self.profiles.iter().position(|p| p.name == name) {
            self.list_state.select(Some(idx));
        }
    }

    fn selected_profile(&self) -> Option<&Profile> {
        self.list_state.selected().and_then(|i| self.profiles.get(i))
    }

    fn move_up(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(0) | None => self.profiles.len() - 1,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(i));
    }

    fn move_down(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1) % self.profiles.len(),
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
                    Mode::ConfirmDelete => {
                        self.handle_confirm_delete(key.code)?;
                    }
                    Mode::AddName => {
                        if self.handle_add_name(key.code)? {
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
        }
    }

    // ── Key handlers ──────────────────────────────────────────────────────────

    fn handle_first_run_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
        // Always allow force-quit
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        if !self.claude_dir_found {
            // Nothing to save — any key drops to normal (empty) view or quits.
            match code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                _ => {
                    self.mode = Mode::Normal;
                }
            }
            return Ok(false);
        }

        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('q') => return Ok(true),

            // [c] Copy existing session
            KeyCode::Char('1') => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    return Ok(false);
                }
                match self.manager.add_profile(&name) {
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
                    Err(e) => {
                        self.mode = Mode::Message(e.to_string(), true);
                    }
                }
            }

            // [l] Login to a new account
            KeyCode::Char('2') => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    return Ok(false);
                }
                ratatui::restore();
                self.manager.login_profile(&name)?;
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

            KeyCode::Enter => {
                if let Some(p) = self.selected_profile() {
                    let name = p.name.clone();
                    ratatui::restore();
                    println!("Launching Claude with profile '{}'…", name);
                    self.manager.launch_claude(&name, &[])?;
                }
            }

            KeyCode::Char('l') => {
                // Open a name input, then login
                self.mode = Mode::LoginName;
                self.input_buffer.clear();
            }

            KeyCode::Char('a') => {
                self.mode = Mode::AddName;
                self.input_buffer.clear();
            }

            KeyCode::Char('d') | KeyCode::Delete => {
                if self.selected_profile().is_some() {
                    self.mode = Mode::ConfirmDelete;
                }
            }

            KeyCode::Char('r') => {
                if let Some(p) = self.selected_profile() {
                    let name = p.name.clone();
                    ratatui::restore();
                    println!("Refreshing profile '{}'…", name);
                    match self.manager.add_profile_force(&name) {
                        Ok(_) => println!("Profile '{}' refreshed from current ~/.claude", name),
                        Err(e) => eprintln!("Error: {}", e),
                    }
                    return Ok(true);
                }
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

    fn handle_add_name(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Enter => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    self.mode = Mode::Normal;
                    return Ok(false);
                }
                match self.manager.add_profile(&name) {
                    Ok(_) => {
                        self.refresh()?;
                        self.select_by_name(&name);
                        self.mode =
                            Mode::Message(format!("Profile '{}' added.", name), false);
                    }
                    Err(e) => self.mode = Mode::Message(e.to_string(), true),
                }
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

    fn handle_login_name(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Enter => {
                let name = self.input_buffer.trim().to_string();
                if name.is_empty() {
                    self.mode = Mode::Normal;
                    return Ok(false);
                }
                ratatui::restore();
                self.manager.login_profile(&name)?;
                // login_profile calls process::exit, won't reach here
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

    // ── Rendering ─────────────────────────────────────────────────────────────

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        if self.mode == Mode::FirstRun {
            self.render_first_run(f, area);
            return;
        }

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        self.render_header(f, layout[0]);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(layout[1]);

        self.render_profile_list(f, cols[0]);
        self.render_detail_panel(f, cols[1]);
        self.render_footer(f, layout[2]);

        match &self.mode.clone() {
            Mode::ConfirmDelete => self.render_confirm_delete(f),
            Mode::AddName => self.render_add_name(f),
            Mode::LoginName => self.render_login_name(f),
            Mode::Message(msg, is_err) => self.render_message(f, msg, *is_err),
            _ => {}
        }
    }

    // ── First-run screen ──────────────────────────────────────────────────────

    fn render_first_run(&self, f: &mut Frame, area: Rect) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        // ── Header ────────────────────────────────────────────────────────────
        let header_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        let header = Paragraph::new(Line::from(vec![
            Span::styled(" ◆ ", Style::default().fg(ACCENT).bold()),
            Span::styled("claude-switch", Style::default().fg(TEXT).bold()),
            Span::styled("  first run setup", Style::default().fg(DIM)),
        ]))
        .block(header_block);
        f.render_widget(header, layout[0]);

        // ── Body ──────────────────────────────────────────────────────────────
        let body_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let inner = body_block.inner(layout[1]);
        f.render_widget(body_block, layout[1]);

        let content: Vec<Line> = if !self.claude_dir_found {
            self.render_first_run_no_claude()
        } else {
            self.render_first_run_detected()
        };

        f.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }),
            inner,
        );

        // ── Footer ────────────────────────────────────────────────────────────
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
            Line::from(vec![Span::styled(
                "  Manage multiple Claude Code accounts using isolated profile directories.",
                Style::default().fg(DIM),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )]),
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
            Line::from(vec![Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Set up your first profile:",
                Style::default().fg(TEXT),
            )]),
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
            Line::from(vec![Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [1] ", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    "Copy active session as this profile",
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(
                    "Uses your existing credentials — no re-login needed",
                    Style::default().fg(DIM),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [2] ", Style::default().fg(ACCENT).bold()),
                Span::styled(
                    "Login to a different account for this profile",
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(
                    "Opens Claude for you to authenticate a new account",
                    Style::default().fg(DIM),
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
            Line::from(vec![Span::styled(
                "  ─────────────────────────────────────────────────────────",
                Style::default().fg(BORDER),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ✗ ", Style::default().fg(DANGER).bold()),
                Span::styled(
                    "No Claude Code installation found at ~/.claude",
                    Style::default().fg(TEXT).bold(),
                ),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  You need to install and log in to Claude Code before adding profiles.",
                Style::default().fg(DIM),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Install   ", Style::default().fg(DIM)),
                Span::styled(
                    "npm install -g @anthropic-ai/claude-code",
                    Style::default().fg(Color::Rgb(140, 200, 140)),
                ),
            ]),
            Line::from(vec![
                Span::styled("    Log in    ", Style::default().fg(DIM)),
                Span::styled("claude", Style::default().fg(Color::Rgb(140, 200, 140))),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Then re-run cswitch to set up your first profile.",
                Style::default().fg(DIM),
            )]),
        ]
    }

    // ── Normal view widgets ───────────────────────────────────────────────────

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        let title = Paragraph::new(Line::from(vec![
            Span::styled(" ◆ ", Style::default().fg(ACCENT).bold()),
            Span::styled("claude-switch", Style::default().fg(TEXT).bold()),
            Span::styled("  profile manager", Style::default().fg(DIM)),
        ]))
        .block(block);

        f.render_widget(title, area);

        let count = self.profiles.len();
        let count_widget = Paragraph::new(Span::styled(
            format!(" {} profile{} ", count, if count == 1 { "" } else { "s" }),
            Style::default().fg(DIM),
        ))
        .alignment(Alignment::Right);

        let count_area = Rect {
            x: area.x + area.width.saturating_sub(14),
            y: area.y + 1,
            width: 12,
            height: 1,
        };
        f.render_widget(count_widget, count_area);
    }

    fn render_profile_list(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .title(Line::from(Span::styled(
                " Profiles ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let items: Vec<ListItem> = self
            .profiles
            .iter()
            .map(|p| {
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
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  No profiles yet. Press 'a' to add one.",
                    Style::default().fg(DIM),
                ))),
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
            Line::from(vec![Span::styled(
                "  ─────────────────────────────────────────",
                Style::default().fg(BORDER),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Launch command",
                Style::default().fg(DIM),
            )]),
            Line::from(vec![Span::styled(
                format!(
                    "  CLAUDE_CONFIG_DIR='{}' claude",
                    profile_dir.display()
                ),
                Style::default().fg(Color::Rgb(140, 200, 140)),
            )]),
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

        let keys: &[(&str, &str)] = &[
            ("↑/↓ j/k", "navigate"),
            ("enter", "launch"),
            ("l", "login new"),
            ("r", "refresh"),
            ("d", "delete"),
            ("q/esc", "quit"),
        ];

        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, v)| {
                vec![
                    Span::styled(format!(" {} ", k), Style::default().fg(ACCENT).bold()),
                    Span::styled(*v, Style::default().fg(DIM)),
                    Span::styled("  ", Style::default()),
                ]
            })
            .collect();

        f.render_widget(
            Paragraph::new(Line::from(spans)).block(block),
            area,
        );
    }

    // ── Overlay popups ────────────────────────────────────────────────────────

    fn render_confirm_delete(&self, f: &mut Frame) {
        let name = self.selected_profile().map(|p| p.name.as_str()).unwrap_or("?");
        let area = centered_rect(50, 7, f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Confirm Delete ",
                Style::default().fg(DANGER).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(DANGER))
            .style(Style::default().bg(PANEL));

        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Delete profile ", Style::default().fg(TEXT)),
                    Span::styled(name.to_string(), Style::default().fg(DANGER).bold()),
                    Span::styled("? This cannot be undone.", Style::default().fg(TEXT)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("y", Style::default().fg(DANGER).bold()),
                    Span::styled(" confirm   ", Style::default().fg(DIM)),
                    Span::styled("any other key", Style::default().fg(ACCENT).bold()),
                    Span::styled(" cancel", Style::default().fg(DIM)),
                ]),
            ]))
            .block(block),
            area,
        );
    }

    fn render_add_name(&self, f: &mut Frame) {
        let area = centered_rect(50, 7, f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Add Profile ",
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
                    Span::styled("  Name: ", Style::default().fg(DIM)),
                    Span::styled(self.input_buffer.clone(), Style::default().fg(TEXT).bold()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "  Copies current ~/.claude into this profile.",
                    Style::default().fg(DIM),
                )),
            ]))
            .block(block),
            area,
        );
    }

    fn render_login_name(&self, f: &mut Frame) {
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
