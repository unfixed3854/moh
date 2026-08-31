use std::{convert::Infallible, io, time::Duration};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use moh::{
    rpc::client::SessionUpdate,
    runtime::rig::ReasoningLevel,
    session::{
        JobSnapshotDto, ModelCatalogState, ModelInfoDto, SessionEvent, SessionId, SessionListScope,
        SessionTitle,
    },
    tools::JobState,
};
use ratatui::{Terminal, backend::Backend};

#[cfg(test)]
use moh::session::{ActiveRunSnapshot, SessionEventEnvelope, SessionSnapshot, TranscriptItem};

use crate::client::{
    ChatProjection, ClientSessionError, WorkspaceClient, WorkspaceUpdate,
    session::SessionListFuture,
    terminal::{CrosstermEvents, EventSource, TerminalSession},
    ui::{
        BrowserAction, EditorOutcome, MenuItem, MenuKind, PopupKind, UiState,
        fuzzy_subsequence_score, render, sanitize_line,
    },
};

#[derive(Clone, Copy)]
enum CommandAction {
    Quit,
    Cancel,
    Model,
    Effort,
    Processes,
    Kill,
    New,
    Sessions,
}

struct CommandSpec {
    name: &'static str,
    description: &'static str,
    action: CommandAction,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/quit",
        description: "Exit moh",
        action: CommandAction::Quit,
    },
    CommandSpec {
        name: "/cancel",
        description: "Cancel the active request",
        action: CommandAction::Cancel,
    },
    CommandSpec {
        name: "/model",
        description: "Change the active model",
        action: CommandAction::Model,
    },
    CommandSpec {
        name: "/effort",
        description: "Change the reasoning effort",
        action: CommandAction::Effort,
    },
    CommandSpec {
        name: "/ps",
        description: "Show running background processes",
        action: CommandAction::Processes,
    },
    CommandSpec {
        name: "/kill",
        description: "Terminate a background process",
        action: CommandAction::Kill,
    },
    CommandSpec {
        name: "/new",
        description: "Open a new ephemeral chat",
        action: CommandAction::New,
    },
    CommandSpec {
        name: "/sessions",
        description: "Browse saved sessions",
        action: CommandAction::Sessions,
    },
];

#[derive(Debug, Eq, PartialEq)]
enum AppAction {
    None,
    Submit(String),
    Cancel,
    OpenHelp,
    ToggleSidebar,
    OpenModelSelector,
    OpenProcessSelector,
    PrepareKill(String),
    SelectModel(String),
    ModelNotFound(String),
    OpenEffortSelector,
    SelectEffort(ReasoningLevel),
    EffortNotFound(String),
    CycleEffort,
    Kill(String),
    KillUsage,
    NewDraft,
    NewUsage,
    OpenSessionBrowser,
    SwitchSession(SessionId),
    RenameSession {
        session_id: SessionId,
        title: String,
    },
    DeleteSession(SessionId),
    Exit,
}

fn best_model_match<'a>(query: &str, models: &'a [ModelInfoDto]) -> Option<&'a ModelInfoDto> {
    let query = normalize_model_query(query);
    if query.is_empty() {
        return None;
    }
    models
        .iter()
        .filter_map(|model| {
            let id = normalize_model_query(&model.id);
            let display_name = normalize_model_query(&model.display_name);
            let score = fuzzy_subsequence_score(&query, &id)
                .into_iter()
                .chain(fuzzy_subsequence_score(&query, &display_name))
                .max()?;
            Some((score, model))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, model)| model)
}

