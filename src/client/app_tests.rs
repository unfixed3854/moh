use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io,
    rc::Rc,
    time::Duration,
};

use chrono::{TimeZone, Utc};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use moh::{
    harness::{RunFailureKind, RunStage},
    rpc::client::SessionUpdate,
    runtime::rig::ReasoningLevel,
    session::{
        ActiveRunSnapshot, JobSnapshotDto, ModelCatalogState, ModelInfoDto, RunFailureSnapshot,
        SessionEvent, SessionEventEnvelope, SessionId, SessionListScope, SessionSelector,
        SessionSettings, SessionSnapshot, SessionSummary, SessionTitle, TranscriptItem,
    },
    tools::{JobKind, JobState},
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};
use serde_json::json;

use super::*;
use crate::client::{
    ChatProjection, DraftState, LaunchMode, WorkspaceClient, WorkspaceUpdate,
    ui::{BrowserAction, BrowserLayer, BrowserMode, BrowserRow, SessionBrowserState},
    workspace::{
        WorkspaceBackend, WorkspaceController, WorkspaceMaterialized, WorkspaceSession,
        WorkspaceStartup,
    },
};

const TEST_MODEL: &str = "test-model";

fn models() -> Vec<ModelInfoDto> {
    vec![
        ModelInfoDto {
            id: TEST_MODEL.into(),
            display_name: "Test model".into(),
            description: "Test-only model".into(),
            reasoning_efforts: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            default_reasoning: Some(ReasoningLevel::Medium),
        },
        ModelInfoDto {
            id: "gpt-5.6-terra".into(),
            display_name: "GPT-5.6-Terra".into(),
            description: "Balanced model".into(),
            reasoning_efforts: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_reasoning: Some(ReasoningLevel::Medium),
        },
    ]
}

fn activity_time() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 27, 12, 1, 0).unwrap()
}

fn running_job() -> JobSnapshotDto {
    JobSnapshotDto {
        id: "job-3".into(),
        kind: JobKind::Bash,
        state: JobState::Running,
        title: "cargo test".into(),
        started_at: activity_time(),
        completed_at: None,
        details: "running cargo test".into(),
    }
}

fn snapshot_fixture(busy: bool) -> SessionSnapshot {
    let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
    let active_run = busy.then(|| ActiveRunSnapshot {
        run_id: 9,
        prompt: "active prompt".into(),
        assistant_text: "partial answer".into(),
    });
    let mut transcript = vec![
        TranscriptItem::User("first prompt".into()),
        TranscriptItem::Assistant("first answer".into()),
        TranscriptItem::User("second prompt".into()),
        TranscriptItem::Assistant("second answer".into()),
    ];
    if busy {
        transcript.extend([
            TranscriptItem::User("active prompt".into()),
            TranscriptItem::ToolStarted {
                run_id: 9,
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: json!({
                    "path": "/work/moh/src/lib.rs",
                    "offset": 1,
                    "limit": 2
                }),
            },
        ]);
    }
    SessionSnapshot {
        summary: SessionSummary {
            id: "session-7".parse().unwrap(),
            title: SessionTitle::parse("fixture chat").unwrap(),
            title_revision: 0,
            cwd: b"/work/moh".to_vec(),
            cwd_display: "/work/moh".into(),
            running_jobs: u32::from(busy),
            running: busy,
            busy,
            attached_clients: 1,
            last_activity: now,
        },
        transcript,
        active_run,
        settings: SessionSettings {
            model: TEST_MODEL.into(),
            reasoning: ReasoningLevel::High,
            context_tokens: 128_000,
        },
        catalog: ModelCatalogState::Ready(models()),
        plan: Vec::new(),
        jobs: vec![running_job()],
        persistence_warning: None,
        sequence: 14,
        busy,
    }
}

fn browser_summary(
    id: &str,
    title: &str,
    cwd: &[u8],
    cwd_display: &str,
    activity_seconds: i64,
) -> SessionSummary {
    SessionSummary {
        id: id.parse().unwrap(),
        title: SessionTitle::parse(title).unwrap(),
        title_revision: 0,
        cwd: cwd.to_vec(),
        cwd_display: cwd_display.into(),
        running_jobs: 0,
        running: false,
        busy: false,
        attached_clients: 0,
        last_activity: activity_time() + chrono::Duration::seconds(activity_seconds),
    }
}

struct ScriptedEvents {
    events: VecDeque<io::Result<Event>>,
}

impl EventSource for ScriptedEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        match self.events.pop_front() {
            Some(Ok(event)) => Ok(Some(event)),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        }
    }
}

#[derive(Default)]
struct ScriptedSessionState {
    updates: VecDeque<Result<SessionUpdate, ClientSessionError>>,
    submission_scripts: VecDeque<Vec<SessionEvent>>,
    next_sequence: u64,
    active_run_id: Option<u64>,
    submissions: Vec<String>,
    cancel_count: usize,
    selected_models: Vec<String>,
    selected_reasoning: Vec<ReasoningLevel>,
    list_count: usize,
    listed_jobs: Vec<JobSnapshotDto>,
    cancelled_jobs: Vec<String>,
    submit_error: Option<ClientSessionError>,
    session_lists: VecDeque<Result<Vec<SessionSummary>, ClientSessionError>>,
    session_list_scopes: Vec<SessionListScope>,
    pending_session_lists: bool,
    active_session_lists: usize,
    max_active_session_lists: usize,
    dropped_session_lists: usize,
    continuous_observer_updates: bool,
    observer_update_count: usize,
}

struct ActiveSessionList {
    state: Rc<RefCell<ScriptedSessionState>>,
}

impl Drop for ActiveSessionList {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.active_session_lists -= 1;
        state.dropped_session_lists += 1;
    }
}

#[derive(Clone)]
struct ScriptedSessionClient {
    projection: ChatProjection,
    state: Rc<RefCell<ScriptedSessionState>>,
}

impl ScriptedSessionClient {
    fn new(snapshot: SessionSnapshot) -> Self {
        let listed_jobs = snapshot.jobs.clone();
        let next_sequence = snapshot.sequence;
        let active_run_id = snapshot.active_run.as_ref().map(|run| run.run_id);
        Self {
            projection: ChatProjection::session(snapshot),
            state: Rc::new(RefCell::new(ScriptedSessionState {
                listed_jobs,
                next_sequence,
                active_run_id,
                ..ScriptedSessionState::default()
            })),
        }
    }

    fn busy() -> Self {
        Self::new(snapshot_fixture(true))
    }

    fn idle() -> Self {
        Self::new(snapshot_fixture(false))
    }

    fn queue_event(&self, event: SessionEvent) {
        let mut state = self.state.borrow_mut();
        match &event {
            SessionEvent::Started { run_id, .. } => state.active_run_id = Some(*run_id),
            SessionEvent::Completed { .. }
            | SessionEvent::Failed { .. }
            | SessionEvent::Cancelled { .. } => state.active_run_id = None,
            _ => {}
        }
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state
            .updates
            .push_back(Ok(SessionUpdate::Event(SessionEventEnvelope {
                sequence,
                event,
            })));
    }

    fn queue_update(&self, update: SessionUpdate) {
        self.state.borrow_mut().updates.push_back(Ok(update));
    }

    fn queue_error(&self, error: ClientSessionError) {
        self.state.borrow_mut().updates.push_back(Err(error));
    }

    fn script_submission(&self, events: impl IntoIterator<Item = SessionEvent>) {
        self.state
            .borrow_mut()
            .submission_scripts
            .push_back(events.into_iter().collect());
    }

    fn cancel_count(&self) -> usize {
        self.state.borrow().cancel_count
    }

    fn script_session_lists(
        &self,
        results: impl IntoIterator<Item = Result<Vec<SessionSummary>, ClientSessionError>>,
    ) {
        self.state.borrow_mut().session_lists.extend(results);
    }

    fn snapshot(&self) -> &SessionSnapshot {
        let ChatProjection::Session(snapshot) = &self.projection else {
            panic!("scripted fixed-session client unexpectedly became a draft");
        };
        snapshot
    }
}

impl WorkspaceClient for ScriptedSessionClient {
    fn current_projection(&self) -> &ChatProjection {
        &self.projection
    }

    async fn next_update(&mut self) -> Result<WorkspaceUpdate, ClientSessionError> {
        if let Some(update) = self.state.borrow_mut().updates.pop_front() {
            let update = update?;
            let ChatProjection::Session(snapshot) = &mut self.projection else {
                return Err(ClientSessionError::scripted(
                    "scripted session received an update while in a draft",
                ));
            };
            apply_session_update(&mut UiState::new(), snapshot, update.clone())
                .map_err(|error| ClientSessionError::scripted(error.to_string()))?;
            return Ok(WorkspaceUpdate::Session(update));
        }
        if self.state.borrow().continuous_observer_updates {
            self.state.borrow_mut().observer_update_count += 1;
            return Ok(WorkspaceUpdate::Warning("observer update".into()));
        }
        std::future::pending().await
    }

    async fn submit(&mut self, prompt: &str) -> Result<u64, ClientSessionError> {
        let (error, events) = {
            let mut state = self.state.borrow_mut();
            state.submissions.push(prompt.to_owned());
            (
                state.submit_error.clone(),
                state.submission_scripts.pop_front().unwrap_or_default(),
            )
        };
        if let Some(error) = error {
            return Err(error);
        }
        for event in events {
            self.queue_event(event);
        }
        Ok(101)
    }

    async fn cancel(&self) -> Result<(), ClientSessionError> {
        let run_id = {
            let mut state = self.state.borrow_mut();
            state.cancel_count += 1;
            state.active_run_id.take()
        };
        if let Some(run_id) = run_id {
            self.queue_event(SessionEvent::Cancelled { run_id });
        }
        Ok(())
    }

    async fn select_model(&mut self, model: String) -> Result<(), ClientSessionError> {
        self.state.borrow_mut().selected_models.push(model);
        Ok(())
    }

    async fn select_reasoning(
        &mut self,
        reasoning: ReasoningLevel,
    ) -> Result<(), ClientSessionError> {
        self.state.borrow_mut().selected_reasoning.push(reasoning);
        Ok(())
    }

    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError> {
        let mut state = self.state.borrow_mut();
        state.list_count += 1;
        Ok(state.listed_jobs.clone())
    }

    async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, ClientSessionError> {
        let mut state = self.state.borrow_mut();
        state.cancelled_jobs.push(id.clone());
        let mut job = state
            .listed_jobs
            .iter()
            .find(|job| job.id == id)
            .cloned()
            .ok_or_else(|| ClientSessionError::scripted("job was not found"))?;
        job.state = JobState::Cancelled;
        Ok(job)
    }

    async fn new_draft(&mut self) -> Result<(), ClientSessionError> {
        let snapshot = self.snapshot();
        self.projection = ChatProjection::Draft(DraftState {
            cwd: snapshot.summary.cwd.clone(),
            settings: SessionSettings {
                model: snapshot.settings.model.clone(),
                reasoning: snapshot.settings.reasoning,
                context_tokens: 0,
            },
            catalog: snapshot.catalog.clone(),
        });
        Ok(())
    }

    fn list_sessions(&self, scope: SessionListScope) -> SessionListFuture {
        let state = Rc::clone(&self.state);
        Box::pin(async move {
            let pending = {
                let mut state = state.borrow_mut();
                state.session_list_scopes.push(scope);
                state.active_session_lists += 1;
                state.max_active_session_lists = state
                    .max_active_session_lists
                    .max(state.active_session_lists);
                state.pending_session_lists
            };
            let _active = ActiveSessionList {
                state: Rc::clone(&state),
            };
            if pending {
                return std::future::pending().await;
            }
            state
                .borrow_mut()
                .session_lists
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        })
    }

    async fn switch_session(&mut self, _id: SessionId) -> Result<(), ClientSessionError> {
        Err(ClientSessionError::scripted("session was not found"))
    }

    async fn rename_session(
        &self,
        _id: SessionId,
        _title: SessionTitle,
    ) -> Result<(), ClientSessionError> {
        Ok(())
    }

    async fn delete_session(&mut self, _id: SessionId) -> Result<(), ClientSessionError> {
        Ok(())
    }

    async fn startup_fallback(&mut self, _cwd: Vec<u8>) -> Result<(), ClientSessionError> {
        Ok(())
    }
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn control(character: char) -> Event {
    Event::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn wheel(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
}

fn ignored_event() -> Event {
    Event::FocusGained
}

async fn run_client_with_events(
    client: ScriptedSessionClient,
    events: impl IntoIterator<Item = Event>,
) -> Result<
    (
        Terminal<TestBackend>,
        UiState,
        SessionSnapshot,
        ScriptedSessionClient,
    ),
    AppError,
> {
    let terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    run_client_with_terminal(client, terminal, events.into_iter().map(Ok)).await
}

async fn run_client_with_terminal(
    client: ScriptedSessionClient,
    mut terminal: Terminal<TestBackend>,
    events: impl IntoIterator<Item = io::Result<Event>>,
) -> Result<
    (
        Terminal<TestBackend>,
        UiState,
        SessionSnapshot,
        ScriptedSessionClient,
    ),
    AppError,
> {
    let mut ui = UiState::new();
    let mut events = ScriptedEvents {
        events: events.into_iter().collect(),
    };
    let mut driven = client.clone();
    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven).await?;
    let projection = driven.snapshot().clone();
    Ok((terminal, ui, projection, client))
}

async fn run_workspace_with_events<C: WorkspaceClient>(
    client: C,
    events: impl IntoIterator<Item = Event>,
) -> Result<(Terminal<TestBackend>, UiState, ChatProjection, C), AppError> {
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    let mut ui = UiState::new();
    let mut events = ScriptedEvents {
        events: events.into_iter().map(Ok).collect(),
    };
    let mut driven = client;
    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven).await?;
    let projection = driven.current_projection().clone();
    Ok((terminal, ui, projection, driven))
}

fn rendered(terminal: &Terminal<TestBackend>) -> String {
    terminal.backend().to_string()
}

fn find_rendered_cell(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    for (y, row) in buffer.content.chunks(width).enumerate() {
        for x in 0..row.len() {
            let suffix = row[x..]
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            if suffix.starts_with(needle) {
                return (x as u16, y as u16);
            }
        }
    }
    panic!("rendered frame did not contain {needle:?}");
}

fn status_row(terminal: &Terminal<TestBackend>) -> String {
    let area = terminal.backend().buffer().area;
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(area.width))
        .nth(usize::from(area.height.saturating_sub(1)))
        .unwrap()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn successful_run(
    run_id: u64,
    prompt: impl Into<String>,
    response: impl Into<String>,
) -> Vec<SessionEvent> {
    vec![
        SessionEvent::Started {
            run_id,
            prompt: prompt.into(),
        },
        SessionEvent::Completed {
            run_id,
            response: response.into(),
            last_activity: activity_time(),
        },
    ]
}

