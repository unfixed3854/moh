use std::{cmp::Ordering, collections::BTreeMap, fmt};

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
use moh::session::{SessionId, SessionSummary, SessionTitle};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};

use super::{EditorOutcome, PromptEditor, fuzzy_subsequence_score, sanitize_line};

const WHEEL_SELECTABLE_ROWS: usize = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::client) enum BrowserMode {
    #[default]
    Project,
    Global,
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::client) enum BrowserAction {
    None,
    Refresh,
    Switch(SessionId),
    Rename {
        session_id: SessionId,
        title: String,
    },
    Delete(SessionId),
}

#[derive(Default)]
pub(in crate::client) enum BrowserLayer {
    #[default]
    List,
    Rename {
        session_id: SessionId,
        editor: PromptEditor,
        error: Option<String>,
    },
    ConfirmDelete {
        session_id: SessionId,
        title: SessionTitle,
    },
}

impl fmt::Debug for BrowserLayer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::List => formatter.write_str("List"),
            Self::Rename {
                session_id,
                editor,
                error,
            } => formatter
                .debug_struct("Rename")
                .field("session_id", session_id)
                .field("editor", &editor.value())
                .field("error", error)
                .finish(),
            Self::ConfirmDelete { session_id, title } => formatter
                .debug_struct("ConfirmDelete")
                .field("session_id", session_id)
                .field("title", title)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::client) enum BrowserRow {
    Group { cwd: Vec<u8>, cwd_display: String },
    Session(SessionSummary),
}

impl BrowserRow {
    pub(in crate::client) const fn session_id(&self) -> Option<SessionId> {
        match self {
            Self::Group { .. } => None,
            Self::Session(summary) => Some(summary.id),
        }
    }

    fn summary(&self) -> Option<&SessionSummary> {
        match self {
            Self::Group { .. } => None,
            Self::Session(summary) => Some(summary),
        }
    }
}

pub(in crate::client) struct SessionBrowserState {
    open: bool,
    mode: BrowserMode,
    query: PromptEditor,
    sessions: Vec<SessionSummary>,
    visible: Vec<BrowserRow>,
    selected: usize,
    selected_id: Option<SessionId>,
    offset: usize,
    layer: BrowserLayer,
    refresh_warning: Option<String>,
    action_error: Option<String>,
    current_cwd: Vec<u8>,
    viewport_rows: usize,
    refresh_requested: bool,
}

impl Default for SessionBrowserState {
    fn default() -> Self {
        Self {
            open: false,
            mode: BrowserMode::Project,
            query: PromptEditor::new(),
            sessions: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            selected_id: None,
            offset: 0,
            layer: BrowserLayer::List,
            refresh_warning: None,
            action_error: None,
            current_cwd: Vec::new(),
            viewport_rows: 1,
            refresh_requested: false,
        }
    }
}

impl SessionBrowserState {
    pub(in crate::client) fn open(&mut self) {
        self.open = true;
        self.mode = BrowserMode::Project;
        self.query.clear();
        self.selected = 0;
        self.selected_id = None;
        self.offset = 0;
        self.layer = BrowserLayer::List;
        self.refresh_warning = None;
        self.action_error = None;
        self.refresh_requested = true;
        self.rebuild_visible();
    }

    pub(in crate::client) fn close(&mut self) {
        self.open = false;
        self.refresh_requested = false;
        self.action_error = None;
    }

    pub(in crate::client) const fn is_open(&self) -> bool {
        self.open
    }

    pub(in crate::client) const fn mode(&self) -> BrowserMode {
        self.mode
    }

    pub(in crate::client) const fn layer(&self) -> &BrowserLayer {
        &self.layer
    }

    #[cfg(test)]
    pub(in crate::client) const fn query(&self) -> &PromptEditor {
        &self.query
    }

    #[cfg(test)]
    pub(in crate::client) fn set_query(&mut self, query: impl Into<String>) {
        self.query.set_value(query);
        self.rebuild_visible();
    }

