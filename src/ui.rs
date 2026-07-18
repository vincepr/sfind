use std::io::{self, IsTerminal};
use std::path::{Component, Path};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, TimeZone};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
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

const STACKED_DETAIL_TEXT_MAX_CHARS: usize = 360;
const STACKED_DETAIL_TITLE_MAX_CHARS: usize = 90;
const HORIZONTAL_LAYOUT_MIN_WIDTH: u16 = 110;
const LIST_ITEM_HEIGHT: usize = 2;
const RANGE_FILTER_WIDTH: u16 = 36;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DayRange {
    All,
    Days(u8),
}

const DAY_RANGES: [(DayRange, &str); 6] = [
    (DayRange::All, "All"),
    (DayRange::Days(1), "Today"),
    (DayRange::Days(2), "2d"),
    (DayRange::Days(3), "3d"),
    (DayRange::Days(7), "7d"),
    (DayRange::Days(30), "30d"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    Continue(usize),
    Fork(usize),
    PrintContinueCommand(usize),
    PrintForkCommand(usize),
}

impl Selection {
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Continue(index)
            | Self::Fork(index)
            | Self::PrintContinueCommand(index)
            | Self::PrintForkCommand(index) => index,
        }
    }
}

pub(crate) fn pick(sessions: &[Session], warning_count: usize) -> Result<Option<Selection>> {
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
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if let Some(outcome) = app.key(key) {
                    return Ok(outcome);
                }
            }
            Event::Key(_) => {}
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
    search_paths: Vec<String>,
    query: String,
    visible: Vec<usize>,
    list_state: ListState,
    list_inner: Rect,
    detail_inner: Rect,
    range_inner: Rect,
    detail_scroll: u16,
    fork_mode: bool,
    day_range: DayRange,
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
            search_paths: sessions
                .iter()
                .map(|session| {
                    session
                        .directory
                        .as_ref()
                        .map_or_else(String::new, |path| path.to_string_lossy().to_lowercase())
                })
                .collect(),
            query: String::new(),
            visible,
            list_state: ListState::default().with_selected(selected),
            list_inner: Rect::default(),
            detail_inner: Rect::default(),
            range_inner: Rect::default(),
            detail_scroll: 0,
            fork_mode: false,
            day_range: DayRange::All,
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
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(RANGE_FILTER_WIDTH)])
            .split(sections[0]);
        let search = Paragraph::new(format!("> {}", self.query)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search messages, titles, and directories "),
        );
        frame.render_widget(search, top[0]);
        let range_block = Block::default().borders(Borders::ALL).title(" Range ");
        self.range_inner = range_block.inner(top[1]);
        frame.render_widget(
            Paragraph::new(day_range_line(self.day_range)).block(range_block),
            top[1],
        );

        let pane_direction = pane_direction(sections[1].width);
        let panes = Layout::default()
            .direction(pane_direction)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections[1]);
        let list_block = Block::default().borders(Borders::ALL).title(format!(
            " Sessions ({}/{}) ",
            self.visible.len(),
            self.sessions.len()
        ));
        self.list_inner = list_block.inner(panes[0]);
        let (list_offset, list_end) = list_viewport(
            self.list_state.selected(),
            self.list_state.offset(),
            list_item_capacity(self.list_inner.height),
            self.visible.len(),
        );
        *self.list_state.offset_mut() = list_offset;
        let title_max_chars = list_title_capacity(self.list_inner.width);
        let items = self.visible[list_offset..list_end].iter().map(|index| {
            let session = &self.sessions[*index];
            let title = session
                .title
                .as_deref()
                .unwrap_or(&session.first_user_message);
            let directory = session.directory.as_ref().map_or_else(
                || "directory unknown".to_owned(),
                |path| compact_path(path, 3),
            );
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{:<8}", session.provider.label()),
                        Style::default().fg(provider_color(session.provider.label())),
                    ),
                    Span::raw(format!(" {}  ", compact_time(session.updated_at))),
                    Span::styled(
                        directory,
                        Style::default().fg(directory_color(session.directory.as_deref())),
                    ),
                ]),
                Line::raw(format!("  {}", truncate(title, title_max_chars))),
            ])
        });
        let list = List::new(items)
            .block(list_block)
            .highlight_style(
                Style::default()
                    .bg(if self.fork_mode {
                        Color::Red
                    } else {
                        Color::DarkGray
                    })
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let local_selection = self
            .list_state
            .selected()
            .map(|selected| selected.saturating_sub(list_offset));
        let mut local_state = ListState::default().with_selected(local_selection);
        frame.render_stateful_widget(list, panes[0], &mut local_state);

        let detail = self
            .selected_session()
            .map(|session| detail_lines(session, pane_direction == Direction::Vertical))
            .unwrap_or_else(|| vec![Line::raw("No matching sessions")]);
        let mode = if self.fork_mode { "FORK" } else { "CONTINUE" };
        let detail_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Details [{mode}] "));
        self.detail_inner = detail_block.inner(panes[1]);
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
        frame.render_widget(detail_paragraph.scroll((self.detail_scroll, 0)), panes[1]);
        let warnings = if self.warning_count == 0 {
            String::new()
        } else {
            format!(" | {} provider warning(s) logged", self.warning_count)
        };
        frame.render_widget(
            Paragraph::new(format!(
                "Tab toggle fork | Enter start | Ctrl-P print command | Up/Down select{warnings}"
            )),
            sections[2],
        );
        frame.set_cursor_position((
            top[0].x + 3 + self.query.chars().count() as u16,
            top[0].y + 1,
        ));
    }

    fn key(&mut self, key: KeyEvent) -> Option<Option<Selection>> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(None);
        }
        match key.code {
            KeyCode::Enter => {
                return Some(self.selected_index().map(|index| {
                    if self.fork_mode {
                        Selection::Fork(index)
                    } else {
                        Selection::Continue(index)
                    }
                }));
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(self.selected_index().map(|index| {
                    if self.fork_mode {
                        Selection::PrintForkCommand(index)
                    } else {
                        Selection::PrintContinueCommand(index)
                    }
                }));
            }
            KeyCode::Tab => self.fork_mode = !self.fork_mode,
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
            KeyCode::Backspace | KeyCode::Delete
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                delete_last_word(&mut self.query);
                self.filter();
            }
            KeyCode::Char('w' | 'h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                delete_last_word(&mut self.query);
                self.filter();
            }
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
            MouseEventKind::Down(_)
                if self.range_inner.contains((mouse.column, mouse.row).into()) =>
            {
                let column = mouse.column.saturating_sub(self.range_inner.x);
                if let Some(range) = day_range_at_column(column) {
                    self.day_range = range;
                    self.filter();
                }
            }
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
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::Down(_)
                if self.list_inner.contains((mouse.column, mouse.row).into()) =>
            {
                let row = usize::from(mouse.row.saturating_sub(self.list_inner.y));
                let index = self
                    .list_state
                    .offset()
                    .saturating_add(row / LIST_ITEM_HEIGHT);
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
        let cutoff = day_range_cutoff(self.day_range, Local::now());
        let mut matches = self
            .search_text
            .iter()
            .zip(&self.search_paths)
            .enumerate()
            .filter_map(|(index, (text, path))| {
                if cutoff.is_some_and(|cutoff| self.sessions[index].updated_at < cutoff) {
                    return None;
                }
                search_rank(text, path, &query).map(|rank| (rank, index))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable_by(|left, right| {
            right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1))
        });
        self.visible = matches.into_iter().map(|(_, index)| index).collect();
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

fn day_range_line(selected: DayRange) -> Line<'static> {
    let mut spans = Vec::with_capacity(DAY_RANGES.len() * 2 - 1);
    for (index, (range, label)) in DAY_RANGES.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if *range == selected {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    Line::from(spans)
}

fn day_range_at_column(column: u16) -> Option<DayRange> {
    let mut start = 0_u16;
    for (index, (range, label)) in DAY_RANGES.iter().enumerate() {
        if index > 0 {
            start = start.saturating_add(1);
        }
        let end = start.saturating_add(label.len() as u16 + 2);
        if (start..end).contains(&column) {
            return Some(*range);
        }
        start = end;
    }
    None
}

fn day_range_cutoff(range: DayRange, now: DateTime<Local>) -> Option<i64> {
    let DayRange::Days(days) = range else {
        return None;
    };
    let date = now
        .date_naive()
        .checked_sub_days(Days::new(u64::from(days.saturating_sub(1))))?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|date| date.timestamp_millis())
}