fn normalize_model_query(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn matching_models<'a>(query: &str, models: &'a [ModelInfoDto]) -> Vec<&'a ModelInfoDto> {
    let query = normalize_model_query(query);
    if query.is_empty() {
        return models.iter().collect();
    }
    let mut matches = models
        .iter()
        .filter_map(|model| {
            let score = fuzzy_subsequence_score(&query, &normalize_model_query(&model.id))
                .into_iter()
                .chain(fuzzy_subsequence_score(
                    &query,
                    &normalize_model_query(&model.display_name),
                ))
                .max()?;
            Some((score, model))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    matches.into_iter().map(|(_, model)| model).collect()
}

fn catalog(projection: &ChatProjection) -> &ModelCatalogState {
    match projection {
        ChatProjection::Draft(draft) => &draft.catalog,
        ChatProjection::Session(snapshot) => &snapshot.catalog,
    }
}

fn settings(projection: &ChatProjection) -> &moh::session::SessionSettings {
    match projection {
        ChatProjection::Draft(draft) => &draft.settings,
        ChatProjection::Session(snapshot) => &snapshot.settings,
    }
}

fn is_draft(projection: &ChatProjection) -> bool {
    matches!(projection, ChatProjection::Draft(_))
}

fn is_busy(projection: &ChatProjection) -> bool {
    matches!(projection, ChatProjection::Session(snapshot) if snapshot.busy)
}

fn current_cwd(projection: &ChatProjection) -> &[u8] {
    match projection {
        ChatProjection::Draft(draft) => &draft.cwd,
        ChatProjection::Session(snapshot) => &snapshot.summary.cwd,
    }
}

fn current_session_id(projection: &ChatProjection) -> Option<SessionId> {
    match projection {
        ChatProjection::Draft(_) => None,
        ChatProjection::Session(snapshot) => Some(snapshot.summary.id),
    }
}

struct PendingSessionBrowserRefresh {
    cwd: Vec<u8>,
    future: SessionListFuture,
}

fn start_session_browser_refresh<C: WorkspaceClient>(
    ui: &UiState,
    client: &C,
) -> PendingSessionBrowserRefresh {
    let cwd = current_cwd(client.current_projection()).to_vec();
    let scope = match ui.session_browser().mode() {
        crate::client::ui::BrowserMode::Project => SessionListScope::Project(cwd.clone()),
        crate::client::ui::BrowserMode::Global => SessionListScope::All,
    };
    PendingSessionBrowserRefresh {
        cwd,
        future: client.list_sessions(scope),
    }
}

fn finish_session_browser_refresh(
    ui: &mut UiState,
    cwd: &[u8],
    result: Result<Vec<moh::session::SessionSummary>, ClientSessionError>,
) {
    match result {
        Ok(sessions) => ui.session_browser_mut().set_sessions(cwd, sessions),
        Err(error) => ui
            .session_browser_mut()
            .set_refresh_warning(sanitize_line(&error.to_string())),
    }
    ui.request_redraw();
}

fn available_models(projection: &ChatProjection) -> &[ModelInfoDto] {
    match catalog(projection) {
        ModelCatalogState::Ready(models) => models,
        ModelCatalogState::Loading | ModelCatalogState::Failed(_) => &[],
    }
}

fn available_efforts(projection: &ChatProjection) -> Vec<ReasoningLevel> {
    available_models(projection)
        .iter()
        .find(|model| model.id == settings(projection).model)
        .into_iter()
        .flat_map(|model| model.reasoning_efforts.iter().copied())
        .collect()
}

fn running_jobs(projection: &ChatProjection) -> Vec<&JobSnapshotDto> {
    match projection {
        ChatProjection::Draft(_) => Vec::new(),
        ChatProjection::Session(snapshot) => snapshot
            .jobs
            .iter()
            .filter(|job| job.state == JobState::Running)
            .collect(),
    }
}

fn resolve_submission(ui: &UiState, projection: &ChatProjection, text: String) -> AppAction {
    match ui.menu().kind() {
        Some(MenuKind::Models) => {
            return best_model_match(&text, available_models(projection)).map_or_else(
                || AppAction::ModelNotFound(text),
                |model| AppAction::SelectModel(model.id.clone()),
            );
        }
        Some(MenuKind::Efforts) => {
            return ReasoningLevel::parse(text.trim())
                .filter(|level| available_efforts(projection).contains(level))
                .map_or_else(|| AppAction::EffortNotFound(text), AppAction::SelectEffort);
        }
        Some(MenuKind::Processes) => return AppAction::None,
        Some(MenuKind::Commands) | None => {}
    }

    let trimmed = text.trim();
    if matches!(trimmed, "/quit" | "/exit") {
        return AppAction::Exit;
    }
    if trimmed == "/cancel" {
        return AppAction::Cancel;
    }
    if trimmed == "/new" {
        return AppAction::NewDraft;
    }
    if trimmed.split_whitespace().next() == Some("/new") {
        return AppAction::NewUsage;
    }
    if trimmed == "/sessions" {
        return AppAction::OpenSessionBrowser;
    }
    if trimmed == "/model" {
        return AppAction::OpenModelSelector;
    }
    if let Some(query) = trimmed.strip_prefix("/model ").map(str::trim) {
        return best_model_match(query, available_models(projection)).map_or_else(
            || AppAction::ModelNotFound(query.to_owned()),
            |model| AppAction::SelectModel(model.id.clone()),
        );
    }
    if trimmed == "/effort" {
        return AppAction::OpenEffortSelector;
    }
    if let Some(query) = trimmed.strip_prefix("/effort ").map(str::trim) {
        return ReasoningLevel::parse(query)
            .filter(|level| available_efforts(projection).contains(level))
            .map_or_else(
                || AppAction::EffortNotFound(query.to_owned()),
                AppAction::SelectEffort,
            );
    }
    if trimmed == "/ps" {
        return AppAction::OpenProcessSelector;
    }
    if trimmed == "/kill" {
        return AppAction::KillUsage;
    }
    if let Some(job) = trimmed.strip_prefix("/kill ") {
        return AppAction::Kill(job.trim().to_owned());
    }
    AppAction::Submit(text)
}

fn refresh_menu(ui: &mut UiState, projection: &ChatProjection) {
    let value = ui.editor().value().to_owned();
    let kind = ui.menu().kind();
    match kind {
        Some(MenuKind::Models) => ui.menu_mut().set(
            MenuKind::Models,
            matching_models(&value, available_models(projection))
                .into_iter()
                .map(|model| MenuItem::new(&model.id, &model.description)),
        ),
        Some(MenuKind::Efforts) => {
            let query = normalize_model_query(&value);
            ui.menu_mut().set(
                MenuKind::Efforts,
                available_efforts(projection)
                    .into_iter()
                    .filter(|effort| {
                        query.is_empty()
                            || fuzzy_subsequence_score(&query, effort.as_str()).is_some()
                    })
                    .map(|effort| MenuItem::new(effort.as_str(), "Supported by the active model")),
            );
        }
        Some(MenuKind::Processes) => ui.menu_mut().set(
            MenuKind::Processes,
            running_jobs(projection)
                .into_iter()
                .map(|job| MenuItem::new(&job.id, &job.title)),
        ),
        Some(MenuKind::Commands) | None => {
            if value.starts_with('/') && !value.chars().any(char::is_whitespace) {
                let items = COMMANDS
                    .iter()
                    .filter_map(|command| {
                        let name = if matches!(command.action, CommandAction::Quit)
                            && value.starts_with("/e")
                        {
                            "/exit"
                        } else {
                            command.name
                        };
                        name.starts_with(&value)
                            .then(|| MenuItem::new(name, command.description))
                    })
                    .collect::<Vec<_>>();
                if items.is_empty() {
                    ui.menu_mut().clear();
                } else {
                    ui.menu_mut().set(MenuKind::Commands, items);
                }
            } else {
                ui.menu_mut().clear();
            }
        }
    }
    ui.request_redraw();
}

fn command_action(value: &str) -> AppAction {
    if value == "/exit" {
        return AppAction::Exit;
    }
    COMMANDS
        .iter()
        .find(|command| command.name == value)
        .map_or(AppAction::None, |command| match command.action {
            CommandAction::Quit => AppAction::Exit,
            CommandAction::Cancel => AppAction::Cancel,
            CommandAction::Model => AppAction::OpenModelSelector,
            CommandAction::Effort => AppAction::OpenEffortSelector,
            CommandAction::Processes => AppAction::OpenProcessSelector,
            CommandAction::Kill => AppAction::KillUsage,
            CommandAction::New => AppAction::NewDraft,
            CommandAction::Sessions => AppAction::OpenSessionBrowser,
        })
}

fn control_shortcut(key: &KeyEvent, character: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&character))
}