fn run_failure(message: impl Into<String>) -> RunFailureSnapshot {
    RunFailureSnapshot {
        stage: RunStage::ModelRequest,
        kind: RunFailureKind::HttpRejected { status: 500 },
        retryable: true,
        message: message.into(),
    }
}

fn input_events_for_updates(prompt: impl Into<String>, update_count: usize) -> Vec<Event> {
    let mut events = vec![Event::Paste(prompt.into()), key(KeyCode::Enter)];
    events.extend(std::iter::repeat_n(
        ignored_event(),
        update_count.saturating_sub(1),
    ));
    events.push(control('c'));
    events
}

#[test]
fn command_resolution_distinguishes_new_sessions_and_model_input() {
    let ui = UiState::new();
    let projection = ChatProjection::session(snapshot_fixture(false));

    assert_eq!(
        resolve_submission(&ui, &projection, "/new".into()),
        AppAction::NewDraft
    );
    assert_eq!(
        resolve_submission(&ui, &projection, "/new anything".into()),
        AppAction::NewUsage
    );
    assert_eq!(
        resolve_submission(&ui, &projection, "/sessions".into()),
        AppAction::OpenSessionBrowser
    );
    assert_eq!(
        resolve_submission(&ui, &projection, "/sessions anything".into()),
        AppAction::Submit("/sessions anything".into())
    );
    assert_eq!(
        resolve_submission(&ui, &projection, "/still-a-prompt".into()),
        AppAction::Submit("/still-a-prompt".into())
    );
}

#[test]
fn session_browser_state_open_resets_to_local_project_mode() {
    let current_cwd = b"/work/current";
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary("session-1", "Current chat", current_cwd, "/work/current", 1),
            browser_summary("session-2", "Other chat", b"/work/other", "/work/other", 2),
        ],
    );

    browser.open();
    browser.toggle_mode();
    browser.set_query("other");
    browser.start_rename();
    browser.close();
    browser.open();

    assert!(browser.is_open());
    assert_eq!(browser.mode(), BrowserMode::Project);
    assert_eq!(browser.query().value(), "");
    assert!(matches!(browser.layer(), BrowserLayer::List));
    assert_eq!(browser.selected_id(), Some("session-1".parse().unwrap()));
    assert_eq!(
        browser
            .visible_rows()
            .iter()
            .filter_map(BrowserRow::session_id)
            .collect::<Vec<_>>(),
        vec!["session-1".parse().unwrap()]
    );
}

#[test]
fn session_browser_state_project_filter_uses_exact_raw_cwd_before_fuzzy_display() {
    let current_cwd = b"/work/\xff";
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary("session-1", "Current raw path", current_cwd, "/work/�", 1),
            browser_summary(
                "session-2",
                "Different raw path",
                b"/work/\xfe",
                "/work/�",
                2,
            ),
        ],
    );
    browser.open();
    browser.set_query("work");

    assert_eq!(
        browser
            .visible_rows()
            .iter()
            .filter_map(BrowserRow::session_id)
            .collect::<Vec<_>>(),
        vec!["session-1".parse().unwrap()]
    );
}

#[test]
fn session_browser_state_global_groups_sort_and_navigation_skip_headings() {
    let current_cwd = b"/work/current";
    let project_b = b"/work/b";
    let project_c = b"/work/c";
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary(
                "session-2",
                "Current older",
                current_cwd,
                "/work/current",
                1,
            ),
            browser_summary(
                "session-4",
                "Current newer",
                current_cwd,
                "/work/current",
                1,
            ),
            browser_summary("session-9", "Project B", project_b, "/work/b", 5),
            browser_summary("session-7", "Project C", project_c, "/work/c", 5),
            browser_summary("session-6", "Project C older", project_c, "/work/c", 3),
        ],
    );
    browser.open();
    browser.toggle_mode();

    let rows = browser.visible_rows();
    assert!(matches!(&rows[0], BrowserRow::Group { cwd, .. } if cwd == current_cwd));
    assert_eq!(rows[1].session_id(), Some("session-4".parse().unwrap()));
    assert_eq!(rows[2].session_id(), Some("session-2".parse().unwrap()));
    assert!(matches!(&rows[3], BrowserRow::Group { cwd, .. } if cwd == project_b));
    assert_eq!(rows[4].session_id(), Some("session-9".parse().unwrap()));
    assert!(matches!(&rows[5], BrowserRow::Group { cwd, .. } if cwd == project_c));
    assert_eq!(rows[6].session_id(), Some("session-7".parse().unwrap()));
    assert_eq!(rows[7].session_id(), Some("session-6".parse().unwrap()));

    assert_eq!(browser.selected_id(), Some("session-4".parse().unwrap()));
    browser.select_next();
    assert_eq!(browser.selected_id(), Some("session-2".parse().unwrap()));
    browser.select_next();
    assert_eq!(browser.selected_id(), Some("session-9".parse().unwrap()));
    browser.select_previous();
    assert_eq!(browser.selected_id(), Some("session-2".parse().unwrap()));
}

#[test]
fn session_browser_state_fuzzy_filter_matches_title_id_and_lossy_cwd() {
    let current_cwd = b"/work/current";
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary(
                "session-12",
                "Investigate Postgres",
                current_cwd,
                "/work/current",
                3,
            ),
            browser_summary(
                "session-21",
                "Review parser",
                b"/srv/moh-worktree",
                "/srv/moh-worktree",
                2,
            ),
            browser_summary(
                "session-30",
                "Release notes",
                b"/tmp/release",
                "/tmp/release",
                1,
            ),
        ],
    );
    browser.open();
    browser.toggle_mode();

    for (query, expected) in [
        ("ivpg", "session-12"),
        ("ssn21", "session-21"),
        ("svmhwk", "session-21"),
    ] {
        browser.set_query(query);
        assert_eq!(
            browser.selected_id(),
            Some(expected.parse().unwrap()),
            "{query}"
        );
        assert_eq!(
            browser
                .visible_rows()
                .iter()
                .filter_map(BrowserRow::session_id)
                .collect::<Vec<_>>(),
            vec![expected.parse().unwrap()],
            "{query}"
        );
    }
}

#[test]
fn session_browser_state_refresh_preserves_selection_by_stable_id() {
    let current_cwd = b"/work/current";
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary("session-1", "One", current_cwd, "/work/current", 3),
            browser_summary("session-2", "Two", current_cwd, "/work/current", 2),
            browser_summary("session-3", "Three", current_cwd, "/work/current", 1),
        ],
    );
    browser.open();
    browser.select_next();
    assert_eq!(browser.selected_id(), Some("session-2".parse().unwrap()));

    browser.set_refresh_warning("refresh failed");
    assert_eq!(browser.warning(), Some("refresh failed"));
    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary("session-1", "One", current_cwd, "/work/current", 3),
            browser_summary("session-2", "Two updated", current_cwd, "/work/current", 5),
            browser_summary("session-3", "Three", current_cwd, "/work/current", 1),
        ],
    );

    assert_eq!(browser.warning(), None);
    assert_eq!(browser.selected_id(), Some("session-2".parse().unwrap()));
    assert_eq!(
        browser.selected_summary().unwrap().title.as_str(),
        "Two updated"
    );

    browser.set_sessions(
        current_cwd,
        vec![
            browser_summary("session-1", "One", current_cwd, "/work/current", 3),
            browser_summary("session-3", "Three", current_cwd, "/work/current", 1),
        ],
    );
    assert_eq!(browser.selected_id(), Some("session-1".parse().unwrap()));
}

#[test]
fn session_browser_action_error_clear_policy_is_explicit_user_input_or_reopen() {
    let current_cwd = b"/work/current";
    let sessions = vec![browser_summary(
        "session-1",
        "Current",
        current_cwd,
        "/work/current",
        1,
    )];
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(current_cwd, sessions.clone());
    browser.open();
    browser.set_refresh_warning("refresh failed");
    browser.set_action_error("switch failed");

    browser.set_sessions(current_cwd, sessions.clone());

    assert_eq!(browser.refresh_warning(), None);
    assert_eq!(browser.action_error(), Some("switch failed"));

    browser.handle_event(&key(KeyCode::Down));
    assert_eq!(browser.action_error(), None);

    browser.set_action_error("delete failed");
    browser.close();
    assert_eq!(browser.action_error(), None);

    browser.set_action_error("stale while closed");
    browser.open();
    assert_eq!(browser.action_error(), None);
}

#[test]
fn session_browser_state_page_and_wheel_navigation_clamp_to_selectable_rows() {
    let current_cwd = b"/work/current";
    let sessions = (1..=8)
        .map(|id| {
            browser_summary(
                &format!("session-{id}"),
                &format!("Session {id}"),
                current_cwd,
                "/work/current",
                0,
            )
        })
        .collect();
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(current_cwd, sessions);
    browser.open();
    browser.set_viewport_rows(3);

    assert_eq!(browser.selected_id(), Some("session-8".parse().unwrap()));
    browser.page_down();
    assert_eq!(browser.selected_id(), Some("session-5".parse().unwrap()));
    assert_eq!(browser.selected(), 3);
    assert_eq!(browser.offset(), 1);
    browser.page_down();
    assert_eq!(browser.selected_id(), Some("session-2".parse().unwrap()));
    browser.page_down();
    assert_eq!(browser.selected_id(), Some("session-1".parse().unwrap()));
    assert_eq!(browser.offset(), 5);
    browser.page_down();
    assert_eq!(browser.selected_id(), Some("session-1".parse().unwrap()));

    browser.wheel_up();
    assert_eq!(browser.selected_id(), Some("session-4".parse().unwrap()));
    browser.wheel_up();
    assert_eq!(browser.selected_id(), Some("session-7".parse().unwrap()));
    browser.wheel_up();
    assert_eq!(browser.selected_id(), Some("session-8".parse().unwrap()));
    assert_eq!(browser.offset(), 0);
    browser.wheel_down();
    assert_eq!(browser.selected_id(), Some("session-5".parse().unwrap()));
    browser.page_up();
    assert_eq!(browser.selected_id(), Some("session-8".parse().unwrap()));
    assert_eq!(browser.offset(), 0);
}

#[test]
fn session_browser_state_empty_navigation_is_a_noop() {
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(b"/work/current", Vec::new());
    browser.open();
    browser.set_viewport_rows(0);

    browser.select_next();
    browser.select_previous();
    browser.page_down();
    browser.page_up();
    browser.wheel_down();
    browser.wheel_up();

    assert_eq!(browser.selected_id(), None);
    assert_eq!(browser.selected(), 0);
    assert_eq!(browser.offset(), 0);
}

#[test]
fn session_browser_state_rename_delete_and_escape_use_latest_selection() {
    let current_cwd = b"/work/current";
    let mut browser = SessionBrowserState::default();
    browser.set_sessions(
        current_cwd,
        vec![browser_summary(
            "session-4",
            "Original title",
            current_cwd,
            "/work/current",
            1,
        )],
    );
    browser.open();
    browser.set_sessions(
        current_cwd,
        vec![browser_summary(
            "session-4",
            "Latest title",
            current_cwd,
            "/work/current",
            2,
        )],
    );

    browser.start_rename();
    match browser.layer() {
        BrowserLayer::Rename {
            session_id,
            editor,
            error,
        } => {
            assert_eq!(*session_id, "session-4".parse().unwrap());
            assert_eq!(editor.value(), "Latest title");
            assert!(error.is_none());
        }
        layer => panic!("expected rename layer, got {layer:?}"),
    }

    browser.escape();
    assert!(browser.is_open());
    assert!(matches!(browser.layer(), BrowserLayer::List));

    browser.set_sessions(
        current_cwd,
        vec![browser_summary(
            "session-4",
            "Confirmation title",
            current_cwd,
            "/work/current",
            3,
        )],
    );
    browser.start_delete_confirmation();
    browser.set_sessions(
        current_cwd,
        vec![browser_summary(
            "session-4",
            "Changed after confirmation",
            current_cwd,
            "/work/current",
            4,
        )],
    );
    match browser.layer() {
        BrowserLayer::ConfirmDelete { session_id, title } => {
            assert_eq!(*session_id, "session-4".parse().unwrap());
            assert_eq!(title.as_str(), "Confirmation title");
        }
        layer => panic!("expected delete confirmation layer, got {layer:?}"),
    }
    browser.escape();
    assert!(browser.is_open());
    assert!(matches!(browser.layer(), BrowserLayer::List));
    browser.escape();
    assert!(!browser.is_open());
}

#[test]
fn session_browser_renders_local_and_global_rows_over_the_chat() {
    let projection = ChatProjection::session(snapshot_fixture(false));
    let mut ui = UiState::new();
    let mut current = browser_summary(
        "session-7",
        "Current browser row",
        b"/work/moh",
        "/work/moh",
        4,
    );
    current.running = true;
    current.busy = true;
    current.running_jobs = 2;
    current.attached_clients = 3;
    ui.session_browser_mut().set_sessions(
        b"/work/moh",
        vec![
            current,
            browser_summary("session-5", "Other local row", b"/work/moh", "/work/moh", 3),
            browser_summary(
                "session-9",
                "Global browser row",
                b"/work/other",
                "/work/other",
                5,
            ),
        ],
    );
    ui.session_browser_mut().open();
    ui.session_browser_mut().set_query("cur");
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();

    let local = rendered(&terminal);
    assert!(local.contains("first prompt"));
    assert!(local.contains("sessions"));
    assert!(local.contains("Local"));
    assert!(local.contains("Global"));
    assert!(local.contains("[current]"));
    assert!(local.contains("[running]"));
    assert!(local.contains("jobs:2"));
    assert!(!local.contains("Global browser row"));
    let (selected_x, selected_y) = find_rendered_cell(&terminal, "[current]");
    let selected = &terminal.backend().buffer()[(selected_x, selected_y)];
    assert_eq!(selected.style().bg, Some(Color::DarkGray));
    assert_eq!(selected.style().fg, Some(Color::Cyan));
    let (query_x, query_y) = find_rendered_cell(&terminal, "Filter: cur");
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!(cursor.y, query_y);
    assert_eq!(cursor.x, query_x + "Filter: cur".len() as u16);

    ui.session_browser_mut().toggle_mode();
    ui.session_browser_mut().set_query("");
    ui.request_redraw();
    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();

    let global = rendered(&terminal);
    assert!(global.contains("Global browser row"));
    let (group_x, group_y) = find_rendered_cell(&terminal, "/work/other");
    let group = &terminal.backend().buffer()[(group_x, group_y)];
    assert!(group.style().add_modifier.contains(Modifier::DIM));
}

