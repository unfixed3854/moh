mod support;

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use futures::future::BoxFuture;
use moh::{
    backend::ActivityTracker,
    harness::{EngineEvent, Message, Role, RunFailure, RunFailureKind, RunStage},
    runtime::rig::ReasoningLevel,
    session::{
        AttachmentId, ConnectionId, DurableTurn, MaterializeSession, ModelCatalogState,
        ModelInfoDto, RunFailureSnapshot, SessionCommandError, SessionEngineBundle,
        SessionEngineFactory, SessionEvent, SessionId, SessionListScope, SessionManagerError,
        SessionManagerHandle, SessionManagerLifecycle, SessionManagerLifecycleError, SessionRecord,
        SessionRepository, SessionSelector, SessionSettings, SessionStore, SessionStoreError,
        SessionSummary, SessionTitle, SessionTitleGenerator, StartupResult, TitleSource,
        TranscriptItem, TurnStatus, fallback_title,
    },
    tools::{JobDetails, JobKind, JobRegistry, JobState},
};
use tempfile::{TempDir, tempdir};

use support::{
    ControlledEngine, ControlledEngineControl, FailingRepository, ScriptedTitleGenerator,
    controlled_engine, engine_bundle,
};

#[derive(Clone)]
struct ControlledEngineFactory {
    controls: Arc<Mutex<Vec<ControlledEngineControl>>>,
    registries: Arc<Mutex<Vec<JobRegistry>>>,
    defaults: SessionSettings,
    catalog: ModelCatalogState,
    title_generator: Arc<ScriptedTitleGenerator>,
}

impl ControlledEngineFactory {
    fn new() -> Self {
        Self {
            controls: Arc::new(Mutex::new(Vec::new())),
            registries: Arc::new(Mutex::new(Vec::new())),
            defaults: SessionSettings {
                model: "gpt-5.6-terra".into(),
                reasoning: ReasoningLevel::Medium,
                context_tokens: 0,
            },
            catalog: ModelCatalogState::Loading,
            title_generator: Arc::new(ScriptedTitleGenerator::default()),
        }
    }

    fn controls(&self) -> Vec<ControlledEngineControl> {
        self.controls.lock().unwrap().clone()
    }

    fn registries(&self) -> Vec<JobRegistry> {
        self.registries.lock().unwrap().clone()
    }

    fn with_catalog(mut self, catalog: ModelCatalogState) -> Self {
        self.catalog = catalog;
        self
    }

    fn title_generator(&self) -> Arc<ScriptedTitleGenerator> {
        Arc::clone(&self.title_generator)
    }
}

impl SessionEngineFactory for ControlledEngineFactory {
    type Engine = ControlledEngine;

    fn catalog(&self) -> ModelCatalogState {
        self.catalog.clone()
    }

    fn default_settings(&self) -> SessionSettings {
        self.defaults.clone()
    }

    fn title_generator(&self) -> Arc<dyn SessionTitleGenerator> {
        self.title_generator.clone()
    }

    fn create(
        &self,
        settings: &SessionSettings,
    ) -> Result<SessionEngineBundle<Self::Engine>, moh::harness::RunFailure> {
        let (engine, control) = controlled_engine();
        self.controls.lock().unwrap().push(control);
        let bundle = engine_bundle(engine, settings);
        self.registries.lock().unwrap().push(bundle.jobs.clone());
        Ok(bundle)
    }
}

#[derive(Clone)]
struct CreateFailingFactory {
    inner: ControlledEngineFactory,
}

impl SessionEngineFactory for CreateFailingFactory {
    type Engine = ControlledEngine;

    fn catalog(&self) -> ModelCatalogState {
        self.inner.catalog()
    }

    fn default_settings(&self) -> SessionSettings {
        self.inner.default_settings()
    }

    fn title_generator(&self) -> Arc<dyn SessionTitleGenerator> {
        self.inner.title_generator()
    }

    fn create(
        &self,
        _settings: &SessionSettings,
    ) -> Result<SessionEngineBundle<Self::Engine>, RunFailure> {
        Err(RunFailure::new(
            RunStage::Startup,
            RunFailureKind::BudgetExhausted,
            false,
            "runtime \u{1b}[31mfactory\u{1b}[0m rejected\nsettings",
        ))
    }
}

#[derive(Clone)]
struct CorruptMaterializedPromptLinkRepository {
    inner: Arc<dyn SessionRepository>,
}

impl SessionRepository for CorruptMaterializedPromptLinkRepository {
    fn resolve(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.inner.resolve(selector, cwd_for_title)
    }

