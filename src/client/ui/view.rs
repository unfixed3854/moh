use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::Path};

use moh::{
    session::{ModelCatalogState, PlanStatus, SessionSnapshot, TranscriptItem},
    tools::{JobState, ReadArgs},
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use crate::client::ChatProjection;

use super::{
    MenuKind, PopupKind, UiState, markdown::markdown_text, sanitize_line,
    session_browser::render_session_browser,
};

const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 3;
const MAX_PROMPT_HEIGHT: u16 = 4;
const STATUS_HEIGHT: u16 = 1;
const CONTEXT_WINDOW_TOKENS: u64 = 256_000;
const HELP_WIDE_MIN_WIDTH: u16 = 72;
const HELP_WIDE_MIN_HEIGHT: u16 = 16;
const HELP_NARROW_MIN_HEIGHT: u16 = 22;
const SIDEBAR_WIDTH: u16 = 42;
const WIDE_SIDEBAR_MIN_WIDTH: u16 = 121;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HelpLayout {
    Wide,
    Narrow,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranscriptKind {
    Plain,
    User,
    Assistant,
    Activity,
}

pub(super) struct TranscriptEntry {
    kind: TranscriptKind,
    text: Text<'static>,
    trailing_space: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptStatus {
    Thinking,
    Ready,
    Error,
}

pub(in crate::client) fn render(
    frame: &mut ratatui::Frame<'_>,
    projection: &ChatProjection,
    ui: &mut UiState,
) {
    let area = frame.area();
    ui.record_frame_width(area.width);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(Paragraph::new("terminal too small (minimum 20×3)"), area);
        return;
    }

    if ui.sidebar_visible(area.width) && area.width >= WIDE_SIDEBAR_MIN_WIDTH {
        let [main_area, sidebar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)]).areas(area);
        render_main(frame, main_area, main_area.width, projection, ui);
        render_sidebar(frame, sidebar_area, projection);
    } else if ui.sidebar_visible(area.width) {
        let width = area.width.min(SIDEBAR_WIDTH);
        let sidebar_area = Rect {
            x: area.right().saturating_sub(width),
            width,
            ..area
        };
        render_main(
            frame,
            area,
            sidebar_area.x.saturating_sub(area.x),
            projection,
            ui,
        );
        frame.render_widget(Clear, sidebar_area);
        render_sidebar(frame, sidebar_area, projection);
        render_modal_overlay(frame, area, projection, ui);
    } else {
        render_main(frame, area, area.width, projection, ui);
    }
}

fn render_main(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    input_width: u16,
    projection: &ChatProjection,
    ui: &mut UiState,
) {
    let input_area = Rect {
        width: input_width.min(area.width),
        ..area
    };
    let prompt_height = prompt_height(input_area, ui);
    let [transcript_area, mut prompt_area, mut status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(prompt_height),
        Constraint::Length(STATUS_HEIGHT),
    ])
    .areas(area);
    prompt_area.width = input_area.width;
    status_area.width = input_area.width;

    render_transcript(frame, transcript_area, projection, ui);
    if !prompt_area.is_empty() {
        render_prompt(frame, prompt_area, ui);
    }
    frame.render_widget(Paragraph::new(status_line(projection, ui)), status_area);
    render_popups(frame, area, prompt_area, projection, ui);
}

fn render_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, projection: &ChatProjection) {
    if area.width < 5 || area.height < 3 {
        return;
    }
    let panel = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);
    let content = Rect {
        x: inner.x.saturating_add(2),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(4),
        height: inner.height.saturating_sub(2),
    };
    if content.is_empty() {
        return;
    }
    let [title_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(content);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "Todo",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        title_area,
    );
    if list_area.is_empty() {
        return;
    }
    let plan = match projection {
        ChatProjection::Draft(_) => &[][..],
        ChatProjection::Session(snapshot) => snapshot.plan.as_slice(),
    };
    if plan.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No todos yet", Style::default().add_modifier(Modifier::DIM)),
                Line::styled(
                    "Tasks appear here as work is planned.",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ])
            .wrap(Wrap { trim: true }),
            list_area,
        );
        return;
    }
    render_todo_items(frame, list_area, plan);
}

fn render_todo_items(frame: &mut ratatui::Frame<'_>, area: Rect, plan: &[moh::session::PlanItem]) {
    const MARKER_WIDTH: u16 = 2;
    let text_width = area.width.saturating_sub(MARKER_WIDTH);
    if text_width == 0 {
        return;
    }

    let mut y = area.y;
    for (index, item) in plan.iter().enumerate() {
        let remaining_height = area.bottom().saturating_sub(y);
        let remaining_items = plan.len().saturating_sub(index + 1);
        let footer_height = u16::from(remaining_items > 0);
        let available_height = remaining_height.saturating_sub(footer_height);
        if available_height == 0 {
            render_more_todos(frame, area, y, plan.len() - index);
            return;
        }

        let (marker, marker_style) = plan_marker(item.status());
        let text = Paragraph::new(sanitize_line(item.step())).wrap(Wrap { trim: true });
        let desired_height = u16::try_from(text.line_count(text_width))
            .unwrap_or(u16::MAX)
            .max(1);
        let item_height = desired_height.min(available_height);
        frame.render_widget(
            Paragraph::new(Span::styled(format!("{marker} "), marker_style)),
            Rect {
                x: area.x,
                y,
                width: MARKER_WIDTH,
                height: 1,
            },
        );
        frame.render_widget(
            text,
            Rect {
                x: area.x.saturating_add(MARKER_WIDTH),
                y,
                width: text_width,
                height: item_height,
            },
        );
        y = y.saturating_add(item_height);
        if item_height < desired_height {
            if y < area.bottom() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        "… more",
                        Style::default().add_modifier(Modifier::DIM),
                    )),
                    Rect {
                        y,
                        height: 1,
                        ..area
                    },
                );
            }
            return;
        }
    }
}

fn render_more_todos(frame: &mut ratatui::Frame<'_>, area: Rect, y: u16, count: usize) {
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("… {count} more"),
            Style::default().add_modifier(Modifier::DIM),
        )),
        Rect {
            y,
            height: 1,
            ..area
        },
    );
}

