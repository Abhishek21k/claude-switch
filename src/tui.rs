use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
    },
    DefaultTerminal, Frame,
};
use crate::profile::{Profile, ProfileManager};

const ACCENT: Color = Color::Rgb(255, 149, 0); // Claude orange
const DIM: Color = Color::Rgb(100, 100, 110);
const SUCCESS: Color = Color::Rgb(80, 200, 120);
const DANGER: Color = Color::Rgb(220, 80, 80);
const BG: Color = Color::Rgb(14, 14, 18);
const PANEL: Color = Color::Rgb(22, 22, 28);
const BORDER: Color = Color::Rgb(50, 50, 60);
const TEXT: Color = Color::Rgb(220, 220, 230);

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Normal,
    ConfirmDelete,
    AddName,
    Message(String, bool), // (message, is_error)
}

pub struct App {
    manager: ProfileManager,
    profiles: Vec<Profile>,
    list_state: ListState,
    mode: Mode,
    input_buffer: String,
}

impl App {
    pub fn new(manager: ProfileManager) -> Result<Self> {
        let profiles = manager.list_profiles()?;
        let mut list_state = ListState::default();
        if !profiles.is_empty() {
            list_state.select(Some(0));
        }
        Ok(Self {
            manager,
            profiles,
            list_state,
            mode: Mode::Normal,
            input_buffer: String::new(),
        })
    }

    fn refresh(&mut self) -> Result<()> {
        self.profiles = self.manager.list_profiles()?;
        if self.profiles.is_empty() {
            self.list_state.select(None);
        } else {
            let idx = self.list_state.selected().unwrap_or(0);
            self.list_state
                .select(Some(idx.min(self.profiles.len() - 1)));
        }
        Ok(())
    }

    fn selected_profile(&self) -> Option<&Profile> {
        self.list_state
            .selected()
            .and_then(|i| self.profiles.get(i))
    }

