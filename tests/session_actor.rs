mod support;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use chrono::{TimeZone, Utc};
use moh::{
    backend::ActivityTracker,
    harness::{
        EngineEvent, Message, Role, RunEngine, RunFailure, RunFailureKind, RunRequest, RunStage,
        RunStream,
    },
    runtime::rig::{ActiveModel, ActiveReasoning, ReasoningLevel},
    session::{
        AttachmentId, ConnectionId, DurableTurn, MaterializeSession, ModelCatalogState,
        ModelInfoDto, PlanItem, PlanStatus, SessionActorOutcome, SessionAttachment,
        SessionCommandError, SessionEngineBundle, SessionEvent, SessionEventEnvelope,
        SessionHandle, SessionProjection, SessionRecord, SessionRepository, SessionSettings,
        SessionStore, SessionTitle, TitleGenerationError, TitleSource, TranscriptItem, TurnStatus,
    },
    tools::{
        JobDetails, JobKind, JobRegistry, JobRegistryError, JobState, UpdatePlanArgs,
        plan_update_channel,
    },
};
use tokio::sync::oneshot;

use support::{
    ControlledEngineControl, FailingRepository, RepositoryWriteOperation, controlled_engine,
    engine_bundle,
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(1);

struct ActorFixture {
    handle: SessionHandle,
    control: ControlledEngineControl,
    repository: FailingRepository,
    active_model: ActiveModel,
    active_reasoning: ActiveReasoning,
    jobs: JobRegistry,
    activity: ActivityTracker,
}

#[derive(Clone)]
struct PollCountingEngine {
    polls: Arc<AtomicUsize>,
    sender: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineEvent>>>>,
}

impl PollCountingEngine {
    fn emit(&self, event: EngineEvent) {
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .expect("poll-counting stream must be started")
            .send(event)
            .expect("poll-counting stream must remain active");
    }
}

impl RunEngine for PollCountingEngine {
    fn start(&self, _request: RunRequest) -> RunStream {
        let polls = Arc::clone(&self.polls);
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        *self.sender.lock().unwrap() = Some(sender);
        Box::pin(futures::stream::unfold(
            (receiver, polls, true),
            |(mut receiver, polls, first_poll)| async move {
                if first_poll {
                    polls.fetch_add(1, Ordering::SeqCst);
                }
                receiver
                    .recv()
                    .await
                    .map(|event| (Ok(event), (receiver, polls, false)))
            },
        ))
    }
}

#[derive(Debug)]
struct TestJobDetails(&'static str);

impl JobDetails for TestJobDetails {
    fn render(&self) -> String {
        self.0.into()
    }
}

fn settings() -> SessionSettings {
    SessionSettings {
        model: "gpt-5.6-terra".into(),
        reasoning: ReasoningLevel::Medium,
        context_tokens: 0,
    }
}

fn record() -> SessionRecord {
    SessionRecord {
        id: "session-1".parse().unwrap(),
        title: moh::session::fallback_title(""),
        title_source: moh::session::TitleSource::Fallback,
        title_revision: 0,
        cwd: b"/work/moh".to_vec(),
        settings: settings(),
        transcript: vec![],
        turns: vec![],
        history: vec![],
        plan: Vec::new(),
        created_at: Utc.with_ymd_and_hms(2026, 8, 26, 8, 0, 0).unwrap(),
        last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 8, 0, 0).unwrap(),
    }
}

fn catalog() -> ModelCatalogState {
    ModelCatalogState::Ready(vec![
        ModelInfoDto {
            id: "gpt-5.6-terra".into(),
            display_name: "GPT-5.6 Terra".into(),
            description: "balanced".into(),
            reasoning_efforts: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            default_reasoning: Some(ReasoningLevel::Medium),
        },
        ModelInfoDto {
            id: "gpt-5.6-sol".into(),
            display_name: "GPT-5.6 Sol".into(),
            description: "frontier".into(),
            reasoning_efforts: vec![ReasoningLevel::Medium, ReasoningLevel::Xhigh],
            default_reasoning: Some(ReasoningLevel::Xhigh),
        },
    ])
}

async fn actor_fixture() -> ActorFixture {
    let record = record();
    let repository = FailingRepository::new(record.clone());
    let (engine, control) = controlled_engine();
    let bundle = engine_bundle(engine, &record.settings);
    let active_model = bundle.active_model.clone();
    let active_reasoning = bundle.active_reasoning.clone();
    let jobs = bundle.jobs.clone();
    let projection = SessionProjection::from_record(record.clone(), catalog());
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let activity = ActivityTracker::new();
    let handle = SessionHandle::spawn(
        repository_boundary,
        record,
        projection,
        bundle,
        activity.clone(),
    );
    ActorFixture {
        handle,
        control,
        repository,
        active_model,
        active_reasoning,
        jobs,
        activity,
    }
}

#[tokio::test]
async fn running_actor_authoritatively_replaces_plan_before_later_run_events() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();

    let plan = vec![PlanItem::parse("Verify", PlanStatus::InProgress).unwrap()];
    let update = fixture.control.invoke_plan_on_next_run(UpdatePlanArgs {
        explanation: Some("begin verification".into()),
        plan: plan.clone(),
    });
    fixture.handle.submit("update plan".into()).await.unwrap();
    let _ = next_event(&mut attachment).await;
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::ToolStarted { .. }
    ));
    let outcome = tokio::time::timeout(EVENT_TIMEOUT, update)
        .await
        .expect("plan update timed out")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.plan(), plan);
    assert!(outcome.is_durable());

    let changed = next_event(&mut attachment).await;
    assert!(matches!(changed.event, SessionEvent::PlanChanged(ref received) if received == &plan));
    let reattached = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    assert_eq!(reattached.snapshot.plan, plan);

    let finished = next_event(&mut attachment).await;
    assert!(matches!(finished.event, SessionEvent::ToolFinished { .. }));
    assert!(finished.sequence > changed.sequence);

    fixture
        .control
        .emit(Ok(EngineEvent::Completed("done".into())));
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::Completed { .. }
    ));
    assert_eq!(
        fixture
            .handle
            .attach(ConnectionId(3), AttachmentId(3))
            .await
            .unwrap()
            .snapshot
            .plan,
        plan
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_plan_checkpoint_keeps_live_plan_dirty_until_flush_succeeds() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.repository.fail_checkpoints(true);
    let plan = vec![PlanItem::parse("Verify", PlanStatus::InProgress).unwrap()];

    let outcome = fixture
        .control
        .update_plan(UpdatePlanArgs {
            explanation: None,
            plan: plan.clone(),
        })
        .await
        .unwrap();
    assert!(!outcome.is_durable());
    assert_eq!(outcome.plan(), plan);
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::PlanChanged(_)
    ));
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::PersistenceWarning(Some(_))
    ));
    assert_eq!(
        fixture
            .handle
            .attach(ConnectionId(2), AttachmentId(2))
            .await
            .unwrap()
            .snapshot
            .plan,
        plan
    );
    assert!(
        fixture
            .repository
            .load(record().id)
            .await
            .unwrap()
            .plan
            .is_empty()
    );

    fixture.repository.fail_checkpoints(false);
    fixture.handle.flush().await.unwrap();
    assert_eq!(
        fixture.repository.load(record().id).await.unwrap().plan,
        plan
    );
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn rename_wins_over_pending_generated_title() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let expected_revision = attachment.snapshot.summary.title_revision;
    let manual = SessionTitle::parse("Manual title").unwrap();
    fixture
        .repository
        .materialize(MaterializeSession {
            cwd: record().cwd,
            title: manual.clone(),
            settings: settings(),
            prompt: "another session with the same title".into(),
            run_id: 0,
            created_at: Utc::now(),
        })
        .await
        .unwrap();

    fixture.handle.rename(manual.clone()).await.unwrap();

    let renamed = next_event(&mut attachment).await;
    assert!(matches!(
        renamed.event,
        SessionEvent::TitleChanged {
            ref title,
            title_revision: 1,
        } if title == &manual
    ));
    fixture
        .handle
        .apply_generated_title(expected_revision, Ok("Generated title".into()))
        .await
        .unwrap();

    let snapshot = fixture.handle.snapshot().await.unwrap();
    assert_eq!(snapshot.summary.title, manual);
    assert_eq!(snapshot.summary.title_revision, 1);
    let stored = fixture.repository.load(record().id).await.unwrap();
    assert_eq!(stored.title, snapshot.summary.title);
    assert_eq!(stored.title_source, TitleSource::Manual);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), attachment.events.recv())
            .await
            .is_err(),
        "stale generated output must not emit an event"
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_rename_does_not_change_projection_or_broadcast() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let before = attachment.snapshot.clone();
    fixture.repository.delete(before.summary.id).await.unwrap();

    assert!(matches!(
        fixture
            .handle
            .rename(SessionTitle::parse("Not persisted").unwrap())
            .await,
        Err(SessionCommandError::Persistence { .. })
    ));

    let after = fixture.handle.snapshot().await.unwrap();
    assert_eq!(after.summary.title, before.summary.title);
    assert_eq!(after.summary.title_revision, before.summary.title_revision);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), attachment.events.recv())
            .await
            .is_err(),
        "a failed rename must not emit a title event"
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn generated_title_applies_once_and_invalid_or_failed_output_is_ignored() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();

    fixture
        .handle
        .apply_generated_title(0, Ok(" **Generated title**\nignored".into()))
        .await
        .unwrap();

    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::TitleChanged {
            ref title,
            title_revision: 1,
        } if title.as_str() == "Generated title"
    ));
    fixture
        .handle
        .apply_generated_title(1, Ok("\u{1b}[2J\n".into()))
        .await
        .unwrap();
    fixture
        .handle
        .apply_generated_title(1, Err(TitleGenerationError::Transport))
        .await
        .unwrap();

    let snapshot = fixture.handle.snapshot().await.unwrap();
    assert_eq!(snapshot.summary.title.as_str(), "Generated title");
    assert_eq!(snapshot.summary.title_revision, 1);
    assert_eq!(
        fixture
            .repository
            .load(record().id)
            .await
            .unwrap()
            .title_source,
        TitleSource::Generated
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), attachment.events.recv())
            .await
            .is_err(),
        "invalid and failed generated output must not emit events"
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn delete_quiesces_run_jobs_and_observers() {
    let fixture = actor_fixture().await;
    let mut first = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut second = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    fixture.handle.submit("delete me".into()).await.unwrap();
    assert!(matches!(
        next_event(&mut first).await.event,
        SessionEvent::Started { run_id: 0, .. }
    ));
    assert!(matches!(
        next_event(&mut second).await.event,
        SessionEvent::Started { run_id: 0, .. }
    ));
    let mut lease = fixture
        .jobs
        .start(
            JobKind::Bash,
            "delete-owned job",
            Arc::new(TestJobDetails("running")),
        )
        .unwrap();
    let job_settled = tokio::spawn(async move {
        lease.cancelled().await;
        lease
            .finish(
                JobState::Cancelled,
                Arc::new(TestJobDetails("cancelled for deletion")),
            )
            .unwrap();
    });

    fixture.handle.prepare_delete().await.unwrap();
    job_settled.await.unwrap();

    assert_eq!(
        fixture.handle.submit("too late".into()).await.unwrap_err(),
        SessionCommandError::Deleting
    );
    assert_eq!(
        fixture.handle.cancel().await,
        Err(SessionCommandError::Deleting)
    );
    assert_eq!(
        fixture.handle.select_model("gpt-5.6-sol".into()).await,
        Err(SessionCommandError::Deleting)
    );
    assert_eq!(
        fixture.handle.list_jobs().await,
        Err(SessionCommandError::Deleting)
    );
    assert!(matches!(
        fixture.handle.cancel_job("job-0".into()).await,
        Err(SessionCommandError::Deleting)
    ));
    assert_eq!(
        fixture
            .handle
            .apply_generated_title(0, Ok("too late".into()))
            .await,
        Err(SessionCommandError::Deleting)
    );
    assert!(matches!(
        fixture
            .handle
            .attach(ConnectionId(3), AttachmentId(3))
            .await,
        Err(SessionCommandError::Deleting)
    ));
    let prepared = fixture.handle.snapshot().await.unwrap();
    assert!(!prepared.busy);
    assert!(matches!(
        prepared.transcript.last(),
        Some(TranscriptItem::Cancelled { run_id: 0 })
    ));
    assert_eq!(fixture.jobs.running_count().unwrap(), 0);
    assert!(matches!(
        fixture.jobs.start(
            JobKind::Bash,
            "too late",
            Arc::new(TestJobDetails("not started")),
        ),
        Err(JobRegistryError::ShuttingDown)
    ));

    assert_eq!(
        fixture.handle.finish_delete().await.unwrap(),
        SessionActorOutcome::Deleted
    );

    for attachment in [&mut first, &mut second] {
        let deleted = next_matching(attachment, |event| {
            matches!(event, SessionEvent::Deleted { .. })
        })
        .await;
        assert!(matches!(
            deleted.event,
            SessionEvent::Deleted { session_id } if session_id == record().id
        ));
        assert_eq!(attachment.events.recv().await, None);
    }
    assert_eq!(
        fixture.handle.snapshot().await.unwrap_err(),
        SessionCommandError::Unavailable
    );
    let activity_changes = fixture.activity.subscribe();
    let activity = *activity_changes.borrow();
    assert_eq!(activity.active_runs, 0);
    assert_eq!(activity.running_jobs, 0);
}