#[test]
fn session_browser_marks_only_active_model_runs_as_running() {
    let projection = ChatProjection::session(snapshot_fixture(false));
    let mut ui = UiState::new();
    let mut jobs_only = browser_summary("session-5", "Jobs only", b"/work/moh", "/work/moh", 2);
    jobs_only.running = true;
    jobs_only.running_jobs = 1;
    let mut model_busy = browser_summary("session-6", "Model busy", b"/work/moh", "/work/moh", 1);
    model_busy.running = true;
    model_busy.busy = true;
    ui.session_browser_mut()
        .set_sessions(b"/work/moh", vec![jobs_only, model_busy]);
    ui.session_browser_mut().open();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();

    let frame = rendered(&terminal);
    let jobs_only_row = frame
        .lines()
        .find(|line| line.contains("Jobs only"))
        .expect("jobs-only row");
    let model_busy_row = frame
        .lines()
        .find(|line| line.contains("Model busy"))
        .expect("model-busy row");
    assert!(!jobs_only_row.contains("[running]"));
    assert!(jobs_only_row.contains("jobs:1"));
    assert!(model_busy_row.contains("[running]"));
}

#[test]
fn session_browser_renders_nested_layers_in_place_of_only_the_list_body() {
    let projection = ChatProjection::session(snapshot_fixture(false));
    let mut ui = UiState::new();
    ui.session_browser_mut().set_sessions(
        b"/work/moh",
        vec![
            browser_summary("session-7", "Rename this row", b"/work/moh", "/work/moh", 2),
            browser_summary("session-6", "Hidden list row", b"/work/moh", "/work/moh", 1),
        ],
    );
    ui.session_browser_mut().open();
    ui.session_browser_mut().start_rename();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();

    let rename = rendered(&terminal);
    assert!(rename.contains("sessions"));
    assert!(rename.contains("Local"));
    assert!(rename.contains("Filter:"));
    assert!(rename.contains("Rename session-7"));
    assert!(rename.contains("Rename this row"));
    assert!(!rename.contains("Hidden list row"));

    ui.session_browser_mut().escape();
    ui.session_browser_mut().start_delete_confirmation();
    ui.request_redraw();
    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();

    let confirmation = rendered(&terminal);
    assert!(confirmation.contains("sessions"));
    assert!(confirmation.contains("Delete Rename this row (session-7)?"));
    assert!(confirmation.contains("permanent"));
    assert!(confirmation.contains("active model run"));
    assert!(confirmation.contains("cancelled"));
    assert!(confirmation.contains("background jobs"));
    assert!(confirmation.contains("attached clients"));
    assert!(confirmation.contains("disconnected"));
    assert!(!confirmation.contains("Hidden list row"));
}

#[test]
fn session_browser_delete_confirmation_reflows_maximum_title_at_80_columns() {
    let projection = ChatProjection::session(snapshot_fixture(false));
    let title = format!("{} tail-marker", "x".repeat(52));
    assert_eq!(title.chars().count(), 64);
    let mut ui = UiState::new();
    ui.session_browser_mut().set_sessions(
        b"/work/moh",
        vec![browser_summary(
            "session-7",
            &title,
            b"/work/moh",
            "/work/moh",
            1,
        )],
    );
    ui.session_browser_mut().open();
    ui.session_browser_mut().start_delete_confirmation();
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();

    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();

    let confirmation = rendered(&terminal);
    assert!(confirmation.contains(&title));
    assert!(confirmation.contains("session-7"));
    assert!(confirmation.contains("permanent"));
    assert!(confirmation.contains("active model run"));
    assert!(confirmation.contains("cancelled"));
    assert!(confirmation.contains("background jobs"));
    assert!(confirmation.contains("terminated"));
    assert!(confirmation.contains("attached clients"));
    assert!(confirmation.contains("disconnected"));
}

#[tokio::test]
async fn session_browser_routes_list_query_navigation_and_nested_inputs() {
    let current_cwd = b"/work/moh";
    let sessions = (1..=8)
        .map(|id| {
            browser_summary(
                &format!("session-{id}"),
                &format!("Session {id}"),
                current_cwd,
                "/work/moh",
                0,
            )
        })
        .collect::<Vec<_>>();
    let client = ScriptedSessionClient::idle();
    let mut driven = client.clone();
    let mut ui = UiState::new();

    handle_event(&mut ui, &mut driven, Event::Paste("/sessions".into()))
        .await
        .unwrap();
    handle_event(&mut ui, &mut driven, key(KeyCode::Enter))
        .await
        .unwrap();

    assert!(ui.session_browser().is_open());
    ui.session_browser_mut().set_sessions(current_cwd, sessions);
    let selected_before_click = ui.session_browser().selected_id();
    handle_event(&mut ui, &mut driven, control('o'))
        .await
        .unwrap();
    handle_event(
        &mut ui,
        &mut driven,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 15,
            modifiers: KeyModifiers::NONE,
        }),
    )
    .await
    .unwrap();
    assert!(!ui.help_open());
    assert_eq!(ui.session_browser().selected_id(), selected_before_click);
    assert!(
        handle_event(&mut ui, &mut driven, Event::Resize(120, 36))
            .await
            .unwrap()
    );
    assert!(ui.session_browser().is_open());
    assert!(
        !handle_event(&mut ui, &mut driven, control('c'))
            .await
            .unwrap()
    );
    assert!(ui.session_browser().is_open());
    assert_eq!(client.cancel_count(), 0);

    handle_event(&mut ui, &mut driven, key(KeyCode::Tab))
        .await
        .unwrap();
    assert_eq!(ui.session_browser().mode(), BrowserMode::Global);
    handle_event(&mut ui, &mut driven, key(KeyCode::Tab))
        .await
        .unwrap();
    assert_eq!(ui.session_browser().mode(), BrowserMode::Project);

    handle_event(&mut ui, &mut driven, Event::Paste("session1".into()))
        .await
        .unwrap();
    assert_eq!(ui.session_browser().query().value(), "session1");
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-1".parse().unwrap())
    );
    handle_event(
        &mut ui,
        &mut driven,
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
    )
    .await
    .unwrap();
    assert_eq!(ui.session_browser().query().value(), "");

    ui.session_browser_mut().set_viewport_rows(3);
    handle_event(&mut ui, &mut driven, key(KeyCode::Up))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-2".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::Down))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-1".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::PageUp))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-4".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::PageDown))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-1".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, wheel(MouseEventKind::ScrollUp))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-4".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, wheel(MouseEventKind::ScrollDown))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-1".parse().unwrap())
    );

    assert_eq!(
        ui.session_browser_mut().handle_event(&key(KeyCode::Enter)),
        BrowserAction::Switch("session-1".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::F(2)))
        .await
        .unwrap();
    assert!(matches!(
        ui.session_browser().layer(),
        BrowserLayer::Rename { .. }
    ));
    handle_event(&mut ui, &mut driven, Event::Paste(" updated".into()))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser_mut().handle_event(&key(KeyCode::Enter)),
        BrowserAction::Rename {
            session_id: "session-1".parse().unwrap(),
            title: "Session 1 updated".into(),
        }
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::Esc))
        .await
        .unwrap();
    assert!(matches!(ui.session_browser().layer(), BrowserLayer::List));

    handle_event(&mut ui, &mut driven, control('d'))
        .await
        .unwrap();
    assert!(matches!(
        ui.session_browser().layer(),
        BrowserLayer::ConfirmDelete { .. }
    ));
    handle_event(&mut ui, &mut driven, key(KeyCode::Char('n')))
        .await
        .unwrap();
    assert!(matches!(ui.session_browser().layer(), BrowserLayer::List));
    handle_event(&mut ui, &mut driven, control('d'))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser_mut()
            .handle_event(&key(KeyCode::Char('y'))),
        BrowserAction::Delete("session-1".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::Esc))
        .await
        .unwrap();
    handle_event(&mut ui, &mut driven, control('d'))
        .await
        .unwrap();
    assert_eq!(
        ui.session_browser_mut().handle_event(&key(KeyCode::Enter)),
        BrowserAction::Delete("session-1".parse().unwrap())
    );
    handle_event(&mut ui, &mut driven, key(KeyCode::Esc))
        .await
        .unwrap();
    handle_event(&mut ui, &mut driven, key(KeyCode::Esc))
        .await
        .unwrap();
    assert!(!ui.session_browser().is_open());
    assert_eq!(ui.editor().value(), "");
}

struct BrowserRefreshEvents {
    opening: VecDeque<Event>,
    client_state: Rc<RefCell<ScriptedSessionState>>,
    closed_at: Option<tokio::time::Instant>,
}

impl EventSource for BrowserRefreshEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        if self.closed_at.is_none() && self.client_state.borrow().session_list_scopes.len() >= 2 {
            self.closed_at = Some(tokio::time::Instant::now());
            return Ok(Some(key(KeyCode::Esc)));
        }
        if self
            .closed_at
            .is_some_and(|closed_at| closed_at.elapsed() >= Duration::from_secs(2))
        {
            return Ok(Some(control('c')));
        }
        Ok(None)
    }
}

#[tokio::test(start_paused = true)]
async fn session_browser_refreshes_only_while_open() {
    let client = ScriptedSessionClient::idle();
    let retained = browser_summary("session-7", "Retained row", b"/work/moh", "/work/moh", 1);
    client.script_session_lists([
        Ok(vec![retained]),
        Err(ClientSessionError::scripted("refresh\x1b[2J failed")),
    ]);
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = BrowserRefreshEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client_state: Rc::clone(&client.state),
        closed_at: None,
    };

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    assert_eq!(client.state.borrow().session_list_scopes.len(), 2);
    assert_eq!(
        client.state.borrow().session_list_scopes,
        [
            SessionListScope::Project(b"/work/moh".to_vec()),
            SessionListScope::Project(b"/work/moh".to_vec()),
        ]
    );
    assert!(!ui.session_browser().is_open());
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-7".parse().unwrap())
    );
    assert_eq!(ui.session_browser().warning(), Some("refresh[2J failed"));
}

struct BrowserActionErrorRefreshEvents {
    opening: VecDeque<Event>,
    client_state: Rc<RefCell<ScriptedSessionState>>,
    action_attempted: bool,
}

impl EventSource for BrowserActionErrorRefreshEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        let refreshes = self.client_state.borrow().session_list_scopes.len();
        if !self.action_attempted && refreshes == 1 {
            self.action_attempted = true;
            return Ok(Some(key(KeyCode::Enter)));
        }
        if self.action_attempted && refreshes == 2 {
            return Ok(Some(control('c')));
        }
        Ok(None)
    }
}

#[tokio::test(start_paused = true)]
async fn switch_action_error_survives_successful_periodic_refresh() {
    let client = ScriptedSessionClient::idle();
    let sessions = vec![browser_summary(
        "session-9",
        "Unavailable target",
        b"/work/moh",
        "/work/moh",
        2,
    )];
    client.script_session_lists([Ok(sessions.clone()), Ok(sessions)]);
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = BrowserActionErrorRefreshEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client_state: Rc::clone(&client.state),
        action_attempted: false,
    };

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    assert_eq!(client.state.borrow().session_list_scopes.len(), 2);
    assert!(ui.session_browser().is_open());
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-9".parse().unwrap())
    );
    assert_eq!(
        ui.session_browser().warning(),
        Some("session was not found")
    );
}

struct BrowserModeRefreshEvents {
    opening: VecDeque<Event>,
    client_state: Rc<RefCell<ScriptedSessionState>>,
    toggled: bool,
}

impl EventSource for BrowserModeRefreshEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        let list_count = self.client_state.borrow().session_list_scopes.len();
        if !self.toggled && list_count == 1 {
            self.toggled = true;
            return Ok(Some(key(KeyCode::Tab)));
        }
        if self.toggled && list_count == 2 {
            return Ok(Some(control('c')));
        }
        Ok(None)
    }
}

#[tokio::test]
async fn session_browser_mode_toggle_refreshes_the_new_scope_immediately() {
    let client = ScriptedSessionClient::idle();
    client.state.borrow_mut().pending_session_lists = true;
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = BrowserModeRefreshEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client_state: Rc::clone(&client.state),
        toggled: false,
    };

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    assert_eq!(
        client.state.borrow().session_list_scopes,
        [
            SessionListScope::Project(b"/work/moh".to_vec()),
            SessionListScope::All,
        ]
    );
    let state = client.state.borrow();
    assert_eq!(state.max_active_session_lists, 1);
    assert_eq!(state.active_session_lists, 0);
    assert_eq!(state.dropped_session_lists, 2);
}

struct BrowserUpdateEvents {
    opening: VecDeque<Event>,
    client: ScriptedSessionClient,
    injected: bool,
}

impl EventSource for BrowserUpdateEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        if !self.injected && !self.client.state.borrow().session_list_scopes.is_empty() {
            self.client.queue_event(SessionEvent::Started {
                run_id: 41,
                prompt: "background update prompt".into(),
            });
            self.injected = true;
            return Ok(Some(ignored_event()));
        }
        if self.injected && self.client.state.borrow().updates.is_empty() {
            return Ok(Some(control('c')));
        }
        Ok(None)
    }
}

#[tokio::test]
async fn session_browser_keeps_background_session_updates_flowing() {
    let client = ScriptedSessionClient::idle();
    client.script_session_lists([Ok(vec![browser_summary(
        "session-7",
        "Current session",
        b"/work/moh",
        "/work/moh",
        1,
    )])]);
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = BrowserUpdateEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client,
        injected: false,
    };

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    assert!(ui.session_browser().is_open());
    let ChatProjection::Session(snapshot) = driven.current_projection() else {
        panic!("browser update changed the durable projection into a draft");
    };
    assert_eq!(
        snapshot.active_run.as_ref().map(|run| run.prompt.as_str()),
        Some("background update prompt")
    );
}

struct PendingBrowserEvents {
    opening: VecDeque<Event>,
    client_state: Rc<RefCell<ScriptedSessionState>>,
}

impl EventSource for PendingBrowserEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        let state = self.client_state.borrow();
        if state.active_session_lists == 1 && state.updates.is_empty() {
            return Ok(Some(control('c')));
        }
        Ok(None)
    }
}

#[tokio::test]
async fn pending_browser_refresh_does_not_block_observer_or_terminal_input() {
    let client = ScriptedSessionClient::idle();
    client.state.borrow_mut().pending_session_lists = true;
    client.queue_event(SessionEvent::Started {
        run_id: 41,
        prompt: "update during pending refresh".into(),
    });
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = PendingBrowserEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client_state: Rc::clone(&client.state),
    };

    tokio::time::timeout(
        Duration::from_millis(500),
        run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven),
    )
    .await
    .expect("pending list RPC blocked the event loop")
    .unwrap();

    let ChatProjection::Session(snapshot) = driven.current_projection() else {
        panic!("pending refresh changed the durable projection into a draft");
    };
    assert_eq!(
        snapshot.active_run.as_ref().map(|run| run.prompt.as_str()),
        Some("update during pending refresh")
    );
    let state = client.state.borrow();
    assert_eq!(state.max_active_session_lists, 1);
    assert_eq!(state.active_session_lists, 0);
    assert_eq!(state.dropped_session_lists, 1);
}

