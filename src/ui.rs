use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::path::{Component, Path};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate, TimeZone};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget, Wrap};
use ratatui::{Frame, Terminal};
use sfind::{ModelUsage, Provider, Session, TokenUsage};

const STACKED_DETAIL_TEXT_MAX_CHARS: usize = 360;
const STACKED_DETAIL_TITLE_MAX_CHARS: usize = 90;
const HORIZONTAL_LAYOUT_MIN_WIDTH: u16 = 110;
const LIST_ITEM_HEIGHT: usize = 2;
const RANGE_FILTER_WIDTH: u16 = 12;
const PROVIDER_FILTER_WIDTH: u16 = 12;
const SEARCH_MIN_WIDTH: u16 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DayRange {
    All,
    Days(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewMode {
    Continue,
    Fork,
    Usage,
    Graph,
}

impl ViewMode {
    const fn next(self) -> Self {
        match self {
            Self::Continue => Self::Fork,
            Self::Fork => Self::Usage,
            Self::Usage => Self::Graph,
            Self::Graph => Self::Continue,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Continue => "CONTINUE",
            Self::Fork => "FORK",
            Self::Usage => "USAGE",
            Self::Graph => "GRAPH",
        }
    }
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
    range_inner: Rect,
    provider_inner: Rect,
    detail_scroll: u16,
    mode: ViewMode,
    day_range: DayRange,
    provider_filter: Option<Provider>,
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
            range_inner: Rect::default(),
            provider_inner: Rect::default(),
            detail_scroll: 0,
            mode: ViewMode::Continue,
            day_range: DayRange::All,
            provider_filter: None,
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

        match self.mode {
            ViewMode::Continue | ViewMode::Fork => {
                self.draw_session_panes(frame, sections[1]);
            }
            ViewMode::Usage => self.draw_usage(frame, sections[1]),
            ViewMode::Graph => self.draw_graph(frame, sections[1]),
        }
        let warnings = if self.warning_count == 0 {
            String::new()
        } else {
            format!(" | {} provider warning(s) logged", self.warning_count)
        };
        let help = match self.mode {
            ViewMode::Continue | ViewMode::Fork => {
                "Tab next view | Enter start | Ctrl-P print command | Up/Down select"
            }
            ViewMode::Usage => "Tab next view | Ctrl-Up/Down or mouse wheel scroll",
            ViewMode::Graph => "Tab next view | filters scope the displayed sessions",
        };
        frame.render_widget(Paragraph::new(format!("{help}{warnings}")), sections[2]);
        if self.search_inner.width != 0 && self.search_inner.height != 0 {
            frame.set_cursor_position(search_cursor(self.search_inner, &self.query));
        }
    }

    fn draw_session_panes(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let pane_direction = pane_direction(area.width);
        let panes = Layout::default()
            .direction(pane_direction)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
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
                    .bg(if self.mode == ViewMode::Fork {
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
        let detail_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Details [{}] ", self.mode.label()));
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
    }

    fn draw_usage(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.list_inner = Rect::default();
        let report = usage_report(self.sessions, &self.visible);
        let block = Block::default().borders(Borders::ALL).title(format!(
            " Usage ({}/{} sessions) [recorded cost] ",
            self.visible.len(),
            self.sessions.len()
        ));
        self.detail_inner = block.inner(area);
        let lines = usage_lines(&report, self.detail_inner.width);
        let max_scroll = lines
            .len()
            .saturating_sub(usize::from(self.detail_inner.height));
        self.detail_scroll = self
            .detail_scroll
            .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .scroll((self.detail_scroll, 0)),
            area,
        );
    }

    fn draw_graph(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.list_inner = Rect::default();
        self.detail_scroll = 0;
        let block = Block::default().borders(Borders::ALL).title(format!(
            " Token activity ({}/{} sessions) ",
            self.visible.len(),
            self.sessions.len()
        ));
        self.detail_inner = block.inner(area);
        let inner = self.detail_inner;
        frame.render_widget(block, area);
        let Some(series) = activity_series(
            self.sessions,
            &self.visible,
            inner.width,
            self.day_range,
            Local::now(),
        ) else {
            frame.render_widget(
                Paragraph::new("No token usage is available for the matching sessions"),
                inner,
            );
            return;
        };
        if inner.height < 4 {
            frame.render_widget(
                Paragraph::new(format!(
                    "{} tokens in {} buckets",
                    format_number(series.total),
                    series.interval.label
                )),
                inner,
            );
            return;
        }
        let mut header = vec![
            Line::raw(format!(
                "Approximation: session totals are assigned to last activity. Bucket: {}",
                series.interval.label
            )),
            Line::raw(format!("Total: {} tokens", format_number(series.total))),
        ];
        let max_legend_lines = usize::from(inner.height.saturating_sub(5));
        let mut legend = graph_legend_lines(&series.models, inner.width);
        legend.truncate(max_legend_lines);
        header.extend(legend);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(u16::try_from(header.len()).unwrap_or(u16::MAX)),
                Constraint::Min(2),
            ])
            .split(inner);
        frame.render_widget(Paragraph::new(header), sections[0]);
        let chart_block = Block::default().borders(Borders::TOP);
        let chart_inner = chart_block.inner(sections[1]);
        frame.render_widget(chart_block, sections[1]);
        frame.render_widget(StackedActivityChart { series: &series }, chart_inner);
    }

    fn key(&mut self, key: KeyEvent) -> Option<Option<Selection>> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(None);
        }
        match key.code {
            KeyCode::Enter if self.mode == ViewMode::Continue => {
                return Some(self.selected_index().map(Selection::Continue));
            }
            KeyCode::Enter if self.mode == ViewMode::Fork => {
                return Some(self.selected_index().map(Selection::Fork));
            }
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.mode == ViewMode::Continue =>
            {
                return Some(self.selected_index().map(Selection::PrintContinueCommand));
            }
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.mode == ViewMode::Fork =>
            {
                return Some(self.selected_index().map(Selection::PrintForkCommand));
            }
            KeyCode::Tab => {
                self.mode = self.mode.next();
                self.detail_scroll = 0;
            }
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
            KeyCode::Enter | KeyCode::Char(_) => {}
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct UsageRow {
    provider: Provider,
    model: String,
    tokens: TokenUsage,
    cost_microusd: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct UsageReport {
    session_count: usize,
    rows: Vec<UsageRow>,
    total: TokenUsage,
    cost_microusd: Option<u64>,
}

trait UsageCostProvider {
    fn cost_microusd(&self, provider: Provider, usage: &ModelUsage) -> Option<u64>;
}

struct RecordedUsageCosts;

impl UsageCostProvider for RecordedUsageCosts {
    fn cost_microusd(&self, _provider: Provider, usage: &ModelUsage) -> Option<u64> {
        usage.recorded_cost_microusd
    }
}

fn usage_report(sessions: &[Session], visible: &[usize]) -> UsageReport {
    usage_report_with_costs(sessions, visible, &RecordedUsageCosts)
}

fn usage_report_with_costs(
    sessions: &[Session],
    visible: &[usize],
    cost_provider: &impl UsageCostProvider,
) -> UsageReport {
    let mut report = UsageReport {
        session_count: visible.len(),
        ..UsageReport::default()
    };
    for session in visible.iter().filter_map(|index| sessions.get(*index)) {
        for usage in &session.usage.models {
            let cost_microusd = cost_provider.cost_microusd(session.provider, usage);
            report.total = add_tokens(report.total, usage.tokens);
            if let Some(row) = report
                .rows
                .iter_mut()
                .find(|row| row.provider == session.provider && row.model == usage.model)
            {
                row.tokens = add_tokens(row.tokens, usage.tokens);
                row.cost_microusd = merge_costs(row.cost_microusd, cost_microusd);
            } else {
                report.rows.push(UsageRow {
                    provider: session.provider,
                    model: usage.model.clone(),
                    tokens: usage.tokens,
                    cost_microusd,
                });
            }
        }
    }
    report.rows.sort_unstable_by(|left, right| {
        provider_order(left.provider)
            .cmp(&provider_order(right.provider))
            .then_with(|| left.model.cmp(&right.model))
    });
    report.cost_microusd = report
        .rows
        .iter()
        .try_fold(0_u64, |total, row| total.checked_add(row.cost_microusd?));
    report
}

const fn provider_order(provider: Provider) -> u8 {
    match provider {
        Provider::Codex => 0,
        Provider::OpenCode => 1,
        Provider::Claude => 2,
    }
}

fn add_tokens(left: TokenUsage, right: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cache_creation_tokens: left
            .cache_creation_tokens
            .saturating_add(right.cache_creation_tokens),
        cache_read_tokens: left
            .cache_read_tokens
            .saturating_add(right.cache_read_tokens),
        reasoning_tokens: left.reasoning_tokens.saturating_add(right.reasoning_tokens),
    }
}

fn merge_costs(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    left.zip(right)
        .and_then(|(left, right)| left.checked_add(right))
}

fn usage_lines(report: &UsageReport, width: u16) -> Vec<Line<'static>> {
    if report.rows.is_empty() {
        return vec![Line::raw(
            "No token usage is available for the matching sessions",
        )];
    }
    if width < 112 {
        return compact_usage_lines(report);
    }
    let model_width = usize::from(width).saturating_sub(95).max(8);
    let mut lines = vec![Line::styled(
        usage_table_line(
            "Agent",
            "Model",
            "Input",
            "Output",
            "Cache Create",
            "Cache Read",
            "Total Tokens",
            "Cost (USD)",
            model_width,
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    lines.push(Line::raw(usage_table_line(
        "All",
        "",
        &format_number(report.total.input_tokens),
        &format_number(report.total.output_tokens),
        &format_number(report.total.cache_creation_tokens),
        &format_number(report.total.cache_read_tokens),
        &format_number(report.total.total_tokens()),
        &format_cost(report.cost_microusd),
        model_width,
    )));
    let mut previous_provider = None;
    for row in &report.rows {
        let provider = if previous_provider == Some(row.provider) {
            ""
        } else {
            previous_provider = Some(row.provider);
            row.provider.label()
        };
        lines.push(Line::raw(usage_table_line(
            provider,
            &truncate(&row.model, model_width),
            &format_number(row.tokens.input_tokens),
            &format_number(row.tokens.output_tokens),
            &format_number(row.tokens.cache_creation_tokens),
            &format_number(row.tokens.cache_read_tokens),
            &format_number(row.tokens.total_tokens()),
            &format_cost(row.cost_microusd),
            model_width,
        )));
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn usage_table_line(
    provider: &str,
    model: &str,
    input: &str,
    output: &str,
    cache_creation: &str,
    cache_read: &str,
    total: &str,
    cost: &str,
    model_width: usize,
) -> String {
    format!(
        "{provider:<10} {model:<model_width$} {input:>13} {output:>13} {cache_creation:>13} {cache_read:>13} {total:>14} {cost:>11}"
    )
}

fn compact_usage_lines(report: &UsageReport) -> Vec<Line<'static>> {
    let mut lines = usage_summary_lines(
        &format!("All ({} sessions)", report.session_count),
        report.total,
        report.cost_microusd,
    );
    for row in &report.rows {
        lines.push(Line::raw(""));
        lines.extend(usage_summary_lines(
            &format!("{} / {}", row.provider.label(), row.model),
            row.tokens,
            row.cost_microusd,
        ));
    }
    lines
}

fn usage_summary_lines(
    title: &str,
    tokens: TokenUsage,
    cost_microusd: Option<u64>,
) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            title.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(format!(
            "Input {}  Output {}",
            format_number(tokens.input_tokens),
            format_number(tokens.output_tokens)
        )),
        Line::raw(format!(
            "Cache create {}  Cache read {}",
            format_number(tokens.cache_creation_tokens),
            format_number(tokens.cache_read_tokens)
        )),
        Line::raw(format!(
            "Total {}  Cost {}",
            format_number(tokens.total_tokens()),
            format_cost(cost_microusd)
        )),
    ]
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

fn format_cost(microusd: Option<u64>) -> String {
    microusd.map_or_else(
        || "unknown".to_owned(),
        |microusd| format!("${:.2}", microusd as f64 / 1_000_000.0),
    )
}

const MINUTE_MILLIS: i64 = 60_000;
const HOUR_MILLIS: i64 = 60 * MINUTE_MILLIS;
const DAY_MILLIS: i64 = 24 * HOUR_MILLIS;
const YEAR_MILLIS: i64 = 365 * DAY_MILLIS;
const MAX_GRAPH_MODELS: usize = 7;
const GRAPH_COLORS: [Color; 10] = [
    Color::Cyan,
    Color::Green,
    Color::Magenta,
    Color::Yellow,
    Color::LightBlue,
    Color::LightGreen,
    Color::LightMagenta,
    Color::LightYellow,
    Color::Blue,
    Color::Red,
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivityInterval {
    millis: i64,
    label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GraphModel {
    source: Option<String>,
    label: String,
    color: Color,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivityPoint {
    start: i64,
    tokens: Vec<u64>,
}

impl ActivityPoint {
    fn total(&self) -> u64 {
        self.tokens
            .iter()
            .fold(0_u64, |total, tokens| total.saturating_add(*tokens))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivitySeries {
    interval: ActivityInterval,
    models: Vec<GraphModel>,
    points: Vec<ActivityPoint>,
    total: u64,
    span: i64,
}

fn activity_series(
    sessions: &[Session],
    visible: &[usize],
    width: u16,
    range: DayRange,
    now: DateTime<Local>,
) -> Option<ActivitySeries> {
    let mut activity = Vec::new();
    for session in visible.iter().filter_map(|index| sessions.get(*index)) {
        for usage in &session.usage.models {
            let tokens = usage.tokens.total_tokens();
            if tokens != 0 {
                activity.push((session.updated_at, usage.model.as_str(), tokens));
            }
        }
    }
    let now_millis = now.timestamp_millis();
    let (start, end) = match range {
        DayRange::All => {
            let start = activity.iter().map(|(time, _, _)| *time).min()?;
            let end = activity.iter().map(|(time, _, _)| *time).max()?;
            (start, end)
        }
        DayRange::Days(_) => (day_range_cutoff(range, now)?, now_millis),
    };
    if end < start {
        return None;
    }
    activity.retain(|(time, _, _)| *time >= start && *time <= end);
    if activity.is_empty() {
        return None;
    }
    let span = end.saturating_sub(start).max(1);
    let max_points = usize::from(width / 7).max(2);
    let interval = activity_interval(span, max_points);
    let point_count = usize::try_from(div_ceil_positive(span, interval.millis))
        .ok()?
        .max(1);
    let models = graph_models(&activity);
    let mut points = (0..point_count)
        .map(|index| ActivityPoint {
            start: start.saturating_add(
                i64::try_from(index)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(interval.millis),
            ),
            tokens: vec![0; models.len()],
        })
        .collect::<Vec<_>>();
    for (time, model, tokens) in activity {
        let point = usize::try_from(time.saturating_sub(start) / interval.millis)
            .unwrap_or(usize::MAX)
            .min(points.len().saturating_sub(1));
        let model = models
            .iter()
            .position(|entry| entry.source.as_deref() == Some(model))
            .or_else(|| models.iter().position(|entry| entry.source.is_none()))?;
        points[point].tokens[model] = points[point].tokens[model].saturating_add(tokens);
    }
    let total = points
        .iter()
        .fold(0_u64, |total, point| total.saturating_add(point.total()));
    Some(ActivitySeries {
        interval,
        models,
        points,
        total,
        span,
    })
}

fn activity_interval(span: i64, max_points: usize) -> ActivityInterval {
    const INTERVALS: [(i64, &str); 17] = [
        (5 * MINUTE_MILLIS, "5 minutes"),
        (15 * MINUTE_MILLIS, "15 minutes"),
        (30 * MINUTE_MILLIS, "30 minutes"),
        (HOUR_MILLIS, "hour"),
        (2 * HOUR_MILLIS, "2 hours"),
        (3 * HOUR_MILLIS, "3 hours"),
        (4 * HOUR_MILLIS, "4 hours"),
        (6 * HOUR_MILLIS, "6 hours"),
        (12 * HOUR_MILLIS, "12 hours"),
        (DAY_MILLIS, "day"),
        (2 * DAY_MILLIS, "2 days"),
        (3 * DAY_MILLIS, "3 days"),
        (7 * DAY_MILLIS, "week"),
        (14 * DAY_MILLIS, "2 weeks"),
        (30 * DAY_MILLIS, "month"),
        (90 * DAY_MILLIS, "quarter"),
        (YEAR_MILLIS, "year"),
    ];
    let max_points = i64::try_from(max_points).unwrap_or(i64::MAX).max(1);
    let minimum = div_ceil_positive(span, max_points);
    if let Some((millis, label)) = INTERVALS.into_iter().find(|(millis, _)| *millis >= minimum) {
        return ActivityInterval {
            millis,
            label: label.to_owned(),
        };
    }
    let years = div_ceil_positive(minimum, YEAR_MILLIS);
    ActivityInterval {
        millis: years.saturating_mul(YEAR_MILLIS),
        label: format!("{years} years"),
    }
}

fn div_ceil_positive(value: i64, divisor: i64) -> i64 {
    value / divisor + i64::from(value % divisor != 0)
}

fn graph_models(activity: &[(i64, &str, u64)]) -> Vec<GraphModel> {
    let mut totals = BTreeMap::new();
    for (_, model, tokens) in activity {
        let total = totals.entry(*model).or_insert(0_u64);
        *total = total.saturating_add(*tokens);
    }
    let mut totals = totals.into_iter().collect::<Vec<_>>();
    totals.sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    let total = totals
        .iter()
        .fold(0_u64, |total, (_, tokens)| total.saturating_add(*tokens));
    let target = total.saturating_sub(total / 10);
    let mut covered = 0_u64;
    let mut models = Vec::new();
    let mut other = 0_u64;
    for (model, tokens) in totals {
        if models.len() < MAX_GRAPH_MODELS && covered < target {
            models.push(GraphModel {
                source: Some(model.to_owned()),
                label: model.to_owned(),
                color: model_color(model),
            });
            covered = covered.saturating_add(tokens);
        } else {
            other = other.saturating_add(tokens);
        }
    }
    if other != 0 {
        models.push(GraphModel {
            source: None,
            label: "Other".to_owned(),
            color: Color::DarkGray,
        });
    }
    models
}

fn model_color(model: &str) -> Color {
    let hash = model.bytes().fold(2_166_136_261_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(16_777_619)
    });
    GRAPH_COLORS[hash as usize % GRAPH_COLORS.len()]
}

fn graph_legend_lines(models: &[GraphModel], width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut line_width = 0_usize;
    for model in models {
        let item_width = model.label.chars().count().saturating_add(5);
        if line_width != 0 && line_width.saturating_add(item_width) > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            line_width = 0;
        }
        spans.push(Span::styled("  ", Style::default().bg(model.color)));
        spans.push(Span::raw(format!(" {}  ", model.label)));
        line_width = line_width.saturating_add(item_width);
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

struct StackedActivityChart<'a> {
    series: &'a ActivitySeries,
}

impl Widget for StackedActivityChart<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width == 0 || area.height < 2 || self.series.points.is_empty() {
            return;
        }
        let point_count = u16::try_from(self.series.points.len())
            .unwrap_or(u16::MAX)
            .max(1);
        let group_width = (area.width / point_count).max(1);
        let bar_width = group_width.saturating_sub(1).clamp(1, 7);
        let plot_height = area.height - 1;
        let plot_bottom = area.top().saturating_add(plot_height);
        let maximum = self
            .series
            .points
            .iter()
            .map(ActivityPoint::total)
            .max()
            .unwrap_or_default()
            .max(1);
        for (index, point) in self.series.points.iter().enumerate() {
            let group_x = area.left().saturating_add(
                u16::try_from(index)
                    .unwrap_or(u16::MAX)
                    .saturating_mul(group_width),
            );
            let bar_x = group_x.saturating_add(group_width.saturating_sub(bar_width) / 2);
            let mut cumulative = 0_u64;
            for (tokens, model) in point.tokens.iter().zip(&self.series.models) {
                let lower = scaled_bar_height(cumulative, maximum, plot_height);
                cumulative = cumulative.saturating_add(*tokens);
                let upper = scaled_bar_height(cumulative, maximum, plot_height);
                for height in lower..upper {
                    let y = plot_bottom.saturating_sub(height).saturating_sub(1);
                    for x in bar_x..bar_x.saturating_add(bar_width).min(area.right()) {
                        buffer[(x, y)]
                            .set_symbol(ratatui::symbols::block::FULL)
                            .set_fg(model.color);
                    }
                }
            }
            let label =
                activity_point_label(point.start, self.series.interval.millis, self.series.span);
            let label = truncate(&label, usize::from(group_width));
            let label_width = u16::try_from(Line::from(label.as_str()).width()).unwrap_or(u16::MAX);
            let label_x = group_x.saturating_add(group_width.saturating_sub(label_width) / 2);
            buffer.set_stringn(
                label_x,
                area.bottom().saturating_sub(1),
                label,
                usize::from(group_width),
                Style::default().fg(Color::DarkGray),
            );
        }
    }
}

fn scaled_bar_height(value: u64, maximum: u64, height: u16) -> u16 {
    let numerator = u128::from(value).saturating_mul(u128::from(height));
    u16::try_from(numerator.div_ceil(u128::from(maximum)))
        .unwrap_or(height)
        .min(height)
}

fn activity_point_label(timestamp: i64, interval: i64, span: i64) -> String {
    let Some(time) = DateTime::from_timestamp_millis(timestamp) else {
        return "unknown".to_owned();
    };
    let time = time.with_timezone(&Local);
    if interval < DAY_MILLIS && span <= DAY_MILLIS {
        time.format("%H:%M").to_string()
    } else if interval < DAY_MILLIS {
        time.format("%d %Hh").to_string()
    } else if interval < YEAR_MILLIS {
        time.format("%m-%d").to_string()
    } else {
        time.format("%Y").to_string()
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
    vec![
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
                .unwrap_or("not available"),
            received_max_chars,
        )),
    ]
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

    use chrono::{Local, NaiveDate, TimeZone};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Direction;
    use sfind::{ModelUsage, Provider, Session, SessionUsage, TokenUsage};

    use super::{
        activity_series, compact_path, day_range_label, day_range_start_date, delete_last_word,
        detail_cutoffs, directory_color, filter_width, format_number, fuzzy_match, list_index_at,
        list_item_capacity, list_title_capacity, list_viewport, next_day_range,
        next_provider_filter, padded_label, pane_direction, provider_short_label, search_cursor,
        search_rank, search_scroll, truncate, truncate_middle, usage_lines, usage_report,
        usage_report_with_costs, App, DayRange, Selection, UsageCostProvider, ViewMode,
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
    fn selected_provider_filters_sessions() {
        let session = |id: &str, provider: Provider| Session {
            provider,
            id: id.to_owned(),
            title: None,
            directory: None,
            updated_at: 0,
            first_user_message: "message".to_owned(),
            last_user_message: "message".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["message".to_owned()],
            usage: SessionUsage::default(),
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
            directory: None,
            updated_at: 0,
            first_user_message: "auth migration".to_owned(),
            last_user_message: "auth migration".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["auth migration".to_owned()],
            usage: SessionUsage::default(),
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
            usage: SessionUsage::default(),
        }];
        let mut app = App::new(&sessions, 0);

        let result = app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));

        assert_eq!(result, Some(Some(Selection::PrintContinueCommand(0))));
    }

    #[test]
    fn tab_cycles_actions_usage_and_graph_modes() {
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
            usage: SessionUsage::default(),
        }];
        let mut app = App::new(&sessions, 0);

        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.mode, ViewMode::Fork);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Some(Selection::Fork(0)))
        );
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Some(Selection::PrintForkCommand(0)))
        );

        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.mode, ViewMode::Usage);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            None
        );

        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.mode, ViewMode::Graph);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.mode, ViewMode::Continue);
    }

    #[test]
    fn usage_report_aggregates_only_visible_sessions_and_preserves_unknown_cost() {
        let sessions = [
            session_with_usage(
                "codex",
                Provider::Codex,
                0,
                "gpt-5.6-sol",
                TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 50,
                    ..TokenUsage::default()
                },
                None,
            ),
            session_with_usage(
                "open",
                Provider::OpenCode,
                0,
                "gpt-5.6-sol",
                TokenUsage {
                    input_tokens: 200,
                    output_tokens: 40,
                    cache_creation_tokens: 10,
                    ..TokenUsage::default()
                },
                Some(500_000),
            ),
        ];

        let all = usage_report(&sessions, &[0, 1]);
        let open_only = usage_report(&sessions, &[1]);

        assert_eq!(all.total.input_tokens, 300);
        assert_eq!(all.total.total_tokens(), 420);
        assert_eq!(all.cost_microusd, None);
        assert_eq!(open_only.total.total_tokens(), 250);
        assert_eq!(open_only.cost_microusd, Some(500_000));
        assert!(usage_lines(&all, 80)
            .iter()
            .any(|line| line.to_string().contains("Cost unknown")));
    }

    #[test]
    fn usage_cost_provider_can_fill_missing_recorded_costs() {
        struct FixedCost;

        impl UsageCostProvider for FixedCost {
            fn cost_microusd(&self, _provider: Provider, _usage: &ModelUsage) -> Option<u64> {
                Some(250_000)
            }
        }

        let sessions = [session_with_usage(
            "codex",
            Provider::Codex,
            0,
            "gpt",
            TokenUsage {
                input_tokens: 100,
                ..TokenUsage::default()
            },
            None,
        )];

        let report = usage_report_with_costs(&sessions, &[0], &FixedCost);

        assert_eq!(report.cost_microusd, Some(250_000));
    }

    #[test]
    fn activity_graph_uses_adaptive_buckets_and_session_activity_dates() {
        let first = Local
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("unambiguous local test time")
            .timestamp_millis();
        let last = Local
            .with_ymd_and_hms(2026, 1, 20, 12, 0, 0)
            .single()
            .expect("unambiguous local test time")
            .timestamp_millis();
        let sessions = [
            session_with_usage(
                "first",
                Provider::Codex,
                first,
                "gpt",
                TokenUsage {
                    input_tokens: 100,
                    ..TokenUsage::default()
                },
                None,
            ),
            session_with_usage(
                "last",
                Provider::OpenCode,
                last,
                "gpt",
                TokenUsage {
                    output_tokens: 50,
                    ..TokenUsage::default()
                },
                Some(0),
            ),
        ];

        let now = Local
            .with_ymd_and_hms(2026, 1, 20, 12, 0, 0)
            .single()
            .expect("unambiguous local test time");
        let series =
            activity_series(&sessions, &[0, 1], 70, DayRange::All, now).expect("usage series");

        assert_eq!(series.interval.label, "2 days");
        assert_eq!(series.total, 150);
        assert!(series.points.iter().any(|point| point.total() == 0));
    }

    #[test]
    fn today_graph_splits_elapsed_time_into_hour_ranges() {
        let now = Local
            .with_ymd_and_hms(2026, 7, 18, 18, 0, 0)
            .single()
            .expect("unambiguous local test time");
        let updated_at = Local
            .with_ymd_and_hms(2026, 7, 18, 16, 0, 0)
            .single()
            .expect("unambiguous local test time")
            .timestamp_millis();
        let sessions = [session_with_usage(
            "today",
            Provider::Codex,
            updated_at,
            "gpt",
            TokenUsage {
                input_tokens: 100,
                ..TokenUsage::default()
            },
            None,
        )];

        let series =
            activity_series(&sessions, &[0], 98, DayRange::Days(1), now).expect("usage series");

        assert_eq!(series.interval.label, "2 hours");
        assert_eq!(series.points.len(), 9);
        assert_eq!(
            series
                .points
                .iter()
                .filter(|point| point.total() != 0)
                .count(),
            1
        );
    }

    #[test]
    fn activity_graph_keeps_recent_data_when_years_exceed_terminal_capacity() {
        let sessions = (2020..=2026)
            .map(|year| {
                let updated_at = Local
                    .with_ymd_and_hms(year, 1, 1, 12, 0, 0)
                    .single()
                    .expect("unambiguous local test time")
                    .timestamp_millis();
                session_with_usage(
                    &year.to_string(),
                    Provider::Codex,
                    updated_at,
                    "gpt",
                    TokenUsage {
                        input_tokens: u64::try_from(year).expect("positive test year"),
                        ..TokenUsage::default()
                    },
                    None,
                )
            })
            .collect::<Vec<_>>();

        let now = Local
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("unambiguous local test time");
        let series = activity_series(
            &sessions,
            &(0..sessions.len()).collect::<Vec<_>>(),
            14,
            DayRange::All,
            now,
        )
        .expect("usage series");

        assert!(series.points.len() <= 2);
        assert!(series
            .points
            .last()
            .is_some_and(|point| point.total() >= 2026));
    }

    #[test]
    fn graph_collapses_models_outside_the_first_ninety_percent() {
        let now = Local
            .with_ymd_and_hms(2026, 7, 18, 18, 0, 0)
            .single()
            .expect("unambiguous local test time");
        let sessions = [
            ("major", 900),
            ("small-a", 50),
            ("small-b", 30),
            ("small-c", 20),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (model, tokens))| {
            session_with_usage(
                &index.to_string(),
                Provider::OpenCode,
                now.timestamp_millis(),
                model,
                TokenUsage {
                    input_tokens: tokens,
                    ..TokenUsage::default()
                },
                Some(0),
            )
        })
        .collect::<Vec<_>>();

        let series = activity_series(
            &sessions,
            &(0..sessions.len()).collect::<Vec<_>>(),
            98,
            DayRange::Days(1),
            now,
        )
        .expect("usage series");

        assert_eq!(
            series
                .models
                .iter()
                .map(|model| model.label.as_str())
                .collect::<Vec<_>>(),
            ["major", "Other"]
        );
        assert_eq!(
            series.points.iter().map(|point| point.total()).sum::<u64>(),
            1_000
        );
    }

    #[test]
    fn token_counts_use_grouped_decimal_formatting() {
        assert_eq!(format_number(128_142_674), "128,142,674");
    }

    fn session_with_usage(
        id: &str,
        provider: Provider,
        updated_at: i64,
        model: &str,
        tokens: TokenUsage,
        cost: Option<u64>,
    ) -> Session {
        let mut usage = SessionUsage::default();
        usage.add(Some(model), tokens, cost);
        Session {
            provider,
            id: id.to_owned(),
            title: None,
            directory: None,
            updated_at,
            first_user_message: "first".to_owned(),
            last_user_message: "last".to_owned(),
            last_assistant_message: None,
            user_messages: vec!["first".to_owned()],
            usage,
        }
    }
}