#[tokio::test]
async fn full_ordinary_queue_reserves_deleted_before_closure() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();

    for _ in 0..128 {
        fixture
            .handle
            .rename(SessionTitle::parse("Repeated title").unwrap())
            .await
            .unwrap();
    }
    fixture.handle.prepare_delete().await.unwrap();
    assert_eq!(
        fixture.handle.finish_delete().await.unwrap(),
        SessionActorOutcome::Deleted
    );

    let mut ordinary = 0;
    let mut deleted = false;
    while let Some(envelope) = attachment.events.recv().await {
        match envelope.event {
            SessionEvent::TitleChanged { .. } => ordinary += 1,
            SessionEvent::Deleted { session_id } => {
                assert_eq!(session_id, record().id);
                deleted = true;
            }
            other => panic!("unexpected queued event: {other:?}"),
        }
    }
    assert_eq!(ordinary, 128);
    assert!(deleted, "the reserved terminal slot must contain Deleted");
}

#[tokio::test]
async fn failed_delete_preparation_stays_deleting_and_abort_closes_without_deleted_event() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.handle.submit("delete me".into()).await.unwrap();
    let _ = next_event(&mut attachment).await;
    fixture.repository.fail_checkpoints(true);

    assert!(matches!(
        fixture.handle.prepare_delete().await,
        Err(SessionCommandError::Persistence { .. })
    ));

    assert_eq!(
        fixture
            .handle
            .rename(SessionTitle::parse("too late").unwrap())
            .await,
        Err(SessionCommandError::Deleting)
    );
    assert_eq!(
        SessionCommandError::Deleting.to_string(),
        "session is being deleted"
    );
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::Cancelled { run_id: 0 }
    ));
    assert_eq!(
        fixture.handle.abort_delete().await.unwrap(),
        SessionActorOutcome::DeleteAborted
    );
    while let Some(envelope) = attachment.events.recv().await {
        assert!(!matches!(envelope.event, SessionEvent::Deleted { .. }));
    }
    assert_eq!(
        fixture.handle.snapshot().await.unwrap_err(),
        SessionCommandError::Unavailable
    );
}