fn is_selector(kind: Option<MenuKind>) -> bool {
    matches!(
        kind,
        Some(MenuKind::Models | MenuKind::Efforts | MenuKind::Processes)
    )
}

fn open_selector(ui: &mut UiState, projection: &ChatProjection, kind: MenuKind) {
    ui.editor_mut().clear();
    ui.menu_mut().set(kind, std::iter::empty());
    refresh_menu(ui, projection);
}

fn close_selector(ui: &mut UiState, projection: &ChatProjection) {
    ui.menu_mut().clear();
    ui.editor_mut().clear();
    refresh_menu(ui, projection);
}

fn clear_command_entry(ui: &mut UiState, projection: &ChatProjection) {
    ui.menu_mut().clear();
    ui.editor_mut().clear();
    refresh_menu(ui, projection);
}

async fn handle_menu_event<C: WorkspaceClient>(
    ui: &mut UiState,
    client: &mut C,
    key: &KeyEvent,
) -> Result<Option<bool>, AppError> {
    if !ui.menu().is_open() {
        return Ok(None);
    }
    match key.code {
        KeyCode::Up => {
            if key.modifiers == KeyModifiers::NONE {
                ui.menu_mut().select_previous();
                ui.request_redraw();
            }
            Ok(Some(true))
        }
        KeyCode::Down => {
            if key.modifiers == KeyModifiers::NONE {
                ui.menu_mut().select_next();
                ui.request_redraw();
            }
            Ok(Some(true))
        }
        _ if key.modifiers != KeyModifiers::NONE => Ok(None),
        KeyCode::Tab => {
            if let Some(value) = ui.menu().selected_value().map(str::to_owned) {
                ui.editor_mut().set_value(value);
                refresh_menu(ui, client.current_projection());
            }
            Ok(Some(true))
        }
        KeyCode::Enter => {
            let Some(value) = ui.menu().selected_value().map(str::to_owned) else {
                return Ok(Some(true));
            };
            let action = match ui.menu().kind() {
                Some(MenuKind::Commands) => command_action(&value),
                Some(MenuKind::Models) => AppAction::SelectModel(value),
                Some(MenuKind::Efforts) => {
                    ReasoningLevel::parse(&value).map_or(AppAction::None, AppAction::SelectEffort)
                }
                Some(MenuKind::Processes) => AppAction::PrepareKill(value),
                None => AppAction::None,
            };
            Ok(Some(apply_action(ui, client, action).await?))
        }
        _ => Ok(None),
    }
}

