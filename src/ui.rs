use std::cmp::Ordering;
use std::io::{self, IsTerminal};
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate, TimeZone};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::{Frame, Terminal};
use sfind::{Provider, Session};

const STACKED_DETAIL_TEXT_MAX_CHARS: usize = 360;
const STACKED_DETAIL_TITLE_MAX_CHARS: usize = 90;
const HORIZONTAL_LAYOUT_MIN_WIDTH: u16 = 110;
const LIST_ITEM_HEIGHT: usize = 2;
const RANGE_FILTER_WIDTH: u16 = 12;
const PROVIDER_FILTER_WIDTH: u16 = 12;
const SEARCH_MIN_WIDTH: u16 = 8;
const UNAVAILABLE: &str = "-";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DayRange {
    All,
    Days(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOrder {
    Date,
    Provider,
    Location,
    Tokens,
}

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
            .context("could not draw sfind")?;
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
    search_inner: Rect,
    list_inner: Rect,
    detail_inner: Rect,
    directory_areas: Vec<Rect>,
    range_inner: Rect,
    provider_inner: Rect,
    sort_areas: [Rect; 4],
    detail_scroll: u16,
    fork_mode: bool,
    day_range: DayRange,
    provider_filter: Option<Provider>,
    session_order: SessionOrder,
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
            search_inner: Rect::default(),
            list_inner: Rect::default(),
            detail_inner: Rect::default(),
            directory_areas: Vec::new(),
            range_inner: Rect::default(),
            provider_inner: Rect::default(),
            sort_areas: [Rect::default(); 4],
            detail_scroll: 0,
            fork_mode: false,
            day_range: DayRange::All,
            provider_filter: None,
            session_order: SessionOrder::Date,
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
        let filter_width = filter_width(sections[0].width);
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(filter_width.min(RANGE_FILTER_WIDTH)),
                Constraint::Length(filter_width.min(PROVIDER_FILTER_WIDTH)),
            ])
            .split(sections[0]);
        let search_block = Block::default()
            .borders(Borders::ALL)
            .title(" Search messages, titles, and directories ");
        self.search_inner = search_block.inner(top[0]);
        let search = Paragraph::new(format!("> {}", self.query))
            .block(search_block)
            .scroll((0, search_scroll(self.search_inner, &self.query)));
        frame.render_widget(search, top[0]);
        let range_block = Block::default().borders(Borders::ALL).title(" Range ");
        self.range_inner = range_block.inner(top[1]);
        frame.render_widget(
            Paragraph::new(Line::styled(
                padded_label(day_range_label(self.day_range), self.range_inner.width),
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ))
            .block(range_block),
            top[1],
        );
        let provider_block = Block::default().borders(Borders::ALL).title(" CLI ");
        self.provider_inner = provider_block.inner(top[2]);
        let provider = self.provider_filter.map_or("All", provider_short_label);
        frame.render_widget(
            Paragraph::new(Line::styled(
                padded_label(provider, self.provider_inner.width),
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ))
            .block(provider_block),
            top[2],
        );
        let pane_direction = pane_direction(sections[1].width);
        let panes = Layout::default()
            .direction(pane_direction)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections[1]);
        let list_block = session_list_block(self.visible.len(), self.sessions.len());
        self.list_inner = list_block.inner(panes[0]);
        self.sort_areas = list_sort_areas(panes[0], self.list_inner);
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
            ListItem::new(vec![
                session_meta_line(session, self.list_inner.width.saturating_sub(2)),
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
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always);
        let local_selection = self
            .list_state
            .selected()
            .map(|selected| selected.saturating_sub(list_offset));
        let mut local_state = ListState::default().with_selected(local_selection);
        frame.render_stateful_widget(list, panes[0], &mut local_state);
        let active_sort_area = self.sort_areas[session_order_index(self.session_order)];
        if active_sort_area.width != 0 {
            let indicator = Rect::new(
                active_sort_area.x + active_sort_area.width / 2,
                active_sort_area.y,
                1,
                1,
            );
            frame.render_widget(
                Paragraph::new("▼").style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                indicator,
            );
        }

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
        let max_scroll = detail_height.saturating_sub(usize::from(self.detail_inner.height));
        self.detail_scroll = self
            .detail_scroll
            .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
        let has_directory = self
            .selected_session()
            .is_some_and(|session| session.directory.is_some());
        let detail_paragraph = Paragraph::new(detail)
            .block(detail_block)
            .wrap(Wrap { trim: false });
        frame.render_widget(detail_paragraph.scroll((self.detail_scroll, 0)), panes[1]);
        self.directory_areas = if has_directory {
            modifier_areas(frame.buffer_mut(), self.detail_inner, Modifier::UNDERLINED)
        } else {
            Vec::new()
        };
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
        if self.search_inner.width != 0 && self.search_inner.height != 0 {
            frame.set_cursor_position(search_cursor(self.search_inner, &self.query));
        }
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
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(order) = sort_order_at(&self.sort_areas, mouse.column, mouse.row) {
                self.session_order = order;
                self.filter();
                return;
            }
            if let Some(directory) = self.directory_at(mouse.column, mouse.row) {
                open_directory_in_code(directory);
                return;
            }
        }
        match mouse.kind {
            MouseEventKind::Down(_)
                if self
                    .provider_inner
                    .contains((mouse.column, mouse.row).into()) =>
            {
                self.provider_filter = next_provider_filter(self.provider_filter);
                self.filter();
            }
            MouseEventKind::Down(_)
                if self.range_inner.contains((mouse.column, mouse.row).into()) =>
            {
                self.day_range = next_day_range(self.day_range);
                self.filter();
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
            MouseEventKind::ScrollUp
                if self.list_inner.contains((mouse.column, mouse.row).into()) =>
            {
                self.move_selection(-1);
            }
            MouseEventKind::ScrollDown
                if self.list_inner.contains((mouse.column, mouse.row).into()) =>
            {
                self.move_selection(1);
            }
            MouseEventKind::Down(_)
                if self.list_inner.contains((mouse.column, mouse.row).into()) =>
            {
                if let Some(index) = list_index_at(
                    self.list_inner,
                    self.list_state.offset(),
                    self.visible.len(),
                    mouse.column,
                    mouse.row,
                ) {
                    self.select(index);
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
    }

    fn directory_at(&self, column: u16, row: u16) -> Option<&Path> {
        if !self
            .directory_areas
            .iter()
            .any(|area| area.contains((column, row).into()))
        {
            return None;
        }
        self.selected_session()?.directory.as_deref()
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
                if self
                    .provider_filter
                    .is_some_and(|provider| self.sessions[index].provider != provider)
                {
                    return None;
                }
                search_rank(text, path, &query).map(|rank| (rank, index))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| {
                    compare_sessions(
                        &self.sessions[left.1],
                        &self.sessions[right.1],
                        self.session_order,
                    )
                })
                .then_with(|| left.1.cmp(&right.1))
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

fn session_order_index(order: SessionOrder) -> usize {
    match order {
        SessionOrder::Provider => 0,
        SessionOrder::Date => 1,
        SessionOrder::Location => 2,
        SessionOrder::Tokens => 3,
    }
}

fn sort_order_at(areas: &[Rect; 4], column: u16, row: u16) -> Option<SessionOrder> {
    let point = (column, row).into();
    [
        SessionOrder::Provider,
        SessionOrder::Date,
        SessionOrder::Location,
        SessionOrder::Tokens,
    ]
    .into_iter()
    .zip(areas)
    .find_map(|(order, area)| area.contains(point).then_some(order))
}

fn compare_sessions(left: &Session, right: &Session, order: SessionOrder) -> Ordering {
    let primary = match order {
        SessionOrder::Date => newest_first(left, right),
        SessionOrder::Provider => left.provider.label().cmp(right.provider.label()),
        SessionOrder::Location => {
            optional_first(left.directory.as_deref(), right.directory.as_deref())
        }
        SessionOrder::Tokens => compare_token_totals(left, right),
    };
    primary
        .then_with(|| {
            if order == SessionOrder::Date {
                Ordering::Equal
            } else {
                newest_first(left, right)
            }
        })
        .then_with(|| left.provider.label().cmp(right.provider.label()))
        .then_with(|| left.id.cmp(&right.id))
}

fn newest_first(left: &Session, right: &Session) -> Ordering {
    right.updated_at.cmp(&left.updated_at)
}

fn compare_token_totals(left: &Session, right: &Session) -> Ordering {
    match (left.usage, right.usage) {
        (Some(left), Some(right)) => right.total_tokens().cmp(&left.total_tokens()),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn optional_first<T: Ord + ?Sized>(left: Option<&T>, right: Option<&T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn next_day_range(current: DayRange) -> DayRange {
    match current {
        DayRange::All => DayRange::Days(1),
        DayRange::Days(1) => DayRange::Days(3),
        DayRange::Days(3) => DayRange::Days(7),
        DayRange::Days(7) => DayRange::Days(30),
        DayRange::Days(30) | DayRange::Days(_) => DayRange::All,
    }
}

fn day_range_label(range: DayRange) -> &'static str {
    match range {
        DayRange::All => "All",
        DayRange::Days(1) => "Today",
        DayRange::Days(3) => "3d",
        DayRange::Days(7) => "7d",
        DayRange::Days(30) => "30d",
        DayRange::Days(_) => "All",
    }
}

fn day_range_cutoff(range: DayRange, now: DateTime<Local>) -> Option<i64> {
    let date = day_range_start_date(range, now.date_naive())?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|date| date.timestamp_millis())
}

fn day_range_start_date(range: DayRange, today: NaiveDate) -> Option<NaiveDate> {
    let DayRange::Days(days) = range else {
        return None;
    };
    today.checked_sub_days(Days::new(u64::from(days.saturating_sub(1))))
}

fn next_provider_filter(current: Option<Provider>) -> Option<Provider> {
    match current {
        None => Some(Provider::Codex),
        Some(Provider::Codex) => Some(Provider::OpenCode),
        Some(Provider::OpenCode) => Some(Provider::Claude),
        Some(Provider::Claude) => None,
    }
}

fn provider_short_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "codex",
        Provider::OpenCode => "open",
        Provider::Claude => "claude",
    }
}

fn padded_label(label: &str, width: u16) -> String {
    let width = usize::from(width);
    let label = truncate(label, width);
    let label_width = label.chars().count();
    let remaining = width.saturating_sub(label_width);
    let left = remaining / 2;
    let right = remaining - left;
    format!("{}{}{}", " ".repeat(left), label, " ".repeat(right))
}

fn filter_width(total_width: u16) -> u16 {
    (total_width.saturating_sub(SEARCH_MIN_WIDTH) / 2)
        .min(RANGE_FILTER_WIDTH.max(PROVIDER_FILTER_WIDTH))
}

fn search_cursor(inner: Rect, query: &str) -> (u16, u16) {
    let query_width = Line::from(query).width();
    let relative_x = 2_usize
        .saturating_add(query_width)
        .min(usize::from(inner.width.saturating_sub(1)));
    (inner.x.saturating_add(relative_x as u16), inner.y)
}

fn search_scroll(inner: Rect, query: &str) -> u16 {
    let content_width = 2_usize.saturating_add(Line::from(query).width());
    let visible_before_cursor = usize::from(inner.width.saturating_sub(1));
    u16::try_from(content_width.saturating_sub(visible_before_cursor)).unwrap_or(u16::MAX)
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

fn session_list_block(visible: usize, total: usize) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title_bottom(Line::raw(format!(" Sessions ({visible}/{total}) ")).right_aligned())
}

fn list_sort_areas(outer: Rect, inner: Rect) -> [Rect; 4] {
    let start = inner.x.saturating_add(2).min(inner.right());
    let available = inner.right().saturating_sub(start);
    let widths = list_column_widths(available);
    let mut x = start;
    std::array::from_fn(|index| {
        let area = Rect::new(x, outer.y, widths[index], 1);
        x = x.saturating_add(widths[index]);
        area
    })
}

fn list_column_widths(available: u16) -> [u16; 4] {
    if available >= 39 {
        return [8, 14, available - 36, 14];
    }
    let base = available / 4;
    let mut widths = [base; 4];
    for width in widths.iter_mut().take(usize::from(available % 4)) {
        *width += 1;
    }
    widths
}

fn session_meta_line(session: &Session, width: u16) -> Line<'static> {
    let columns = list_column_widths(width);
    let provider = fit_left(session.provider.label(), columns[0]);
    let time = fit_left(
        &format!(" {}  ", compact_time(session.updated_at)),
        columns[1],
    );
    let directory = session
        .directory
        .as_ref()
        .map_or_else(|| UNAVAILABLE.to_owned(), |path| compact_path(path, 3));
    let directory = fit_left(&directory, columns[2]);
    let total = session
        .usage
        .map(|usage| format_number(usage.total_tokens()))
        .map_or_else(
            || " ".repeat(usize::from(columns[3])),
            |total| fit_right(&total, columns[3]),
        );
    Line::from(vec![
        Span::styled(
            provider,
            Style::default().fg(provider_color(session.provider.label())),
        ),
        Span::raw(time),
        Span::styled(
            directory,
            Style::default().fg(directory_color(session.directory.as_deref())),
        ),
        Span::styled(
            total,
            Style::default()
                .fg(provider_color(session.provider.label()))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn fit_left(value: &str, width: u16) -> String {
    let width = usize::from(width);
    let value = truncate_to_width(value, width);
    let padding = width.saturating_sub(Line::from(value.as_str()).width());
    format!("{value}{}", " ".repeat(padding))
}

fn fit_right(value: &str, width: u16) -> String {
    let width = usize::from(width);
    let value_width = Line::from(value).width();
    if value_width > width {
        return " ".repeat(width);
    }
    format!("{}{value}", " ".repeat(width - value_width))
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if Line::from(value).width() <= max_width {
        return value.to_owned();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let retained_width = max_width - 3;
    let mut result = String::new();
    let mut width = 0_usize;
    for character in value.chars() {
        let character_width = Line::from(character.to_string()).width();
        if width.saturating_add(character_width) > retained_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push_str("...");
    result
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

fn list_index_at(
    inner: Rect,
    offset: usize,
    item_count: usize,
    column: u16,
    row: u16,
) -> Option<usize> {
    if !inner.contains((column, row).into()) {
        return None;
    }
    let relative_row = usize::from(row.saturating_sub(inner.y));
    let index = offset.saturating_add(relative_row / LIST_ITEM_HEIGHT);
    (index < item_count).then_some(index)
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
    let model = match (
        session.model.as_deref(),
        session.reasoning_effort.as_deref(),
    ) {
        (Some(model), Some(effort)) => format!("{model} - {effort}"),
        (Some(model), None) => model.to_owned(),
        (None, Some(effort)) => format!("{UNAVAILABLE} - {effort}"),
        (None, None) => UNAVAILABLE.to_owned(),
    };
    let directory = session
        .directory
        .as_ref()
        .map_or_else(|| UNAVAILABLE.to_owned(), |path| compact_path(path, 3));
    let directory = field_with_style(
        "Directory",
        &directory,
        if session.directory.is_some() {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        },
    );
    let mut lines = vec![
        field("Provider", session.provider.label()),
        field("Updated", &full_time(session.updated_at)),
        field("Session", &session.id),
        field(
            "Title",
            &truncate(
                session.title.as_deref().unwrap_or(UNAVAILABLE),
                title_max_chars,
            ),
        ),
        directory,
        Line::raw(""),
        label("First user message", Color::LightBlue),
        Line::raw(truncate(&session.first_user_message, text_max_chars)),
        Line::raw(""),
        label("Last user message", Color::LightBlue),
        Line::raw(truncate(&session.last_user_message, text_max_chars)),
        Line::raw(""),
        label(
            &format!("Last {} message", provider_short_label(session.provider)),
            provider_color(session.provider.label()),
        ),
        Line::raw(truncate_middle(
            session
                .last_assistant_message
                .as_deref()
                .unwrap_or(UNAVAILABLE),
            received_max_chars,
        )),
        Line::raw(""),
        label("Stats", Color::LightGreen),
        field("Model", &model),
    ];
    match session.usage {
        Some(usage) => {
            lines.push(Line::raw(format!(
                "In {}  Out {}  Total {}",
                format_number(usage.input_tokens),
                format_number(usage.output_tokens),
                format_number(usage.total_tokens())
            )));
            lines.push(Line::raw(format!(
                "Cache create {}  Cache read {}",
                format_number(usage.cache_creation_tokens),
                format_number(usage.cache_read_tokens)
            )));
        }
        None => lines.push(Line::raw(UNAVAILABLE)),
    }
    lines
}

fn detail_cutoffs(stacked: bool) -> (usize, usize, usize) {
    let multiplier = if stacked { 1 } else { 2 };
    let title = STACKED_DETAIL_TITLE_MAX_CHARS * multiplier;
    let text = STACKED_DETAIL_TEXT_MAX_CHARS * multiplier;
    (title, text, text * 2)
}

fn modifier_areas(buffer: &Buffer, area: Rect, modifier: Modifier) -> Vec<Rect> {
    (area.top()..area.bottom())
        .filter_map(|y| {
            let mut columns = (area.left()..area.right())
                .filter(|x| buffer[(*x, y)].style().add_modifier.contains(modifier));
            let first = columns.next()?;
            let last = columns.next_back().unwrap_or(first);
            Some(Rect::new(
                first,
                y,
                last.saturating_sub(first).saturating_add(1),
                1,
            ))
        })
        .collect()
}

fn code_directory_process(directory: &Path) -> Command {
    let mut command = Command::new("code");
    command
        .arg(".")
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn open_directory_in_code(directory: &Path) {
    if let Ok(mut child) = code_directory_process(directory).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
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
    field_with_style(label, value, Style::default())
}

fn field_with_style(label: &str, value: &str, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_owned(), value_style),
    ])
}

fn label(value: &str, color: Color) -> Line<'static> {
    Line::styled(
        format!("{value}:"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn provider_color(provider: &str) -> Color {
    match provider {
        "codex" => Color::Green,
        "opencode" => Color::Cyan,
        "claude" => Color::Yellow,
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
        .unwrap_or_else(|| UNAVAILABLE.to_owned())
}

fn full_time(timestamp: i64) -> String {
    DateTime::from_timestamp_millis(timestamp)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| UNAVAILABLE.to_owned())
}

fn format_number(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index != 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(character);
    }
    result
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
    let character_count = value.chars().count();
    if character_count <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let retained = max_chars - 3;
    let prefix_len = retained / 2;
    let suffix_len = retained - prefix_len;
    let mut result = String::with_capacity(max_chars);
    result.extend(value.chars().take(prefix_len));
    result.push_str("...");
    let suffix = value.chars().rev().take(suffix_len).collect::<Vec<_>>();
    result.extend(suffix.into_iter().rev());
    result
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::NaiveDate;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Direction, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    use sfind::{Provider, Session, TokenUsage};

    use super::{
        code_directory_process, compact_path, day_range_label, day_range_start_date,
        delete_last_word, detail_cutoffs, detail_lines, directory_color, filter_width, fuzzy_match,
        list_column_widths, list_index_at, list_item_capacity, list_sort_areas,
        list_title_capacity, list_viewport, modifier_areas, next_day_range, next_provider_filter,
        padded_label, pane_direction, provider_color, provider_short_label, search_cursor,
        search_rank, search_scroll, session_list_block, session_meta_line, session_order_index,
        sort_order_at, truncate, truncate_middle, App, DayRange, Selection, SessionOrder,
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
                "unrelated message /home/vince/code/sfind",
                "/home/vince/code/sfind",
                "sfind"
            ),
            Some(3)
        );
        assert_eq!(search_rank("discuss sfind behavior", "", "sfind"), Some(1));
    }

    #[test]
    fn range_filter_cycles_through_each_period() {
        let mut range = DayRange::All;
        let expected = [
            (DayRange::Days(1), "Today"),
            (DayRange::Days(3), "3d"),
            (DayRange::Days(7), "7d"),
            (DayRange::Days(30), "30d"),
            (DayRange::All, "All"),
        ];

        for (next, label) in expected {
            range = next_day_range(range);
            assert_eq!(range, next);
            assert_eq!(day_range_label(range), label);
        }
    }

    #[test]
    fn short_filter_labels_fill_their_button_width() {
        let label = padded_label("open", 12);

        assert_eq!(label.chars().count(), 12);
        assert_eq!(label.trim(), "open");
    }

    #[test]
    fn narrow_headers_reserve_space_for_search() {
        assert_eq!(filter_width(20), 6);
        assert_eq!(filter_width(40), 12);
    }

    #[test]
    fn search_cursor_accounts_for_origin_width_and_unicode() {
        let inner = ratatui::layout::Rect::new(4, 2, 8, 1);

        assert_eq!(search_cursor(inner, "é"), (7, 2));
        assert_eq!(search_cursor(inner, "query longer than pane"), (11, 2));
        assert_eq!(search_scroll(inner, "é"), 0);
        assert_eq!(search_scroll(inner, "12345678"), 3);
    }

    #[test]
    fn day_ranges_use_local_calendar_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 18).expect("valid test date");
        let expected = NaiveDate::from_ymd_opt(2026, 7, 16).expect("valid test date");

        assert_eq!(day_range_start_date(DayRange::All, today), None);
        assert_eq!(
            day_range_start_date(DayRange::Days(3), today),
            Some(expected)
        );
    }

    #[test]
    fn provider_filter_cycles_through_each_cli() {
        assert_eq!(next_provider_filter(None), Some(Provider::Codex));
        assert_eq!(
            next_provider_filter(Some(Provider::Codex)),
            Some(Provider::OpenCode)
        );
        assert_eq!(
            next_provider_filter(Some(Provider::OpenCode)),
            Some(Provider::Claude)
        );
        assert_eq!(next_provider_filter(Some(Provider::Claude)), None);
        assert_eq!(provider_short_label(Provider::OpenCode), "open");
    }

    #[test]
    fn sort_areas_follow_list_columns_and_remain_usable_when_narrow() {
        let outer = Rect::new(10, 5, 82, 20);
        let inner = Rect::new(11, 6, 80, 18);

        let areas = list_sort_areas(outer, inner);

        assert_eq!(areas.map(|area| area.width), [8, 14, 42, 14]);
        assert!(areas.iter().all(|area| area.y == outer.y));
        assert_eq!(session_order_index(SessionOrder::Provider), 0);
        assert_eq!(session_order_index(SessionOrder::Date), 1);
        assert_eq!(session_order_index(SessionOrder::Location), 2);
        assert_eq!(session_order_index(SessionOrder::Tokens), 3);

        let narrow = list_sort_areas(Rect::new(0, 0, 20, 5), Rect::new(1, 1, 18, 3));
        assert_eq!(narrow.map(|area| area.width), list_column_widths(16));
        assert!(narrow.iter().all(|area| area.width > 0));
    }

    #[test]
    fn session_count_is_rendered_on_the_bottom_right_border() {
        let area = Rect::new(0, 0, 30, 4);
        let mut buffer = Buffer::empty(area);

        session_list_block(23, 23).render(area, &mut buffer);

        let row = |y| {
            (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(!row(area.top()).contains("Sessions"));
        assert!(row(area.bottom() - 1).contains("Sessions (23/23)"));
    }

    #[test]
    fn clicking_list_border_selects_that_columns_sort() {
        let sessions = [Session {
            provider: Provider::Codex,
            id: "a".to_owned(),
            title: None,
            model: Some("gpt".to_owned()),
            reasoning_effort: None,
            directory: None,
            updated_at: 0,
            first_user_message: "message".to_owned(),
            last_user_message: "message".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["message".to_owned()],
            usage: None,
        }];
        let mut app = App::new(&sessions, 0);
        app.sort_areas = list_sort_areas(Rect::new(10, 2, 82, 20), Rect::new(11, 3, 80, 18));

        for expected in [
            SessionOrder::Provider,
            SessionOrder::Date,
            SessionOrder::Location,
            SessionOrder::Tokens,
        ] {
            let area = app.sort_areas[session_order_index(expected)];
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
            assert_eq!(app.session_order, expected);
            assert_eq!(
                sort_order_at(&app.sort_areas, area.x, area.y),
                Some(expected)
            );
        }

        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: app.sort_areas[0].x,
            row: app.sort_areas[0].y,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.session_order, SessionOrder::Tokens);
    }

    #[test]
    fn directory_hit_areas_follow_rendered_wrapping_and_scroll() {
        let area = Rect::new(10, 5, 10, 3);
        let mut buffer = Buffer::empty(area);
        let lines = vec![
            Line::raw("12345678901234567890"),
            Line::styled(
                "directoryvalue",
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
        ];
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((2, 0))
            .render(area, &mut buffer);

        assert_eq!(
            modifier_areas(&buffer, area, Modifier::UNDERLINED),
            [Rect::new(10, 5, 10, 1), Rect::new(10, 6, 4, 1)]
        );
    }

    #[test]
    fn directory_click_resolves_the_selected_path() {
        let sessions = [Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: Some(Path::new("/work/project").to_path_buf()),
            updated_at: 0,
            first_user_message: "message".to_owned(),
            last_user_message: "message".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["message".to_owned()],
            usage: None,
        }];
        let mut app = App::new(&sessions, 0);
        app.directory_areas = vec![Rect::new(20, 8, 30, 2)];

        let details = detail_lines(&sessions[0], false);
        let directory_line = details
            .iter()
            .find(|line| line.to_string().starts_with("Directory:"))
            .expect("directory detail line");
        assert!(directory_line
            .spans
            .last()
            .is_some_and(|span| span.style.add_modifier.contains(Modifier::UNDERLINED)));

        assert_eq!(app.directory_at(25, 9), Some(Path::new("/work/project")));
        assert_eq!(app.directory_at(19, 9), None);
    }

    #[test]
    fn builds_vscode_directory_process() {
        let directory = Path::new("/work/project");

        let command = code_directory_process(directory);

        assert_eq!(command.get_program(), "code");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["."]);
        assert_eq!(command.get_current_dir(), Some(directory));
    }

    #[test]
    fn selected_order_sorts_with_newest_date_as_the_tie_breaker() {
        let session = |id: &str,
                       provider: Provider,
                       directory: Option<&str>,
                       tokens: Option<u64>,
                       updated_at: i64| Session {
            provider,
            id: id.to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: directory.map(Into::into),
            updated_at,
            first_user_message: "message".to_owned(),
            last_user_message: "message".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["message".to_owned()],
            usage: tokens.map(|tokens| TokenUsage {
                input_tokens: tokens,
                ..TokenUsage::default()
            }),
        };
        let sessions = [
            session("c", Provider::Claude, Some("/a"), Some(200), 100),
            session("d", Provider::Codex, None, None, 400),
            session("b", Provider::OpenCode, Some("/b"), Some(100), 200),
            session("a", Provider::Codex, Some("/z"), Some(100), 300),
        ];
        let mut app = App::new(&sessions, 0);

        for (order, expected) in [
            (SessionOrder::Date, ["d", "a", "b", "c"]),
            (SessionOrder::Provider, ["c", "d", "a", "b"]),
            (SessionOrder::Location, ["c", "b", "a", "d"]),
            (SessionOrder::Tokens, ["c", "a", "b", "d"]),
        ] {
            app.session_order = order;
            app.filter();
            let ids = app
                .visible
                .iter()
                .map(|index| sessions[*index].id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(ids, expected);
        }
    }

    #[test]
    fn selected_provider_filters_sessions() {
        let session = |id: &str, provider: Provider| Session {
            provider,
            id: id.to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: None,
            updated_at: 0,
            first_user_message: "message".to_owned(),
            last_user_message: "message".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["message".to_owned()],
            usage: None,
        };
        let sessions = [
            session("a", Provider::Codex),
            session("b", Provider::Claude),
        ];
        let mut app = App::new(&sessions, 0);
        app.provider_filter = Some(Provider::Claude);

        app.filter();

        assert_eq!(app.visible, [1]);
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
    fn detail_panel_shows_compact_token_stats() {
        let session = Session {
            provider: Provider::OpenCode,
            id: "session-1".to_owned(),
            title: None,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            directory: None,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: Some("response".to_owned()),
            user_messages: vec!["first".to_owned()],
            usage: Some(TokenUsage {
                input_tokens: 1_299_667,
                output_tokens: 171_958,
                cache_creation_tokens: 88_966,
                cache_read_tokens: 61_227_008,
            }),
        };

        let details = detail_lines(&session, false)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();

        assert!(details.iter().any(|line| line == "Stats:"));
        assert!(details
            .iter()
            .any(|line| line == "Model: gpt-5.6-sol - high"));
        assert!(!details
            .iter()
            .any(|line| line.starts_with("Reasoning effort:")));
        assert!(details
            .iter()
            .any(|line| line == "In 1,299,667  Out 171,958  Total 62,787,599"));
        assert!(details
            .iter()
            .any(|line| line == "Cache create 88,966  Cache read 61,227,008"));

        let meta = session_meta_line(&session, 80);
        assert_eq!(meta.width(), 80);
        assert_eq!(
            meta.spans.last().and_then(|span| span.style.fg),
            Some(Color::Cyan)
        );
        assert!(meta.to_string().ends_with("62,787,599"));
        assert_eq!(meta.to_string().chars().nth(22), Some('-'));
    }

    #[test]
    fn narrow_session_rows_keep_left_metadata_when_total_does_not_fit() {
        let session = Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: Some(Path::new("/work/project").to_path_buf()),
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage: Some(TokenUsage {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cache_creation_tokens: u64::MAX,
                cache_read_tokens: u64::MAX,
            }),
        };

        let meta = session_meta_line(&session, 30);
        let text = meta.to_string();

        assert_eq!(meta.width(), 30);
        assert!(text.starts_with("codex"));
        assert!(text.contains("/wor..."));
        assert!(!text.ends_with("18,446,744,073,709,551,615"));
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
    fn list_mouse_index_accounts_for_pane_origin_and_viewport_offset() {
        let inner = ratatui::layout::Rect::new(10, 5, 30, 8);

        assert_eq!(list_index_at(inner, 20, 100, 11, 7), Some(21));
        assert_eq!(list_index_at(inner, 20, 100, 9, 7), None);
        assert_eq!(list_index_at(inner, 99, 100, 11, 7), None);
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
            model: None,
            reasoning_effort: None,
            directory: None,
            updated_at: 0,
            first_user_message: "auth migration".to_owned(),
            last_user_message: "auth migration".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["auth migration".to_owned()],
            usage: None,
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
            compact_path(Path::new("/home/vince/code/sfind"), 3),
            ".../vince/code/sfind"
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
    fn claude_uses_a_high_contrast_provider_color() {
        assert_eq!(provider_color("claude"), Color::Yellow);
    }

    #[test]
    fn control_p_returns_print_command_selection() {
        let sessions = [Session {
            provider: Provider::Codex,
            id: "session-1".to_owned(),
            title: None,
            model: None,
            reasoning_effort: None,
            directory: None,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage: None,
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
            model: None,
            reasoning_effort: None,
            directory: None,
            updated_at: 0,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage: None,
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