#[tokio::test]
async fn delete_preparation_checkpoints_terminal_transcript_before_returning() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap()
        .store;
    let persisted = store
        .materialize(MaterializeSession {
            cwd: b"/work/moh".to_vec(),
            title: moh::session::fallback_title("persist cancellation"),
            settings: settings(),
            prompt: "persist cancellation".into(),
            run_id: 0,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let (engine, _control) = controlled_engine();
    let bundle = engine_bundle(engine, &persisted.settings);
    let projection = SessionProjection::from_record(persisted.clone(), catalog());
    let repository: Arc<dyn SessionRepository> = Arc::new(store.clone());
    let handle = SessionHandle::spawn_materialized(
        repository,
        persisted.clone(),
        projection,
        bundle,
        "persist cancellation".into(),
        ActivityTracker::new(),
    )
    .unwrap();

    handle.prepare_delete().await.unwrap();

    let stored = store.load(persisted.id).await.unwrap();
    assert!(matches!(
        stored.transcript.as_slice(),
        [TranscriptItem::User(prompt), TranscriptItem::Cancelled { run_id: 0 }]
            if prompt == "persist cancellation"
    ));
    assert!(matches!(
        stored.turns.as_slice(),
        [DurableTurn {
            run_id: 0,
            status: TurnStatus::Cancelled,
            ..
        }]
    ));
    assert_eq!(
        handle.abort_delete().await.unwrap(),
        SessionActorOutcome::DeleteAborted
    );
}

#[tokio::test]
async fn materialization_checkpoints_before_stream_poll() {
    let repository = FailingRepository::default();
    let materialized = repository
        .materialize(MaterializeSession {
            cwd: b"/work/moh".to_vec(),
            title: moh::session::fallback_title("first"),
            settings: settings(),
            prompt: "first".into(),
            run_id: 0,
            created_at: Utc.with_ymd_and_hms(2026, 8, 26, 8, 0, 0).unwrap(),
        })
        .await
        .unwrap();
    let stream_polls = Arc::new(AtomicUsize::new(0));
    let engine = PollCountingEngine {
        polls: Arc::clone(&stream_polls),
        sender: Arc::new(Mutex::new(None)),
    };
    let (_, plans) = plan_update_channel();
    let bundle = SessionEngineBundle {
        engine: engine.clone(),
        active_model: ActiveModel::new(materialized.settings.model.clone()),
        active_reasoning: ActiveReasoning::new(materialized.settings.reasoning),
        jobs: JobRegistry::new(),
        plans,
    };
    let projection = SessionProjection::from_record(materialized.clone(), catalog());
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());

    let handle = SessionHandle::spawn_materialized(
        repository_boundary,
        materialized.clone(),
        projection,
        bundle,
        "first".into(),
        ActivityTracker::new(),
    )
    .unwrap();

    assert_eq!(stream_polls.load(Ordering::SeqCst), 0);
    assert!(
        materialized
            .transcript
            .contains(&TranscriptItem::User("first".into()))
    );

    let mut attachment = handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    assert!(attachment.snapshot.busy);
    assert_eq!(stream_polls.load(Ordering::SeqCst), 1);
    engine.emit(EngineEvent::AssistantDelta("working".into()));
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::AssistantDelta { run_id: 0, ref text } if text == "working"
    ));
    handle.cancel().await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_reduction_failure_cancels_unpersisted_harness_stream() {
    let mut stale = record();
    stale.transcript = vec![TranscriptItem::User("stale prompt".into())];
    stale.turns = vec![DurableTurn {
        ordinal: 0,
        run_id: 99,
        prompt_position: 0,
        status: TurnStatus::Running,
    }];
    let repository = FailingRepository::new(stale.clone());
    let stream_polls = Arc::new(AtomicUsize::new(0));
    let engine = PollCountingEngine {
        polls: Arc::clone(&stream_polls),
        sender: Arc::new(Mutex::new(None)),
    };
    let (_, plans) = plan_update_channel();
    let bundle = SessionEngineBundle {
        engine,
        active_model: ActiveModel::new(stale.settings.model.clone()),
        active_reasoning: ActiveReasoning::new(stale.settings.reasoning),
        jobs: JobRegistry::new(),
        plans,
    };
    let projection = SessionProjection::from_record(stale.clone(), catalog());
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let handle = SessionHandle::spawn(
        repository_boundary,
        stale,
        projection,
        bundle,
        ActivityTracker::new(),
    );

    assert!(matches!(
        handle.submit("must not execute".into()).await,
        Err(SessionCommandError::Projection { .. })
    ));
    let snapshot = handle.snapshot().await.unwrap();

    assert!(!snapshot.busy);
    assert_eq!(stream_polls.load(Ordering::SeqCst), 0);
    assert!(repository.write_operations().is_empty());

    handle.shutdown().await.unwrap();
}