fn plan_marker(status: PlanStatus) -> (&'static str, Style) {
    match status {
        PlanStatus::Pending => ("○", Style::default().fg(Color::DarkGray)),
        PlanStatus::InProgress => (
            "▶",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        PlanStatus::Completed => ("✓", Style::default().fg(Color::Green)),
        PlanStatus::Blocked => (
            "!",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        PlanStatus::Cancelled => ("–", Style::default().add_modifier(Modifier::DIM)),
    }
}

fn prompt_height(area: Rect, ui: &UiState) -> u16 {
    let maximum = area
        .height
        .saturating_sub(STATUS_HEIGHT)
        .saturating_sub(1)
        .max(1);
    ui.editor()
        .visual_height(area.width.saturating_sub(2))
        .clamp(1, MAX_PROMPT_HEIGHT.min(maximum))
}

pub(super) fn transcript_entries(
    projection: &ChatProjection,
    ui: &UiState,
) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    if let ChatProjection::Session(snapshot) = projection {
        if let ModelCatalogState::Failed(error) = &snapshot.catalog {
            entries.push(plain_entry(
                format!("Model selection is unavailable: {}.", sanitize_line(error)),
                true,
            ));
        }
        if let Some(warning) = &snapshot.persistence_warning {
            entries.push(plain_entry(
                format!("Session persistence warning: {}.", sanitize_line(warning)),
                true,
            ));
        }

        for item in &snapshot.transcript {
            match item {
                TranscriptItem::User(text) => entries.push(TranscriptEntry {
                    kind: TranscriptKind::User,
                    text: markdown_text(text),
                    trailing_space: true,
                }),
                TranscriptItem::Assistant(text) => entries.push(TranscriptEntry {
                    kind: TranscriptKind::Assistant,
                    text: markdown_text(text),
                    trailing_space: true,
                }),
                TranscriptItem::ToolStarted {
                    name, arguments, ..
                } => entries.push(TranscriptEntry {
                    kind: TranscriptKind::Activity,
                    text: Text::from(format_tool_started(name, arguments, snapshot)),
                    trailing_space: false,
                }),
                TranscriptItem::Failed { failure, .. } => {
                    entries.push(plain_entry(&failure.message, true));
                }
                TranscriptItem::Cancelled { .. } => {}
            }
        }

        if let Some(active) = &snapshot.active_run
            && !active.assistant_text.is_empty()
        {
            entries.push(TranscriptEntry {
                kind: TranscriptKind::Assistant,
                text: markdown_text(&active.assistant_text),
                trailing_space: true,
            });
        }
    }
    entries.extend(ui.notices().iter().map(|notice| plain_entry(notice, true)));
    entries
}

fn plain_entry(message: impl AsRef<str>, trailing_space: bool) -> TranscriptEntry {
    TranscriptEntry {
        kind: TranscriptKind::Plain,
        text: Text::from(sanitize_line(message.as_ref())),
        trailing_space,
    }
}

fn format_tool_started(
    name: &str,
    arguments: &serde_json::Value,
    snapshot: &SessionSnapshot,
) -> String {
    if name == "read"
        && let Ok(read) = serde_json::from_value::<ReadArgs>(arguments.clone())
    {
        return format_read_activity(&read.path, read.offset, read.limit, snapshot);
    }
    format!("Running {}", sanitize_line(name))
}

fn format_read_activity(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    snapshot: &SessionSnapshot,
) -> String {
    let path = Path::new(path);
    let cwd = OsString::from_vec(snapshot.summary.cwd.clone());
    let path = path
        .strip_prefix(Path::new(&cwd))
        .unwrap_or(path)
        .to_string_lossy();
    let range = match (offset, limit) {
        (Some(offset), Some(limit)) => {
            let end = offset.saturating_add(limit.saturating_sub(1));
            format!(" · lines {offset}–{end}")
        }
        (Some(offset), None) => format!(" · from line {offset}"),
        (None, Some(limit)) => format!(" · first {limit} lines"),
        (None, None) => String::new(),
    };
    format!("Read {}{range}", sanitize_line(&path))
}

fn render_transcript(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    projection: &ChatProjection,
    ui: &mut UiState,
) {
    if area.is_empty() {
        return;
    }
    let entries = transcript_entries(projection, ui);
    let chat_is_empty = match projection {
        ChatProjection::Draft(_) => true,
        ChatProjection::Session(snapshot) => {
            snapshot.transcript.is_empty() && snapshot.active_run.is_none()
        }
    };
    if !ui.welcome_dismissed() && chat_is_empty {
        let notice_content_height = entry_heights(&entries, area.width)
            .into_iter()
            .sum::<usize>();
        if notice_content_height >= usize::from(area.height) {
            render_entries(frame, area, &entries, ui);
            return;
        }
        let notice_height = notice_content_height as u16;
        let welcome_area = Rect {
            height: area.height.saturating_sub(notice_height),
            ..area
        };
        render_welcome(frame, welcome_area);
        if notice_height == 0 {
            ui.scroll_mut().update_metrics(0, area.height);
        } else {
            render_entries(
                frame,
                Rect {
                    y: welcome_area.bottom(),
                    height: notice_height,
                    ..area
                },
                &entries,
                ui,
            );
        }
        return;
    }
    render_entries(frame, area, &entries, ui);
}

fn render_entries(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    entries: &[TranscriptEntry],
    ui: &mut UiState,
) {
    if area.is_empty() {
        return;
    }
    let mut content_width = area.width;
    let mut heights = entry_heights(entries, content_width);
    let mut content_height = heights.iter().sum::<usize>();
    let needs_scrollbar = area.width > 1 && content_height > usize::from(area.height);
    if needs_scrollbar {
        content_width = content_width.saturating_sub(1);
        heights = entry_heights(entries, content_width);
        content_height = heights.iter().sum();
    }
    ui.scroll_mut().update_metrics(content_height, area.height);
    let content_area = Rect {
        width: content_width,
        ..area
    };
    let visible_top = ui.scroll().top();
    let visible_bottom = visible_top.saturating_add(usize::from(area.height));
    let mut entry_top = 0_usize;
    for (entry, height) in entries.iter().zip(heights) {
        let entry_bottom = entry_top.saturating_add(height);
        let clipped_top = entry_top.max(visible_top);
        let clipped_bottom = entry_bottom.min(visible_bottom);
        if clipped_top < clipped_bottom {
            render_entry_slice(
                frame,
                content_area,
                entry,
                entry_top,
                clipped_top,
                clipped_bottom,
                visible_top,
            );
        }
        entry_top = entry_bottom;
    }
    if needs_scrollbar {
        let scrollbar_area = Rect {
            x: area.right().saturating_sub(1),
            width: 1,
            ..area
        };
        let mut state = ScrollbarState::new(content_height)
            .position(ui.scroll().top())
            .viewport_content_length(usize::from(area.height));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollbar_area,
            &mut state,
        );
    }
}