struct ObserverContentionEvents {
    opening: VecDeque<Event>,
    client_state: Rc<RefCell<ScriptedSessionState>>,
    crossed_refresh_deadline: bool,
}

impl EventSource for ObserverContentionEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        let state = self.client_state.borrow();
        if state.session_list_scopes.len() >= 2 || state.observer_update_count >= 100 {
            return Ok(Some(control('c')));
        }
        if !self.crossed_refresh_deadline && state.session_list_scopes.len() == 1 {
            drop(state);
            std::thread::sleep(Duration::from_millis(1_100));
            self.crossed_refresh_deadline = true;
        }
        Ok(None)
    }
}

#[tokio::test]
async fn overdue_browser_refresh_wins_under_continuously_ready_observer_updates() {
    let client = ScriptedSessionClient::idle();
    client.state.borrow_mut().continuous_observer_updates = true;
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = ObserverContentionEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client_state: Rc::clone(&client.state),
        crossed_refresh_deadline: false,
    };

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    let state = client.state.borrow();
    assert_eq!(state.session_list_scopes.len(), 2);
    assert!(state.observer_update_count < 100);
}

struct PendingRefreshCloseEvents {
    opening: VecDeque<Event>,
    client_state: Rc<RefCell<ScriptedSessionState>>,
    pending_since: Option<tokio::time::Instant>,
    closed_at: Option<tokio::time::Instant>,
}

impl EventSource for PendingRefreshCloseEvents {
    fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.opening.pop_front() {
            return Ok(Some(event));
        }
        if self.pending_since.is_none() && self.client_state.borrow().active_session_lists == 1 {
            self.pending_since = Some(tokio::time::Instant::now());
        }
        if self.closed_at.is_none()
            && self
                .pending_since
                .is_some_and(|pending_since| pending_since.elapsed() >= Duration::from_secs(2))
        {
            self.closed_at = Some(tokio::time::Instant::now());
            return Ok(Some(key(KeyCode::Esc)));
        }
        if self
            .closed_at
            .is_some_and(|closed_at| closed_at.elapsed() >= Duration::from_secs(2))
        {
            return Ok(Some(control('c')));
        }
        Ok(None)
    }
}

#[tokio::test(start_paused = true)]
async fn pending_browser_refresh_is_single_flight_and_cancelled_on_close() {
    let client = ScriptedSessionClient::idle();
    client.state.borrow_mut().pending_session_lists = true;
    let mut driven = client.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut ui = UiState::new();
    let mut events = PendingRefreshCloseEvents {
        opening: [Event::Paste("/sessions".into()), key(KeyCode::Enter)]
            .into_iter()
            .collect(),
        client_state: Rc::clone(&client.state),
        pending_since: None,
        closed_at: None,
    };

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    let state = client.state.borrow();
    assert_eq!(state.session_list_scopes.len(), 1);
    assert_eq!(state.max_active_session_lists, 1);
    assert_eq!(state.active_session_lists, 0);
    assert_eq!(state.dropped_session_lists, 1);
}

#[tokio::test]
async fn control_c_exits() {
    let client = ScriptedSessionClient::busy();
    let (_, _, _, client) = run_client_with_events(client, [control('c')])
        .await
        .unwrap();
    assert_eq!(client.cancel_count(), 0);
}

#[tokio::test]
async fn control_c_detaches_busy_before_a_ready_observer_error() {
    let client = ScriptedSessionClient::busy();
    client.queue_error(ClientSessionError::scripted("ready observer error"));

    let result = run_client_with_events(client.clone(), [control('c')]).await;

    if let Err(error) = result {
        panic!("Ctrl+C returned {error}");
    }
    assert_eq!(client.cancel_count(), 0);
    assert_eq!(client.state.borrow().updates.len(), 1);
}