    pub(in crate::client) fn set_sessions(
        &mut self,
        current_cwd: &[u8],
        sessions: Vec<SessionSummary>,
    ) {
        self.current_cwd = current_cwd.to_vec();
        self.sessions = sessions;
        self.refresh_warning = None;
        self.rebuild_visible();
    }

    pub(in crate::client) fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            BrowserMode::Project => BrowserMode::Global,
            BrowserMode::Global => BrowserMode::Project,
        };
        self.refresh_requested = true;
        self.rebuild_visible();
    }

    pub(in crate::client) fn take_refresh_request(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    pub(in crate::client) fn visible_rows(&self) -> &[BrowserRow] {
        &self.visible
    }

    pub(in crate::client) const fn selected_id(&self) -> Option<SessionId> {
        self.selected_id
    }

    pub(in crate::client) fn selected_summary(&self) -> Option<&SessionSummary> {
        self.visible.get(self.selected)?.summary()
    }

    pub(in crate::client) const fn selected(&self) -> usize {
        self.selected
    }

    pub(in crate::client) const fn offset(&self) -> usize {
        self.offset
    }

    pub(in crate::client) fn set_viewport_rows(&mut self, viewport_rows: usize) {
        self.viewport_rows = viewport_rows.max(1);
        self.ensure_selection_visible();
    }

    pub(in crate::client) fn select_next(&mut self) {
        if self.selected_id.is_none() {
            return;
        }
        let Some(next) = self.visible[self.selected.saturating_add(1)..]
            .iter()
            .position(|row| row.session_id().is_some())
            .map(|relative| self.selected + relative + 1)
        else {
            return;
        };
        self.select(next);
    }

    pub(in crate::client) fn select_previous(&mut self) {
        let Some(previous) = self.visible[..self.selected]
            .iter()
            .rposition(|row| row.session_id().is_some())
        else {
            return;
        };
        self.select(previous);
    }

    pub(in crate::client) fn page_down(&mut self) {
        if self.selected_id.is_none() {
            return;
        }
        let target = self
            .selected
            .saturating_add(self.viewport_rows)
            .min(self.visible.len().saturating_sub(1));
        let next = self.visible[target..]
            .iter()
            .position(|row| row.session_id().is_some())
            .map(|relative| target + relative)
            .or_else(|| {
                self.visible[..target]
                    .iter()
                    .rposition(|row| row.session_id().is_some())
            });
        if let Some(next) = next {
            self.select(next);
        }
    }

    pub(in crate::client) fn page_up(&mut self) {
        if self.selected_id.is_none() {
            return;
        }
        let target = self.selected.saturating_sub(self.viewport_rows);
        let previous = self.visible[..=target]
            .iter()
            .rposition(|row| row.session_id().is_some())
            .or_else(|| {
                self.visible[target..]
                    .iter()
                    .position(|row| row.session_id().is_some())
                    .map(|relative| target + relative)
            });
        if let Some(previous) = previous {
            self.select(previous);
        }
    }

    pub(in crate::client) fn wheel_down(&mut self) {
        self.move_selectable(WHEEL_SELECTABLE_ROWS, true);
    }

    pub(in crate::client) fn wheel_up(&mut self) {
        self.move_selectable(WHEEL_SELECTABLE_ROWS, false);
    }

    pub(in crate::client) fn start_rename(&mut self) {
        if !matches!(self.layer, BrowserLayer::List) {
            return;
        }
        let Some(summary) = self.selected_summary() else {
            return;
        };
        let session_id = summary.id;
        let title = summary.title.to_string();
        let mut editor = PromptEditor::new();
        editor.set_value(title);
        self.layer = BrowserLayer::Rename {
            session_id,
            editor,
            error: None,
        };
    }

    pub(in crate::client) fn start_delete_confirmation(&mut self) {
        if !matches!(self.layer, BrowserLayer::List) {
            return;
        }
        let Some(summary) = self.selected_summary() else {
            return;
        };
        self.layer = BrowserLayer::ConfirmDelete {
            session_id: summary.id,
            title: summary.title.clone(),
        };
    }

    pub(in crate::client) fn escape(&mut self) {
        match self.layer {
            BrowserLayer::List => self.close(),
            BrowserLayer::Rename { .. } | BrowserLayer::ConfirmDelete { .. } => {
                self.layer = BrowserLayer::List;
            }
        }
    }

    pub(in crate::client) fn set_refresh_warning(&mut self, warning: impl Into<String>) {
        self.refresh_warning = Some(warning.into());
    }

    pub(in crate::client) fn set_action_error(&mut self, error: impl Into<String>) {
        self.action_error = Some(error.into());
    }

    pub(in crate::client) fn finish_rename(&mut self) {
        if matches!(self.layer, BrowserLayer::Rename { .. }) {
            self.layer = BrowserLayer::List;
            self.refresh_requested = true;
        }
    }

    pub(in crate::client) fn set_rename_error(&mut self, message: impl Into<String>) {
        if let BrowserLayer::Rename { error, .. } = &mut self.layer {
            *error = Some(message.into());
        }
    }

    pub(in crate::client) fn finish_delete(&mut self, session_id: SessionId) {
        self.sessions.retain(|summary| summary.id != session_id);
        self.layer = BrowserLayer::List;
        self.refresh_requested = true;
        self.rebuild_visible();
    }

    #[cfg(test)]
    pub(in crate::client) fn warning(&self) -> Option<&str> {
        self.action_error
            .as_deref()
            .or(self.refresh_warning.as_deref())
    }

    pub(in crate::client) fn refresh_warning(&self) -> Option<&str> {
        self.refresh_warning.as_deref()
    }

    pub(in crate::client) fn action_error(&self) -> Option<&str> {
        self.action_error.as_deref()
    }

    fn message_count(&self) -> u16 {
        u16::from(self.action_error.is_some()) + u16::from(self.refresh_warning.is_some())
    }

    pub(in crate::client) fn handle_event(&mut self, event: &Event) -> BrowserAction {
        let explicit_input = matches!(event, Event::Key(_) | Event::Paste(_))
            || matches!(
                event,
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    )
            );
        if explicit_input {
            self.action_error = None;
        }
        match self.layer {
            BrowserLayer::List => self.handle_list_event(event),
            BrowserLayer::Rename { .. } => self.handle_rename_event(event),
            BrowserLayer::ConfirmDelete { .. } => self.handle_confirmation_event(event),
        }
    }

    fn handle_list_event(&mut self, event: &Event) -> BrowserAction {
        match event {
            Event::Key(key) if key.modifiers == KeyModifiers::NONE => match key.code {
                KeyCode::Esc => self.escape(),
                KeyCode::Tab => {
                    self.toggle_mode();
                    return BrowserAction::Refresh;
                }
                KeyCode::Up => self.select_previous(),
                KeyCode::Down => self.select_next(),
                KeyCode::PageUp => self.page_up(),
                KeyCode::PageDown => self.page_down(),
                KeyCode::Enter => {
                    return self
                        .selected_id
                        .map_or(BrowserAction::None, BrowserAction::Switch);
                }
                KeyCode::F(2) => self.start_rename(),
                _ => return self.edit_query(event),
            },
            Event::Key(key)
                if key.modifiers == KeyModifiers::CONTROL
                    && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&'d')) =>
            {
                self.start_delete_confirmation();
            }
            Event::Key(_) => return self.edit_query(event),
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => self.wheel_up(),
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => self.wheel_down(),
            Event::Paste(_) => return self.edit_query(event),
            _ => {}
        }
        BrowserAction::None
    }

    fn edit_query(&mut self, event: &Event) -> BrowserAction {
        if matches!(self.query.handle_event(event), EditorOutcome::Changed) {
            self.rebuild_visible();
        }
        BrowserAction::None
    }

    fn handle_rename_event(&mut self, event: &Event) -> BrowserAction {
        if matches!(
            event,
            Event::Key(key)
                if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Esc
        ) {
            self.escape();
            return BrowserAction::None;
        }
        if matches!(
            event,
            Event::Key(key)
                if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Enter
        ) {
            let BrowserLayer::Rename {
                session_id, editor, ..
            } = &self.layer
            else {
                unreachable!();
            };
            return BrowserAction::Rename {
                session_id: *session_id,
                title: editor.value().to_owned(),
            };
        }
        let BrowserLayer::Rename { editor, .. } = &mut self.layer else {
            unreachable!();
        };
        let _ = editor.handle_event(event);
        BrowserAction::None
    }

    fn handle_confirmation_event(&mut self, event: &Event) -> BrowserAction {
        let Event::Key(key) = event else {
            return BrowserAction::None;
        };
        if key.modifiers != KeyModifiers::NONE {
            return BrowserAction::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.escape();
                BrowserAction::None
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                let BrowserLayer::ConfirmDelete { session_id, .. } = self.layer else {
                    unreachable!();
                };
                BrowserAction::Delete(session_id)
            }
            _ => BrowserAction::None,
        }
    }

    fn rebuild_visible(&mut self) {
        let query = normalize_fuzzy_text(self.query.value());
        let mut sessions = self
            .sessions
            .iter()
            .filter(|summary| {
                (self.mode == BrowserMode::Global || summary.cwd == self.current_cwd)
                    && summary_matches(summary, &query)
            })
            .cloned()
            .collect::<Vec<_>>();

        let visible = match self.mode {
            BrowserMode::Project => {
                sessions.sort_by(compare_sessions);
                sessions.into_iter().map(BrowserRow::Session).collect()
            }
            BrowserMode::Global => self.global_rows(sessions),
        };
        self.visible = visible;

        let selected = self
            .selected_id
            .and_then(|selected_id| {
                self.visible
                    .iter()
                    .position(|row| row.session_id() == Some(selected_id))
            })
            .or_else(|| {
                self.visible
                    .iter()
                    .position(|row| row.session_id().is_some())
            });
        if let Some(selected) = selected {
            self.selected = selected;
            self.selected_id = self.visible[selected].session_id();
        } else {
            self.selected = 0;
            self.selected_id = None;
        }
        self.ensure_selection_visible();
    }

    fn global_rows(&self, sessions: Vec<SessionSummary>) -> Vec<BrowserRow> {
        let mut grouped = BTreeMap::<Vec<u8>, Vec<SessionSummary>>::new();
        for summary in sessions {
            grouped
                .entry(summary.cwd.clone())
                .or_default()
                .push(summary);
        }
        let mut groups = grouped
            .into_iter()
            .map(|(cwd, mut sessions)| {
                sessions.sort_by(compare_sessions);
                let cwd_display = sessions
                    .first()
                    .map_or_else(String::new, |summary| summary.cwd_display.clone());
                (cwd, cwd_display, sessions)
            })
            .collect::<Vec<_>>();
        groups.sort_by(|left, right| {
            let left_is_current = left.0 == self.current_cwd;
            let right_is_current = right.0 == self.current_cwd;
            match (left_is_current, right_is_current) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => compare_sessions(&left.2[0], &right.2[0]),
            }
        });

        groups
            .into_iter()
            .flat_map(|(cwd, cwd_display, sessions)| {
                std::iter::once(BrowserRow::Group { cwd, cwd_display })
                    .chain(sessions.into_iter().map(BrowserRow::Session))
            })
            .collect()
    }

    fn move_selectable(&mut self, count: usize, forward: bool) {
        let selectable = self
            .visible
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.session_id().map(|_| index))
            .collect::<Vec<_>>();
        let Some(position) = selectable.iter().position(|index| *index == self.selected) else {
            return;
        };
        let target = if forward {
            position
                .saturating_add(count)
                .min(selectable.len().saturating_sub(1))
        } else {
            position.saturating_sub(count)
        };
        self.select(selectable[target]);
    }

    fn select(&mut self, index: usize) {
        let Some(session_id) = self.visible.get(index).and_then(BrowserRow::session_id) else {
            return;
        };
        self.selected = index;
        self.selected_id = Some(session_id);
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        let max_offset = self.visible.len().saturating_sub(self.viewport_rows);
        self.offset = self.offset.min(max_offset);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset.saturating_add(self.viewport_rows) {
            self.offset = self.selected + 1 - self.viewport_rows;
        }
        self.offset = self.offset.min(max_offset);
    }
}