fn render_welcome(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let compact = Line::styled(
        "moh · Ctrl+O help",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    if area.height < 3 {
        frame.render_widget(Paragraph::new(compact).alignment(Alignment::Center), area);
        return;
    }
    if area.height < 7 || area.width < 48 {
        let width = area.width.min(24);
        let rect = Rect {
            x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
            y: area.y.saturating_add(area.height.saturating_sub(3) / 2),
            width,
            height: 3,
        };
        frame.render_widget(
            Paragraph::new(compact).alignment(Alignment::Center).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded),
            ),
            rect,
        );
        return;
    }
    let width = area.width.min(48);
    let rect = Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area.y.saturating_add(area.height.saturating_sub(7) / 2),
        width,
        height: 7,
    };
    let content = Text::from(vec![
        Line::styled(
            "moh",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::default(),
        Line::raw("Your personal coding harness"),
        Line::default(),
        Line::styled(
            "Enter sends · / commands · Ctrl+O help",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(content).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        ),
        rect,
    );
}

fn entry_heights(entries: &[TranscriptEntry], width: u16) -> Vec<usize> {
    entries
        .iter()
        .map(|entry| {
            let lines = entry_line_count(entry, width);
            lines.saturating_add(usize::from(entry.trailing_space))
        })
        .collect()
}

fn entry_paragraph(entry: &TranscriptEntry) -> Paragraph<'static> {
    let paragraph = Paragraph::new(entry.text.clone()).wrap(Wrap { trim: false });
    if entry.kind == TranscriptKind::User {
        paragraph.block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)),
        )
    } else if entry.kind == TranscriptKind::Activity {
        paragraph.style(Style::default().add_modifier(Modifier::DIM))
    } else {
        paragraph
    }
}

fn entry_line_count(entry: &TranscriptEntry, width: u16) -> usize {
    let content_width = if entry.kind == TranscriptKind::User {
        width.saturating_sub(1)
    } else {
        width
    };
    entry_paragraph(entry).line_count(content_width)
}

fn render_entry_slice(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    entry: &TranscriptEntry,
    entry_top: usize,
    clipped_top: usize,
    clipped_bottom: usize,
    visible_top: usize,
) {
    let content_lines = entry_line_count(entry, area.width);
    let entry_offset = clipped_top.saturating_sub(entry_top);
    if entry_offset >= content_lines {
        return;
    }
    let remaining_content = content_lines.saturating_sub(entry_offset);
    let visible_lines = clipped_bottom
        .saturating_sub(clipped_top)
        .min(remaining_content);
    if visible_lines == 0 {
        return;
    }
    let y = area
        .y
        .saturating_add((clipped_top.saturating_sub(visible_top)) as u16);
    frame.render_widget(
        entry_paragraph(entry).scroll((entry_offset as u16, 0)),
        Rect {
            y,
            height: visible_lines as u16,
            ..area
        },
    );
}

fn render_popups(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    prompt_area: Rect,
    projection: &ChatProjection,
    ui: &mut UiState,
) {
    if render_modal_overlay(frame, area, projection, ui) {
        return;
    }
    if ui.menu().is_open() {
        render_menu(frame, area, prompt_area, ui);
    }
}

fn render_modal_overlay(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    projection: &ChatProjection,
    ui: &mut UiState,
) -> bool {
    if ui.session_browser().is_open() {
        let current_session_id = match projection {
            ChatProjection::Draft(_) => None,
            ChatProjection::Session(snapshot) => Some(snapshot.summary.id),
        };
        render_session_browser(frame, area, ui.session_browser_mut(), current_session_id);
        return true;
    }
    match ui.popup() {
        Some(PopupKind::Help) => {
            render_help(frame, area);
            true
        }
        None => false,
    }
}