#[tokio::test]
async fn quit_leaves_a_queued_update_unobserved() {
    let client = ScriptedSessionClient::idle();
    client.queue_event(SessionEvent::SettingsChanged {
        settings: SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Low,
            context_tokens: 64_000,
        },
        last_activity: activity_time(),
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut ui = UiState::new();
    ui.editor_mut().set_value("/quit");
    ui.menu_mut()
        .set(MenuKind::Commands, [MenuItem::new("/quit", "Exit moh")]);
    let mut events = ScriptedEvents {
        events: [Ok(key(KeyCode::Enter))].into_iter().collect(),
    };
    let mut driven = client.clone();

    run_event_loop(&mut terminal, &mut ui, &mut events, &mut driven)
        .await
        .unwrap();

    assert_eq!(client.snapshot().settings.model, TEST_MODEL);
    assert_eq!(client.state.borrow().updates.len(), 1);
}

#[tokio::test]
async fn control_shortcuts_preserve_shifted_character_events() {
    let shifted_control_c = Event::Key(KeyEvent::new(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    let client = ScriptedSessionClient::busy();
    let (_, _, _, client) = tokio::time::timeout(
        Duration::from_millis(250),
        run_client_with_events(client, [shifted_control_c]),
    )
    .await
    .expect("shifted Ctrl+C should detach")
    .unwrap();
    assert_eq!(client.cancel_count(), 0);
}

#[tokio::test]
async fn control_o_opens_help() {
    let (terminal, ui, _, _) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            control('o'),
            ignored_event(),
            key(KeyCode::Esc),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert!(!ui.help_open());
    assert!(!rendered(&terminal).contains("moh help"));
}

#[tokio::test]
async fn submitted_input_carries_its_text() {
    let client = ScriptedSessionClient::idle();
    let (_, _, _, client) = run_client_with_events(
        client,
        [
            Event::Paste("hello".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(client.state.borrow().submissions, ["hello"]);
}

#[tokio::test]
async fn submission_clears_the_prompt_before_any_backend_update() {
    let (terminal, ui, _, _) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            Event::Paste("pending prompt".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(ui.editor().value(), "");
    assert!(!rendered(&terminal).contains("pending prompt"));
}

#[tokio::test]
async fn snapshot_reconstructs_transcript_live_response_settings_context_and_jobs() {
    let (terminal, ui, projection, _) =
        run_client_with_events(ScriptedSessionClient::busy(), [control('c')])
            .await
            .unwrap();
    let frame = rendered(&terminal);
    assert_eq!(frame.matches("first prompt").count(), 1);
    assert_eq!(frame.matches("first answer").count(), 1);
    assert_eq!(frame.matches("active prompt").count(), 1);
    assert_eq!(frame.matches("partial answer").count(), 1);
    assert_eq!(frame.matches("Read src/lib.rs · lines 1–2").count(), 1);
    assert_eq!(projection.settings.model, TEST_MODEL);
    assert_eq!(projection.settings.reasoning, ReasoningLevel::High);
    assert_eq!(projection.settings.context_tokens, 128_000);
    assert_eq!(
        running_jobs(&ChatProjection::session(projection.clone())).len(),
        1
    );
    assert_eq!(ui.editor().value(), "");
    let status = status_row(&terminal);
    assert!(status.contains(TEST_MODEL));
    assert!(status.contains("high"));
    assert!(status.contains("50%/256K"));
    assert!(status.contains("thinking..."));
    assert!(status.contains("1 processes"));
    assert!(status.contains("/work/moh"));
}

#[tokio::test]
async fn matching_command_suggestions_render_above_the_prompt() {
    let (terminal, _, _, _) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [Event::Paste("/".into()), control('c')],
    )
    .await
    .unwrap();
    let rows = terminal
        .backend()
        .buffer()
        .content
        .chunks(100)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    let suggestions = rows.iter().position(|row| row.contains("/quit")).unwrap();
    let prompt = rows.iter().position(|row| row.contains('❯')).unwrap();
    assert!(suggestions < prompt);
}

#[tokio::test]
async fn backend_events_stream_markdown_and_sanitize_terminal_controls() {
    let client = ScriptedSessionClient::idle();
    client.queue_event(SessionEvent::Started {
        run_id: 10,
        prompt: "safe\x1b[2J prompt".into(),
    });
    client.queue_event(SessionEvent::AssistantDelta {
        run_id: 10,
        text: "**partial**\x1b]0;pwned\x07".into(),
    });
    client.queue_event(SessionEvent::Completed {
        run_id: 10,
        response: "## answer\n\nbody\x1b[2J".into(),
        last_activity: activity_time(),
    });
    let (terminal, _, projection, _) = run_client_with_events(
        client,
        [
            ignored_event(),
            ignored_event(),
            ignored_event(),
            control('c'),
        ],
    )
    .await
    .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("safe prompt"));
    assert!(frame.contains("answer"));
    assert!(frame.contains("body"));
    assert!(!frame.contains('\x1b'));
    assert!(!frame.contains("pwned"));
    assert!(!projection.busy);
}

#[tokio::test]
async fn process_commands_use_session_rpc() {
    let client = ScriptedSessionClient::idle();
    let (_, _, _, client) = run_client_with_events(
        client,
        [
            Event::Paste("/ps".into()),
            key(KeyCode::Enter),
            key(KeyCode::Esc),
            Event::Paste("/kill job-3".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(client.state.borrow().list_count, 1);
    assert_eq!(client.state.borrow().cancelled_jobs, ["job-3"]);
}

#[tokio::test]
async fn empty_process_list_is_neutral_and_keeps_ready_status() {
    let mut snapshot = snapshot_fixture(false);
    snapshot.jobs.clear();
    let (terminal, ui, _, _) = run_client_with_events(
        ScriptedSessionClient::new(snapshot),
        [
            Event::Paste("/ps".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(rendered(&terminal).contains("No running background processes."));
    assert!(status_row(&terminal).contains("ready"));
    assert!(!status_row(&terminal).contains("error"));
    assert!(!ui.local_error());
}

#[tokio::test]
async fn successful_kill_is_neutral_and_keeps_thinking_status() {
    let (terminal, ui, _, client) = run_client_with_events(
        ScriptedSessionClient::busy(),
        [
            Event::Paste("/kill job-3".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(rendered(&terminal).contains("Terminated job-3."));
    assert!(status_row(&terminal).contains("thinking..."));
    assert!(!status_row(&terminal).contains("error"));
    assert!(!ui.local_error());
    assert_eq!(client.state.borrow().cancelled_jobs, ["job-3"]);
}

#[tokio::test]
async fn terminal_event_error_detaches_without_cancelling_and_preserves_error() {
    let client = ScriptedSessionClient::busy();
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let result = run_client_with_terminal(
        client.clone(),
        terminal,
        [Err(io::Error::other("event failed"))],
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("terminal event failure unexpectedly succeeded"),
    };
    assert_eq!(error.to_string(), "event failed");
    assert_eq!(client.cancel_count(), 0);
}

#[test]
fn application_error_wins_while_restore_is_attempted() {
    let attempts = Cell::new(0);
    let error = restore_after_application(Err(AppError::Projection("application failed")), || {
        attempts.set(attempts.get() + 1);
        Err(io::Error::other("cleanup failed"))
    })
    .unwrap_err();
    assert_eq!(attempts.get(), 1);
    assert_eq!(
        error.to_string(),
        "invalid backend session update: application failed; terminal cleanup also failed: cleanup failed"
    );
}

#[tokio::test]
async fn end_at_prompt_end_resumes_following() {
    let mut ui = UiState::new();
    ui.scroll_mut().update_metrics(30, 8);
    ui.scroll_mut().page_up();
    let mut client = ScriptedSessionClient::idle();
    handle_event(&mut ui, &mut client, key(KeyCode::End))
        .await
        .unwrap();
    assert!(ui.scroll().auto_follow());
    assert_eq!(ui.scroll().top(), 22);
}

#[tokio::test]
async fn end_before_prompt_end_moves_the_editor_only() {
    let mut ui = UiState::new();
    ui.editor_mut().set_value("hello");
    ui.editor_mut().handle_event(&key(KeyCode::Home));
    ui.scroll_mut().update_metrics(30, 8);
    ui.scroll_mut().page_up();
    let top = ui.scroll().top();
    let mut client = ScriptedSessionClient::idle();
    handle_event(&mut ui, &mut client, key(KeyCode::End))
        .await
        .unwrap();
    assert!(ui.editor().at_end());
    assert_eq!(ui.scroll().top(), top);
    assert!(!ui.scroll().auto_follow());
}

#[tokio::test]
async fn end_with_a_capturing_popup_does_not_resume_following() {
    let mut ui = UiState::new();
    ui.editor_mut().set_value("hello");
    ui.editor_mut().handle_event(&key(KeyCode::Home));
    ui.menu_mut()
        .set(MenuKind::Commands, [MenuItem::new("/quit", "Exit moh")]);
    ui.scroll_mut().update_metrics(30, 8);
    ui.scroll_mut().page_up();
    let mut client = ScriptedSessionClient::idle();
    handle_event(&mut ui, &mut client, key(KeyCode::End))
        .await
        .unwrap();
    assert!(ui.editor().at_end());
    assert!(!ui.scroll().auto_follow());
}

#[tokio::test]
async fn wheel_scrolls_exactly_three_rows_and_disables_following() {
    let mut ui = UiState::new();
    ui.scroll_mut().update_metrics(30, 8);
    let mut client = ScriptedSessionClient::idle();
    handle_event(&mut ui, &mut client, wheel(MouseEventKind::ScrollUp))
        .await
        .unwrap();
    assert_eq!(ui.scroll().top(), 19);
    assert!(!ui.scroll().auto_follow());
    handle_event(&mut ui, &mut client, wheel(MouseEventKind::ScrollDown))
        .await
        .unwrap();
    assert_eq!(ui.scroll().top(), 22);
}

#[tokio::test]
async fn page_keys_scroll_by_viewport_height_minus_one() {
    let mut ui = UiState::new();
    ui.scroll_mut().update_metrics(30, 8);
    let mut client = ScriptedSessionClient::idle();
    handle_event(&mut ui, &mut client, key(KeyCode::PageUp))
        .await
        .unwrap();
    assert_eq!(ui.scroll().top(), 15);
    handle_event(&mut ui, &mut client, key(KeyCode::PageDown))
        .await
        .unwrap();
    assert_eq!(ui.scroll().top(), 22);
}

#[tokio::test]
async fn mouse_clicks_are_ignored() {
    let mut ui = UiState::new();
    ui.editor_mut().set_value("keep");
    let mut client = ScriptedSessionClient::idle();
    handle_event(
        &mut ui,
        &mut client,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }),
    )
    .await
    .unwrap();
    assert_eq!(ui.editor().value(), "keep");
    assert!(ui.menu().kind().is_none());
}

#[tokio::test]
async fn release_events_are_ignored() {
    let mut ui = UiState::new();
    let mut client = ScriptedSessionClient::idle();
    let release = Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    handle_event(&mut ui, &mut client, release).await.unwrap();
    assert_eq!(ui.editor().value(), "");
}

#[tokio::test]
async fn escape_cancels_and_keeps_the_client_open() {
    let client = ScriptedSessionClient::busy();
    let (_, _, projection, client) =
        run_client_with_events(client, [key(KeyCode::Esc), control('c')])
            .await
            .unwrap();
    assert_eq!(client.cancel_count(), 1);
    assert!(!projection.busy);
}

#[tokio::test]
async fn cancel_command_cancels_and_keeps_the_client_open() {
    let client = ScriptedSessionClient::busy();
    let (_, _, _, client) = run_client_with_events(
        client,
        [
            Event::Paste("/cancel".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(client.cancel_count(), 1);
}

#[tokio::test]
async fn snapshot_replacement_resets_local_state_without_transcript_duplication() {
    let client = ScriptedSessionClient::idle();
    let mut replacement = snapshot_fixture(false);
    replacement.sequence = 22;
    replacement.transcript = vec![
        TranscriptItem::User("replacement prompt".into()),
        TranscriptItem::Assistant("replacement answer".into()),
    ];
    client.queue_update(SessionUpdate::SnapshotReplaced(Box::new(replacement)));
    let (terminal, ui, projection, _) =
        run_client_with_events(client, [ignored_event(), control('c')])
            .await
            .unwrap();
    let frame = rendered(&terminal);
    assert_eq!(frame.matches("replacement prompt").count(), 1);
    assert_eq!(frame.matches("replacement answer").count(), 1);
    assert!(!frame.contains("first prompt"));
    assert!(ui.notices().is_empty());
    assert_eq!(projection.sequence, 22);
}

#[tokio::test]
async fn tab_completes_a_command_without_submitting_it() {
    let (_, ui, _, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [Event::Paste("/q".into()), key(KeyCode::Tab), control('c')],
    )
    .await
    .unwrap();
    assert_eq!(ui.editor().value(), "/quit");
    assert!(client.state.borrow().submissions.is_empty());
}

#[tokio::test]
async fn enter_executes_the_selected_quit_command_without_a_submission() {
    let (_, _, _, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [Event::Paste("/q".into()), key(KeyCode::Enter)],
    )
    .await
    .unwrap();
    assert!(client.state.borrow().submissions.is_empty());
}

#[tokio::test]
async fn control_l_opens_a_navigable_model_selector() {
    let (_, _, _, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            control('l'),
            key(KeyCode::Down),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(client.state.borrow().selected_models, ["gpt-5.6-terra"]);
    assert!(client.state.borrow().submissions.is_empty());
}

#[tokio::test]
async fn shift_tab_cycles_only_efforts_supported_by_the_active_model() {
    let shift_tab = Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
    let (_, _, _, client) =
        run_client_with_events(ScriptedSessionClient::idle(), [shift_tab, control('c')])
            .await
            .unwrap();
    assert_eq!(
        client.state.borrow().selected_reasoning,
        [ReasoningLevel::Low]
    );
}

#[tokio::test]
async fn modified_backtab_does_not_trigger_the_shift_tab_shortcut() {
    let modified_backtab = Event::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    let (_, _, _, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [modified_backtab, control('c')],
    )
    .await
    .unwrap();
    assert!(client.state.borrow().selected_reasoning.is_empty());
}

#[tokio::test]
async fn effort_command_rejects_a_level_not_supported_by_the_active_model() {
    let (terminal, ui, _, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            Event::Paste("/effort xhigh".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert!(client.state.borrow().selected_reasoning.is_empty());
    assert!(rendered(&terminal).contains("not supported by the active model"));
    assert!(ui.local_error());
}

#[tokio::test]
async fn effort_selector_filters_typed_effort_before_enter_selects_it() {
    let (_, _, _, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            control('r'),
            Event::Paste("high".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        client.state.borrow().selected_reasoning,
        [ReasoningLevel::High]
    );
}

#[tokio::test]
async fn unmatched_model_command_keeps_active_model_and_reports_failure() {
    let (terminal, ui, projection, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            Event::Paste("/model claude".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert!(client.state.borrow().selected_models.is_empty());
    assert_eq!(projection.settings.model, TEST_MODEL);
    assert!(rendered(&terminal).contains("No available model matches `claude`."));
    assert!(ui.local_error());
}

#[test]
fn fuzzy_model_matching_prefers_exact_and_abbreviated_queries() {
    let available = models();
    assert_eq!(
        best_model_match("gpt-5.6-terra", &available).map(|model| model.id.as_str()),
        Some("gpt-5.6-terra")
    );
    assert_eq!(
        best_model_match("terra", &available).map(|model| model.id.as_str()),
        Some("gpt-5.6-terra")
    );
    assert!(best_model_match("claude", &available).is_none());
}

#[tokio::test]
async fn catalog_failure_is_visible_without_blocking_default_submissions() {
    let mut snapshot = snapshot_fixture(false);
    snapshot.catalog = ModelCatalogState::Failed("catalog transport failed\x1b[2J".into());
    let client = ScriptedSessionClient::new(snapshot);
    let script = successful_run(21, "hello", "model answer");
    client.script_submission(script.clone());
    let (terminal, _, _, client) =
        run_client_with_events(client, input_events_for_updates("hello", script.len()))
            .await
            .unwrap();
    assert_eq!(client.state.borrow().submissions, ["hello"]);
    let frame = rendered(&terminal);
    assert!(frame.contains("Model selection is unavailable"));
    assert!(frame.contains("catalog transport failed[2J"));
    assert!(frame.contains("model answer"));
    assert!(!frame.contains('\x1b'));
}

#[tokio::test]
async fn unmatched_slash_input_is_submitted_to_the_session() {
    let client = ScriptedSessionClient::idle();
    let script = successful_run(22, "/unknown", "model answer");
    client.script_submission(script.clone());
    let (_, _, projection, client) =
        run_client_with_events(client, input_events_for_updates("/unknown", script.len()))
            .await
            .unwrap();
    assert_eq!(client.state.borrow().submissions, ["/unknown"]);
    assert!(
        projection
            .transcript
            .contains(&TranscriptItem::User("/unknown".into()))
    );
    assert!(
        projection
            .transcript
            .contains(&TranscriptItem::Assistant("model answer".into()))
    );
}

#[tokio::test]
async fn successful_request_appends_model_answer_and_returns_to_ready() {
    let client = ScriptedSessionClient::idle();
    let script = successful_run(23, "hello", "model answer");
    client.script_submission(script.clone());
    let (terminal, ui, projection, _) =
        run_client_with_events(client, input_events_for_updates("hello", script.len()))
            .await
            .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("hello"));
    assert!(frame.contains("model answer"));
    assert!(status_row(&terminal).contains("ready"));
    assert!(!projection.busy);
    assert!(!ui.local_error());
}

#[tokio::test]
async fn context_usage_updates_the_status_line() {
    let client = ScriptedSessionClient::idle();
    let script = vec![
        SessionEvent::Started {
            run_id: 24,
            prompt: "hello".into(),
        },
        SessionEvent::ContextUsage {
            run_id: 24,
            input_tokens: 51_200,
            last_activity: activity_time(),
        },
        SessionEvent::Completed {
            run_id: 24,
            response: "model answer".into(),
            last_activity: activity_time(),
        },
    ];
    client.script_submission(script.clone());
    let (terminal, _, _, _) =
        run_client_with_events(client, input_events_for_updates("hello", script.len()))
            .await
            .unwrap();
    assert!(status_row(&terminal).contains("20%/256K"));
}

#[tokio::test]
async fn tool_started_projects_read_arguments_with_the_session_cwd() {
    let client = ScriptedSessionClient::idle();
    let script = vec![
        SessionEvent::Started {
            run_id: 28,
            prompt: "hello".into(),
        },
        SessionEvent::ToolStarted {
            run_id: 28,
            call_id: "read-1".into(),
            name: "read".into(),
            arguments: json!({
                "path": "/work/moh/src/client/app.rs",
                "offset": 480,
                "limit": 101,
            }),
        },
        SessionEvent::Completed {
            run_id: 28,
            response: "model answer".into(),
            last_activity: activity_time(),
        },
    ];
    client.script_submission(script.clone());
    let (terminal, _, _, _) =
        run_client_with_events(client, input_events_for_updates("hello", script.len()))
            .await
            .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("Read src/client/app.rs · lines 480–580"));
    assert!(!frame.contains("/work/moh/src/client/app.rs"));
}

#[tokio::test]
async fn unknown_tool_arguments_fall_back_to_a_safe_generic_activity_label() {
    let client = ScriptedSessionClient::idle();
    let script = vec![
        SessionEvent::Started {
            run_id: 29,
            prompt: "hello".into(),
        },
        SessionEvent::ToolStarted {
            run_id: 29,
            call_id: "unknown-1".into(),
            name: "shell\x1b[2J".into(),
            arguments: json!({"command": "never display me", "nested": {"secret": ["raw"]}}),
        },
        SessionEvent::Completed {
            run_id: 29,
            response: "model answer".into(),
            last_activity: activity_time(),
        },
    ];
    client.script_submission(script.clone());
    let (terminal, _, _, _) =
        run_client_with_events(client, input_events_for_updates("hello", script.len()))
            .await
            .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("Running shell[2J"));
    assert!(!frame.contains("never display me"));
    assert!(!frame.contains("secret"));
    assert!(!frame.contains('\x1b'));
}

#[test]
fn assistant_delta_is_visible_before_completion() {
    let mut projection = snapshot_fixture(false);
    let mut ui = UiState::new();
    apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::Started {
                run_id: 30,
                prompt: "hello".into(),
            },
        }),
    )
    .unwrap();
    apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 16,
            event: SessionEvent::AssistantDelta {
                run_id: 30,
                text: "partial".into(),
            },
        }),
    )
    .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw_if_needed(
        &mut terminal,
        &mut ui,
        &ChatProjection::session(projection.clone()),
    )
    .unwrap();
    assert!(rendered(&terminal).contains("partial"));
    assert!(projection.active_run.is_some());
}

#[tokio::test]
async fn model_command_waits_for_authoritative_settings_event() {
    let (_, _, projection, client) = run_client_with_events(
        ScriptedSessionClient::idle(),
        [
            Event::Paste("/model terra".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    assert_eq!(client.state.borrow().selected_models, ["gpt-5.6-terra"]);
    assert_eq!(projection.settings.model, TEST_MODEL);
}

#[tokio::test]
async fn settings_event_updates_model_reasoning_and_context() {
    let client = ScriptedSessionClient::idle();
    client.queue_event(SessionEvent::SettingsChanged {
        settings: SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Low,
            context_tokens: 64_000,
        },
        last_activity: activity_time(),
    });
    let (_, _, projection, _) = run_client_with_events(client, [ignored_event(), control('c')])
        .await
        .unwrap();
    assert_eq!(projection.settings.model, "gpt-5.6-terra");
    assert_eq!(projection.settings.reasoning, ReasoningLevel::Low);
    assert_eq!(projection.settings.context_tokens, 64_000);
}

#[test]
fn jobs_update_preserves_command_menu_selection() {
    let mut projection = snapshot_fixture(false);
    let mut ui = UiState::new();
    ui.editor_mut().set_value("/");
    ui.menu_mut().set(
        MenuKind::Commands,
        [
            MenuItem::new("/quit", "Exit moh"),
            MenuItem::new("/cancel", "Cancel the active request"),
        ],
    );
    ui.menu_mut().select_next();

    apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::JobsChanged(vec![running_job()]),
        }),
    )
    .unwrap();

    assert_eq!(ui.menu().selected_value(), Some("/cancel"));
    assert_eq!(ui.editor().value(), "/");
}

#[test]
fn jobs_update_preserves_model_menu_selection() {
    let mut projection = snapshot_fixture(false);
    let mut ui = UiState::new();
    ui.menu_mut().set(
        MenuKind::Models,
        [
            MenuItem::new(TEST_MODEL, "Test-only model"),
            MenuItem::new("gpt-5.6-terra", "Balanced model"),
        ],
    );
    ui.menu_mut().select_next();

    apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::JobsChanged(vec![running_job()]),
        }),
    )
    .unwrap();

    assert_eq!(ui.menu().selected_value(), Some("gpt-5.6-terra"));
    assert_eq!(ui.editor().value(), "");
}

#[test]
fn jobs_update_preserves_effort_menu_selection() {
    let mut projection = snapshot_fixture(false);
    let mut ui = UiState::new();
    ui.menu_mut().set(
        MenuKind::Efforts,
        [
            MenuItem::new("low", "Supported by the active model"),
            MenuItem::new("medium", "Supported by the active model"),
        ],
    );
    ui.menu_mut().select_next();

    apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::JobsChanged(vec![running_job()]),
        }),
    )
    .unwrap();

    assert_eq!(ui.menu().selected_value(), Some("medium"));
    assert_eq!(ui.editor().value(), "");
}

#[test]
fn jobs_update_refreshes_the_process_menu() {
    let mut projection = snapshot_fixture(false);
    let mut ui = UiState::new();
    ui.menu_mut().set(
        MenuKind::Processes,
        [MenuItem::new("job-stale", "stale process")],
    );
    let mut replacement = running_job();
    replacement.id = "job-4".into();
    replacement.title = "cargo nextest".into();

    apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::JobsChanged(vec![replacement]),
        }),
    )
    .unwrap();

    assert_eq!(ui.menu().selected_value(), Some("job-4"));
    assert_eq!(projection.jobs[0].id, "job-4");
}

#[tokio::test]
async fn streamed_error_discards_partial_text_and_allows_the_next_submission() {
    let client = ScriptedSessionClient::idle();
    client.script_submission([
        SessionEvent::Started {
            run_id: 31,
            prompt: "first".into(),
        },
        SessionEvent::AssistantDelta {
            run_id: 31,
            text: "partial".into(),
        },
        SessionEvent::Failed {
            run_id: 31,
            failure: run_failure("request failed"),
        },
    ]);
    client.script_submission(successful_run(32, "second", "recovered"));
    let (terminal, _, projection, client) = run_client_with_events(
        client,
        [
            Event::Paste("first".into()),
            key(KeyCode::Enter),
            ignored_event(),
            ignored_event(),
            Event::Paste("second".into()),
            key(KeyCode::Enter),
            ignored_event(),
            control('c'),
        ],
    )
    .await
    .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("request failed"));
    assert!(frame.contains("recovered"));
    assert!(!frame.contains("partial"));
    assert_eq!(client.state.borrow().submissions, ["first", "second"]);
    assert!(!projection.busy);
}

#[tokio::test]
async fn failed_run_keeps_previous_committed_history_and_sanitizes_failure() {
    let client = ScriptedSessionClient::idle();
    client.script_submission([
        SessionEvent::Started {
            run_id: 33,
            prompt: "new prompt".into(),
        },
        SessionEvent::AssistantDelta {
            run_id: 33,
            text: "partial".into(),
        },
        SessionEvent::Failed {
            run_id: 33,
            failure: run_failure("safe\x1b[2J failure only"),
        },
    ]);
    let (terminal, ui, projection, _) =
        run_client_with_events(client, input_events_for_updates("new prompt", 3))
            .await
            .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("first prompt"));
    assert!(frame.contains("first answer"));
    assert!(frame.contains("safe[2J failure only"));
    assert!(!frame.contains("partial"));
    assert!(!frame.contains('\x1b'));
    assert!(!ui.local_error());
    assert!(matches!(
        projection.transcript.last().unwrap(),
        TranscriptItem::Failed { failure, .. } if failure.message == "safe[2J failure only"
    ));
}