pub(in crate::client) fn render_session_browser(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    browser: &mut SessionBrowserState,
    current_session_id: Option<SessionId>,
) {
    if !browser.is_open() {
        return;
    }

    let popup = browser_rect(area, browser);
    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title("sessions");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    let message_height = browser.message_count();
    let [tabs_area, query_area, message_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(message_height),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let selected_tab = match browser.mode() {
        BrowserMode::Project => 0,
        BrowserMode::Global => 1,
    };
    frame.render_widget(
        Tabs::new(["Local", "Global"])
            .select(selected_tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        tabs_area,
    );
    render_query(frame, query_area, browser);
    let mut messages = Vec::new();
    if let Some(error) = browser.action_error() {
        messages.push(Line::styled(
            format!("Error: {}", sanitize_line(error)),
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(warning) = browser.refresh_warning() {
        messages.push(Line::styled(
            format!("Warning: {}", sanitize_line(warning)),
            Style::default().fg(Color::Yellow),
        ));
    }
    if !messages.is_empty() {
        frame.render_widget(Paragraph::new(messages), message_area);
    }

    let footer = match browser.layer() {
        BrowserLayer::List => {
            render_session_list(frame, body_area, browser, current_session_id);
            "Tab mode · ↑/↓ select · PgUp/PgDn page · Enter switch · F2 rename · Ctrl+D delete · Esc close"
        }
        BrowserLayer::Rename { .. } => {
            render_rename(frame, body_area, browser);
            "Enter rename · Esc return"
        }
        BrowserLayer::ConfirmDelete { .. } => {
            render_delete_confirmation(frame, body_area, browser);
            "y/Enter delete · n/Esc return"
        }
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().add_modifier(Modifier::DIM)),
        footer_area,
    );
}

fn browser_rect(area: Rect, browser: &SessionBrowserState) -> Rect {
    let available = if area.width > 2 && area.height > 2 {
        Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        }
    } else {
        area
    };
    let width = available.width.clamp(1, 88);
    let body_height = match browser.layer() {
        BrowserLayer::List => browser.visible_rows().len().clamp(4, 16) as u16,
        BrowserLayer::Rename { .. } => 5,
        BrowserLayer::ConfirmDelete { session_id, title } => {
            let content_width = width.saturating_sub(4).max(1);
            let lines = Paragraph::new(delete_confirmation_text(*session_id, title))
                .wrap(Wrap { trim: false })
                .line_count(content_width);
            u16::try_from(lines).unwrap_or(u16::MAX).saturating_add(2)
        }
    };
    let height = body_height
        .saturating_add(5)
        .saturating_add(browser.message_count())
        .min(available.height)
        .max(1);
    Rect {
        x: available
            .x
            .saturating_add(available.width.saturating_sub(width) / 2),
        y: available
            .y
            .saturating_add(available.height.saturating_sub(height) / 2),
        width,
        height,
    }
}

fn render_query(frame: &mut ratatui::Frame<'_>, area: Rect, browser: &mut SessionBrowserState) {
    if area.is_empty() {
        return;
    }
    const PREFIX: &str = "Filter: ";
    let window = browser
        .query
        .display_window(area.width.saturating_sub(PREFIX.len() as u16));
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(PREFIX), Span::raw(window.text)])),
        area,
    );
    if matches!(browser.layer(), BrowserLayer::List) {
        frame.set_cursor_position((
            area.x
                .saturating_add(PREFIX.len() as u16)
                .saturating_add(window.cursor_column)
                .min(area.right().saturating_sub(1)),
            area.y,
        ));
    }
}