async fn wait_for_activity(
    changes: &mut tokio::sync::watch::Receiver<moh::backend::ActivitySnapshot>,
    predicate: impl Fn(moh::backend::ActivitySnapshot) -> bool,
) -> moh::backend::ActivitySnapshot {
    loop {
        let snapshot = *changes.borrow_and_update();
        if predicate(snapshot) {
            return snapshot;
        }
        changes.changed().await.unwrap();
    }
}

async fn next_event(attachment: &mut SessionAttachment) -> SessionEventEnvelope {
    tokio::time::timeout(EVENT_TIMEOUT, attachment.events.recv())
        .await
        .expect("actor event timed out")
        .expect("actor observer closed")
}

async fn next_matching(
    attachment: &mut SessionAttachment,
    predicate: impl Fn(&SessionEvent) -> bool,
) -> SessionEventEnvelope {
    loop {
        let event = next_event(attachment).await;
        if predicate(&event.event) {
            return event;
        }
    }
}

#[tokio::test]
async fn detached_actor_keeps_polling_and_reconnects_from_snapshot() {
    let fixture = actor_fixture().await;
    let first = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture
        .handle
        .submit("continue working".into())
        .await
        .unwrap();
    drop(first.events);

    fixture
        .control
        .emit(Ok(EngineEvent::AssistantDelta("half".into())));
    tokio::task::yield_now().await;

    let second = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    assert!(second.snapshot.busy);
    assert_eq!(second.snapshot.active_run.unwrap().assistant_text, "half");
    assert!(fixture.control.requests()[0].history.is_empty());

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_observers_receive_identical_events_in_sequence_order() {
    let fixture = actor_fixture().await;
    let mut first = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut second = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();

    assert_eq!(fixture.handle.submit("question".into()).await.unwrap(), 0);
    let first_started = next_event(&mut first).await;
    let second_started = next_event(&mut second).await;
    assert_eq!(first_started, second_started);
    assert_eq!(first_started.sequence, 1);
    assert!(matches!(
        first_started.event,
        SessionEvent::Started { run_id: 0, ref prompt } if prompt == "question"
    ));

    fixture
        .control
        .emit(Ok(EngineEvent::AssistantDelta("answering".into())));
    let first_delta = next_event(&mut first).await;
    let second_delta = next_event(&mut second).await;
    assert_eq!(first_delta, second_delta);
    assert_eq!(first_delta.sequence, 2);

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn detach_removes_only_the_exact_attachment() {
    let fixture = actor_fixture().await;
    let mut first = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut second = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(2))
        .await
        .unwrap();

    let remaining = fixture
        .handle
        .detach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    assert_eq!(remaining, 1);

    assert_eq!(
        fixture
            .handle
            .snapshot()
            .await
            .unwrap()
            .summary
            .attached_clients,
        1
    );
    fixture
        .handle
        .submit("still attached".into())
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut second).await.event,
        SessionEvent::Started { run_id: 0, .. }
    ));
    assert_eq!(first.events.recv().await, None);

    fixture
        .handle
        .detach_connection(ConnectionId(1))
        .await
        .unwrap();
    assert_eq!(
        fixture
            .handle
            .snapshot()
            .await
            .unwrap()
            .summary
            .attached_clients,
        0
    );

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn detach_connection_removes_all_of_its_observers_without_cancelling() {
    let fixture = actor_fixture().await;
    let mut first = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut duplicate = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(2))
        .await
        .unwrap();
    let mut other = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(3))
        .await
        .unwrap();

    fixture
        .handle
        .detach_connection(ConnectionId(1))
        .await
        .unwrap();
    fixture.handle.submit("still running".into()).await.unwrap();

    assert!(matches!(
        next_event(&mut other).await.event,
        SessionEvent::Started { run_id: 0, .. }
    ));
    assert_eq!(first.events.recv().await, None);
    assert_eq!(duplicate.events.recv().await, None);
    let current = fixture
        .handle
        .attach(ConnectionId(3), AttachmentId(4))
        .await
        .unwrap();
    assert!(current.snapshot.busy);
    assert_eq!(current.snapshot.summary.attached_clients, 2);

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_observer_queue_is_removed_without_stalling_other_observers() {
    let fixture = actor_fixture().await;
    let _slow = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut draining = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    fixture.handle.submit("stream".into()).await.unwrap();
    let _ = next_event(&mut draining).await;

    for _ in 0..128 {
        fixture
            .control
            .emit(Ok(EngineEvent::AssistantDelta("x".into())));
        let event = next_event(&mut draining).await;
        assert!(matches!(event.event, SessionEvent::AssistantDelta { .. }));
    }

    let current = fixture
        .handle
        .attach(ConnectionId(3), AttachmentId(3))
        .await
        .unwrap();
    assert_eq!(
        current.snapshot.active_run.unwrap().assistant_text,
        "x".repeat(128)
    );
    assert_eq!(current.snapshot.summary.attached_clients, 2);

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn running_actor_rejects_a_second_submission() {
    let fixture = actor_fixture().await;

    assert_eq!(fixture.handle.submit("first".into()).await.unwrap(), 0);
    assert_eq!(
        fixture.handle.submit("second".into()).await.unwrap_err(),
        SessionCommandError::Busy
    );
    assert_eq!(fixture.control.requests().len(), 1);

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_cancel_broadcasts_terminal_state_and_allows_another_run() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.handle.submit("stop me".into()).await.unwrap();
    let _ = next_event(&mut attachment).await;

    fixture.handle.cancel().await.unwrap();

    let cancelled = next_event(&mut attachment).await;
    assert_eq!(cancelled.sequence, 2);
    assert!(matches!(
        cancelled.event,
        SessionEvent::Cancelled { run_id: 0 }
    ));
    assert_eq!(fixture.handle.submit("next".into()).await.unwrap(), 1);
    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn global_run_activity_clears_after_cancel_failure_and_completion() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let activity = fixture.activity.subscribe();

    fixture.handle.submit("cancelled".into()).await.unwrap();
    assert_eq!(activity.borrow().active_runs, 1);
    fixture.handle.cancel().await.unwrap();
    assert_eq!(activity.borrow().active_runs, 0);

    fixture.handle.submit("failed".into()).await.unwrap();
    assert_eq!(activity.borrow().active_runs, 1);
    fixture.control.emit(Err(RunFailure::new(
        RunStage::ModelRequest,
        RunFailureKind::Transport,
        true,
        "controlled failure",
    )));
    let _ = next_matching(&mut attachment, |event| {
        matches!(event, SessionEvent::Failed { run_id: 1, .. })
    })
    .await;
    assert_eq!(activity.borrow().active_runs, 0);

    fixture.handle.submit("completed".into()).await.unwrap();
    assert_eq!(activity.borrow().active_runs, 1);
    fixture
        .control
        .emit(Ok(EngineEvent::Completed("done".into())));
    let _ = next_matching(&mut attachment, |event| {
        matches!(event, SessionEvent::Completed { run_id: 2, .. })
    })
    .await;
    assert_eq!(activity.borrow().active_runs, 0);

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn detached_job_changes_update_global_counts_and_actor_projection() {
    let fixture = actor_fixture().await;
    let mut activity = fixture.activity.subscribe();
    let lease = fixture
        .jobs
        .start(
            JobKind::Bash,
            "detached job",
            Arc::new(TestJobDetails("running")),
        )
        .unwrap();

    let running = wait_for_activity(&mut activity, |snapshot| snapshot.running_jobs == 1).await;
    assert_eq!(running.connections, 0);
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    assert_eq!(attachment.snapshot.jobs.len(), 1);
    lease
        .finish(JobState::Completed, Arc::new(TestJobDetails("done")))
        .unwrap();

    wait_for_activity(&mut activity, |snapshot| snapshot.running_jobs == 0).await;
    let changed = next_matching(&mut attachment, |event| {
        matches!(
            event,
            SessionEvent::JobsChanged(jobs)
                if jobs.len() == 1 && jobs[0].state == JobState::Completed
        )
    })
    .await;
    assert!(matches!(
        changed.event,
        SessionEvent::JobsChanged(ref jobs)
            if jobs.len() == 1 && jobs[0].state == JobState::Completed
    ));

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn job_monitor_updates_global_counts_while_actor_is_persisting() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut activity = fixture.activity.subscribe();
    fixture
        .handle
        .submit("slow checkpoint".into())
        .await
        .unwrap();
    let _ = next_matching(&mut attachment, |event| {
        matches!(event, SessionEvent::Started { .. })
    })
    .await;
    let gate = fixture.repository.gate_checkpoints();
    fixture
        .control
        .emit(Ok(EngineEvent::Completed("done".into())));
    gate.wait_until_entered().await;

    let lease = fixture
        .jobs
        .start(
            JobKind::Bash,
            "started during persistence",
            Arc::new(TestJobDetails("running")),
        )
        .unwrap();
    tokio::time::timeout(
        EVENT_TIMEOUT,
        wait_for_activity(&mut activity, |snapshot| snapshot.running_jobs == 1),
    )
    .await
    .expect("job activity monitor was blocked by actor persistence");
    lease
        .finish(JobState::Completed, Arc::new(TestJobDetails("done")))
        .unwrap();
    tokio::time::timeout(
        EVENT_TIMEOUT,
        wait_for_activity(&mut activity, |snapshot| snapshot.running_jobs == 0),
    )
    .await
    .expect("job settlement monitor was blocked by actor persistence");

    gate.release();
    let _ = next_matching(&mut attachment, |event| {
        matches!(event, SessionEvent::Completed { .. })
    })
    .await;
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_turn_is_checkpointed_before_observers_receive_completion() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.handle.submit("persist this".into()).await.unwrap();
    let _ = next_event(&mut attachment).await;

    fixture
        .control
        .emit(Ok(EngineEvent::Completed("stored answer".into())));

    let completion_activity = match next_event(&mut attachment).await.event {
        SessionEvent::Completed {
            run_id: 0,
            response,
            last_activity,
        } => {
            assert_eq!(response, "stored answer");
            last_activity
        }
        other => panic!("expected Completed, got {other:?}"),
    };
    let stored = fixture.repository.load(record().id).await.unwrap();
    assert_eq!(
        stored.history,
        vec![
            Message::new(Role::User, "persist this"),
            Message::new(Role::Assistant, "stored answer"),
        ]
    );
    assert_eq!(stored.last_activity, completion_activity);
    let later = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    assert_eq!(later.snapshot.summary.last_activity, completion_activity);
    assert_eq!(
        fixture.repository.write_operations(),
        vec![
            RepositoryWriteOperation::Checkpoint,
            RepositoryWriteOperation::Checkpoint,
        ]
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_usage_updates_metadata_without_rewriting_history() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.handle.submit("measure".into()).await.unwrap();
    let _ = next_event(&mut attachment).await;

    fixture
        .control
        .emit(Ok(EngineEvent::ContextUsage { input_tokens: 42 }));

    let context_activity = match next_event(&mut attachment).await.event {
        SessionEvent::ContextUsage {
            run_id: 0,
            input_tokens: 42,
            last_activity,
        } => last_activity,
        other => panic!("expected ContextUsage, got {other:?}"),
    };
    let stored = fixture.repository.load(record().id).await.unwrap();
    assert_eq!(stored.settings.context_tokens, 42);
    assert_eq!(stored.last_activity, context_activity);
    assert!(stored.history.is_empty());
    assert_eq!(
        fixture.repository.write_operations(),
        vec![
            RepositoryWriteOperation::Checkpoint,
            RepositoryWriteOperation::UpdateMetadata,
        ]
    );
    let later = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    assert_eq!(later.snapshot.summary.last_activity, context_activity);

    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_checkpoint_keeps_live_completion_dirty_until_flush_succeeds() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.handle.submit("keep this".into()).await.unwrap();
    let _ = next_event(&mut attachment).await;
    fixture.repository.fail_checkpoints(true);

    fixture
        .control
        .emit(Ok(EngineEvent::Completed("done".into())));

    let completed = next_event(&mut attachment).await;
    let warning = next_event(&mut attachment).await;
    assert!(matches!(completed.event, SessionEvent::Completed { .. }));
    assert!(matches!(
        warning.event,
        SessionEvent::PersistenceWarning(Some(_))
    ));
    let later = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    assert!(
        later
            .snapshot
            .transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::Assistant(response) if response == "done"))
    );
    assert!(later.snapshot.persistence_warning.is_some());
    assert!(
        fixture
            .repository
            .load(record().id)
            .await
            .unwrap()
            .history
            .is_empty()
    );

    fixture.repository.fail_checkpoints(false);
    fixture.handle.flush().await.unwrap();
    assert_eq!(
        fixture.repository.load(record().id).await.unwrap().history,
        vec![
            Message::new(Role::User, "keep this"),
            Message::new(Role::Assistant, "done"),
        ]
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_validate_catalog_update_future_runtime_and_persist_metadata() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();

    assert!(matches!(
        fixture.handle.select_model("missing".into()).await,
        Err(SessionCommandError::ModelNotFound { .. })
    ));
    assert!(matches!(
        fixture.handle.select_reasoning(ReasoningLevel::Xhigh).await,
        Err(SessionCommandError::UnsupportedReasoning { .. })
    ));

    fixture
        .handle
        .select_model("gpt-5.6-sol".into())
        .await
        .unwrap();
    fixture
        .handle
        .select_reasoning(ReasoningLevel::Xhigh)
        .await
        .unwrap();

    let model_event = next_event(&mut attachment).await;
    let reasoning_event = next_event(&mut attachment).await;
    assert!(matches!(
        model_event.event,
        SessionEvent::SettingsChanged { ref settings, .. } if settings.model == "gpt-5.6-sol"
            && settings.reasoning == ReasoningLevel::Medium
    ));
    assert!(matches!(
        reasoning_event.event,
        SessionEvent::SettingsChanged { ref settings, .. } if settings.model == "gpt-5.6-sol"
            && settings.reasoning == ReasoningLevel::Xhigh
    ));
    assert_eq!(fixture.active_model.name(), "gpt-5.6-sol");
    assert_eq!(fixture.active_reasoning.level(), ReasoningLevel::Xhigh);
    let stored = fixture.repository.load(record().id).await.unwrap();
    assert_eq!(stored.settings.model, "gpt-5.6-sol");
    assert_eq!(stored.settings.reasoning, ReasoningLevel::Xhigh);

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_activity_is_identical_in_event_store_and_later_snapshot() {
    let fixture = actor_fixture().await;
    let original_activity = record().last_activity;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();

    fixture
        .handle
        .select_model("gpt-5.6-sol".into())
        .await
        .unwrap();

    let changed_at = match next_event(&mut attachment).await.event {
        SessionEvent::SettingsChanged {
            settings,
            last_activity,
        } => {
            assert_eq!(settings.model, "gpt-5.6-sol");
            last_activity
        }
        other => panic!("expected SettingsChanged, got {other:?}"),
    };
    assert!(changed_at > original_activity);
    assert_eq!(
        fixture
            .repository
            .load(record().id)
            .await
            .unwrap()
            .last_activity,
        changed_at
    );
    let later = fixture
        .handle
        .attach(ConnectionId(2), AttachmentId(2))
        .await
        .unwrap();
    assert_eq!(later.snapshot.summary.last_activity, changed_at);

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_metadata_write_is_retried_as_a_full_checkpoint() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    fixture.repository.fail_checkpoints(true);

    fixture
        .handle
        .select_model("gpt-5.6-sol".into())
        .await
        .unwrap();

    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::SettingsChanged { .. }
    ));
    assert!(matches!(
        next_event(&mut attachment).await.event,
        SessionEvent::PersistenceWarning(Some(_))
    ));
    assert_eq!(
        fixture.repository.write_operations(),
        vec![RepositoryWriteOperation::UpdateMetadata]
    );

    fixture.repository.fail_checkpoints(false);
    fixture.handle.flush().await.unwrap();
    assert_eq!(
        fixture.repository.write_operations(),
        vec![
            RepositoryWriteOperation::UpdateMetadata,
            RepositoryWriteOperation::Checkpoint,
        ]
    );
    assert_eq!(
        fixture
            .repository
            .load(record().id)
            .await
            .unwrap()
            .settings
            .model,
        "gpt-5.6-sol"
    );

    fixture.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn job_cancellation_does_not_block_harness_event_reduction() {
    let fixture = actor_fixture().await;
    let mut attachment = fixture
        .handle
        .attach(ConnectionId(1), AttachmentId(1))
        .await
        .unwrap();
    let mut lease = fixture
        .jobs
        .start(
            JobKind::Bash,
            "long job",
            Arc::new(TestJobDetails("running")),
        )
        .unwrap();
    let job_id = lease.id().to_string();
    let listed = fixture.handle.list_jobs().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "job-0");

    fixture.handle.submit("keep polling".into()).await.unwrap();
    let _ = next_matching(&mut attachment, |event| {
        matches!(event, SessionEvent::Started { .. })
    })
    .await;
    let (cancel_seen_tx, cancel_seen_rx) = oneshot::channel();
    let (settle_tx, settle_rx) = oneshot::channel();
    tokio::spawn(async move {
        lease.cancelled().await;
        let _ = cancel_seen_tx.send(());
        let _ = settle_rx.await;
        lease
            .finish(JobState::Cancelled, Arc::new(TestJobDetails("stopped")))
            .unwrap();
    });
    let cancel_handle = fixture.handle.clone();
    let cancel_task = tokio::spawn(async move { cancel_handle.cancel_job(job_id).await });
    tokio::time::timeout(EVENT_TIMEOUT, cancel_seen_rx)
        .await
        .expect("job cancellation was not requested")
        .unwrap();

    fixture
        .control
        .emit(Ok(EngineEvent::AssistantDelta("still live".into())));
    let delta = next_matching(&mut attachment, |event| {
        matches!(event, SessionEvent::AssistantDelta { .. })
    })
    .await;
    assert!(matches!(
        delta.event,
        SessionEvent::AssistantDelta { ref text, .. } if text == "still live"
    ));

    settle_tx.send(()).unwrap();
    let cancelled_job = cancel_task.await.unwrap().unwrap();
    assert_eq!(cancelled_job.state, JobState::Cancelled);
    fixture.handle.cancel().await.unwrap();
    fixture.handle.shutdown().await.unwrap();
}