#[tokio::test]
async fn failed_request_leaves_authoritative_error_status() {
    let client = ScriptedSessionClient::idle();
    client.script_submission([
        SessionEvent::Started {
            run_id: 36,
            prompt: "first".into(),
        },
        SessionEvent::Failed {
            run_id: 36,
            failure: run_failure("request failed"),
        },
    ]);
    let (terminal, ui, _, _) = run_client_with_events(client, input_events_for_updates("first", 2))
        .await
        .unwrap();
    let status = status_row(&terminal);
    assert!(status.contains("error"));
    assert!(!status.contains("thinking..."));
    assert!(!status.contains("ready"));
    assert!(!ui.local_error());
}

#[tokio::test]
async fn typed_command_errors_are_visible_and_terminal_safe() {
    let client = ScriptedSessionClient::idle();
    client.state.borrow_mut().submit_error = Some(ClientSessionError::scripted(
        "safe\x1b[2J message without source chain",
    ));
    let (terminal, ui, _, _) = run_client_with_events(
        client,
        [
            Event::Paste("hello".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();
    let frame = rendered(&terminal);
    assert!(frame.contains("safe[2J message without source chain"));
    assert!(!frame.contains('\x1b'));
    assert!(ui.local_error());
}

#[tokio::test]
async fn completed_run_preserves_catalog_failure_status() {
    let mut snapshot = snapshot_fixture(true);
    snapshot.catalog = ModelCatalogState::Failed("catalog unavailable".into());
    let client = ScriptedSessionClient::new(snapshot);
    client.queue_event(SessionEvent::Completed {
        run_id: 9,
        response: "answer".into(),
        last_activity: activity_time(),
    });
    let (terminal, _, projection, _) =
        run_client_with_events(client, [ignored_event(), control('c')])
            .await
            .unwrap();
    assert!(!projection.busy);
    assert!(status_row(&terminal).contains("error"));
}

#[tokio::test]
async fn cancelled_run_preserves_persistence_warning_status() {
    let mut snapshot = snapshot_fixture(true);
    snapshot.persistence_warning = Some("checkpoint failed".into());
    let client = ScriptedSessionClient::new(snapshot);
    let (terminal, _, projection, _) =
        run_client_with_events(client, [key(KeyCode::Esc), control('c')])
            .await
            .unwrap();
    assert!(!projection.busy);
    assert!(status_row(&terminal).contains("error"));
}

#[tokio::test]
async fn resize_redraws_help_at_the_new_geometry_while_busy() {
    let mut client = ScriptedSessionClient::busy();
    let mut ui = UiState::new();
    ui.set_help_open(true);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw_if_needed(&mut terminal, &mut ui, client.current_projection()).unwrap();
    assert_eq!(terminal.backend().buffer()[(2, 4)].symbol(), "╭");
    terminal.backend_mut().resize(40, 12);

    handle_event(&mut ui, &mut client, Event::Resize(40, 12))
        .await
        .unwrap();
    draw_if_needed(&mut terminal, &mut ui, client.current_projection()).unwrap();

    assert!(ui.help_open());
    assert_eq!(terminal.backend().buffer().area.width, 40);
    assert_eq!(terminal.backend().buffer().area.height, 12);
    assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "╭");
    assert_eq!(terminal.get_cursor_position().unwrap().y, 10);
}

#[tokio::test]
async fn exit_remains_responsive_while_observer_never_completes() {
    tokio::time::timeout(
        Duration::from_millis(250),
        run_client_with_events(
            ScriptedSessionClient::busy(),
            [Event::Paste("editable".into()), control('c')],
        ),
    )
    .await
    .expect("Ctrl+C should detach without waiting for the observer")
    .unwrap();
}

#[tokio::test]
async fn observer_error_is_returned_without_mutating_the_editor() {
    let client = ScriptedSessionClient::idle();
    client.queue_error(ClientSessionError::scripted("RPC observer failed"));
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let result = run_client_with_terminal(client, terminal, [Ok(ignored_event())]).await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("observer failure unexpectedly succeeded"),
    };
    assert_eq!(error.to_string(), "RPC observer failed");
}

#[test]
fn invalid_sequence_and_run_id_are_rejected_without_panicking() {
    let mut projection = snapshot_fixture(false);
    let mut ui = UiState::new();
    let sequence_error = apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 16,
            event: SessionEvent::Started {
                run_id: 41,
                prompt: "hello".into(),
            },
        }),
    )
    .unwrap_err();
    assert!(sequence_error.to_string().contains("not contiguous"));

    let run_error = apply_session_update(
        &mut ui,
        &mut projection,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::AssistantDelta {
                run_id: 41,
                text: "orphan".into(),
            },
        }),
    )
    .unwrap_err();
    assert!(run_error.to_string().contains("while idle"));
}

#[test]
fn redraw_is_consumed_by_exactly_one_draw() {
    let projection = ChatProjection::session(snapshot_fixture(false));
    let mut ui = UiState::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();
    let first = terminal.backend().buffer().clone();
    draw_if_needed(&mut terminal, &mut ui, &projection).unwrap();
    assert_eq!(terminal.backend().buffer(), &first);
    assert!(!ui.take_redraw());
}

#[derive(Clone)]
struct ScriptedWorkspaceSession {
    snapshot: SessionSnapshot,
    updates: Rc<RefCell<VecDeque<Result<SessionUpdate, ClientSessionError>>>>,
    log: Rc<RefCell<Vec<String>>>,
    detach_error: Option<ClientSessionError>,
}

impl ScriptedWorkspaceSession {
    fn new(snapshot: SessionSnapshot, log: Rc<RefCell<Vec<String>>>) -> Self {
        Self {
            snapshot,
            updates: Rc::new(RefCell::new(VecDeque::new())),
            log,
            detach_error: None,
        }
    }

    fn with_detach_error(mut self, message: &str) -> Self {
        self.detach_error = Some(ClientSessionError::scripted(message));
        self
    }

    fn queue_update(&self, update: SessionUpdate) {
        self.updates.borrow_mut().push_back(Ok(update));
    }
}

impl WorkspaceSession for ScriptedWorkspaceSession {
    fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    async fn next_update(&mut self) -> Result<SessionUpdate, ClientSessionError> {
        if let Some(update) = self.updates.borrow_mut().pop_front() {
            return update;
        }
        std::future::pending().await
    }

    async fn submit(&self, _prompt: String) -> Result<u64, ClientSessionError> {
        Ok(101)
    }

    async fn cancel(&self) -> Result<(), ClientSessionError> {
        self.log
            .borrow_mut()
            .push(format!("cancel:{}", self.snapshot.summary.id));
        Ok(())
    }

    async fn select_model(&self, _model: String) -> Result<(), ClientSessionError> {
        Ok(())
    }

    async fn select_reasoning(&self, _reasoning: ReasoningLevel) -> Result<(), ClientSessionError> {
        Ok(())
    }

    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError> {
        Ok(Vec::new())
    }

    async fn cancel_job(&self, _id: String) -> Result<JobSnapshotDto, ClientSessionError> {
        Err(ClientSessionError::scripted("job was not found"))
    }

    async fn detach(self) -> Result<(), ClientSessionError> {
        self.log
            .borrow_mut()
            .push(format!("detach:{}", self.snapshot.summary.id));
        match self.detach_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[derive(Default)]
struct ScriptedWorkspaceBackendState {
    startups: VecDeque<Result<WorkspaceStartup<ScriptedWorkspaceSession>, ClientSessionError>>,
    draft_defaults: VecDeque<Result<DraftState, ClientSessionError>>,
    materializations:
        VecDeque<Result<WorkspaceMaterialized<ScriptedWorkspaceSession>, ClientSessionError>>,
    opens: VecDeque<Result<ScriptedWorkspaceSession, ClientSessionError>>,
    session_lists: VecDeque<Result<Vec<SessionSummary>, ClientSessionError>>,
    list_updates: VecDeque<(ScriptedWorkspaceSession, SessionUpdate)>,
    renames: VecDeque<Result<(), ClientSessionError>>,
    rename_updates: VecDeque<(ScriptedWorkspaceSession, SessionUpdate)>,
    deletes: VecDeque<Result<(), ClientSessionError>>,
    log: Rc<RefCell<Vec<String>>>,
}

#[derive(Clone, Default)]
struct ScriptedWorkspaceBackend {
    state: Rc<RefCell<ScriptedWorkspaceBackendState>>,
}

impl ScriptedWorkspaceBackend {
    fn log(&self) -> Vec<String> {
        self.state.borrow().log.borrow().clone()
    }

    fn clear_log(&self) {
        self.state.borrow().log.borrow_mut().clear();
    }

    fn session(&self, id: &str, cwd: &[u8]) -> ScriptedWorkspaceSession {
        ScriptedWorkspaceSession::new(
            workspace_snapshot(id, cwd),
            Rc::clone(&self.state.borrow().log),
        )
    }

    fn push_startup(
        &self,
        result: Result<WorkspaceStartup<ScriptedWorkspaceSession>, ClientSessionError>,
    ) {
        self.state.borrow_mut().startups.push_back(result);
    }

    fn push_draft_defaults(&self, result: Result<DraftState, ClientSessionError>) {
        self.state.borrow_mut().draft_defaults.push_back(result);
    }

    fn push_materialization(
        &self,
        result: Result<WorkspaceMaterialized<ScriptedWorkspaceSession>, ClientSessionError>,
    ) {
        self.state.borrow_mut().materializations.push_back(result);
    }

    fn push_open(&self, result: Result<ScriptedWorkspaceSession, ClientSessionError>) {
        self.state.borrow_mut().opens.push_back(result);
    }

    fn push_session_list(&self, result: Result<Vec<SessionSummary>, ClientSessionError>) {
        self.state.borrow_mut().session_lists.push_back(result);
    }

    fn push_list_update(&self, session: ScriptedWorkspaceSession, update: SessionUpdate) {
        self.state
            .borrow_mut()
            .list_updates
            .push_back((session, update));
    }

    fn push_rename(&self, result: Result<(), ClientSessionError>) {
        self.state.borrow_mut().renames.push_back(result);
    }

    fn push_rename_update(&self, session: ScriptedWorkspaceSession, update: SessionUpdate) {
        self.state
            .borrow_mut()
            .rename_updates
            .push_back((session, update));
    }

    fn push_delete(&self, result: Result<(), ClientSessionError>) {
        self.state.borrow_mut().deletes.push_back(result);
    }

    fn record(&self, entry: impl Into<String>) {
        self.state.borrow().log.borrow_mut().push(entry.into());
    }
}

impl WorkspaceBackend for ScriptedWorkspaceBackend {
    type Session = ScriptedWorkspaceSession;

    async fn startup(
        &self,
        cwd: Vec<u8>,
    ) -> Result<WorkspaceStartup<Self::Session>, ClientSessionError> {
        self.record(format!("startup:{}", String::from_utf8_lossy(&cwd)));
        self.state
            .borrow_mut()
            .startups
            .pop_front()
            .expect("scripted startup result")
    }

    async fn draft_defaults(&self, cwd: Vec<u8>) -> Result<DraftState, ClientSessionError> {
        self.record(format!("draft-defaults:{}", String::from_utf8_lossy(&cwd)));
        self.state
            .borrow_mut()
            .draft_defaults
            .pop_front()
            .expect("scripted draft-defaults result")
    }

    async fn materialize(
        &self,
        cwd: Vec<u8>,
        prompt: String,
        _settings: SessionSettings,
    ) -> Result<WorkspaceMaterialized<Self::Session>, ClientSessionError> {
        self.record(format!(
            "materialize:{}:{prompt}",
            String::from_utf8_lossy(&cwd)
        ));
        self.state
            .borrow_mut()
            .materializations
            .pop_front()
            .expect("scripted materialization result")
    }

    async fn open_session(
        &self,
        selector: SessionSelector,
        _cwd_for_title: Vec<u8>,
    ) -> Result<Self::Session, ClientSessionError> {
        self.record(format!("open:{selector}"));
        self.state
            .borrow_mut()
            .opens
            .pop_front()
            .expect("scripted open result")
    }

    fn list_sessions(&self, scope: SessionListScope) -> SessionListFuture {
        let scope = match scope {
            SessionListScope::Project(cwd) => {
                format!("project:{}", String::from_utf8_lossy(&cwd))
            }
            SessionListScope::All => "all".into(),
        };
        self.record(format!("list:{scope}"));
        let result = self
            .state
            .borrow_mut()
            .session_lists
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()));
        if let Some((session, update)) = self.state.borrow_mut().list_updates.pop_front() {
            session.queue_update(update);
        }
        Box::pin(async move { result })
    }

    async fn rename_session(
        &self,
        session_id: SessionId,
        title: SessionTitle,
    ) -> Result<(), ClientSessionError> {
        self.record(format!("rename:{session_id}:{title}"));
        let result = self
            .state
            .borrow_mut()
            .renames
            .pop_front()
            .unwrap_or(Ok(()));
        if result.is_ok()
            && let Some((session, update)) = self.state.borrow_mut().rename_updates.pop_front()
        {
            session.queue_update(update);
        }
        result
    }