    fn load(&self, id: SessionId) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.inner.load(id)
    }

    fn materialize(
        &self,
        request: MaterializeSession,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let mut record = inner.materialize(request).await?;
            record.turns[0].prompt_position = 1;
            Ok(record)
        })
    }

    fn list(
        &self,
        scope: SessionListScope,
    ) -> BoxFuture<'static, Result<Vec<SessionSummary>, SessionStoreError>> {
        self.inner.list(scope)
    }

    fn rename(
        &self,
        id: SessionId,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.inner.rename(id, title)
    }

    fn compare_and_set_generated_title(
        &self,
        id: SessionId,
        expected_revision: u64,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<Option<SessionRecord>, SessionStoreError>> {
        self.inner
            .compare_and_set_generated_title(id, expected_revision, title)
    }

    fn delete(&self, id: SessionId) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        self.inner.delete(id)
    }

    fn checkpoint(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        self.inner.checkpoint(record)
    }

    fn update_metadata(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        self.inner.update_metadata(record)
    }
}

#[derive(Debug)]
struct TestJobDetails(&'static str);

impl JobDetails for TestJobDetails {
    fn render(&self) -> String {
        self.0.into()
    }
}

struct ManagerFixture {
    _directory: TempDir,
    repository: Arc<dyn SessionRepository>,
    factory: ControlledEngineFactory,
    manager: SessionManagerHandle,
    lifecycle: SessionManagerLifecycle,
    cwd: Vec<u8>,
}

async fn manager_fixture() -> ManagerFixture {
    let directory = tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let repository: Arc<dyn SessionRepository> = Arc::new(opened.store);
    let factory = ControlledEngineFactory::new();
    let activity = ActivityTracker::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(Arc::clone(&repository), factory.clone(), activity.clone());
    ManagerFixture {
        _directory: directory,
        repository,
        factory,
        manager,
        lifecycle,
        cwd: b"/work/moh".to_vec(),
    }
}

#[tokio::test]
async fn materialization_rejects_blank_prompt_before_creating_durable_state() {
    let fixture = manager_fixture().await;

    let result = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            " \n\t ".into(),
            fixture.factory.default_settings(),
            ConnectionId(9),
            AttachmentId(1),
        )
        .await;
    let Err(error) = result else {
        panic!("blank materialization unexpectedly succeeded");
    };

    assert_eq!(
        error.to_string(),
        "the first session prompt must contain non-whitespace text"
    );
    assert!(matches!(
        error,
        SessionManagerError::Session(SessionCommandError::InvalidPrompt)
    ));
    assert!(
        fixture
            .repository
            .list(SessionListScope::All)
            .await
            .unwrap()
            .is_empty(),
        "blank materialization must not create a durable row"
    );
    assert!(fixture.factory.controls().is_empty());

    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn draft_defaults_are_fresh_and_do_not_attach_or_select_running_work() {
    let fixture = manager_fixture().await;
    let mut running_settings = fixture.factory.default_settings();
    running_settings.model = "running-session-model".into();
    let running = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "running work".into(),
            running_settings,
            ConnectionId(10),
            AttachmentId(1),
        )
        .await
        .unwrap();

    let defaults = fixture
        .manager
        .draft_defaults(fixture.cwd.clone())
        .await
        .unwrap();

    assert_eq!(defaults.cwd, fixture.cwd);
    assert_eq!(defaults.settings, fixture.factory.default_settings());
    assert_eq!(defaults.catalog, fixture.factory.catalog());
    let summaries = fixture.manager.list(SessionListScope::All).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, running.session.snapshot.summary.id);
    assert_eq!(summaries[0].attached_clients, 1);
    assert_eq!(fixture.factory.controls().len(), 1);

    running.session.handle.cancel().await.unwrap();
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

async fn assert_reopened_failed_first_turn(
    path: &Path,
    prompt: &str,
    expected_failure: RunFailureSnapshot,
) {
    let reopened = SessionStore::open_at(path).await.unwrap().store;
    let summaries = reopened.list(SessionListScope::All).await.unwrap();
    assert_eq!(summaries.len(), 1, "the failed submission keeps one row");
    assert!(!summaries[0].running);
    assert!(!summaries[0].busy);
    let record = reopened.load(summaries[0].id).await.unwrap();
    assert_eq!(
        record.transcript,
        vec![
            TranscriptItem::User(prompt.into()),
            TranscriptItem::Failed {
                run_id: 0,
                failure: expected_failure,
            },
        ]
    );
    assert_eq!(
        record.turns,
        vec![DurableTurn {
            ordinal: 0,
            run_id: 0,
            prompt_position: 0,
            status: TurnStatus::Failed,
        }]
    );
    assert!(record.history.is_empty());
}