async fn handle_event<C: WorkspaceClient>(
    ui: &mut UiState,
    client: &mut C,
    event: Event,
) -> Result<bool, AppError> {
    if matches!(
        event,
        Event::Key(KeyEvent {
            kind: KeyEventKind::Release,
            ..
        })
    ) {
        return Ok(true);
    }

    if let Event::Key(key) = &event
        && control_shortcut(key, 'c')
    {
        return Ok(false);
    }
    if matches!(event, Event::Resize(_, _)) {
        ui.request_redraw();
        return Ok(true);
    }
    if ui.session_browser().is_open() {
        let action = ui.session_browser_mut().handle_event(&event);
        ui.request_redraw();
        return match action {
            BrowserAction::None => Ok(true),
            BrowserAction::Refresh => Ok(true),
            BrowserAction::Switch(session_id) => {
                apply_action(ui, client, AppAction::SwitchSession(session_id)).await
            }
            BrowserAction::Rename { session_id, title } => {
                apply_action(ui, client, AppAction::RenameSession { session_id, title }).await
            }
            BrowserAction::Delete(session_id) => {
                apply_action(ui, client, AppAction::DeleteSession(session_id)).await
            }
        };
    }

    if let Event::Key(key) = &event {
        if control_shortcut(key, 'o') {
            return apply_action(ui, client, AppAction::OpenHelp).await;
        }
        if control_shortcut(key, 't') {
            return apply_action(ui, client, AppAction::ToggleSidebar).await;
        }
        if key.code == KeyCode::Esc && ui.popup().is_some() {
            ui.set_popup(None);
            return Ok(true);
        }
    }

    match &event {
        Event::Key(key) if key.code == KeyCode::PageUp => {
            ui.scroll_mut().page_up();
            ui.request_redraw();
            return Ok(true);
        }
        Event::Key(key) if key.code == KeyCode::PageDown => {
            ui.scroll_mut().page_down();
            ui.request_redraw();
            return Ok(true);
        }
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
            ui.scroll_mut().wheel_up();
            ui.request_redraw();
            return Ok(true);
        }
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
            ui.scroll_mut().wheel_down();
            ui.request_redraw();
            return Ok(true);
        }
        _ => {}
    }

    if let Event::Key(key) = &event
        && key.code == KeyCode::End
        && ui.editor().at_final_line_end()
        && ui.popup().is_none()
        && !ui.menu().is_open()
    {
        ui.scroll_mut().follow_latest();
        ui.request_redraw();
        return Ok(true);
    }

    if let Event::Key(key) = &event
        && key.code == KeyCode::Esc
    {
        if is_selector(ui.menu().kind()) {
            close_selector(ui, client.current_projection());
            return Ok(true);
        }
        if is_busy(client.current_projection()) {
            return apply_action(ui, client, AppAction::Cancel).await;
        }
    }

    if let Event::Key(key) = &event {
        let selector_is_process = ui.menu().kind() == Some(MenuKind::Processes);
        let selector_guard =
            is_busy(client.current_projection()) || ui.help_open() || selector_is_process;
        if control_shortcut(key, 'l') {
            if !selector_guard {
                return apply_action(ui, client, AppAction::OpenModelSelector).await;
            }
            return Ok(true);
        }
        if control_shortcut(key, 'r') {
            if !selector_guard {
                return apply_action(ui, client, AppAction::OpenEffortSelector).await;
            }
            return Ok(true);
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && key.modifiers == KeyModifiers::SHIFT
        {
            if !selector_guard {
                return apply_action(ui, client, AppAction::CycleEffort).await;
            }
            return Ok(true);
        }
    }

    if ui.popup().is_some() {
        return Ok(true);
    }
    if let Event::Key(key) = &event
        && let Some(running) = handle_menu_event(ui, client, key).await?
    {
        return Ok(running);
    }

    match ui.editor_mut().handle_event(&event) {
        EditorOutcome::Ignored | EditorOutcome::Consumed => {}
        EditorOutcome::Changed => {
            refresh_menu(ui, client.current_projection());
            ui.request_redraw();
        }
        EditorOutcome::Submitted(text) => {
            ui.request_redraw();
            let action = resolve_submission(ui, client.current_projection(), text);
            return apply_action(ui, client, action).await;
        }
    }
    Ok(true)
}