    async fn delete_session(&self, session_id: SessionId) -> Result<(), ClientSessionError> {
        self.record(format!("delete:{session_id}"));
        self.state
            .borrow_mut()
            .deletes
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn disconnect(self) -> Result<(), ClientSessionError> {
        self.record("disconnect");
        Ok(())
    }
}

fn workspace_snapshot(id: &str, cwd: &[u8]) -> SessionSnapshot {
    let mut snapshot = snapshot_fixture(false);
    snapshot.summary.id = id.parse().unwrap();
    snapshot.summary.title = SessionTitle::parse(format!("title for {id}")).unwrap();
    snapshot.summary.cwd = cwd.to_vec();
    snapshot.summary.cwd_display = String::from_utf8_lossy(cwd).into_owned();
    snapshot
}

fn workspace_draft(cwd: &[u8]) -> DraftState {
    DraftState {
        cwd: cwd.to_vec(),
        settings: SessionSettings {
            model: TEST_MODEL.into(),
            reasoning: ReasoningLevel::Medium,
            context_tokens: 0,
        },
        catalog: ModelCatalogState::Ready(models()),
    }
}

#[tokio::test]
async fn sessions_command_opens_the_minimal_browser_state() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();

    let (_, ui, _, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(ui.session_browser().is_open());
}

#[tokio::test]
async fn draft_model_and_effort_commands_update_local_settings() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, _, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/model terra".into()),
            key(KeyCode::Enter),
            Event::Paste("/effort low".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    let ChatProjection::Draft(draft) = projection else {
        panic!("settings commands must not materialize a draft");
    };
    assert_eq!(draft.settings.model, "gpt-5.6-terra");
    assert_eq!(draft.settings.reasoning, ReasoningLevel::Low);
    assert_eq!(draft.settings.context_tokens, 0);
    assert!(backend.log().is_empty());
}

#[tokio::test]
async fn draft_run_and_job_commands_are_neutral() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();

    let (terminal, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/cancel".into()),
            key(KeyCode::Enter),
            Event::Paste("/ps".into()),
            key(KeyCode::Enter),
            Event::Paste("/kill job-3".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(matches!(projection, ChatProjection::Draft(_)));
    assert!(!ui.local_error());
    assert!(rendered(&terminal).contains("No running request."));
    assert_eq!(
        ui.notices()
            .iter()
            .filter(|notice| *notice == "No running background processes.")
            .count(),
        2
    );
}

#[tokio::test]
async fn draft_materialization_failure_restores_the_first_prompt() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    backend.push_materialization(Err(ClientSessionError::scripted("storage failed")));
    let workspace =
        WorkspaceController::launch(backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("keep this first prompt".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(matches!(projection, ChatProjection::Draft(_)));
    assert_eq!(ui.editor().value(), "keep this first prompt");
    assert!(ui.local_error());
    assert!(ui.notices().iter().any(|notice| notice == "storage failed"));
}

#[tokio::test]
async fn blank_draft_submission_is_a_noop_without_backend_materialization() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste(" \n\t ".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(matches!(projection, ChatProjection::Draft(_)));
    assert!(backend.log().is_empty());
    assert!(ui.editor().value().is_empty());
    assert!(ui.notices().is_empty());
}

#[tokio::test]
async fn successful_draft_retry_clears_the_materialization_error() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    backend.push_materialization(Err(ClientSessionError::scripted("storage failed")));
    backend.push_materialization(Ok(WorkspaceMaterialized {
        session: backend.session("session-8", b"/work/moh"),
        run_id: 0,
    }));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("keep this first prompt".into()),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-8"
    ));
    assert_eq!(
        backend.log(),
        [
            "materialize:/work/moh:keep this first prompt",
            "materialize:/work/moh:keep this first prompt",
        ]
    );
    assert!(ui.editor().value().is_empty());
    assert!(ui.notices().is_empty());
    assert!(!ui.local_error());
}

#[tokio::test]
async fn workspace_warning_is_a_sanitized_nonfatal_notice() {
    let backend = ScriptedWorkspaceBackend::default();
    let session = backend
        .session("session-7", b"/work/moh")
        .with_detach_error("exact detach\x1b[2J failed");
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    let workspace =
        WorkspaceController::launch(backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();

    let (terminal, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/new".into()),
            key(KeyCode::Enter),
            ignored_event(),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(matches!(projection, ChatProjection::Draft(_)));
    assert!(!ui.local_error());
    assert!(rendered(&terminal).contains("exact detach[2J failed"));
    assert!(!rendered(&terminal).contains('\x1b'));
}

#[tokio::test]
async fn deleted_update_uses_the_workspace_applied_fallback_once() {
    let backend = ScriptedWorkspaceBackend::default();
    let session = backend.session("session-7", b"/work/moh");
    session.queue_update(SessionUpdate::Deleted {
        session_id: "session-7".parse().unwrap(),
        cwd: b"/work/moh".to_vec(),
    });
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) =
        run_workspace_with_events(workspace, [ignored_event(), control('c')])
            .await
            .unwrap();

    assert!(matches!(projection, ChatProjection::Draft(_)));
    assert_eq!(backend.log(), ["startup:/work/moh"]);
    assert!(ui.editor().value().is_empty());
}

#[tokio::test]
async fn new_command_enters_ephemeral_draft() {
    let backend = ScriptedWorkspaceBackend::default();
    let mut snapshot = workspace_snapshot("session-7", b"/work/moh");
    snapshot.busy = true;
    snapshot.summary.busy = true;
    snapshot.summary.running = true;
    snapshot.active_run = Some(ActiveRunSnapshot {
        run_id: 9,
        prompt: "still running".into(),
        assistant_text: String::new(),
    });
    let log = Rc::clone(&backend.state.borrow().log);
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        ScriptedWorkspaceSession::new(snapshot, log),
    )));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/new".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert!(matches!(
        projection,
        ChatProjection::Draft(DraftState { cwd, .. }) if cwd == b"/work/moh"
    ));
    assert_eq!(backend.log(), ["detach:session-7"]);
    assert_eq!(ui.editor().value(), "");
    assert!(ui.notices().is_empty());
}

#[tokio::test]
async fn workspace_startup_installs_draft_or_attachment() {
    let draft_backend = ScriptedWorkspaceBackend::default();
    let expected_draft = workspace_draft(b"/work/draft");
    draft_backend.push_startup(Ok(WorkspaceStartup::Draft(expected_draft.clone())));
    let draft =
        WorkspaceController::launch(draft_backend, b"/work/draft".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    assert_eq!(
        draft.current_projection(),
        &ChatProjection::Draft(expected_draft)
    );

    let attached_backend = ScriptedWorkspaceBackend::default();
    let session = attached_backend.session("session-7", b"/work/moh");
    let expected_snapshot = session.snapshot().clone();
    attached_backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    let attached =
        WorkspaceController::launch(attached_backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    assert_eq!(
        attached.current_projection(),
        &ChatProjection::session(expected_snapshot)
    );
}

#[tokio::test]
async fn workspace_new_launch_uses_fresh_nonselecting_backend_defaults() {
    let backend = ScriptedWorkspaceBackend::default();
    let mut running_snapshot = workspace_snapshot("session-7", b"/work/moh");
    running_snapshot.settings.model = "running-session-model".into();
    let log = Rc::clone(&backend.state.borrow().log);
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        ScriptedWorkspaceSession::new(running_snapshot, log),
    )));
    let mut fresh = workspace_draft(b"/work/moh");
    fresh.settings.model = "fresh-default-model".into();
    backend.push_draft_defaults(Ok(fresh.clone()));

    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::NewDraft)
            .await
            .unwrap();

    assert_eq!(
        workspace.current_projection(),
        &ChatProjection::Draft(fresh)
    );
    assert_eq!(backend.log(), ["draft-defaults:/work/moh"]);
}

#[tokio::test]
async fn workspace_new_draft_detaches_without_consulting_startup() {
    let backend = ScriptedWorkspaceBackend::default();
    let session = backend.session("session-7", b"/work/moh");
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    workspace.new_draft().await.unwrap();

    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Draft(DraftState { cwd, .. }) if cwd == b"/work/moh"
    ));
    assert_eq!(backend.log(), ["detach:session-7"]);
}

#[tokio::test]
async fn workspace_first_submit_materializes_and_failure_preserves_the_draft_prompt() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    backend.push_materialization(Err(ClientSessionError::scripted("storage failed")));
    let materialized = backend.session("session-8", b"/work/moh");
    backend.push_materialization(Ok(WorkspaceMaterialized {
        session: materialized,
        run_id: 0,
    }));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();
    let prompt = String::from("keep this first prompt");

    let error = workspace.submit(&prompt).await.unwrap_err();
    assert_eq!(error.to_string(), "storage failed");
    assert_eq!(prompt, "keep this first prompt");
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Draft(_)
    ));

    assert_eq!(workspace.submit(&prompt).await.unwrap(), 0);
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-8"
    ));
    assert_eq!(
        backend.log(),
        [
            "materialize:/work/moh:keep this first prompt",
            "materialize:/work/moh:keep this first prompt",
        ]
    );
}

#[tokio::test]
async fn workspace_switch_opens_target_before_detaching_old_and_adopts_target_cwd() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_open(Ok(backend.session("session-9", b"/work/other")));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    workspace
        .switch_session("session-9".parse().unwrap())
        .await
        .unwrap();

    assert_eq!(backend.log(), ["open:session-9", "detach:session-7"]);
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Session(snapshot)
            if snapshot.summary.id.to_string() == "session-9"
                && snapshot.summary.cwd == b"/work/other"
    ));
}

#[tokio::test]
async fn switch_opens_target_then_detaches_old_without_cancel() {
    let backend = ScriptedWorkspaceBackend::default();
    let old = backend
        .session("session-7", b"/work/moh")
        .with_detach_error("old attachment remained until disconnect");
    backend.push_startup(Ok(WorkspaceStartup::Attached(old)));
    let sessions = vec![
        browser_summary(
            "session-9",
            "Target session",
            b"/work/other",
            "/work/other",
            2,
        ),
        browser_summary("session-7", "Current session", b"/work/moh", "/work/moh", 1),
    ];
    backend.push_session_list(Ok(sessions.clone()));
    backend.push_session_list(Ok(sessions));
    backend.push_open(Ok(backend.session("session-9", b"/work/other")));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            key(KeyCode::Down),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "list:all",
            "open:session-9",
            "detach:session-7",
        ]
    );
    assert!(
        !backend
            .log()
            .iter()
            .any(|entry| entry.starts_with("cancel:"))
    );
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot)
            if snapshot.summary.id.to_string() == "session-9"
                && snapshot.summary.cwd == b"/work/other"
    ));
    assert!(!ui.session_browser().is_open());
    assert!(ui.editor().value().is_empty());
    assert_eq!(ui.notices(), ["old attachment remained until disconnect"]);
}

#[tokio::test]
async fn switch_failure_keeps_old_chat_and_browser_open() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![
        browser_summary("session-9", "Target session", b"/work/moh", "/work/moh", 2),
        browser_summary("session-7", "Current session", b"/work/moh", "/work/moh", 1),
    ]));
    backend.push_open(Err(ClientSessionError::scripted(
        "target\x1b[2J could not be opened",
    )));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            Event::Paste("target".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(backend.log(), ["list:project:/work/moh", "open:session-9"]);
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-7"
    ));
    assert!(ui.session_browser().is_open());
    assert_eq!(ui.session_browser().query().value(), "target");
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-9".parse().unwrap())
    );
    assert_eq!(
        ui.session_browser().warning(),
        Some("target[2J could not be opened")
    );
}

#[tokio::test]
async fn rename_updates_row_and_current_status_without_closing_browser() {
    let backend = ScriptedWorkspaceBackend::default();
    let current = backend.session("session-7", b"/work/moh");
    backend.push_startup(Ok(WorkspaceStartup::Attached(current.clone())));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "title for session-7",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    backend.push_rename(Ok(()));
    backend.push_rename_update(
        current,
        SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::TitleChanged {
                title: SessionTitle::parse("title for session-7 renamed").unwrap(),
                title_revision: 1,
            },
        }),
    );
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "title for session-7 renamed",
        b"/work/moh",
        "/work/moh",
        2,
    )]));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (terminal, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            key(KeyCode::F(2)),
            Event::Paste(" renamed".into()),
            key(KeyCode::Enter),
            ignored_event(),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "rename:session-7:title for session-7 renamed",
            "list:project:/work/moh",
        ]
    );
    assert!(ui.session_browser().is_open());
    assert!(matches!(ui.session_browser().layer(), BrowserLayer::List));
    assert_eq!(
        ui.session_browser()
            .selected_summary()
            .unwrap()
            .title
            .as_str(),
        "title for session-7 renamed"
    );
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot)
            if snapshot.summary.title.as_str() == "title for session-7 renamed"
    ));
    assert!(status_row(&terminal).contains("title for session-7 renamed"));
}

#[tokio::test]
async fn rename_error_preserves_inline_text() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "title for session-7",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    backend.push_rename(Err(ClientSessionError::scripted("rename\x1b[2J failed")));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            key(KeyCode::F(2)),
            Event::Paste(" attempted".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "rename:session-7:title for session-7 attempted",
        ]
    );
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot)
            if snapshot.summary.title.as_str() == "title for session-7"
    ));
    match ui.session_browser().layer() {
        BrowserLayer::Rename { editor, error, .. } => {
            assert_eq!(editor.value(), "title for session-7 attempted");
            assert_eq!(error.as_deref(), Some("rename[2J failed"));
        }
        layer => panic!("expected retained rename layer, got {layer:?}"),
    }
}

#[tokio::test]
async fn rename_invalid_title_preserves_inline_text_without_rpc() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "title for session-7",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, _, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            key(KeyCode::F(2)),
            Event::Paste(" ".into()),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(backend.log(), ["list:project:/work/moh"]);
    match ui.session_browser().layer() {
        BrowserLayer::Rename { editor, error, .. } => {
            assert_eq!(editor.value(), "title for session-7 ");
            assert!(
                error
                    .as_deref()
                    .is_some_and(|error| error.contains("1-64 scalars"))
            );
        }
        layer => panic!("expected retained rename layer, got {layer:?}"),
    }
}

#[tokio::test]
async fn delete_other_session_keeps_browser_open() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![
        browser_summary("session-9", "Delete other", b"/work/moh", "/work/moh", 2),
        browser_summary("session-7", "Current session", b"/work/moh", "/work/moh", 1),
    ]));
    backend.push_delete(Ok(()));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "Current session",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            control('d'),
            key(KeyCode::Char('y')),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "delete:session-9",
            "list:project:/work/moh",
        ]
    );
    assert!(ui.session_browser().is_open());
    assert!(matches!(ui.session_browser().layer(), BrowserLayer::List));
    assert_eq!(
        ui.session_browser().selected_id(),
        Some("session-7".parse().unwrap())
    );
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-7"
    ));
}