    fn move_up(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.profiles.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
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
                    Mode::Message(_, _) => {
                        self.mode = Mode::Normal;
                    }
                }
            }
        }
    }

    fn handle_normal_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),

            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),

            KeyCode::Enter => {
                if let Some(p) = self.selected_profile() {
                    let name = p.name.clone();
                    ratatui::restore();
                    println!("Launching Claude with profile '{}'...", name);
                    self.manager.launch_claude(&name, &[])?;
                }
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
                    println!("Refreshing profile '{}'...", name);
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
                        Err(e) => {
                            self.mode = Mode::Message(e.to_string(), true);
                        }
                    }
                }
            }
            _ => {
                self.mode = Mode::Normal;
            }
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
                        // Select the newly added profile
                        if let Some(idx) = self.profiles.iter().position(|p| p.name == name) {
                            self.list_state.select(Some(idx));
                        }
                        self.mode =
                            Mode::Message(format!("Profile '{}' added successfully.", name), false);
                    }
                    Err(e) => {
                        self.mode = Mode::Message(e.to_string(), true);
                    }
                }
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                // Only allow alphanumeric and hyphen/underscore in profile names
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    self.input_buffer.push(c);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();

        // Background
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        let main_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // header
                Constraint::Min(0),    // content
                Constraint::Length(3), // footer
            ])
            .split(area);

        self.render_header(f, main_layout[0]);

        let content_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(main_layout[1]);

        self.render_profile_list(f, content_layout[0]);
        self.render_detail_panel(f, content_layout[1]);
        self.render_footer(f, main_layout[2]);

        // Overlays
        match &self.mode.clone() {
            Mode::ConfirmDelete => self.render_confirm_delete(f),
            Mode::AddName => self.render_add_name(f),
            Mode::Message(msg, is_err) => self.render_message(f, msg, *is_err),
            Mode::Normal => {}
        }
    }

    fn render_header(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        let title = Line::from(vec![
            Span::styled(" ◆ ", Style::default().fg(ACCENT).bold()),
            Span::styled("claude-switch", Style::default().fg(TEXT).bold()),
            Span::styled(
                "  profile manager",
                Style::default().fg(DIM),
            ),
        ]);

        let count = self.profiles.len();
        let subtitle = Span::styled(
            format!(" {} profile{} ", count, if count == 1 { "" } else { "s" }),
            Style::default().fg(DIM),
        );

        let para = Paragraph::new(title)
            .block(block)
            .alignment(Alignment::Left);

        f.render_widget(para, area);

        // Profile count on right side of header
        let count_area = ratatui::layout::Rect {
            x: area.x + area.width.saturating_sub(14),
            y: area.y + 1,
            width: 12,
            height: 1,
        };
        f.render_widget(Paragraph::new(subtitle).alignment(Alignment::Right), count_area);
    }

    fn render_profile_list(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .title(Line::from(vec![
                Span::styled(" Profiles ", Style::default().fg(ACCENT).bold()),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let items: Vec<ListItem> = self
            .profiles
            .iter()
            .map(|p| {
                let email = p.email.as_deref().unwrap_or("no email");
                let name_line = Line::from(vec![
                    Span::styled(" ", Style::default()),
                    Span::styled(&p.name, Style::default().fg(TEXT).bold()),
                ]);
                let email_line = Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(email, Style::default().fg(DIM)),
                ]);
                ListItem::new(vec![name_line, email_line])
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

    fn render_detail_panel(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .title(Line::from(vec![
                Span::styled(" Details ", Style::default().fg(ACCENT).bold()),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let inner = block.inner(area);
        f.render_widget(block, area);

        let Some(profile) = self.selected_profile() else {
            let empty = Paragraph::new(
                Line::from(Span::styled("  No profiles yet. Press 'a' to add one.", Style::default().fg(DIM)))
            );
            f.render_widget(empty, inner);
            return;
        };

        let profile_dir = self.manager.profile_dir(&profile.name);

        let lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Name         ", Style::default().fg(DIM)),
                Span::styled(&profile.name, Style::default().fg(ACCENT).bold()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Email        ", Style::default().fg(DIM)),
                Span::styled(
                    profile.email.as_deref().unwrap_or("unknown"),
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
                        .unwrap_or("never".to_string()),
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Config dir   ", Style::default().fg(DIM)),
                Span::styled(
                    profile_dir.display().to_string(),
                    Style::default().fg(DIM),
                ),
            ]),
            Line::from(""),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ─────────────────────────────────────", Style::default().fg(BORDER)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Launch command:", Style::default().fg(DIM)),
            ]),
            Line::from(vec![
                Span::styled(
                    format!("  CLAUDE_CONFIG_DIR='{}' claude", profile_dir.display()),
                    Style::default().fg(Color::Rgb(140, 200, 140)),
                ),
            ]),
        ];

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        f.render_widget(para, inner);
    }

    fn render_footer(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL));

        let keys: &[(&str, &str)] = &[
            ("↑/↓ j/k", "navigate"),
            ("enter", "launch"),
            ("a", "add current"),
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

        let para = Paragraph::new(Line::from(spans))
            .block(block)
            .alignment(Alignment::Left);

        f.render_widget(para, area);
    }

    fn render_confirm_delete(&self, f: &mut Frame) {
        let name = self
            .selected_profile()
            .map(|p| p.name.as_str())
            .unwrap_or("?");

        let popup_area = centered_rect(50, 7, f.area());
        f.render_widget(Clear, popup_area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Confirm Delete ",
                Style::default().fg(DANGER).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(DANGER))
            .style(Style::default().bg(PANEL));

        let text = Text::from(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Delete profile ", Style::default().fg(TEXT)),
                Span::styled(name, Style::default().fg(DANGER).bold()),
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
        ]);

        let para = Paragraph::new(text).block(block);
        f.render_widget(para, popup_area);
    }

    fn render_add_name(&self, f: &mut Frame) {
        let popup_area = centered_rect(50, 7, f.area());
        f.render_widget(Clear, popup_area);

        let block = Block::default()
            .title(Line::from(Span::styled(
                " Add Profile ",
                Style::default().fg(ACCENT).bold(),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));

        let text = Text::from(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  Name: ", Style::default().fg(DIM)),
                Span::styled(&self.input_buffer, Style::default().fg(TEXT).bold()),
                Span::styled("█", Style::default().fg(ACCENT)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Copies current ~/.claude into this profile.",
                    Style::default().fg(DIM),
                ),
            ]),
        ]);

        let para = Paragraph::new(text).block(block);
        f.render_widget(para, popup_area);
    }

    fn render_message(&self, f: &mut Frame, msg: &str, is_err: bool) {
        let popup_area = centered_rect(55, 6, f.area());
        f.render_widget(Clear, popup_area);

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

        let text = Text::from(vec![
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
        ]);

        let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
        f.render_widget(para, popup_area);
    }
}

fn centered_rect(
    percent_x: u16,
    height: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let popup_width = area.width * percent_x / 100;
    let popup_x = area.x + (area.width - popup_width) / 2;
    let popup_y = area.y + (area.height.saturating_sub(height)) / 2;

    ratatui::layout::Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: height.min(area.height),
    }
}