async fn apply_action<C: WorkspaceClient>(
    ui: &mut UiState,
    client: &mut C,
    action: AppAction,
) -> Result<bool, AppError> {
    match action {
        AppAction::None => {}
        AppAction::Submit(text)
            if is_draft(client.current_projection()) && text.trim().is_empty() => {}
        AppAction::Submit(text) if is_busy(client.current_projection()) => {
            ui.editor_mut().set_value(text);
            refresh_menu(ui, client.current_projection());
        }
        AppAction::Submit(text) => {
            let restore_draft_prompt = is_draft(client.current_projection());
            match client.submit(&text).await {
                Ok(_) if restore_draft_prompt => ui.chat_transition_reset(),
                Ok(_) => {}
                Err(error) => {
                    if restore_draft_prompt {
                        ui.editor_mut().set_value(text);
                        refresh_menu(ui, client.current_projection());
                    }
                    ui.push_error(error.to_string());
                }
            }
        }
        AppAction::Cancel => {
            if is_draft(client.current_projection()) {
                clear_command_entry(ui, client.current_projection());
                ui.push_notice("No running request.");
            } else if let Err(error) = client.cancel().await {
                ui.push_error(error.to_string());
            }
        }
        AppAction::NewDraft => match client.new_draft().await {
            Ok(()) => ui.chat_transition_reset(),
            Err(error) => ui.push_error(error.to_string()),
        },
        AppAction::NewUsage => ui.push_error("Usage: /new"),
        AppAction::OpenSessionBrowser => {
            clear_command_entry(ui, client.current_projection());
            ui.session_browser_mut().open();
        }
        AppAction::SwitchSession(session_id) => match client.switch_session(session_id).await {
            Ok(()) => ui.chat_transition_reset(),
            Err(error) => ui
                .session_browser_mut()
                .set_action_error(sanitize_line(&error.to_string())),
        },
        AppAction::RenameSession { session_id, title } => {
            let title = match SessionTitle::parse(title) {
                Ok(title) => title,
                Err(error) => {
                    ui.session_browser_mut()
                        .set_rename_error(sanitize_line(&error.to_string()));
                    return Ok(true);
                }
            };
            match client.rename_session(session_id, title).await {
                Ok(()) => ui.session_browser_mut().finish_rename(),
                Err(error) => ui
                    .session_browser_mut()
                    .set_rename_error(sanitize_line(&error.to_string())),
            }
        }
        AppAction::DeleteSession(session_id) => {
            let deleting_current = current_session_id(client.current_projection())
                .is_some_and(|current| current == session_id);
            match client.delete_session(session_id).await {
                Ok(()) if deleting_current => ui.chat_transition_reset(),
                Ok(()) => ui.session_browser_mut().finish_delete(session_id),
                Err(error) => ui
                    .session_browser_mut()
                    .set_action_error(sanitize_line(&error.to_string())),
            }
        }
        AppAction::OpenModelSelector => {
            open_selector(ui, client.current_projection(), MenuKind::Models);
        }
        AppAction::OpenEffortSelector => {
            open_selector(ui, client.current_projection(), MenuKind::Efforts);
        }
        AppAction::OpenHelp => ui.set_popup(Some(PopupKind::Help)),
        AppAction::ToggleSidebar => ui.toggle_sidebar(),
        AppAction::SelectModel(model) => {
            close_selector(ui, client.current_projection());
            let draft = is_draft(client.current_projection());
            match client.select_model(model).await {
                Ok(()) if draft => ui.clear_local_error(),
                Ok(()) => {}
                Err(error) => ui.push_error(error.to_string()),
            }
        }
        AppAction::ModelNotFound(query) => {
            close_selector(ui, client.current_projection());
            ui.push_error(format!(
                "No available model matches `{}`.",
                sanitize_line(&query)
            ));
        }
        AppAction::SelectEffort(effort) => {
            close_selector(ui, client.current_projection());
            let draft = is_draft(client.current_projection());
            match client.select_reasoning(effort).await {
                Ok(()) if draft => ui.clear_local_error(),
                Ok(()) => {}
                Err(error) => ui.push_error(error.to_string()),
            }
        }
        AppAction::EffortNotFound(query) => {
            close_selector(ui, client.current_projection());
            ui.push_error(format!(
                "Reasoning effort `{}` is not supported by the active model.",
                sanitize_line(&query)
            ));
        }
        AppAction::CycleEffort => {
            let efforts = available_efforts(client.current_projection());
            let current = settings(client.current_projection()).reasoning;
            if let Some(position) = efforts.iter().position(|effort| *effort == current)
                && let Some(next) = efforts.get((position + 1) % efforts.len())
            {
                let draft = is_draft(client.current_projection());
                match client.select_reasoning(*next).await {
                    Ok(()) if draft => ui.clear_local_error(),
                    Ok(()) => {}
                    Err(error) => ui.push_error(error.to_string()),
                }
            }
        }
        AppAction::OpenProcessSelector => {
            if is_draft(client.current_projection()) {
                clear_command_entry(ui, client.current_projection());
                ui.push_notice("No running background processes.");
            } else {
                match client.list_jobs().await {
                    Ok(jobs) => {
                        ui.editor_mut().clear();
                        let items = jobs
                            .iter()
                            .filter(|job| job.state == JobState::Running)
                            .map(|job| MenuItem::new(&job.id, &job.title))
                            .collect::<Vec<_>>();
                        if items.is_empty() {
                            ui.menu_mut().clear();
                            ui.push_notice("No running background processes.");
                        } else {
                            ui.menu_mut().set(MenuKind::Processes, items);
                            ui.request_redraw();
                        }
                    }
                    Err(error) => ui.push_error(error.to_string()),
                }
            }
        }
        AppAction::PrepareKill(job) => {
            ui.menu_mut().clear();
            ui.editor_mut().set_value(format!("/kill {job}"));
            refresh_menu(ui, client.current_projection());
        }
        AppAction::Kill(job) => {
            if is_draft(client.current_projection()) {
                ui.push_notice("No running background processes.");
            } else {
                match client.cancel_job(job).await {
                    Ok(snapshot) => {
                        let message = if snapshot.state == JobState::Cancelled {
                            format!("Terminated {}.", snapshot.id)
                        } else {
                            format!("{} is already {}.", snapshot.id, snapshot.state)
                        };
                        ui.push_notice(message);
                    }
                    Err(error) => ui.push_error(error.to_string()),
                }
            }
        }
        AppAction::KillUsage if is_draft(client.current_projection()) => {
            clear_command_entry(ui, client.current_projection());
            ui.push_notice("No running background processes.");
        }
        AppAction::KillUsage => ui.push_error("Usage: /kill job-N"),
        AppAction::Exit => return Ok(false),
    }
    Ok(true)
}