fn render_session_list(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    browser: &mut SessionBrowserState,
    current_session_id: Option<SessionId>,
) {
    browser.set_viewport_rows(usize::from(area.height));
    let items = if browser.visible_rows().is_empty() {
        vec![ListItem::new("No sessions match the filter.")]
    } else {
        browser
            .visible_rows()
            .iter()
            .map(|row| match row {
                BrowserRow::Group { cwd_display, .. } => ListItem::new(Line::styled(
                    sanitize_line(cwd_display),
                    Style::default().add_modifier(Modifier::DIM),
                )),
                BrowserRow::Session(summary) => {
                    ListItem::new(session_row(summary, current_session_id))
                }
            })
            .collect()
    };
    let list =
        List::new(items).highlight_style(Style::default().bg(Color::DarkGray).fg(Color::Cyan));
    let selected = browser.selected_id().map(|_| browser.selected());
    let mut list_state = ListState::default()
        .with_offset(browser.offset())
        .with_selected(selected);
    frame.render_stateful_widget(list, area, &mut list_state);
    browser.offset = list_state.offset();
}

fn session_row(summary: &SessionSummary, current_session_id: Option<SessionId>) -> Line<'static> {
    let mut markers = Vec::new();
    if current_session_id == Some(summary.id) {
        markers.push("[current]".to_owned());
    }
    if summary.busy {
        markers.push("[running]".to_owned());
    }
    markers.push(format!("jobs:{}", summary.running_jobs));
    markers.push(format!("clients:{}", summary.attached_clients));
    Line::from(format!(
        "{} {} · {} · {}",
        markers.join(" "),
        sanitize_line(summary.title.as_str()),
        summary.id,
        summary.last_activity.format("%Y-%m-%d %H:%MZ")
    ))
}