#[tokio::test]
async fn delete_current_closes_browser_and_selects_latest_running_local_session() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "Delete current",
        b"/work/moh",
        "/work/moh",
        2,
    )]));
    backend.push_delete(Ok(()));
    let mut fallback = workspace_snapshot("session-6", b"/work/moh");
    fallback.summary.running = true;
    fallback.summary.running_jobs = 1;
    fallback.jobs = vec![running_job()];
    let fallback = ScriptedWorkspaceSession::new(fallback, Rc::clone(&backend.state.borrow().log));
    backend.push_startup(Ok(WorkspaceStartup::Attached(fallback)));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            control('d'),
            key(KeyCode::Enter),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "delete:session-7",
            "startup:/work/moh",
        ]
    );
    assert!(!ui.session_browser().is_open());
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot)
            if snapshot.summary.id.to_string() == "session-6" && snapshot.summary.running
    ));
}

#[tokio::test]
async fn delete_current_with_no_running_session_shows_draft() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "Delete current",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    backend.push_delete(Ok(()));
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            control('d'),
            key(KeyCode::Char('y')),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "delete:session-7",
            "startup:/work/moh",
        ]
    );
    assert!(!ui.session_browser().is_open());
    assert!(
        matches!(projection, ChatProjection::Draft(DraftState { cwd, .. }) if cwd == b"/work/moh")
    );
}

#[tokio::test]
async fn delete_current_fallback_failure_shows_truthful_draft_and_nonfatal_warning() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "Delete current",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    backend.push_delete(Ok(()));
    backend.push_startup(Err(ClientSessionError::scripted(
        "fallback\x1b[2J unavailable",
    )));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (terminal, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            control('d'),
            key(KeyCode::Char('y')),
            ignored_event(),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "delete:session-7",
            "startup:/work/moh",
        ]
    );
    assert!(!ui.session_browser().is_open());
    assert!(!ui.local_error());
    assert!(
        matches!(projection, ChatProjection::Draft(DraftState { cwd, .. }) if cwd == b"/work/moh")
    );
    assert!(rendered(&terminal).contains("fallback[2J unavailable"));
    assert!(!rendered(&terminal).contains("title for session-7"));
}

#[tokio::test]
async fn remote_delete_applies_the_same_fallback() {
    let backend = ScriptedWorkspaceBackend::default();
    let current = backend.session("session-7", b"/work/moh");
    backend.push_startup(Ok(WorkspaceStartup::Attached(current.clone())));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "Deleted remotely",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    backend.push_list_update(
        current,
        SessionUpdate::Deleted {
            session_id: "session-7".parse().unwrap(),
            cwd: b"/work/moh".to_vec(),
        },
    );
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            ignored_event(),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        ["list:project:/work/moh", "startup:/work/moh"]
    );
    assert!(!ui.session_browser().is_open());
    assert!(matches!(projection, ChatProjection::Draft(_)));
}

#[tokio::test]
async fn delete_failure_keeps_row_and_reports_no_success() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_session_list(Ok(vec![browser_summary(
        "session-7",
        "Retained current",
        b"/work/moh",
        "/work/moh",
        1,
    )]));
    backend.push_delete(Err(ClientSessionError::scripted(
        "delete\x1b[2J persistence failed",
    )));
    backend.push_open(Ok(backend.session("session-7", b"/work/moh")));
    let workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let (_, ui, projection, _) = run_workspace_with_events(
        workspace,
        [
            Event::Paste("/sessions".into()),
            key(KeyCode::Enter),
            control('d'),
            key(KeyCode::Char('y')),
            control('c'),
        ],
    )
    .await
    .unwrap();

    assert_eq!(
        backend.log(),
        [
            "list:project:/work/moh",
            "delete:session-7",
            "open:session-7",
            "detach:session-7",
        ]
    );
    assert!(ui.session_browser().is_open());
    assert!(matches!(
        ui.session_browser().layer(),
        BrowserLayer::ConfirmDelete { .. }
    ));
    assert_eq!(
        ui.session_browser()
            .selected_summary()
            .unwrap()
            .title
            .as_str(),
        "Retained current"
    );
    assert_eq!(
        ui.session_browser().warning(),
        Some("delete[2J persistence failed")
    );
    assert!(
        !ui.notices()
            .iter()
            .any(|notice| notice.to_lowercase().contains("deleted"))
    );
    assert!(matches!(
        projection,
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-7"
    ));
}

#[tokio::test]
async fn workspace_current_delete_failure_reopens_before_returning_the_original_error() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_delete(Err(ClientSessionError::scripted("original delete failure")));
    backend.push_open(Ok(backend.session("session-7", b"/work/moh")));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    let error = workspace
        .delete_session("session-7".parse().unwrap())
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "original delete failure");
    assert_eq!(
        backend.log(),
        ["delete:session-7", "open:session-7", "detach:session-7"]
    );
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-7"
    ));
}

#[tokio::test]
async fn workspace_current_delete_uses_startup_fallback_for_the_deleted_cwd() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_delete(Ok(()));
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    workspace
        .delete_session("session-7".parse().unwrap())
        .await
        .unwrap();

    assert_eq!(backend.log(), ["delete:session-7", "startup:/work/moh"]);
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Draft(_)
    ));
}

#[tokio::test]
async fn workspace_current_delete_keeps_truthful_draft_when_fallback_fails() {
    let backend = ScriptedWorkspaceBackend::default();
    backend.push_startup(Ok(WorkspaceStartup::Attached(
        backend.session("session-7", b"/work/moh"),
    )));
    backend.push_delete(Ok(()));
    backend.push_startup(Err(ClientSessionError::scripted("fallback unavailable")));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    workspace
        .delete_session("session-7".parse().unwrap())
        .await
        .unwrap();

    assert_eq!(backend.log(), ["delete:session-7", "startup:/work/moh"]);
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Draft(DraftState { cwd, settings, .. })
            if cwd == b"/work/moh" && settings.context_tokens == 0
    ));
    assert_eq!(
        workspace.next_update().await.unwrap(),
        WorkspaceUpdate::Warning("fallback unavailable".into())
    );
}

#[tokio::test]
async fn workspace_remote_delete_uses_the_same_startup_fallback() {
    let backend = ScriptedWorkspaceBackend::default();
    let session = backend.session("session-7", b"/work/moh");
    session.queue_update(SessionUpdate::Deleted {
        session_id: "session-7".parse().unwrap(),
        cwd: b"/work/moh".to_vec(),
    });
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    backend.push_startup(Ok(WorkspaceStartup::Draft(workspace_draft(b"/work/moh"))));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    assert_eq!(
        workspace.next_update().await.unwrap(),
        WorkspaceUpdate::Deleted {
            session_id: "session-7".parse().unwrap(),
            cwd: b"/work/moh".to_vec(),
        }
    );
    assert_eq!(backend.log(), ["startup:/work/moh"]);
    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Draft(_)
    ));
}

#[tokio::test]
async fn workspace_new_draft_commits_when_old_detach_fails_and_warns_before_waiting() {
    let backend = ScriptedWorkspaceBackend::default();
    let session = backend
        .session("session-7", b"/work/moh")
        .with_detach_error("exact detach failed");
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    workspace.new_draft().await.unwrap();

    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Draft(DraftState { cwd, .. }) if cwd == b"/work/moh"
    ));
    assert_eq!(backend.log(), ["detach:session-7"]);
    assert_eq!(
        workspace.next_update().await.unwrap(),
        WorkspaceUpdate::Warning("exact detach failed".into())
    );
}

#[tokio::test]
async fn workspace_switch_commits_when_old_detach_fails_and_warns_before_target_updates() {
    let backend = ScriptedWorkspaceBackend::default();
    let old = backend
        .session("session-7", b"/work/moh")
        .with_detach_error("old attachment remained");
    let target = backend.session("session-9", b"/work/other");
    target.queue_update(SessionUpdate::Event(SessionEventEnvelope {
        sequence: 15,
        event: SessionEvent::TitleChanged {
            title: SessionTitle::parse("updated target").unwrap(),
            title_revision: 1,
        },
    }));
    backend.push_startup(Ok(WorkspaceStartup::Attached(old)));
    backend.push_open(Ok(target));
    let mut workspace =
        WorkspaceController::launch(backend.clone(), b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();
    backend.clear_log();

    workspace
        .switch_session("session-9".parse().unwrap())
        .await
        .unwrap();

    assert!(matches!(
        workspace.current_projection(),
        ChatProjection::Session(snapshot) if snapshot.summary.id.to_string() == "session-9"
    ));
    assert_eq!(backend.log(), ["open:session-9", "detach:session-7"]);
    assert_eq!(
        workspace.next_update().await.unwrap(),
        WorkspaceUpdate::Warning("old attachment remained".into())
    );
    assert!(matches!(
        workspace.next_update().await.unwrap(),
        WorkspaceUpdate::Session(SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15,
            event: SessionEvent::TitleChanged { .. },
        }))
    ));
}

#[tokio::test]
async fn workspace_events_update_projection_before_delivery_and_seed_the_next_draft() {
    let backend = ScriptedWorkspaceBackend::default();
    let mut session = backend.session("session-7", b"/work/moh");
    session.snapshot.jobs.clear();
    let changed_at = activity_time();
    let title = SessionTitle::parse("latest title").unwrap();
    let settings = SessionSettings {
        model: "gpt-5.6-terra".into(),
        reasoning: ReasoningLevel::Low,
        context_tokens: 456,
    };
    let catalog = ModelCatalogState::Ready(models());
    let jobs = vec![running_job()];
    let events = [
        SessionEvent::TitleChanged {
            title: title.clone(),
            title_revision: 2,
        },
        SessionEvent::Started {
            run_id: 88,
            prompt: "latest prompt".into(),
        },
        SessionEvent::AssistantDelta {
            run_id: 88,
            text: "partial ".into(),
        },
        SessionEvent::ToolStarted {
            run_id: 88,
            call_id: "call-latest".into(),
            name: "read".into(),
            arguments: json!({"path": "src/main.rs"}),
        },
        SessionEvent::ToolFinished {
            run_id: 88,
            call_id: "call-latest".into(),
            name: "read".into(),
        },
        SessionEvent::ContextUsage {
            run_id: 88,
            input_tokens: 321,
            last_activity: changed_at,
        },
        SessionEvent::SettingsChanged {
            settings: settings.clone(),
            last_activity: changed_at,
        },
        SessionEvent::CatalogChanged(catalog.clone()),
        SessionEvent::JobsChanged(jobs.clone()),
        SessionEvent::PersistenceWarning(Some("latest warning".into())),
        SessionEvent::Completed {
            run_id: 88,
            response: "latest answer".into(),
            last_activity: changed_at,
        },
    ];
    for (offset, event) in events.into_iter().enumerate() {
        session.queue_update(SessionUpdate::Event(SessionEventEnvelope {
            sequence: 15 + u64::try_from(offset).unwrap(),
            event,
        }));
    }
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    let mut workspace =
        WorkspaceController::launch(backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();

    for expected_sequence in 15..=25 {
        assert!(matches!(
            workspace.next_update().await.unwrap(),
            WorkspaceUpdate::Session(SessionUpdate::Event(SessionEventEnvelope {
                sequence,
                ..
            })) if sequence == expected_sequence
        ));
        assert!(matches!(
            workspace.current_projection(),
            ChatProjection::Session(snapshot) if snapshot.sequence == expected_sequence
        ));
        if expected_sequence == 17 {
            assert!(matches!(
                workspace.current_projection(),
                ChatProjection::Session(snapshot)
                    if matches!(
                        snapshot.active_run.as_ref(),
                        Some(ActiveRunSnapshot { assistant_text, .. }) if assistant_text == "partial "
                    )
            ));
        }
        if expected_sequence == 16 {
            assert!(matches!(
                workspace.current_projection(),
                ChatProjection::Session(snapshot)
                    if snapshot.summary.running && snapshot.summary.running_jobs == 0
            ));
        }
        if expected_sequence == 20 {
            assert!(matches!(
                workspace.current_projection(),
                ChatProjection::Session(snapshot) if snapshot.settings.context_tokens == 321
            ));
        }
        if expected_sequence == 23 {
            assert!(matches!(
                workspace.current_projection(),
                ChatProjection::Session(snapshot)
                    if snapshot.summary.running && snapshot.summary.running_jobs == 1
            ));
        }
    }

    let ChatProjection::Session(snapshot) = workspace.current_projection() else {
        panic!("workspace must remain attached");
    };
    assert_eq!(snapshot.summary.title, title);
    assert_eq!(snapshot.summary.title_revision, 2);
    assert_eq!(snapshot.summary.last_activity, changed_at);
    assert_eq!(snapshot.settings, settings);
    assert_eq!(snapshot.catalog, catalog);
    assert_eq!(snapshot.jobs, jobs);
    assert_eq!(
        snapshot.persistence_warning.as_deref(),
        Some("latest warning")
    );
    assert_eq!(snapshot.active_run, None);
    assert!(!snapshot.busy);
    assert!(!snapshot.summary.busy);
    assert_eq!(snapshot.summary.running_jobs, 1);
    assert!(snapshot.summary.running);
    assert!(matches!(
        snapshot.transcript.as_slice(),
        [.., TranscriptItem::User(prompt), TranscriptItem::ToolStarted { call_id, .. }, TranscriptItem::Assistant(answer)]
            if prompt == "latest prompt" && call_id == "call-latest" && answer == "latest answer"
    ));

    workspace.new_draft().await.unwrap();
    let ChatProjection::Draft(draft) = workspace.current_projection() else {
        panic!("new draft must replace the attachment");
    };
    assert_eq!(draft.settings.model, "gpt-5.6-terra");
    assert_eq!(draft.settings.reasoning, ReasoningLevel::Low);
    assert_eq!(draft.settings.context_tokens, 0);
    assert_eq!(draft.catalog, catalog);
}

#[tokio::test]
async fn workspace_snapshot_replacement_is_authoritative_before_delivery() {
    let backend = ScriptedWorkspaceBackend::default();
    let session = backend.session("session-7", b"/work/moh");
    let mut replacement = workspace_snapshot("session-7", b"/work/other");
    replacement.summary.title = SessionTitle::parse("recovered title").unwrap();
    replacement.settings = SessionSettings {
        model: "gpt-5.6-terra".into(),
        reasoning: ReasoningLevel::Low,
        context_tokens: 777,
    };
    replacement.sequence = 91;
    session.queue_update(SessionUpdate::SnapshotReplaced(Box::new(
        replacement.clone(),
    )));
    backend.push_startup(Ok(WorkspaceStartup::Attached(session)));
    let mut workspace =
        WorkspaceController::launch(backend, b"/work/moh".to_vec(), LaunchMode::Startup)
            .await
            .unwrap();

    assert_eq!(
        workspace.next_update().await.unwrap(),
        WorkspaceUpdate::Session(SessionUpdate::SnapshotReplaced(Box::new(
            replacement.clone()
        )))
    );
    assert_eq!(
        workspace.current_projection(),
        &ChatProjection::session(replacement)
    );
}