fn consume_workspace_update(
    ui: &mut UiState,
    projection: &ChatProjection,
    update: WorkspaceUpdate,
) -> Result<(), AppError> {
    match update {
        WorkspaceUpdate::Warning(warning) => ui.push_notice(warning),
        WorkspaceUpdate::Deleted { .. } => ui.chat_transition_reset(),
        WorkspaceUpdate::Session(SessionUpdate::SnapshotReplaced(_)) => {
            ui.authoritative_reset();
            refresh_menu(ui, projection);
        }
        WorkspaceUpdate::Session(SessionUpdate::Warning(warning)) => ui.push_notice(warning),
        WorkspaceUpdate::Session(SessionUpdate::Event(envelope)) => match envelope.event {
            SessionEvent::Deleted { .. } => {
                return Err(AppError::Projection(
                    "deleted event bypassed workspace fallback",
                ));
            }
            SessionEvent::Started { .. }
            | SessionEvent::Completed { .. }
            | SessionEvent::Failed { .. }
            | SessionEvent::Cancelled { .. }
            | SessionEvent::PersistenceWarning(_) => ui.clear_local_error(),
            SessionEvent::SettingsChanged { .. } | SessionEvent::CatalogChanged(_) => {
                ui.clear_local_error();
                refresh_menu(ui, projection);
            }
            SessionEvent::JobsChanged(_) => {
                if ui.menu().kind() == Some(MenuKind::Processes) {
                    refresh_menu(ui, projection);
                }
            }
            SessionEvent::PlanChanged(_)
            | SessionEvent::TitleChanged { .. }
            | SessionEvent::AssistantDelta { .. }
            | SessionEvent::ContextUsage { .. }
            | SessionEvent::ToolStarted { .. }
            | SessionEvent::ToolFinished { .. } => {}
        },
        WorkspaceUpdate::Session(SessionUpdate::Deleted { .. }) => {
            return Err(AppError::Projection(
                "deleted update bypassed workspace fallback",
            ));
        }
    }
    ui.request_redraw();
    Ok(())
}

// The scripted fixed-session fixture uses this reducer to emulate Task 11's
// authoritative workspace projection. The production event loop never calls it.
#[cfg(test)]
fn apply_session_update(
    ui: &mut UiState,
    projection: &mut SessionSnapshot,
    update: SessionUpdate,
) -> Result<(), AppError> {
    match update {
        SessionUpdate::SnapshotReplaced(snapshot) => {
            let snapshot = *snapshot;
            validate_snapshot(&snapshot)?;
            *projection = snapshot;
            ui.authoritative_reset();
            refresh_menu(ui, &ChatProjection::session(projection.clone()));
        }
        SessionUpdate::Event(envelope) => apply_session_event(ui, projection, envelope)?,
        SessionUpdate::Warning(warning) => ui.push_notice(warning),
        SessionUpdate::Deleted { .. } => return Err(AppError::SessionDeleted),
    }
    Ok(())
}