fn render_rename(frame: &mut ratatui::Frame<'_>, area: Rect, browser: &mut SessionBrowserState) {
    let BrowserLayer::Rename {
        session_id,
        editor,
        error,
    } = &mut browser.layer
    else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("Rename {session_id}"));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    const PREFIX: &str = "Title: ";
    let window = editor.display_window(inner.width.saturating_sub(PREFIX.len() as u16));
    let mut lines = vec![Line::from(vec![Span::raw(PREFIX), Span::raw(window.text)])];
    if let Some(error) = error {
        lines.push(Line::styled(
            sanitize_line(error),
            Style::default().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    frame.set_cursor_position((
        inner
            .x
            .saturating_add(PREFIX.len() as u16)
            .saturating_add(window.cursor_column)
            .min(inner.right().saturating_sub(1)),
        inner.y,
    ));
}

fn render_delete_confirmation(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    browser: &SessionBrowserState,
) {
    let BrowserLayer::ConfirmDelete { session_id, title } = browser.layer() else {
        return;
    };
    frame.render_widget(
        Paragraph::new(delete_confirmation_text(*session_id, title))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("confirm delete"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn delete_confirmation_text(session_id: SessionId, title: &SessionTitle) -> String {
    format!(
        "Delete {} ({session_id})?\n\nThis is permanent.\nThe active model run will be cancelled; background jobs terminated.\nAll attached clients will be disconnected.",
        sanitize_line(title.as_str())
    )
}

fn compare_sessions(left: &SessionSummary, right: &SessionSummary) -> Ordering {
    right
        .last_activity
        .cmp(&left.last_activity)
        .then_with(|| right.id.cmp(&left.id))
}

fn normalize_fuzzy_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn summary_matches(summary: &SessionSummary, query: &str) -> bool {
    query.is_empty()
        || [
            summary.title.as_str().to_owned(),
            summary.id.to_string(),
            summary.cwd_display.clone(),
        ]
        .into_iter()
        .map(|candidate| normalize_fuzzy_text(&candidate))
        .any(|candidate| fuzzy_subsequence_score(query, &candidate).is_some())
}