#[tokio::test]
async fn detach_connection_cleans_every_live_actor_without_cancelling_runs() {
    let fixture = manager_fixture().await;
    let first = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "first work".into(),
            fixture.factory.default_settings(),
            ConnectionId(11),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let second = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "second work".into(),
            fixture.factory.default_settings(),
            ConnectionId(11),
            AttachmentId(2),
        )
        .await
        .unwrap();

    fixture
        .manager
        .detach_connection(ConnectionId(11))
        .await
        .unwrap();

    let summaries = fixture
        .manager
        .list(SessionListScope::Project(fixture.cwd.clone()))
        .await
        .unwrap();
    assert_eq!(summaries.len(), 2);
    assert!(summaries.iter().all(|summary| summary.busy));
    assert!(
        summaries
            .iter()
            .all(|summary| summary.attached_clients == 0)
    );

    first.session.handle.cancel().await.unwrap();
    second.session.handle.cancel().await.unwrap();
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn exact_detach_routes_to_one_session_and_is_idempotent() {
    let fixture = manager_fixture().await;
    let mut first = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "first attached run".into(),
            fixture.factory.default_settings(),
            ConnectionId(12),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let mut second = fixture
        .manager
        .open(
            SessionSelector::Id(first.session.snapshot.summary.id),
            fixture.cwd.clone(),
            ConnectionId(12),
            AttachmentId(2),
        )
        .await
        .unwrap();
    let extra = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "independent attached run".into(),
            fixture.factory.default_settings(),
            ConnectionId(12),
            AttachmentId(3),
        )
        .await
        .unwrap();

    let remaining = fixture
        .manager
        .detach(
            first.session.snapshot.summary.id,
            ConnectionId(12),
            AttachmentId(1),
        )
        .await
        .unwrap();
    assert_eq!(remaining, 1);
    let remaining = fixture
        .manager
        .detach(
            first.session.snapshot.summary.id,
            ConnectionId(12),
            AttachmentId(1),
        )
        .await
        .unwrap();
    assert_eq!(remaining, 1);

    let summaries = fixture
        .manager
        .list(SessionListScope::Project(fixture.cwd.clone()))
        .await
        .unwrap();
    assert_eq!(
        summaries
            .iter()
            .find(|summary| summary.id == first.session.snapshot.summary.id)
            .unwrap()
            .attached_clients,
        1
    );
    assert_eq!(
        summaries
            .iter()
            .find(|summary| summary.id == extra.session.snapshot.summary.id)
            .unwrap()
            .attached_clients,
        1
    );

    second.handle.cancel().await.unwrap();
    assert!(matches!(
        second.events.recv().await.unwrap().event,
        SessionEvent::Cancelled { run_id: 0 }
    ));
    assert_eq!(first.session.events.recv().await, None);

    extra.session.handle.cancel().await.unwrap();
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn opening_by_id_lazily_restores_stored_history_after_manager_restart() {
    let fixture = manager_fixture().await;
    let mut first = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "persist me".into(),
            fixture.factory.default_settings(),
            ConnectionId(21),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let id = first.session.snapshot.summary.id;
    fixture.factory.controls()[0].emit(Ok(EngineEvent::Completed("stored answer".into())));
    loop {
        let event = first.session.events.recv().await.unwrap();
        if matches!(event.event, moh::session::SessionEvent::Completed { .. }) {
            break;
        }
    }
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();

    let restarted_factory = ControlledEngineFactory::new();
    let (restarted, restarted_lifecycle) = SessionManagerHandle::spawn(
        Arc::clone(&fixture.repository),
        restarted_factory.clone(),
        ActivityTracker::new(),
    );
    let restored = restarted
        .open(
            SessionSelector::Id(id),
            b"/different/name-scope".to_vec(),
            ConnectionId(22),
            AttachmentId(1),
        )
        .await
        .unwrap();

    assert_eq!(restarted_factory.controls().len(), 1);
    assert_eq!(
        restored.snapshot.transcript,
        vec![
            TranscriptItem::User("persist me".into()),
            TranscriptItem::Assistant("stored answer".into()),
        ]
    );
    assert_eq!(
        fixture.repository.load(id).await.unwrap().history,
        vec![
            Message::new(Role::User, "persist me"),
            Message::new(Role::Assistant, "stored answer"),
        ]
    );

    restarted.shutdown().await.unwrap();
    restarted_lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn opening_by_name_is_scoped_to_the_supplied_cwd() {
    let fixture = manager_fixture().await;
    let title = SessionTitle::parse("review").unwrap();
    let created = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "review this change".into(),
            fixture.factory.default_settings(),
            ConnectionId(31),
            AttachmentId(1),
        )
        .await
        .unwrap();
    fixture
        .manager
        .rename(created.session.snapshot.summary.id, title.clone())
        .await
        .unwrap();

    assert!(
        fixture
            .manager
            .open(
                SessionSelector::Title(title.clone()),
                b"/work/other".to_vec(),
                ConnectionId(32),
                AttachmentId(1),
            )
            .await
            .is_err()
    );
    let reopened = fixture
        .manager
        .open(
            SessionSelector::Title(title),
            fixture.cwd.clone(),
            ConnectionId(33),
            AttachmentId(2),
        )
        .await
        .unwrap();
    assert_eq!(
        reopened.snapshot.summary.id,
        created.session.snapshot.summary.id
    );

    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn list_overlays_live_activity_when_the_repository_checkpoint_is_stale() {
    let repository = FailingRepository::default();
    repository.fail_checkpoints(true);
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let factory = ControlledEngineFactory::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(repository_boundary, factory.clone(), ActivityTracker::new());
    let cwd = b"/work/moh".to_vec();
    let mut session = manager
        .materialize_and_submit(
            cwd.clone(),
            "measure activity".into(),
            factory.default_settings(),
            ConnectionId(41),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let stored_activity = session.session.snapshot.summary.last_activity;
    factory.controls()[0].emit(Ok(EngineEvent::ContextUsage { input_tokens: 42 }));
    let live_activity = loop {
        let event = session.session.events.recv().await.unwrap();
        if let SessionEvent::ContextUsage { last_activity, .. } = event.event {
            break last_activity;
        }
    };

    let summary = manager
        .list(SessionListScope::Project(cwd))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(live_activity > stored_activity);
    assert_eq!(summary.last_activity, live_activity);
    assert_eq!(
        repository.load(summary.id).await.unwrap().last_activity,
        stored_activity
    );

    session.session.handle.cancel().await.unwrap();
    assert!(matches!(
        manager.shutdown().await.unwrap_err(),
        SessionManagerError::Session(SessionCommandError::Persistence { .. })
    ));
    repository.fail_checkpoints(false);
    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn lifecycle_join_surfaces_dirty_shutdown_failure_after_command_channel_closes() {
    let repository = FailingRepository::default();
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let factory = ControlledEngineFactory::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(repository_boundary, factory.clone(), ActivityTracker::new());
    let mut session = manager
        .materialize_and_submit(
            b"/work/moh".to_vec(),
            "committed live answer".into(),
            factory.default_settings(),
            ConnectionId(51),
            AttachmentId(1),
        )
        .await
        .unwrap();
    repository.fail_checkpoints(true);
    factory.controls()[0].emit(Ok(EngineEvent::Completed("not durable yet".into())));
    loop {
        if matches!(
            session.session.events.recv().await.unwrap().event,
            SessionEvent::Completed { .. }
        ) {
            break;
        }
    }

    drop(session);
    drop(manager);
    let error = lifecycle.join().await.unwrap_err();
    assert!(matches!(
        error,
        SessionManagerLifecycleError::Manager(SessionManagerError::Session(
            SessionCommandError::Persistence { .. }
        ))
    ));
}

#[tokio::test]
async fn startup_selects_latest_running_run_or_job_in_project() {
    let fixture = manager_fixture().await;
    let first = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "older running work".into(),
            fixture.factory.default_settings(),
            ConnectionId(61),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let latest = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "latest background work".into(),
            fixture.factory.default_settings(),
            ConnectionId(62),
            AttachmentId(1),
        )
        .await
        .unwrap();
    latest.session.handle.cancel().await.unwrap();
    let latest_job = fixture.factory.registries()[1]
        .start(
            JobKind::Bash,
            "latest background work",
            Arc::new(TestJobDetails("running")),
        )
        .unwrap();
    let outside = fixture
        .manager
        .materialize_and_submit(
            b"/work/other".to_vec(),
            "newest but outside the project".into(),
            fixture.factory.default_settings(),
            ConnectionId(64),
            AttachmentId(1),
        )
        .await
        .unwrap();

    let StartupResult::Attached(started) = fixture
        .manager
        .startup(fixture.cwd.clone(), ConnectionId(63), AttachmentId(1))
        .await
        .unwrap()
    else {
        panic!("a running project session must be attached");
    };
    assert_eq!(
        started.snapshot.summary.id,
        latest.session.snapshot.summary.id
    );
    assert_eq!(started.snapshot.summary.attached_clients, 2);

    latest_job
        .finish(JobState::Completed, Arc::new(TestJobDetails("done")))
        .unwrap();
    first.session.handle.cancel().await.unwrap();
    outside.session.handle.cancel().await.unwrap();
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn startup_returns_draft_when_only_idle_sessions_exist() {
    let fixture = manager_fixture().await;
    let idle = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "completed work".into(),
            fixture.factory.default_settings(),
            ConnectionId(71),
            AttachmentId(1),
        )
        .await
        .unwrap();
    idle.session.handle.cancel().await.unwrap();

    let StartupResult::Draft(draft) = fixture
        .manager
        .startup(fixture.cwd.clone(), ConnectionId(72), AttachmentId(1))
        .await
        .unwrap()
    else {
        panic!("an idle project must return draft defaults");
    };
    assert_eq!(draft.cwd, fixture.cwd);
    assert_eq!(draft.settings, fixture.factory.default_settings());
    assert_eq!(
        fixture
            .manager
            .list(SessionListScope::Project(draft.cwd.clone()))
            .await
            .unwrap()
            .len(),
        1,
        "startup must not persist another empty session"
    );

    drop(idle);
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn materialization_persists_before_actor_stream_is_polled() {
    let repository = FailingRepository::default();
    let gate = repository.gate_materializations();
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let factory = ControlledEngineFactory::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(repository_boundary, factory.clone(), ActivityTracker::new());
    let cwd = b"/work/moh".to_vec();
    let prompt = "Persist this before polling".to_owned();
    let materializing = tokio::spawn({
        let manager = manager.clone();
        let cwd = cwd.clone();
        let prompt = prompt.clone();
        let settings = factory.default_settings();
        async move {
            manager
                .materialize_and_submit(cwd, prompt, settings, ConnectionId(81), AttachmentId(1))
                .await
        }
    });

    gate.wait_until_entered().await;
    assert!(factory.controls().is_empty());
    gate.release();

    let materialized = materializing.await.unwrap().unwrap();
    assert_eq!(materialized.run_id, 0);
    assert_eq!(
        materialized
            .session
            .snapshot
            .active_run
            .as_ref()
            .unwrap()
            .run_id,
        0
    );
    let control = factory.controls().into_iter().next().unwrap();
    assert_eq!(control.wait_for_request_count(1).await[0].prompt, prompt);
    let stored = repository
        .load(materialized.session.snapshot.summary.id)
        .await
        .unwrap();
    assert_eq!(stored.transcript, vec![TranscriptItem::User(prompt)]);
    assert_eq!(stored.turns.len(), 1);
    assert_eq!(stored.turns[0].run_id, 0);
    assert_eq!(stored.turns[0].status, moh::session::TurnStatus::Running);
    assert_eq!(
        factory.title_generator().requests()[0].reasoning,
        ReasoningLevel::Medium,
        "selected effort is the fallback while catalog metadata is unavailable"
    );

    materialized.session.handle.cancel().await.unwrap();
    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn factory_create_failure_persists_failed_first_turn_without_actor_or_ghost_row() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let repository: Arc<dyn SessionRepository> =
        Arc::new(SessionStore::open_at(&path).await.unwrap().store);
    let factory = CreateFailingFactory {
        inner: ControlledEngineFactory::new(),
    };
    let activity = ActivityTracker::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(Arc::clone(&repository), factory.clone(), activity.clone());
    let cwd = b"/work/factory-failure".to_vec();
    let prompt = "Keep the prompt when runtime creation fails";

    let error = match manager
        .materialize_and_submit(
            cwd.clone(),
            prompt.into(),
            factory.default_settings(),
            ConnectionId(82),
            AttachmentId(1),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("runtime construction failure must reject materialization"),
    };
    let SessionManagerError::Runtime(error) = error else {
        panic!("the original runtime construction error must be returned");
    };
    assert_eq!(error.stage(), RunStage::Startup);
    assert_eq!(error.kind(), &RunFailureKind::BudgetExhausted);
    assert_eq!(
        error.message(),
        "runtime \u{1b}[31mfactory\u{1b}[0m rejected\nsettings"
    );
    assert!(matches!(
        manager
            .startup(cwd, ConnectionId(83), AttachmentId(1))
            .await
            .unwrap(),
        StartupResult::Draft(_)
    ));
    assert_eq!(activity.subscribe().borrow().active_runs, 0);

    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
    drop(repository);
    assert_reopened_failed_first_turn(
        &path,
        prompt,
        RunFailureSnapshot {
            stage: RunStage::Startup,
            kind: RunFailureKind::RuntimeInfrastructure,
            retryable: false,
            message: "runtime factory rejected settings".into(),
        },
    )
    .await;
}

#[tokio::test]
async fn spawn_materialized_failure_persists_failed_first_turn_without_actor_or_ghost_row() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let stored: Arc<dyn SessionRepository> =
        Arc::new(SessionStore::open_at(&path).await.unwrap().store);
    let repository: Arc<dyn SessionRepository> =
        Arc::new(CorruptMaterializedPromptLinkRepository {
            inner: Arc::clone(&stored),
        });
    let factory = ControlledEngineFactory::new();
    let activity = ActivityTracker::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(Arc::clone(&repository), factory.clone(), activity.clone());
    let cwd = b"/work/spawn-failure".to_vec();
    let prompt = "Keep the prompt when actor startup fails";

    let error = match manager
        .materialize_and_submit(
            cwd.clone(),
            prompt.into(),
            factory.default_settings(),
            ConnectionId(84),
            AttachmentId(1),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("actor startup failure must reject materialization"),
    };
    assert!(matches!(
        error,
        SessionManagerError::Session(SessionCommandError::Projection { ref message })
            if message == "materialized running turn does not reference the first prompt"
    ));
    assert!(matches!(
        manager
            .startup(cwd, ConnectionId(85), AttachmentId(1))
            .await
            .unwrap(),
        StartupResult::Draft(_)
    ));
    assert_eq!(activity.subscribe().borrow().active_runs, 0);

    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
    drop(repository);
    drop(stored);
    assert_reopened_failed_first_turn(
        &path,
        prompt,
        RunFailureSnapshot {
            stage: RunStage::Startup,
            kind: RunFailureKind::RuntimeInfrastructure,
            retryable: false,
            message: "session projection failed: materialized running turn does not reference the first prompt".into(),
        },
    )
    .await;
}

#[tokio::test]
async fn failed_compensating_checkpoint_surfaces_persistence_and_deletes_false_running_row() {
    let repository = FailingRepository::default();
    repository.fail_checkpoints(true);
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let factory = CreateFailingFactory {
        inner: ControlledEngineFactory::new(),
    };
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(repository_boundary, factory.clone(), ActivityTracker::new());

    let error = match manager
        .materialize_and_submit(
            b"/work/checkpoint-failure".to_vec(),
            "Do not retain a false running row".into(),
            factory.default_settings(),
            ConnectionId(86),
            AttachmentId(1),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("failed compensation must never report success"),
    };
    assert!(matches!(
        error,
        SessionManagerError::Store(SessionStoreError::Database {
            operation: "checkpoint failpoint",
            ..
        })
    ));
    assert_eq!(repository.take_checkpoint_attempts().len(), 1);
    assert!(
        repository
            .list(SessionListScope::All)
            .await
            .unwrap()
            .is_empty(),
        "best-effort deletion must remove the false-running row"
    );

    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn list_overlays_live_run_job_and_attachment_state_in_both_scopes() {
    let fixture = manager_fixture().await;
    let first = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "Run in the current project".into(),
            fixture.factory.default_settings(),
            ConnectionId(91),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let reopened = fixture
        .manager
        .open(
            SessionSelector::Id(first.session.snapshot.summary.id),
            Vec::new(),
            ConnectionId(92),
            AttachmentId(2),
        )
        .await
        .unwrap();
    let other_cwd = b"/work/other".to_vec();
    let other = fixture
        .manager
        .materialize_and_submit(
            other_cwd.clone(),
            "Run a background job".into(),
            fixture.factory.default_settings(),
            ConnectionId(93),
            AttachmentId(1),
        )
        .await
        .unwrap();
    other.session.handle.cancel().await.unwrap();
    let job = fixture.factory.registries()[1]
        .start(
            JobKind::Bash,
            "background work",
            Arc::new(TestJobDetails("running")),
        )
        .unwrap();

    let project = fixture
        .manager
        .list(SessionListScope::Project(fixture.cwd.clone()))
        .await
        .unwrap();
    let all = fixture.manager.list(SessionListScope::All).await.unwrap();

    assert_eq!(project.len(), 1);
    assert_eq!(project[0].id, first.session.snapshot.summary.id);
    assert!(project[0].busy);
    assert!(project[0].running);
    assert_eq!(project[0].running_jobs, 0);
    assert_eq!(project[0].attached_clients, 2);
    let other_summary = all
        .iter()
        .find(|summary| summary.id == other.session.snapshot.summary.id)
        .unwrap();
    assert!(!other_summary.busy);
    assert!(other_summary.running);
    assert_eq!(other_summary.running_jobs, 1);
    assert_eq!(other_summary.attached_clients, 1);
    assert_eq!(all.len(), 2);

    job.finish(JobState::Completed, Arc::new(TestJobDetails("done")))
        .unwrap();
    first.session.handle.cancel().await.unwrap();
    drop(reopened);
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn title_task_success_updates_title_and_manual_race_is_ignored() {
    let directory = tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let repository: Arc<dyn SessionRepository> = Arc::new(opened.store);
    let catalog = ModelCatalogState::Ready(vec![ModelInfoDto {
        id: "gpt-5.6-terra".into(),
        display_name: "GPT-5.6 Terra".into(),
        description: "balanced".into(),
        reasoning_efforts: vec![
            ReasoningLevel::High,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
        ],
        default_reasoning: Some(ReasoningLevel::Medium),
    }]);
    let factory = ControlledEngineFactory::new().with_catalog(catalog);
    let generator = factory.title_generator();
    generator.push(Ok("  **Generated manager title**  ".into()));
    let activity = ActivityTracker::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(Arc::clone(&repository), factory.clone(), activity.clone());
    let mut invalid_model = factory.default_settings();
    invalid_model.model = "missing-model".into();
    let invalid_model_error = match manager
        .materialize_and_submit(
            b"/work/invalid-model".to_vec(),
            "must not persist".into(),
            invalid_model,
            ConnectionId(99),
            AttachmentId(1),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an unknown catalog model must be rejected"),
    };
    assert!(matches!(
        invalid_model_error,
        SessionManagerError::Session(SessionCommandError::ModelNotFound { .. })
    ));
    let mut invalid_reasoning = factory.default_settings();
    invalid_reasoning.reasoning = ReasoningLevel::None;
    let invalid_reasoning_error = match manager
        .materialize_and_submit(
            b"/work/invalid-reasoning".to_vec(),
            "must not persist either".into(),
            invalid_reasoning,
            ConnectionId(100),
            AttachmentId(1),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an unsupported catalog effort must be rejected"),
    };
    assert!(matches!(
        invalid_reasoning_error,
        SessionManagerError::Session(SessionCommandError::UnsupportedReasoning { .. })
    ));
    assert!(
        manager
            .list(SessionListScope::All)
            .await
            .unwrap()
            .is_empty()
    );
    let first_cwd = b"/work/first".to_vec();
    let first = manager
        .materialize_and_submit(
            first_cwd.clone(),
            "Generate a concise title".into(),
            factory.default_settings(),
            ConnectionId(101),
            AttachmentId(1),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let summary = manager
                .list(SessionListScope::Project(first_cwd.clone()))
                .await
                .unwrap()
                .pop()
                .unwrap();
            if summary.title.as_str() == "Generated manager title" {
                assert_eq!(summary.title_revision, 1);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generated title was not routed back through the manager");
    let requests = generator.requests();
    assert_eq!(requests[0].model, "gpt-5.6-terra");
    assert_eq!(requests[0].reasoning, ReasoningLevel::Low);

    let generated_race = generator.block_next();
    let second_cwd = b"/work/second".to_vec();
    let second = manager
        .materialize_and_submit(
            second_cwd.clone(),
            "Manual rename must win".into(),
            factory.default_settings(),
            ConnectionId(102),
            AttachmentId(1),
        )
        .await
        .unwrap();
    assert_eq!(activity.subscribe().borrow().title_tasks, 1);
    let manual = SessionTitle::parse("Manual manager title").unwrap();
    manager
        .rename(second.session.snapshot.summary.id, manual.clone())
        .await
        .unwrap();
    generated_race
        .send(Ok("Generated title that arrived too late".into()))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let summary = manager
                .list(SessionListScope::Project(second_cwd.clone()))
                .await
                .unwrap()
                .pop()
                .unwrap();
            if activity.subscribe().borrow().title_tasks == 0 {
                assert_eq!(summary.title, manual);
                assert_eq!(summary.title_revision, 1);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("blocked title task did not complete");

    first.session.handle.cancel().await.unwrap();
    second.session.handle.cancel().await.unwrap();
    let stored = repository
        .load(second.session.snapshot.summary.id)
        .await
        .unwrap();
    assert_eq!(stored.title_source, TitleSource::Manual);
    let shutdown_title = generator.block_next();
    let shutdown_session = manager
        .materialize_and_submit(
            b"/work/shutdown".to_vec(),
            "wait for this title".into(),
            factory.default_settings(),
            ConnectionId(103),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let shutdown_id = shutdown_session.session.snapshot.summary.id;
    shutdown_session.session.handle.cancel().await.unwrap();
    let shutdown = tokio::spawn({
        let manager = manager.clone();
        async move { manager.shutdown().await }
    });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    shutdown_title
        .send(Ok("Title completed during shutdown".into()))
        .unwrap();
    shutdown.await.unwrap().unwrap();
    lifecycle.join().await.unwrap();
    let stored = repository.load(shutdown_id).await.unwrap();
    assert_eq!(stored.title.as_str(), "Title completed during shutdown");
    assert_eq!(stored.title_source, TitleSource::Generated);
}

#[tokio::test]
async fn delete_removes_cold_session() {
    let fixture = manager_fixture().await;
    let record = fixture
        .repository
        .materialize(MaterializeSession {
            cwd: fixture.cwd.clone(),
            title: fallback_title("cold durable work"),
            settings: fixture.factory.default_settings(),
            prompt: "cold durable work".into(),
            run_id: 0,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let cold_title = SessionTitle::parse("Renamed while cold").unwrap();
    fixture
        .manager
        .rename(record.id, cold_title.clone())
        .await
        .unwrap();
    assert_eq!(
        fixture.repository.load(record.id).await.unwrap().title,
        cold_title
    );

    fixture.manager.delete(record.id).await.unwrap();

    assert!(matches!(
        fixture.repository.load(record.id).await,
        Err(moh::session::SessionStoreError::NotFound { .. })
    ));
    assert!(
        fixture
            .manager
            .list(SessionListScope::Project(fixture.cwd.clone()))
            .await
            .unwrap()
            .is_empty()
    );
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn delete_coordinates_live_actor_and_repository() {
    let fixture = manager_fixture().await;
    let mut materialized = fixture
        .manager
        .materialize_and_submit(
            fixture.cwd.clone(),
            "delete active work".into(),
            fixture.factory.default_settings(),
            ConnectionId(111),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let id = materialized.session.snapshot.summary.id;
    let handle = materialized.session.handle.clone();

    fixture.manager.delete(id).await.unwrap();

    let mut saw_deleted = false;
    while let Some(envelope) = materialized.session.events.recv().await {
        if matches!(envelope.event, SessionEvent::Deleted { session_id } if session_id == id) {
            saw_deleted = true;
        }
    }
    assert!(saw_deleted);
    assert_eq!(
        handle.snapshot().await.unwrap_err(),
        SessionCommandError::Unavailable
    );
    assert!(matches!(
        fixture.repository.load(id).await,
        Err(moh::session::SessionStoreError::NotFound { .. })
    ));
    fixture.manager.shutdown().await.unwrap();
    fixture.lifecycle.join().await.unwrap();
}

#[tokio::test]
async fn delete_failure_drops_quiesced_actor_but_retains_record() {
    let repository = FailingRepository::default();
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let factory = ControlledEngineFactory::new();
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(repository_boundary, factory.clone(), ActivityTracker::new());
    let cwd = b"/work/moh".to_vec();
    let mut materialized = manager
        .materialize_and_submit(
            cwd.clone(),
            "retain after delete failure".into(),
            factory.default_settings(),
            ConnectionId(121),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let id = materialized.session.snapshot.summary.id;
    let first_handle = materialized.session.handle.clone();
    repository.fail_deletes(true);

    assert!(matches!(
        manager.delete(id).await.unwrap_err(),
        SessionManagerError::Store(moh::session::SessionStoreError::Database {
            operation: "delete failpoint",
            ..
        })
    ));
    while let Some(envelope) = materialized.session.events.recv().await {
        assert!(!matches!(envelope.event, SessionEvent::Deleted { .. }));
    }
    assert_eq!(
        first_handle.snapshot().await.unwrap_err(),
        SessionCommandError::Unavailable
    );
    assert!(repository.load(id).await.is_ok());

    repository.fail_deletes(false);
    let reopened = manager
        .open(
            SessionSelector::Id(id),
            cwd.clone(),
            ConnectionId(122),
            AttachmentId(1),
        )
        .await
        .unwrap();
    reopened
        .handle
        .submit("fail delete preparation".into())
        .await
        .unwrap();
    repository.fail_checkpoints(true);
    assert!(matches!(
        manager.delete(id).await.unwrap_err(),
        SessionManagerError::Session(SessionCommandError::Persistence { .. })
    ));
    assert_eq!(
        reopened.handle.snapshot().await.unwrap_err(),
        SessionCommandError::Unavailable
    );
    assert!(repository.load(id).await.is_ok());

    repository.fail_checkpoints(false);
    let final_actor = manager
        .open(
            SessionSelector::Id(id),
            cwd,
            ConnectionId(123),
            AttachmentId(1),
        )
        .await
        .unwrap();
    assert_eq!(factory.controls().len(), 3);
    drop(final_actor);
    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
}