#[cfg(test)]
fn validate_snapshot(snapshot: &SessionSnapshot) -> Result<(), AppError> {
    if snapshot.busy != snapshot.summary.busy || snapshot.busy != snapshot.active_run.is_some() {
        return Err(AppError::Projection("snapshot busy state is inconsistent"));
    }
    Ok(())
}

#[cfg(test)]
fn apply_session_event(
    ui: &mut UiState,
    projection: &mut SessionSnapshot,
    envelope: SessionEventEnvelope,
) -> Result<(), AppError> {
    if projection.sequence.checked_add(1) != Some(envelope.sequence) {
        return Err(AppError::Projection("event sequence is not contiguous"));
    }
    validate_event_run(projection, &envelope.event)?;
    let mut refresh = false;
    match envelope.event {
        SessionEvent::TitleChanged { .. } | SessionEvent::Deleted { .. } => {
            return Err(AppError::Projection(
                "session lifecycle event requires protocol v2",
            ));
        }
        SessionEvent::Started { run_id, prompt } => {
            projection
                .transcript
                .push(TranscriptItem::User(prompt.clone()));
            projection.active_run = Some(ActiveRunSnapshot {
                run_id,
                prompt,
                assistant_text: String::new(),
            });
            projection.busy = true;
            projection.summary.busy = true;
            ui.clear_local_error();
        }
        SessionEvent::AssistantDelta { text, .. } => {
            let active = projection
                .active_run
                .as_mut()
                .ok_or(AppError::Projection("run event arrived while idle"))?;
            active.assistant_text.push_str(&text);
        }
        SessionEvent::ContextUsage {
            input_tokens,
            last_activity,
            ..
        } => {
            projection.settings.context_tokens = input_tokens;
            projection.summary.last_activity = last_activity;
        }
        SessionEvent::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => projection.transcript.push(TranscriptItem::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        }),
        SessionEvent::ToolFinished { .. } => {}
        SessionEvent::Completed {
            response,
            last_activity,
            ..
        } => {
            projection
                .transcript
                .push(TranscriptItem::Assistant(response));
            projection.active_run = None;
            projection.busy = false;
            projection.summary.busy = false;
            projection.summary.last_activity = last_activity;
            ui.clear_local_error();
        }
        SessionEvent::Failed {
            run_id,
            mut failure,
        } => {
            failure.message = sanitize_line(&failure.message);
            projection
                .transcript
                .push(TranscriptItem::Failed { run_id, failure });
            projection.active_run = None;
            projection.busy = false;
            projection.summary.busy = false;
            ui.clear_local_error();
        }
        SessionEvent::Cancelled { run_id } => {
            projection
                .transcript
                .push(TranscriptItem::Cancelled { run_id });
            projection.active_run = None;
            projection.busy = false;
            projection.summary.busy = false;
            ui.clear_local_error();
        }
        SessionEvent::SettingsChanged {
            settings,
            last_activity,
        } => {
            projection.settings = settings;
            projection.summary.last_activity = last_activity;
            ui.clear_local_error();
            refresh = true;
        }
        SessionEvent::PlanChanged(plan) => projection.plan = plan,
        SessionEvent::JobsChanged(jobs) => {
            projection.jobs = jobs;
            refresh = ui.menu().kind() == Some(MenuKind::Processes);
        }
        SessionEvent::CatalogChanged(catalog) => {
            projection.catalog = catalog;
            ui.clear_local_error();
            refresh = true;
        }
        SessionEvent::PersistenceWarning(warning) => {
            projection.persistence_warning = warning;
            ui.clear_local_error();
        }
    }
    projection.sequence = envelope.sequence;
    if refresh {
        refresh_menu(ui, &ChatProjection::session(projection.clone()));
    }
    ui.request_redraw();
    Ok(())
}

#[cfg(test)]
fn validate_event_run(projection: &SessionSnapshot, event: &SessionEvent) -> Result<(), AppError> {
    match event {
        SessionEvent::Started { .. } if projection.active_run.is_some() => Err(
            AppError::Projection("a run started while another was active"),
        ),
        SessionEvent::Started { .. }
        | SessionEvent::TitleChanged { .. }
        | SessionEvent::SettingsChanged { .. }
        | SessionEvent::PlanChanged(_)
        | SessionEvent::JobsChanged(_)
        | SessionEvent::CatalogChanged(_)
        | SessionEvent::PersistenceWarning(_)
        | SessionEvent::Deleted { .. } => Ok(()),
        SessionEvent::AssistantDelta { run_id, .. }
        | SessionEvent::ContextUsage { run_id, .. }
        | SessionEvent::ToolStarted { run_id, .. }
        | SessionEvent::ToolFinished { run_id, .. }
        | SessionEvent::Completed { run_id, .. }
        | SessionEvent::Failed { run_id, .. }
        | SessionEvent::Cancelled { run_id } => match &projection.active_run {
            Some(active) if active.run_id == *run_id => Ok(()),
            Some(_) => Err(AppError::Projection("event run ID does not match")),
            None => Err(AppError::Projection("run event arrived while idle")),
        },
    }
}