fn render_menu(frame: &mut ratatui::Frame<'_>, area: Rect, prompt_area: Rect, ui: &UiState) {
    let menu = ui.menu();
    let item_count = menu.items.len().min(5);
    let available_height = prompt_area.y.saturating_sub(area.y);
    let height = (item_count as u16).saturating_add(2).min(available_height);
    if height == 0 {
        return;
    }
    let title = match menu.kind {
        Some(MenuKind::Commands) => "commands",
        Some(MenuKind::Models) => "models",
        Some(MenuKind::Efforts) => "effort",
        Some(MenuKind::Processes) => "processes",
        None => return,
    };
    let desired_width = menu
        .items
        .iter()
        .take(5)
        .map(|item| item.value.chars().count() + item.description.chars().count() + 3)
        .max()
        .unwrap_or(1)
        .saturating_add(2) as u16;
    let width = desired_width.min(area.width).max(1);
    let rect = Rect {
        x: area.x,
        y: prompt_area.y.saturating_sub(height),
        width,
        height,
    };
    let items = menu
        .items
        .iter()
        .map(|item| ListItem::new(format!("{}  {}", item.value, item.description)))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::Cyan));
    let mut state = ListState::default();
    state.select(Some(menu.selected));
    frame.render_widget(Clear, rect);
    frame.render_stateful_widget(list, rect, &mut state);
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let horizontal_margin = u16::from(area.width > 2);
    let vertical_margin = u16::from(area.height >= 12);
    let available = Rect {
        x: area.x.saturating_add(horizontal_margin),
        y: area.y.saturating_add(vertical_margin),
        width: area
            .width
            .saturating_sub(horizontal_margin.saturating_mul(2)),
        height: area
            .height
            .saturating_sub(vertical_margin.saturating_mul(2)),
    };
    let layout =
        if available.width >= HELP_WIDE_MIN_WIDTH && available.height >= HELP_WIDE_MIN_HEIGHT {
            HelpLayout::Wide
        } else if available.height >= HELP_NARROW_MIN_HEIGHT {
            HelpLayout::Narrow
        } else {
            HelpLayout::Compact
        };
    let (desired_width, desired_height) = match layout {
        HelpLayout::Wide => (76, HELP_WIDE_MIN_HEIGHT),
        HelpLayout::Narrow => (48, HELP_NARROW_MIN_HEIGHT),
        HelpLayout::Compact => (40, 10),
    };
    let width = available.width.min(desired_width).max(1);
    let height = available.height.min(desired_height).max(1);
    let rect = Rect {
        x: available
            .x
            .saturating_add(available.width.saturating_sub(width) / 2),
        y: available
            .y
            .saturating_add(available.height.saturating_sub(height) / 2),
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::styled(
            " Help ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    if inner.is_empty() {
        return;
    }

    let content_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let footer_area = Rect {
        y: inner.bottom().saturating_sub(1),
        height: 1,
        ..inner
    };
    match layout {
        HelpLayout::Wide => {
            let [left, _, right] = Layout::horizontal([
                Constraint::Fill(1),
                Constraint::Length(2),
                Constraint::Fill(1),
            ])
            .areas(content_area);
            let (left_help, right_help) = wide_help();
            frame.render_widget(Paragraph::new(left_help), left);
            frame.render_widget(Paragraph::new(right_help), right);
        }
        HelpLayout::Narrow => {
            frame.render_widget(Paragraph::new(narrow_help()), content_area);
        }
        HelpLayout::Compact => {
            let [left, _, middle, _, right] = Layout::horizontal([
                Constraint::Length(11),
                Constraint::Length(1),
                Constraint::Length(11),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(content_area);
            for (column, help) in [left, middle, right].into_iter().zip(compact_help()) {
                frame.render_widget(Paragraph::new(help), column);
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            "Ctrl+O help · Ctrl+T sidebar · Esc",
            Style::default().add_modifier(Modifier::DIM),
        ))
        .alignment(Alignment::Right),
        footer_area,
    );
}

fn help_heading(label: &'static str) -> Line<'static> {
    Line::styled(
        label,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn help_row(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<15}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(description),
    ])
}

fn wide_help() -> (Text<'static>, Text<'static>) {
    let left = Text::from(vec![
        help_heading("Prompt"),
        help_row("Enter", "Send message"),
        help_row("Shift+Enter", "New line"),
        help_row("Ctrl+← / →", "Move by word"),
        help_row("Ctrl+⌫ / Del", "Delete by word"),
        Line::default(),
        help_heading("Commands"),
        help_row("/", "Show commands"),
        help_row("/cancel", "Cancel request"),
        help_row("/model [id]", "Set model"),
        help_row("/effort [level]", "Set reasoning"),
        help_row("/ps", "Running processes"),
        help_row("/kill job-N", "Stop process"),
    ]);
    let right = Text::from(vec![
        help_heading("Navigation"),
        help_row("↑ / ↓", "Select item"),
        help_row("Tab", "Complete selection"),
        help_row("PgUp/PgDn/wheel", "Scroll"),
        help_row("End", "Follow latest"),
        Line::default(),
        help_heading("Settings"),
        help_row("Ctrl+L", "Choose model"),
        help_row("Ctrl+R / ⇧Tab", "Reasoning effort"),
        Line::default(),
        help_heading("General"),
        help_row("Ctrl+C", "Exit moh"),
        help_row("Ctrl+T", "Toggle sidebar"),
    ]);
    (left, right)
}

fn narrow_help() -> Text<'static> {
    Text::from(vec![
        help_heading("Prompt"),
        help_row("Enter", "Send message"),
        help_row("Shift+Enter", "New line"),
        help_row("Ctrl+← / →", "Move by word"),
        help_row("Ctrl+⌫ / Del", "Delete by word"),
        help_heading("Commands"),
        help_row("/", "Show commands"),
        help_row("/cancel", "Cancel request"),
        help_row("/model [id]", "Set model"),
        help_row("/effort [level]", "Set reasoning"),
        help_row("/ps", "Running processes"),
        help_row("/kill job-N", "Stop process"),
        help_heading("Shortcuts"),
        help_row("↑ / ↓", "Select item"),
        help_row("Tab", "Complete selection"),
        help_row("PgUp/PgDn/wheel", "Scroll"),
        help_row("End", "Follow latest"),
        help_row("Ctrl+L / Ctrl+R / ⇧Tab", "Model / effort"),
        help_row("Ctrl+C", "Exit moh"),
        help_row("Ctrl+T", "Toggle sidebar"),
    ])
}

fn compact_row(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            key,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(description),
    ])
}

