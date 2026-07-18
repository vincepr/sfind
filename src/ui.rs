use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use session_search::Session;

const DETAIL_TEXT_MAX_CHARS: usize = 360;

pub(crate) fn pick(sessions: &[Session], warning_count: usize) -> Result<Option<usize>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("interactive mode requires a terminal; use --list for plain output");
    }
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(sessions, warning_count);
    loop {
        terminal
            .terminal
            .draw(|frame| app.draw(frame))
            .context("could not draw session search")?;
        if !event::poll(Duration::from_millis(250)).context("could not poll terminal input")? {
            continue;
        }
        match event::read().context("could not read terminal input")? {
            Event::Key(key) => {
                if let Some(outcome) = app.key(key) {
                    return Ok(outcome);
                }
            }
            Event::Mouse(mouse) => app.mouse(mouse),
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("could not enable raw terminal mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error).context("could not enter alternate terminal screen");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
                return Err(error).context("could not initialize terminal");
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

struct App<'a> {
    sessions: &'a [Session],
    search_text: Vec<String>,
    query: String,
    visible: Vec<usize>,
    list_state: ListState,
    list_inner: Rect,
    detail_inner: Rect,
    detail_scroll: u16,
    warning_count: usize,
}

impl<'a> App<'a> {
    fn new(sessions: &'a [Session], warning_count: usize) -> Self {
        let visible = (0..sessions.len()).collect::<Vec<_>>();
        let selected = (!visible.is_empty()).then_some(0);
        Self {
            sessions,
            search_text: sessions
                .iter()
                .map(|session| session.search_text().to_lowercase())
                .collect(),
            query: String::new(),
            visible,
            list_state: ListState::default().with_selected(selected),
            list_inner: Rect::default(),
            detail_inner: Rect::default(),
            detail_scroll: 0,
            warning_count,
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(frame.area());
        let search = Paragraph::new(format!("> {}", self.query)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search user messages and titles "),
        );
        frame.render_widget(search, sections[0]);

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(sections[1]);
        let list_block = Block::default().borders(Borders::ALL).title(format!(
            " Sessions ({}/{}) ",
            self.visible.len(),
            self.sessions.len()
        ));
        self.list_inner = list_block.inner(columns[0]);
        let items = self.visible.iter().map(|index| {
            let session = &self.sessions[*index];
            let title = session
                .title
                .as_deref()
                .unwrap_or(&session.first_user_message);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<8}", session.provider.label()),
                    Style::default().fg(provider_color(session.provider.label())),
                ),
                Span::raw(format!(
                    " {}  {}",
                    compact_time(session.updated_at),
                    truncate(title, 54)
                )),
            ]))
        });
        let list = List::new(items)
            .block(list_block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, columns[0], &mut self.list_state);

        let detail = self
            .selected_session()
            .map(detail_lines)
            .unwrap_or_else(|| vec![Line::raw("No matching sessions")]);
        let detail_block = Block::default().borders(Borders::ALL).title(" Details ");
        self.detail_inner = detail_block.inner(columns[1]);
        let detail_width = usize::from(self.detail_inner.width.max(1));
        let detail_height = detail
            .iter()
            .map(|line| line.width().max(1).div_ceil(detail_width))
            .sum::<usize>();
        let detail_paragraph = Paragraph::new(detail)
            .block(detail_block)
            .wrap(Wrap { trim: false });
        let max_scroll = detail_height.saturating_sub(usize::from(self.detail_inner.height));
        self.detail_scroll = self
            .detail_scroll
            .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
        frame.render_widget(detail_paragraph.scroll((self.detail_scroll, 0)), columns[1]);
        let warnings = if self.warning_count == 0 {
            String::new()
        } else {
            format!(" | {} provider warning(s) logged", self.warning_count)
        };
        frame.render_widget(
            Paragraph::new(format!(
                "Enter resume | Up/Down select | Ctrl-Up/Down scroll details | Esc clear/quit{warnings}"
            )),
            sections[2],
        );
        frame.set_cursor_position((
            sections[0].x + 3 + self.query.chars().count() as u16,
            sections[0].y + 1,
        ));
    }

    fn key(&mut self, key: KeyEvent) -> Option<Option<usize>> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(None);
        }
        match key.code {
            KeyCode::Enter => return Some(self.selected_index()),
            KeyCode::Esc if self.query.is_empty() => return Some(None),
            KeyCode::Esc => {
                self.query.clear();
                self.filter();
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(self.visible.len().saturating_sub(1)),
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.query.push(character);
                self.filter();
            }
            KeyCode::Char(_) => {}
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Delete
            | KeyCode::Insert
            | KeyCode::F(_)
            | KeyCode::Null
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::CapsLock
            | KeyCode::ScrollLock
            | KeyCode::NumLock
            | KeyCode::PrintScreen
            | KeyCode::Pause
            | KeyCode::Menu
            | KeyCode::KeypadBegin
            | KeyCode::Media(_)
            | KeyCode::Modifier(_) => {}
        }
        None
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp
                if self.detail_inner.contains((mouse.column, mouse.row).into()) =>
            {
                self.detail_scroll = self.detail_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown
                if self.detail_inner.contains((mouse.column, mouse.row).into()) =>
            {
                self.detail_scroll = self.detail_scroll.saturating_add(3);
            }
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::Down(_)
                if self.list_inner.contains((mouse.column, mouse.row).into()) =>
            {
                let row = usize::from(mouse.row.saturating_sub(self.list_inner.y));
                let index = self.list_state.offset().saturating_add(row);
                if index < self.visible.len() {
                    self.select(index);
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.visible = self
            .search_text
            .iter()
            .enumerate()
            .filter(|(_, text)| fuzzy_match(text, &query))
            .map(|(index, _)| index)
            .collect();
        self.list_state
            .select((!self.visible.is_empty()).then_some(0));
        self.detail_scroll = 0;
    }

    fn move_selection(&mut self, amount: isize) {
        if self.visible.is_empty() {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        let next = current
            .saturating_add_signed(amount)
            .min(self.visible.len().saturating_sub(1));
        self.select(next);
    }

    fn select(&mut self, index: usize) {
        if !self.visible.is_empty() {
            self.list_state
                .select(Some(index.min(self.visible.len() - 1)));
            self.detail_scroll = 0;
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.list_state
            .selected()
            .and_then(|index| self.visible.get(index))
            .copied()
    }

    fn selected_session(&self) -> Option<&Session> {
        self.selected_index()
            .and_then(|index| self.sessions.get(index))
    }
}

fn fuzzy_match(haystack: &str, query: &str) -> bool {
    query.split_whitespace().all(|word| {
        let mut characters = word.chars();
        let mut wanted = characters.next();
        for character in haystack.chars() {
            if Some(character) == wanted {
                wanted = characters.next();
                if wanted.is_none() {
                    return true;
                }
            }
        }
        wanted.is_none()
    })
}

fn detail_lines(session: &Session) -> Vec<Line<'static>> {
    let mut lines = vec![
        field("Provider", session.provider.label()),
        field("Updated", &full_time(session.updated_at)),
        field("Session", &session.id),
        field(
            "Title",
            &truncate(
                session.title.as_deref().unwrap_or("not provided"),
                DETAIL_TEXT_MAX_CHARS,
            ),
        ),
        field(
            "Directory",
            &session.directory.as_ref().map_or_else(
                || "not provided".to_owned(),
                |path| path.display().to_string(),
            ),
        ),
        Line::raw(""),
        label("First sent message"),
        Line::raw(truncate(&session.first_user_message, DETAIL_TEXT_MAX_CHARS)),
        Line::raw(""),
        label("Last sent message"),
        Line::raw(truncate(&session.last_user_message, DETAIL_TEXT_MAX_CHARS)),
        Line::raw(""),
        label("Last received message"),
        Line::raw(truncate(
            session
                .last_assistant_message
                .as_deref()
                .unwrap_or("not available"),
            DETAIL_TEXT_MAX_CHARS,
        )),
    ];
    lines.shrink_to_fit();
    lines
}

fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_owned()),
    ])
}

fn label(value: &str) -> Line<'static> {
    Line::styled(
        format!("{value}:"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn provider_color(provider: &str) -> Color {
    match provider {
        "codex" => Color::Green,
        "opencode" => Color::Cyan,
        "claude" => Color::Magenta,
        _ => Color::White,
    }
}

fn compact_time(timestamp: i64) -> String {
    DateTime::from_timestamp_millis(timestamp)
        .map(|time| time.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn full_time(timestamp: i64) -> String {
    DateTime::from_timestamp_millis(timestamp)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut result: String = value.chars().take(max_chars.saturating_sub(3)).collect();
    result.push_str("...");
    result
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, truncate};

    #[test]
    fn fuzzy_matching_accepts_non_contiguous_case_folded_terms() {
        assert!(fuzzy_match(
            "fix authentication migration in the backend",
            "ath mig"
        ));
        assert!(!fuzzy_match(
            "fix authentication migration in the backend",
            "queue"
        ));
    }

    #[test]
    fn detail_text_is_truncated_to_the_requested_length() {
        let text = "a".repeat(200);

        let preview = truncate(&text, 20);

        assert_eq!(preview.chars().count(), 20);
        assert!(preview.ends_with("..."));
    }
}