fn draw_if_needed<B: Backend>(
    terminal: &mut Terminal<B>,
    ui: &mut UiState,
    projection: &ChatProjection,
) -> Result<(), AppError>
where
    B::Error: Into<AppError>,
{
    if !ui.take_redraw() {
        return Ok(());
    }
    terminal
        .draw(|frame| render(frame, projection, ui))
        .map_err(Into::into)?;
    Ok(())
}

async fn run_event_loop<B, E, C>(
    terminal: &mut Terminal<B>,
    ui: &mut UiState,
    events: &mut E,
    client: &mut C,
) -> Result<(), AppError>
where
    B: Backend,
    B::Error: Into<AppError>,
    E: EventSource,
    C: WorkspaceClient,
{
    enum LoopWake {
        Update(WorkspaceUpdate),
        RefreshBrowser(Result<Vec<moh::session::SessionSummary>, ClientSessionError>),
        Frame,
    }

    let mut running = true;
    let mut next_browser_refresh = None;
    let mut pending_browser_refresh: Option<PendingSessionBrowserRefresh> = None;
    while running {
        draw_if_needed(terminal, ui, client.current_projection())?;
        let timeout = if is_busy(client.current_projection()) {
            Duration::ZERO
        } else {
            Duration::from_millis(16)
        };
        if let Some(event) = events.poll_event(timeout)? {
            running = handle_event(ui, client, event).await?;
        }
        if !running {
            break;
        }
        draw_if_needed(terminal, ui, client.current_projection())?;

        if !ui.session_browser().is_open() {
            next_browser_refresh = None;
            pending_browser_refresh = None;
        } else {
            let now = tokio::time::Instant::now();
            let next_due = *next_browser_refresh.get_or_insert(now + Duration::from_secs(1));
            let immediate = ui.session_browser_mut().take_refresh_request();
            if immediate {
                pending_browser_refresh = None;
            }
            if pending_browser_refresh.is_none() && (immediate || now >= next_due) {
                pending_browser_refresh = Some(start_session_browser_refresh(ui, client));
                next_browser_refresh = Some(now + Duration::from_secs(1));
            }
        }

        let wake = if let Some(refresh) = pending_browser_refresh.as_mut() {
            tokio::select! {
                biased; // Poll refresh first so a ready observer cannot starve it.
                result = &mut refresh.future => LoopWake::RefreshBrowser(result),
                update = client.next_update() => LoopWake::Update(update?),
                () = tokio::time::sleep(Duration::from_millis(16)) => LoopWake::Frame,
            }
        } else {
            tokio::select! {
                update = client.next_update() => LoopWake::Update(update?),
                () = tokio::time::sleep(Duration::from_millis(16)) => LoopWake::Frame,
            }
        };
        match wake {
            LoopWake::Update(update) => {
                consume_workspace_update(ui, client.current_projection(), update)?;
            }
            LoopWake::RefreshBrowser(result) => {
                let refresh = pending_browser_refresh
                    .take()
                    .expect("completed browser refresh is pending");
                if ui.session_browser().is_open() {
                    finish_session_browser_refresh(ui, &refresh.cwd, result);
                }
            }
            LoopWake::Frame => {}
        }
    }
    draw_if_needed(terminal, ui, client.current_projection())?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AppError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Session(#[from] ClientSessionError),
    #[error("invalid backend session update: {0}")]
    Projection(&'static str),
    #[cfg(test)]
    #[error("the attached backend session was deleted")]
    SessionDeleted,
    #[error("{application}; terminal cleanup also failed: {cleanup}")]
    ApplicationAndCleanup {
        application: Box<AppError>,
        cleanup: io::Error,
    },
}

impl From<Infallible> for AppError {
    fn from(error: Infallible) -> Self {
        match error {}
    }
}

fn restore_after_application(
    application: Result<(), AppError>,
    restore: impl FnOnce() -> io::Result<()>,
) -> Result<(), AppError> {
    let cleanup = restore();
    match (application, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(application), Ok(())) => Err(application),
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
        (Err(application), Err(cleanup)) => Err(AppError::ApplicationAndCleanup {
            application: Box::new(application),
            cleanup,
        }),
    }
}

pub(crate) async fn run<C: WorkspaceClient>(client: &mut C) -> Result<(), AppError> {
    let (mut terminal, mut session) = TerminalSession::start()?;
    let mut ui = UiState::new();
    let mut events = CrosstermEvents;
    let application = run_event_loop(&mut terminal, &mut ui, &mut events, client).await;
    restore_after_application(application, || session.restore())
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