fn pane_direction(width: u16) -> Direction {
    if width >= HORIZONTAL_LAYOUT_MIN_WIDTH {
        Direction::Horizontal
    } else {
        Direction::Vertical
    }
}

fn list_item_capacity(height: u16) -> usize {
    (usize::from(height) / LIST_ITEM_HEIGHT).max(1)
}

fn list_title_capacity(width: u16) -> usize {
    usize::from(width).saturating_sub(4)
}

fn delete_last_word(value: &mut String) {
    while value.chars().next_back().is_some_and(char::is_whitespace) {
        value.pop();
    }
    while value
        .chars()
        .next_back()
        .is_some_and(|character| !character.is_whitespace())
    {
        value.pop();
    }
}

fn list_viewport(
    selected: Option<usize>,
    current_offset: usize,
    height: usize,
    item_count: usize,
) -> (usize, usize) {
    if height == 0 || item_count == 0 {
        return (0, 0);
    }
    let max_offset = item_count.saturating_sub(height);
    let mut offset = current_offset.min(max_offset);
    if let Some(selected) = selected {
        if selected < offset {
            offset = selected;
        } else if selected >= offset.saturating_add(height) {
            offset = selected.saturating_add(1).saturating_sub(height);
        }
    }
    offset = offset.min(max_offset);
    (offset, offset.saturating_add(height).min(item_count))
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

fn search_rank(search_text: &str, path: &str, query: &str) -> Option<u8> {
    if query.trim().is_empty() {
        return Some(0);
    }
    if !path.is_empty() && path.contains(query) {
        return Some(3);
    }
    if !path.is_empty() && fuzzy_match(path, query) {
        return Some(2);
    }
    fuzzy_match(search_text, query).then_some(1)
}

fn detail_lines(session: &Session, stacked: bool) -> Vec<Line<'static>> {
    let (title_max_chars, text_max_chars, received_max_chars) = detail_cutoffs(stacked);
    let mut lines = vec![
        field("Provider", session.provider.label()),
        field("Updated", &full_time(session.updated_at)),
        field("Session", &session.id),
        field(
            "Title",
            &truncate(
                session.title.as_deref().unwrap_or("not provided"),
                title_max_chars,
            ),
        ),
        field(
            "Directory",
            &session
                .directory
                .as_ref()
                .map_or_else(|| "not provided".to_owned(), |path| compact_path(path, 3)),
        ),
        Line::raw(""),
        label("First sent message"),
        Line::raw(truncate(&session.first_user_message, text_max_chars)),
        Line::raw(""),
        label("Last sent message"),
        Line::raw(truncate(&session.last_user_message, text_max_chars)),
        Line::raw(""),
        label("Last received message"),
        Line::raw(truncate_middle(
            session
                .last_assistant_message
                .as_deref()
                .unwrap_or("not available"),
            received_max_chars,
        )),
    ];
    lines.shrink_to_fit();
    lines
}