fn compact_key(key: &'static str) -> Line<'static> {
    Line::styled(
        key,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn compact_help() -> [Text<'static>; 3] {
    [
        Text::from(vec![
            help_heading("Prompt"),
            compact_row("Enter", "Send"),
            compact_row("⇧↵", "Line"),
            compact_row("C←/→", "Word"),
            compact_row("C⌫/Del", "Del"),
            compact_row("C-L/R", "Set"),
            compact_row("C-C", "Exit"),
        ]),
        Text::from(vec![
            help_heading("Commands"),
            compact_row("/", "Commands"),
            compact_key("/cancel"),
            compact_key("/model"),
            compact_key("/effort"),
            compact_row("/ps", "Jobs"),
            compact_row("/kill", "Stop"),
        ]),
        Text::from(vec![
            help_heading("Navigate"),
            compact_row("↑/↓", "Select"),
            compact_row("Tab", "Complete"),
            Line::raw("PgUp/Dn"),
            compact_row("Wheel", "Scroll"),
            compact_row("End", "Follow"),
            compact_row("S-Tab", "Cycle"),
        ]),
    ]
}

fn render_prompt(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, ui: &mut UiState) {
    let editor_width = area.width.saturating_sub(2);
    let rows = ui.editor_mut().display_rows(editor_width, area.height);
    let prompt = Text::from(
        rows.lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let prefix = if index == 0 {
                    Span::styled("❯ ", Style::default().fg(Color::Cyan))
                } else {
                    Span::raw("  ")
                };
                Line::from(vec![prefix, Span::raw(line)])
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(prompt), area);
    let cursor_x = area
        .x
        .saturating_add(2)
        .saturating_add(rows.cursor_column)
        .min(area.right().saturating_sub(1));
    let cursor_y = area
        .y
        .saturating_add(rows.cursor_row)
        .min(area.bottom().saturating_sub(1));
    frame.set_cursor_position((cursor_x, cursor_y));
}

pub(super) fn status_line(projection: &ChatProjection, ui: &UiState) -> Line<'static> {
    let (
        chat_label,
        settings,
        cwd,
        transcript_status,
        session_error,
        thinking,
        running_processes,
        plan,
    ) = match projection {
        ChatProjection::Draft(draft) => (
            String::from("new chat"),
            &draft.settings,
            sanitize_line(&String::from_utf8_lossy(&draft.cwd)),
            None,
            false,
            false,
            0,
            &[][..],
        ),
        ChatProjection::Session(snapshot) => {
            let transcript_status = transcript_status(&snapshot.transcript);
            (
                sanitize_line(snapshot.summary.title.as_str()),
                &snapshot.settings,
                sanitize_line(&snapshot.summary.cwd_display),
                transcript_status,
                matches!(snapshot.catalog, ModelCatalogState::Failed(_))
                    || snapshot.persistence_warning.is_some(),
                transcript_status == Some(TranscriptStatus::Thinking)
                    || (transcript_status.is_none()
                        && (snapshot.busy
                            || snapshot.summary.busy
                            || snapshot.active_run.is_some())),
                snapshot
                    .jobs
                    .iter()
                    .filter(|job| job.state == JobState::Running)
                    .count(),
                snapshot.plan.as_slice(),
            )
        }
    };
    let (color, label) = if ui.local_error()
        || session_error
        || transcript_status == Some(TranscriptStatus::Error)
    {
        (Color::Red, "error")
    } else if thinking {
        (Color::Yellow, "thinking...")
    } else {
        (Color::Green, "ready")
    };
    let context_percentage = settings.context_tokens.saturating_mul(100) / CONTEXT_WINDOW_TOKENS;
    let process_segment = if running_processes > 0 {
        format!(" · {running_processes} processes")
    } else {
        String::new()
    };
    let plan_segment = if plan.is_empty() {
        String::new()
    } else {
        let completed = plan
            .iter()
            .filter(|item| item.status() == PlanStatus::Completed)
            .count();
        let active = plan
            .iter()
            .filter(|item| item.status() != PlanStatus::Cancelled)
            .count();
        format!(" · plan {completed}/{active}")
    };
    let dim = Style::default().add_modifier(Modifier::DIM);

    Line::from(vec![
        Span::styled("╰─ ", dim),
        Span::styled(
            sanitize_line(&settings.model),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(format!(" · {chat_label}"), dim),
        Span::styled(
            format!(
                " · {} · {context_percentage}%/256K · ",
                settings.reasoning.as_str()
            ),
            dim,
        ),
        Span::styled(label, Style::default().fg(color)),
        Span::styled(format!("{process_segment}{plan_segment} · {}", cwd), dim),
    ])
}

fn transcript_status(transcript: &[TranscriptItem]) -> Option<TranscriptStatus> {
    transcript.last().map(|item| match item {
        TranscriptItem::User(_) | TranscriptItem::ToolStarted { .. } => TranscriptStatus::Thinking,
        TranscriptItem::Assistant(_) | TranscriptItem::Cancelled { .. } => TranscriptStatus::Ready,
        TranscriptItem::Failed { .. } => TranscriptStatus::Error,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moh::{
        harness::{RunFailureKind, RunStage},
        runtime::rig::ReasoningLevel,
        session::{
            JobSnapshotDto, ModelCatalogState, PlanItem, PlanStatus, RunFailureSnapshot,
            SessionSettings, SessionSnapshot, SessionSummary, TranscriptItem,
        },
        tools::{JobKind, JobState},
    };
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::client::ui::{MenuItem, PopupKind, UiState};

    fn snapshot_fixture() -> SessionSnapshot {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        SessionSnapshot {
            summary: SessionSummary {
                id: "session-7".parse().unwrap(),
                title: moh::session::fallback_title("fixture chat"),
                title_revision: 0,
                cwd: b"/work/moh".to_vec(),
                cwd_display: "/work/moh".into(),
                running_jobs: 0,
                running: false,
                busy: false,
                attached_clients: 1,
                last_activity: now,
            },
            transcript: vec![
                TranscriptItem::User("first prompt".into()),
                TranscriptItem::Assistant("first answer".into()),
            ],
            active_run: None,
            settings: SessionSettings {
                model: "test-model".into(),
                reasoning: ReasoningLevel::High,
                context_tokens: 128_000,
            },
            catalog: ModelCatalogState::Ready(Vec::new()),
            plan: Vec::new(),
            jobs: Vec::new(),
            persistence_warning: None,
            sequence: 14,
            busy: false,
        }
    }

    fn draw(
        snapshot: &SessionSnapshot,
        ui: &mut UiState,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let projection = ChatProjection::session(snapshot.clone());
        terminal
            .draw(|frame| render(frame, &projection, ui))
            .unwrap();
        terminal
    }

    fn rendered(terminal: &Terminal<TestBackend>) -> String {
        terminal.backend().to_string()
    }

    fn entry_text(entry: &TranscriptEntry) -> String {
        entry
            .text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn status_row(terminal: &Terminal<TestBackend>, width: u16, height: u16) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .nth(usize::from(height.saturating_sub(1)))
            .unwrap()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn failure(message: &str) -> TranscriptItem {
        TranscriptItem::Failed {
            run_id: 8,
            failure: RunFailureSnapshot {
                stage: RunStage::ModelRequest,
                kind: RunFailureKind::Transport,
                retryable: false,
                message: message.into(),
            },
        }
    }

    #[test]
    fn frame_pins_prompt_above_status() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        let lines = terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(lines[8].contains('❯'));
        assert!(lines[9].contains("test-model"));
        assert!(lines[9].contains("50%/256K"));
        assert!(lines[9].contains("/work/moh"));
    }

    fn plan_fixture() -> Vec<PlanItem> {
        [
            ("Queue work", PlanStatus::Pending),
            ("Execute work", PlanStatus::InProgress),
            ("Verify result", PlanStatus::Completed),
            ("Await input", PlanStatus::Blocked),
            ("Superseded step", PlanStatus::Cancelled),
        ]
        .into_iter()
        .map(|(step, status)| PlanItem::parse(step, status).unwrap())
        .collect()
    }

    fn plan_row(terminal: &Terminal<TestBackend>, marker: &str, step: &str) -> (u16, u16) {
        let width = terminal.backend().buffer().area.width;
        let expected = format!("{marker} {step}");
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .enumerate()
            .find_map(|(y, row)| {
                let row_text = row.iter().map(|cell| cell.symbol()).collect::<String>();
                row_text.find(&expected).map(|byte_offset| {
                    let cell_offset = row_text[..byte_offset].chars().count();
                    (
                        u16::try_from(y).unwrap(),
                        u16::try_from(cell_offset).unwrap(),
                    )
                })
            })
            .unwrap_or_else(|| panic!("missing plan row {expected}"))
    }

    #[test]
    fn plan_progress_counts_completed_and_ignores_cancelled_steps() {
        let mut snapshot = snapshot_fixture();
        snapshot.plan = plan_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 100, 16);

        assert!(status_row(&terminal, 100, 16).contains("plan 1/4"));
    }

    #[test]
    fn empty_plans_do_not_use_status_space() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 100, 16);

        assert!(!status_row(&terminal, 100, 16).contains("plan"));
    }

    #[test]
    fn wide_frames_dock_the_todo_sidebar_with_semantic_cell_styles() {
        let mut snapshot = snapshot_fixture();
        snapshot.plan = plan_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 121, 24);
        assert!(rendered(&terminal).contains("Todo"));
        assert_eq!(terminal.backend().buffer()[(79, 0)].symbol(), "│");
        let expected_rows = [
            ("○", "Queue work", Color::DarkGray, Modifier::empty()),
            ("▶", "Execute work", Color::Cyan, Modifier::BOLD),
            ("✓", "Verify result", Color::Green, Modifier::empty()),
            ("!", "Await input", Color::Red, Modifier::BOLD),
            ("–", "Superseded step", Color::Reset, Modifier::DIM),
        ];
        let rows = expected_rows
            .iter()
            .map(|(marker, step, color, modifier)| {
                let (y, x) = plan_row(&terminal, marker, step);
                let cell = &terminal.backend().buffer()[(x, y)];
                assert_eq!(cell.fg, *color, "wrong color for {marker} {step}");
                assert_eq!(
                    cell.modifier, *modifier,
                    "wrong modifier for {marker} {step}"
                );
                assert!(x >= 79, "todo row rendered outside the sidebar");
                y
            })
            .collect::<Vec<_>>();
        assert!(rows.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn visible_empty_sidebar_explains_what_it_will_contain() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 121, 10);

        let frame = rendered(&terminal);
        assert!(frame.contains("Todo"));
        assert!(frame.contains("No todos yet"));
        assert!(frame.contains("Tasks appear here as work is planned."));
    }

    #[test]
    fn automatic_sidebar_visibility_tracks_the_wide_breakpoint() {
        let mut snapshot = snapshot_fixture();
        snapshot.plan = vec![PlanItem::parse("Responsive todo", PlanStatus::Pending).unwrap()];
        let mut ui = UiState::new();

        let wide = draw(&snapshot, &mut ui, 121, 10);
        assert!(rendered(&wide).contains("Responsive todo"));

        let narrow = draw(&snapshot, &mut ui, 120, 10);
        assert!(!rendered(&narrow).contains("Responsive todo"));
    }

    #[test]
    fn long_todo_text_wraps_inside_the_sidebar() {
        let mut snapshot = snapshot_fixture();
        snapshot.plan = vec![
            PlanItem::parse(
                "This todo description is deliberately long enough to wrap cleanly",
                PlanStatus::InProgress,
            )
            .unwrap(),
        ];
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 121, 10);
        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(121)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let first = rows
            .iter()
            .position(|row| row.contains("▶ This todo"))
            .expect("missing first todo row");
        let continuation = rows
            .iter()
            .position(|row| row.contains("cleanly"))
            .expect("missing wrapped todo tail");

        assert!(continuation > first);
        assert!(rows[continuation][79..].contains("cleanly"));
    }

    #[test]
    fn narrow_sidebar_keeps_a_long_prompt_cursor_outside_the_overlay() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let _ = draw(&snapshot, &mut ui, 80, 10);
        ui.toggle_sidebar();
        ui.editor_mut().set_value("prompt text ".repeat(5));

        let mut terminal = draw(&snapshot, &mut ui, 80, 10);

        assert!(terminal.get_cursor_position().unwrap().x < 38);
        assert_eq!(terminal.backend().buffer()[(38, 0)].symbol(), "│");
    }

    #[test]
    fn multiline_prompt_aligns_continuations_and_pins_status() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.editor_mut().set_value("first\nsecond");
        let terminal = draw(&snapshot, &mut ui, 20, 8);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 5)].symbol(), "❯");
        assert_eq!(buffer[(2, 5)].symbol(), "f");
        assert_eq!(buffer[(0, 6)].symbol(), " ");
        assert_eq!(buffer[(2, 6)].symbol(), "s");
        assert!(status_row(&terminal, 20, 8).contains("test-model"));
    }

    #[test]
    fn prompt_height_caps_at_four_rows_and_keeps_cursor_visible() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.editor_mut().set_value("a\nb\nc\nd\ne");
        let mut terminal = draw(&snapshot, &mut ui, 20, 10);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 5)].symbol(), "❯");
        assert_eq!(buffer[(2, 5)].symbol(), "b");
        assert_eq!(buffer[(2, 8)].symbol(), "e");
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!(cursor.y, 8);
        assert!(cursor.x < 20);
        assert!(status_row(&terminal, 20, 10).contains("test-model"));
    }

    #[test]
    fn status_uses_the_latest_transcript_state_and_includes_running_processes() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript = vec![TranscriptItem::ToolStarted {
            run_id: 8,
            call_id: "call-8".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        }];
        snapshot.jobs = vec![
            JobSnapshotDto {
                id: "job-1".into(),
                kind: JobKind::Bash,
                state: JobState::Running,
                title: "first".into(),
                started_at: snapshot.summary.last_activity,
                completed_at: None,
                details: String::new(),
            },
            JobSnapshotDto {
                id: "job-2".into(),
                kind: JobKind::Bash,
                state: JobState::Running,
                title: "second".into(),
                started_at: snapshot.summary.last_activity,
                completed_at: None,
                details: String::new(),
            },
            JobSnapshotDto {
                id: "job-3".into(),
                kind: JobKind::Bash,
                state: JobState::Completed,
                title: "finished".into(),
                started_at: snapshot.summary.last_activity,
                completed_at: Some(snapshot.summary.last_activity),
                details: String::new(),
            },
        ];
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        assert!(status_row(&terminal, 80, 10).contains("thinking..."));
        assert!(status_row(&terminal, 80, 10).contains("2 processes"));

        snapshot
            .transcript
            .push(TranscriptItem::Assistant("done".into()));
        snapshot.busy = true;
        snapshot.summary.busy = true;
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        assert!(status_row(&terminal, 80, 10).contains("ready"));
        assert!(!status_row(&terminal, 80, 10).contains("thinking..."));

        snapshot
            .transcript
            .push(TranscriptItem::Cancelled { run_id: 8 });
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        assert!(status_row(&terminal, 80, 10).contains("ready"));

        ui.push_error("local notice");
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        let status = status_row(&terminal, 80, 10);
        assert!(status.contains("error"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .chunks(80)
                .nth(9)
                .unwrap()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );
    }

    #[test]
    fn undersized_frame_shows_only_the_minimum_size_message() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 19, 2);
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("terminal too small"));
        assert!(!rendered.contains('❯'));
    }

    #[test]
    fn transcript_keeps_a_cyan_dim_user_rail_and_open_assistant_text() {
        let snapshot = snapshot_fixture();
        let ui = UiState::new();
        let projection = ChatProjection::session(snapshot.clone());
        let entries = transcript_entries(&projection, &ui);
        let user = entries
            .iter()
            .find(|entry| entry_text(entry).contains("first prompt"))
            .unwrap();
        let assistant = entries
            .iter()
            .find(|entry| entry_text(entry).contains("first answer"))
            .unwrap();
        assert_eq!(user.kind, TranscriptKind::User);
        assert_eq!(assistant.kind, TranscriptKind::Assistant);
        assert_eq!(entry_text(assistant), "first answer");
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        let rail = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "│")
            .unwrap();
        assert_eq!(rail.fg, Color::Cyan);
        assert!(rail.modifier.contains(Modifier::DIM));
    }

    #[test]
    fn failure_and_local_notice_render_as_messages_and_surface_error_status() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript = vec![failure("request failed")];
        let mut ui = UiState::new();
        ui.push_error("local transport notice");
        let terminal = draw(&snapshot, &mut ui, 80, 12);
        let rendered = rendered(&terminal);
        assert!(rendered.contains("request failed"));
        assert!(rendered.contains("local transport notice"));
        assert!(!rendered.contains("ModelRequest"));
        assert!(status_row(&terminal, 80, 12).contains("error"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .chunks(80)
                .nth(11)
                .unwrap()
                .iter()
                .any(|cell| cell.fg == Color::Red)
        );
    }

    #[test]
    fn welcome_card_is_only_rendered_for_an_empty_session() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript.clear();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 80, 12);
        let empty_frame = rendered(&terminal);

        assert!(empty_frame.contains("Your personal coding harness"));
        assert!(empty_frame.contains("Enter sends · / commands · Ctrl+O help"));

        let buffer = terminal.backend().buffer();
        let position = |symbol: &str| {
            let index = buffer
                .content
                .iter()
                .position(|cell| cell.symbol() == symbol)
                .unwrap();
            (
                index as u16 % buffer.area.width,
                index as u16 / buffer.area.width,
            )
        };
        let top_left = position("╭");
        let top_right = position("╮");
        let bottom_left = position("╰");

        assert!(top_left.0.abs_diff(buffer.area.width - 1 - top_right.0) <= 1);
        assert!(top_left.1.abs_diff(9 - bottom_left.1) <= 1);

        snapshot
            .transcript
            .push(TranscriptItem::User("first prompt".into()));
        let terminal = draw(&snapshot, &mut ui, 80, 12);
        let conversation_frame = rendered(&terminal);
        assert!(conversation_frame.contains("first prompt"));
        assert!(!conversation_frame.contains("Your personal coding harness"));
        assert!(!conversation_frame.contains("Enter sends · / commands · Ctrl+O help"));
    }

    #[test]
    fn small_empty_session_prioritizes_warnings_and_notices_over_the_welcome_card() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript.clear();
        snapshot.persistence_warning = Some("disk unavailable".into());
        let mut ui = UiState::new();
        ui.push_notice("local transport notice");

        let terminal = draw(&snapshot, &mut ui, 80, 6);
        let frame = rendered(&terminal);

        assert!(frame.contains("Session persistence warning: disk unavailable."));
        assert!(frame.contains("local transport notice"));
        assert!(!frame.contains("moh · Ctrl+O help"));
        assert!(!frame.contains("Your personal coding harness"));
    }

    #[test]
    fn minimum_normal_frame_uses_a_compact_welcome_message() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript.clear();
        let mut ui = UiState::new();

        let terminal = draw(&snapshot, &mut ui, 20, 3);
        let frame = rendered(&terminal);

        assert!(frame.contains("moh · Ctrl+O help"));
        assert!(frame.contains('❯'));
        assert!(!frame.contains("terminal too small"));
    }

    #[test]
    fn active_assistant_text_is_rendered_once() {
        let mut snapshot = snapshot_fixture();
        snapshot.active_run = Some(moh::session::ActiveRunSnapshot {
            run_id: 8,
            prompt: "second prompt".into(),
            assistant_text: "streaming answer".into(),
        });
        snapshot.busy = true;
        snapshot.summary.busy = true;
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 80, 10);
        assert_eq!(rendered(&terminal).matches("streaming answer").count(), 1);
    }

    #[test]
    fn read_activity_uses_a_path_relative_to_the_session_cwd() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript = vec![TranscriptItem::ToolStarted {
            run_id: 8,
            call_id: "call-8".into(),
            name: "read".into(),
            arguments: serde_json::json!({
                "path": "/work/moh/src/lib.rs",
                "offset": 4,
                "limit": 3,
            }),
        }];
        let ui = UiState::new();
        let projection = ChatProjection::session(snapshot);
        let entries = transcript_entries(&projection, &ui);
        assert!(
            entries
                .iter()
                .map(entry_text)
                .any(|text| text == "Read src/lib.rs · lines 4–6")
        );
    }

    #[test]
    fn long_transcript_renders_a_right_side_scrollbar() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript = (0..20)
            .map(|index| TranscriptItem::Assistant(format!("answer {index}")))
            .collect();
        let mut ui = UiState::new();
        let terminal = draw(&snapshot, &mut ui, 30, 8);
        let has_scrollbar = terminal
            .backend()
            .buffer()
            .content
            .chunks(30)
            .take(6)
            .any(|row| row[29].symbol() != " ");
        assert!(has_scrollbar);
    }

    #[test]
    fn manual_scroll_preserves_the_viewport_when_live_text_grows() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript = (0..20)
            .map(|index| TranscriptItem::Assistant(format!("answer {index}")))
            .collect();
        let mut ui = UiState::new();
        let _ = draw(&snapshot, &mut ui, 30, 8);
        ui.scroll_mut().page_up();
        let top = ui.scroll().top();
        let scrolled = draw(&snapshot, &mut ui, 30, 8);
        assert!(rendered(&scrolled).contains("answer 15"));
        assert!(!rendered(&scrolled).contains("answer 19"));
        snapshot.active_run = Some(moh::session::ActiveRunSnapshot {
            run_id: 8,
            prompt: "next".into(),
            assistant_text: "live text that is deliberately longer than the transcript viewport"
                .into(),
        });
        let terminal = draw(&snapshot, &mut ui, 30, 8);
        assert_eq!(ui.scroll().top(), top);
        assert!(rendered(&terminal).contains("answer"));
    }

    #[test]
    fn long_prompt_keeps_the_cursor_in_the_prompt_row() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.editor_mut()
            .set_value("a prompt that is much wider than this terminal row");
        let mut terminal = draw(&snapshot, &mut ui, 20, 5);
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!(cursor.y, 3);
        assert!(cursor.x < 20);
    }

    #[test]
    fn wide_help_is_a_centered_two_column_reference_with_one_title() {
        let mut snapshot = snapshot_fixture();
        snapshot.transcript = vec![TranscriptItem::Assistant("z".repeat(80))];
        let mut ui = UiState::new();
        ui.set_popup(Some(PopupKind::Help));
        let terminal = draw(&snapshot, &mut ui, 80, 24);
        let frame = rendered(&terminal);

        assert_eq!(frame.matches("Help").count(), 1);
        assert!(!frame.contains("moh help"));
        for label in ["Prompt", "Commands", "Navigation", "Settings", "General"] {
            assert!(frame.contains(label), "missing help section {label}");
        }
        for shortcut in [
            "Enter",
            "Shift+Enter",
            "Ctrl+← / →",
            "Ctrl+⌫ / Del",
            "/cancel",
            "/model [id]",
            "/effort [level]",
            "/ps",
            "/kill job-N",
            "↑ / ↓",
            "Tab",
            "PgUp/PgDn/wheel",
            "End",
            "Ctrl+L",
            "Ctrl+R / ⇧Tab",
            "Ctrl+C",
            "Ctrl+O help · Ctrl+T sidebar · Esc",
        ] {
            assert!(frame.contains(shortcut), "missing shortcut {shortcut}");
        }

        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let prompt_row = rows.iter().position(|row| row.contains("Prompt")).unwrap();
        let navigation_row = rows
            .iter()
            .position(|row| row.contains("Navigation"))
            .unwrap();
        assert_eq!(prompt_row, navigation_row);
        let commands_row = rows
            .iter()
            .position(|row| row.contains("Commands"))
            .unwrap();
        let settings_row = rows
            .iter()
            .position(|row| row.contains("Settings"))
            .unwrap();
        assert_eq!(commands_row, settings_row);

        let enter_row = rows.iter().position(|row| row.contains("Enter")).unwrap();
        let enter_column = rows[enter_row].find("Enter").unwrap() as u16;
        let enter = &terminal.backend().buffer()[(enter_column, enter_row as u16)];
        assert_eq!(enter.fg, Color::Cyan);
        assert!(enter.modifier.contains(Modifier::BOLD));
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol() == "╭")
        );
        assert_ne!(terminal.backend().buffer()[(40, 5)].symbol(), "z");
    }

    #[test]
    fn narrow_help_keeps_the_complete_reference_in_one_column() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.set_popup(Some(PopupKind::Help));
        let terminal = draw(&snapshot, &mut ui, 40, 24);
        let frame = rendered(&terminal);

        for label in ["Prompt", "Commands", "Shortcuts"] {
            assert!(frame.contains(label), "missing help section {label}");
        }
        for shortcut in [
            "Shift+Enter",
            "Ctrl+⌫ / Del",
            "/kill job-N",
            "PgUp/PgDn/wheel",
            "End",
            "Ctrl+L / Ctrl+R / ⇧Tab",
            "Ctrl+C",
            "Ctrl+O help · Ctrl+T sidebar · Esc",
        ] {
            assert!(frame.contains(shortcut), "missing shortcut {shortcut}");
        }
    }

    #[test]
    fn short_help_uses_a_complete_compact_reference_without_clipping() {
        let snapshot = snapshot_fixture();
        for height in [12, 10] {
            let mut ui = UiState::new();
            ui.set_popup(Some(PopupKind::Help));
            let terminal = draw(&snapshot, &mut ui, 40, height);
            let frame = rendered(&terminal);

            for label in ["Prompt", "Commands", "Navigate"] {
                assert!(frame.contains(label), "missing compact section {label}");
            }
            for shortcut in [
                "Enter Send",
                "⇧↵ Line",
                "C←/→ Word",
                "C⌫/Del Del",
                "C-L/R Set",
                "C-C Exit",
                "/cancel",
                "/model",
                "/effort",
                "/ps",
                "/kill",
                "↑/↓ Select",
                "Tab Complete",
                "PgUp/Dn",
                "Wheel Scroll",
                "End Follow",
                "S-Tab Cycle",
                "Ctrl+O help · Ctrl+T sidebar · Esc",
            ] {
                assert!(
                    frame.contains(shortcut),
                    "missing compact shortcut {shortcut} at 40×{height}"
                );
            }
            assert!(!frame.contains("StopTab"));
            assert!(!frame.contains("ModelPgUp"));
        }
    }

    #[test]
    fn menus_limit_to_five_items_and_highlight_the_selected_item() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.menu_mut().set(
            super::super::MenuKind::Commands,
            (0..6).map(|index| MenuItem::new(format!("/command-{index}"), "description")),
        );
        ui.menu_mut().select_next();
        let terminal = draw(&snapshot, &mut ui, 40, 12);
        let rendered = rendered(&terminal);
        assert!(rendered.contains("/command-4"));
        assert!(!rendered.contains("/command-5"));
        let selected = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "/")
            .unwrap();
        assert!(selected.style().bg.is_some());
    }

    #[test]
    fn popup_rectangles_stay_within_a_minimum_normal_frame() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.menu_mut().set(
            super::super::MenuKind::Commands,
            [MenuItem::new("/quit", "Exit")],
        );
        let menu = draw(&snapshot, &mut ui, 20, 3);
        assert!(menu.backend().buffer()[(0, 0)].symbol().starts_with('┌'));
        assert_eq!(menu.backend().buffer()[(0, 1)].symbol(), "❯");

        ui.menu_mut().clear();
        ui.set_popup(Some(PopupKind::Help));
        let help = draw(&snapshot, &mut ui, 20, 3);
        assert_eq!(help.backend().buffer()[(0, 1)].symbol(), "❯");
        assert!(help.backend().buffer()[(1, 0)].symbol().starts_with('╭'));
        assert!(help.backend().buffer()[(1, 2)].symbol().starts_with('╰'));
    }

    #[test]
    fn narrow_session_browser_renders_above_the_sidebar_across_the_full_frame() {
        let snapshot = snapshot_fixture();
        let mut ui = UiState::new();
        ui.toggle_sidebar();
        ui.session_browser_mut().open();

        let terminal = draw(&snapshot, &mut ui, 80, 24);
        let frame = rendered(&terminal);

        assert!(frame.contains("sessions"));
        assert_eq!(terminal.backend().buffer()[(78, 7)].symbol(), "┐");
    }
}