fn detail_cutoffs(stacked: bool) -> (usize, usize, usize) {
    let multiplier = if stacked { 1 } else { 2 };
    let title = STACKED_DETAIL_TITLE_MAX_CHARS * multiplier;
    let text = STACKED_DETAIL_TEXT_MAX_CHARS * multiplier;
    (title, text, text * 2)
}

fn compact_path(path: &Path, visible_parts: usize) -> String {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy()),
            Component::RootDir | Component::CurDir | Component::ParentDir => None,
        })
        .collect::<Vec<_>>();
    if parts.len() <= visible_parts {
        return path.display().to_string();
    }
    format!(".../{}", parts[parts.len() - visible_parts..].join("/"))
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

fn directory_color(directory: Option<&Path>) -> Color {
    const COLORS: [Color; 9] = [
        Color::LightBlue,
        Color::LightCyan,
        Color::LightGreen,
        Color::LightMagenta,
        Color::LightYellow,
        Color::Cyan,
        Color::Green,
        Color::Magenta,
        Color::Yellow,
    ];
    let Some(directory) = directory else {
        return Color::DarkGray;
    };
    let hash = directory
        .as_os_str()
        .to_string_lossy()
        .bytes()
        .fold(2_166_136_261_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(16_777_619)
        });
    COLORS[hash as usize % COLORS.len()]
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
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut result: String = value.chars().take(max_chars.saturating_sub(3)).collect();
    result.push_str("...");
    result
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let retained = max_chars - 3;
    let prefix_len = retained / 2;
    let suffix_len = retained - prefix_len;
    let mut result = String::with_capacity(max_chars);
    result.extend(&characters[..prefix_len]);
    result.push_str("...");
    result.extend(&characters[characters.len() - suffix_len..]);
    result
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::{Local, TimeZone};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Direction;
    use session_search::{Provider, Session};

    use super::{
        compact_path, day_range_at_column, day_range_cutoff, delete_last_word, detail_cutoffs,
        directory_color, fuzzy_match, list_item_capacity, list_title_capacity, list_viewport,
        pane_direction, search_rank, truncate, truncate_middle, App, DayRange, Selection,
    };

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
    fn directory_matches_rank_above_message_matches() {
        assert_eq!(
            search_rank(
                "unrelated message /home/vince/code/session-search",
                "/home/vince/code/session-search",
                "session-sea"
            ),
            Some(3)
        );
        assert_eq!(
            search_rank("discuss session search behavior", "", "session search"),
            Some(1)
        );
    }

    #[test]
    fn range_filter_hitboxes_match_visible_labels() {
        assert_eq!(day_range_at_column(0), Some(DayRange::All));
        assert_eq!(day_range_at_column(6), Some(DayRange::Days(1)));
        assert_eq!(day_range_at_column(14), Some(DayRange::Days(2)));
        assert_eq!(day_range_at_column(5), None);
    }

    #[test]
    fn day_ranges_start_at_local_calendar_midnight() {
        let now = Local
            .with_ymd_and_hms(2026, 7, 18, 15, 30, 0)
            .single()
            .expect("unambiguous local test time");
        let expected = Local
            .with_ymd_and_hms(2026, 7, 17, 0, 0, 0)
            .earliest()
            .expect("local midnight")
            .timestamp_millis();

        assert_eq!(day_range_cutoff(DayRange::All, now), None);
        assert_eq!(day_range_cutoff(DayRange::Days(2), now), Some(expected));
    }

    #[test]
    fn detail_text_is_truncated_to_the_requested_length() {
        let text = "a".repeat(200);

        let preview = truncate(&text, 20);

        assert_eq!(preview.chars().count(), 20);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn received_text_truncation_preserves_beginning_and_end() {
        let text = "0123456789abcdefghijklmnopqrstuvwxyz";

        let preview = truncate_middle(text, 15);

        assert_eq!(preview, "012345...uvwxyz");
        assert_eq!(preview.chars().count(), 15);
    }

    #[test]
    fn list_viewport_contains_selection_and_only_visible_rows() {
        assert_eq!(list_viewport(Some(0), 0, 20, 10_000), (0, 20));
        assert_eq!(list_viewport(Some(20), 0, 20, 10_000), (1, 21));
        assert_eq!(
            list_viewport(Some(5_000), 4_980, 20, 10_000),
            (4_981, 5_001)
        );
        assert_eq!(
            list_viewport(Some(9_999), 9_980, 20, 10_000),
            (9_980, 10_000)
        );
    }

    #[test]
    fn empty_or_zero_height_list_has_an_empty_viewport() {
        assert_eq!(list_viewport(None, 0, 20, 0), (0, 0));
        assert_eq!(list_viewport(Some(0), 0, 0, 100), (0, 0));
    }

    #[test]
    fn two_line_session_rows_are_accounted_for_in_viewport_height() {
        assert_eq!(list_item_capacity(20), 10);
        assert_eq!(list_item_capacity(5), 2);
        assert_eq!(list_item_capacity(1), 1);
    }

    #[test]
    fn title_preview_uses_available_list_width() {
        assert_eq!(list_title_capacity(40), 36);
        assert_eq!(list_title_capacity(100), 96);
    }

    #[test]
    fn control_delete_removes_the_last_query_word() {
        let mut query = "auth migration   ".to_owned();

        delete_last_word(&mut query);

        assert_eq!(query, "auth ");
    }

    #[test]
    fn control_delete_key_removes_a_word_from_the_fuzzy_query() {
        let sessions = [Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            directory: None,
            updated_at: 0,
            first_user_message: "auth migration".to_owned(),
            last_user_message: "auth migration".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["auth migration".to_owned()],
        }];
        let mut app = App::new(&sessions, 0);
        app.query = "auth migration".to_owned();

        app.key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL));

        assert_eq!(app.query, "auth ");
    }

    #[test]
    fn narrow_terminals_stack_session_list_above_details() {
        assert_eq!(pane_direction(109), Direction::Vertical);
        assert_eq!(pane_direction(110), Direction::Horizontal);
    }

    #[test]
    fn side_by_side_detail_cutoffs_are_twice_stacked_cutoffs() {
        let stacked = detail_cutoffs(true);
        let side_by_side = detail_cutoffs(false);

        assert_eq!(stacked, (90, 360, 720));
        assert_eq!(side_by_side, (180, 720, 1_440));
    }

    #[test]
    fn long_directories_show_only_the_last_three_parts() {
        assert_eq!(
            compact_path(Path::new("/home/vince/code/session-search"), 3),
            ".../vince/code/session-search"
        );
        assert_eq!(compact_path(Path::new("code/project"), 3), "code/project");
    }

    #[test]
    fn identical_directories_have_identical_colors() {
        let first = directory_color(Some(Path::new("/work/api")));
        let second = directory_color(Some(Path::new("/work/api")));

        assert_eq!(first, second);
    }

    #[test]
    fn control_p_returns_print_command_selection() {
        let sessions = [Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            directory: None,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
        }];
        let mut app = App::new(&sessions, 0);

        let result = app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));

        assert_eq!(result, Some(Some(Selection::PrintContinueCommand(0))));
    }

    #[test]
    fn tab_switches_enter_and_printing_to_fork_mode() {
        let sessions = [Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            directory: None,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
        }];
        let mut app = App::new(&sessions, 0);

        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert!(app.fork_mode);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Some(Selection::Fork(0)))
        );
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Some(Selection::PrintForkCommand(0)))
        );
    }
}
