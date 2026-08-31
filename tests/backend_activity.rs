mod support;

use std::time::Duration;
use std::{
    future,
    sync::{Arc, Mutex},
};

use moh::{
    backend::{ActivityTracker, ShutdownVeto, flush_for_idle_shutdown, wait_for_idle},
    harness::EngineEvent,
    local::ServerConfig,
    runtime::rig::ReasoningLevel,
    session::{
        AttachmentId, ConnectionId, SessionEngineBundle, SessionEngineFactory, SessionEvent,
        SessionManagerHandle, SessionRepository, SessionSettings, SessionTitleGenerator,
    },
    tools::{JobDetails, JobKind, JobRegistry, JobRegistryError, JobState},
};

use support::{
    ControlledEngine, ControlledEngineControl, FailingRepository, ScriptedTitleGenerator,
    controlled_engine, engine_bundle,
};

fn session_id(value: u64) -> moh::session::SessionId {
    format!("session-{value}").parse().unwrap()
}

#[derive(Debug)]
struct TestJobDetails(&'static str);

impl JobDetails for TestJobDetails {
    fn render(&self) -> String {
        self.0.into()
    }
}

#[derive(Clone)]
struct ActivityEngineFactory {
    controls: Arc<Mutex<Vec<ControlledEngineControl>>>,
    registries: Arc<Mutex<Vec<JobRegistry>>>,
    title_generator: Arc<ScriptedTitleGenerator>,
}

impl ActivityEngineFactory {
    fn new() -> Self {
        Self {
            controls: Arc::new(Mutex::new(Vec::new())),
            registries: Arc::new(Mutex::new(Vec::new())),
            title_generator: Arc::new(ScriptedTitleGenerator::default()),
        }
    }

    fn controls(&self) -> Vec<ControlledEngineControl> {
        self.controls.lock().unwrap().clone()
    }

    fn registries(&self) -> Vec<JobRegistry> {
        self.registries.lock().unwrap().clone()
    }
}

impl SessionEngineFactory for ActivityEngineFactory {
    type Engine = ControlledEngine;

    fn default_settings(&self) -> SessionSettings {
        SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Medium,
            context_tokens: 0,
        }
    }

    fn title_generator(&self) -> Arc<dyn SessionTitleGenerator> {
        self.title_generator.clone()
    }

    fn create(
        &self,
        settings: &SessionSettings,
    ) -> Result<SessionEngineBundle<Self::Engine>, moh::harness::RunFailure> {
        let (engine, control) = controlled_engine();
        let bundle = engine_bundle(engine, settings);
        self.controls.lock().unwrap().push(control);
        self.registries.lock().unwrap().push(bundle.jobs.clone());
        Ok(bundle)
    }
}

#[tokio::test(start_paused = true)]
async fn idle_deadline_uses_the_default_timeout_and_restarts_after_connection_activity() {
    let tracker = ActivityTracker::new();
    let timeout = Duration::from_secs(15 * 60);
    assert_eq!(ServerConfig::default().idle_timeout, timeout);
    let waiter = tokio::spawn(wait_for_idle(tracker.subscribe(), timeout));
    tokio::time::advance(Duration::from_secs(14 * 60)).await;
    assert!(!waiter.is_finished());

    tracker.set_connection(ConnectionId(1), true);
    tracker.set_connection(ConnectionId(1), false);
    tokio::time::advance(Duration::from_secs(15 * 60 - 1)).await;
    assert!(!waiter.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(waiter.await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn active_runs_and_running_jobs_each_block_the_idle_deadline() {
    let tracker = ActivityTracker::new();
    let timeout = Duration::from_secs(60);
    let session = session_id(1);
    tracker.set_run(session, true);
    let waiter = tokio::spawn(wait_for_idle(tracker.subscribe(), timeout));

    tokio::time::advance(timeout).await;
    assert!(!waiter.is_finished());
    tracker.set_run(session, false);
    tracker.set_running_jobs(session, 2);
    tokio::time::advance(timeout).await;
    assert!(!waiter.is_finished());
    tracker.set_running_jobs(session, 0);
    tokio::time::advance(timeout - Duration::from_secs(1)).await;
    assert!(!waiter.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(waiter.await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn title_tasks_veto_idle_shutdown() {
    let tracker = ActivityTracker::new();
    let timeout = Duration::from_secs(60);
    let title_task = tracker.begin_title_task();
    let waiter = tokio::spawn(wait_for_idle(tracker.subscribe(), timeout));
    let active = *tracker.subscribe().borrow();
    assert_eq!(active.title_tasks, 1);
    assert_eq!(active.generation, 1);

    tokio::time::advance(timeout).await;
    assert!(!waiter.is_finished());
    drop(title_task);
    let inactive = *tracker.subscribe().borrow();
    assert_eq!(inactive.title_tasks, 0);
    assert_eq!(inactive.generation, 2);
    tokio::time::advance(timeout - Duration::from_secs(1)).await;
    assert!(!waiter.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(waiter.await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_title_task_releases_its_idle_veto_once() {
    let tracker = ActivityTracker::new();
    let task_tracker = tracker.clone();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _title_task = task_tracker.begin_title_task();
        started.send(()).unwrap();
        future::pending::<()>().await;
    });
    started_rx.await.unwrap();
    let active = *tracker.subscribe().borrow();
    assert_eq!(active.title_tasks, 1);
    assert_eq!(active.generation, 1);

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let inactive = *tracker.subscribe().borrow();
    assert_eq!(inactive.title_tasks, 0);
    assert_eq!(inactive.generation, 2);

    let timeout = Duration::from_secs(60);
    let waiter = tokio::spawn(wait_for_idle(tracker.subscribe(), timeout));
    tokio::time::advance(timeout).await;
    assert!(waiter.await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_connection_racing_deadline_delivery_invalidates_the_expired_timer() {
    let tracker = ActivityTracker::new();
    let timeout = Duration::from_secs(30);
    let waiter = wait_for_idle(tracker.subscribe(), timeout);
    tokio::pin!(waiter);
    assert!(futures::poll!(waiter.as_mut()).is_pending());

    tokio::time::advance(timeout).await;
    tracker.set_connection(ConnectionId(7), true);
    assert!(futures::poll!(waiter.as_mut()).is_pending());

    tracker.set_connection(ConnectionId(7), false);
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    tokio::time::advance(timeout - Duration::from_secs(1)).await;
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    waiter.await;
}

#[tokio::test]
async fn keyed_setters_publish_only_real_state_changes() {
    let tracker = ActivityTracker::new();
    let first_session = session_id(1);
    let second_session = session_id(2);

    tracker.set_connection(ConnectionId(1), true);
    tracker.set_connection(ConnectionId(1), true);
    tracker.set_connection(ConnectionId(2), true);
    tracker.set_connection(ConnectionId(1), false);
    tracker.set_run(first_session, true);
    tracker.set_run(first_session, true);
    tracker.set_run(second_session, true);
    tracker.set_run(second_session, false);
    tracker.set_running_jobs(first_session, 3);
    tracker.set_running_jobs(first_session, 3);
    tracker.set_running_jobs(second_session, 2);
    tracker.set_running_jobs(first_session, 1);
    tracker.set_running_jobs(second_session, 0);
    tracker.set_running_jobs(second_session, 0);

    let snapshot = *tracker.subscribe().borrow();
    assert_eq!(snapshot.connections, 1);
    assert_eq!(snapshot.active_runs, 1);
    assert_eq!(snapshot.running_jobs, 1);
    assert_eq!(snapshot.generation, 10);
}

#[tokio::test(start_paused = true)]
async fn dirty_idle_flush_veto_attempts_every_actor_and_allows_retryable_shutdown() {
    let repository = FailingRepository::default();
    let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
    let factory = ActivityEngineFactory::new();
    let tracker = ActivityTracker::new();
    let timeout = Duration::from_secs(60);
    let waiter = tokio::spawn(wait_for_idle(tracker.subscribe(), timeout));
    let (manager, lifecycle) =
        SessionManagerHandle::spawn(repository_boundary, factory.clone(), tracker.clone());
    let mut first = manager
        .materialize_and_submit(
            b"/work/moh".to_vec(),
            "commit first".into(),
            factory.default_settings(),
            ConnectionId(91),
            AttachmentId(1),
        )
        .await
        .unwrap();
    let mut second = manager
        .materialize_and_submit(
            b"/work/moh".to_vec(),
            "commit second".into(),
            factory.default_settings(),
            ConnectionId(92),
            AttachmentId(2),
        )
        .await
        .unwrap();
    repository.fail_checkpoints(true);

    assert_eq!(tracker.subscribe().borrow().active_runs, 2);
    let controls = factory.controls();
    controls[0].emit(Ok(EngineEvent::Completed("first answer".into())));
    controls[1].emit(Ok(EngineEvent::Completed("second answer".into())));
    for events in [&mut first.session.events, &mut second.session.events] {
        loop {
            if matches!(
                events.recv().await.unwrap().event,
                SessionEvent::Completed { .. }
            ) {
                break;
            }
        }
    }
    assert_eq!(tracker.subscribe().borrow().active_runs, 0);
    tokio::time::advance(timeout).await;
    waiter.await.unwrap();
    let mut completion_attempts = repository.take_checkpoint_attempts();
    completion_attempts.sort_unstable();
    assert_eq!(
        completion_attempts,
        vec![
            first.session.snapshot.summary.id,
            second.session.snapshot.summary.id,
        ]
    );

    assert_eq!(
        flush_for_idle_shutdown(&manager).await,
        Err(ShutdownVeto::DirtySessions)
    );
    let mut flush_attempts = repository.take_checkpoint_attempts();
    flush_attempts.sort_unstable();
    assert_eq!(
        flush_attempts,
        vec![
            first.session.snapshot.summary.id,
            second.session.snapshot.summary.id,
        ]
    );
    let registries = factory.registries();
    for registry in &registries {
        let lease = registry
            .start(
                JobKind::Bash,
                "still accepted after veto",
                Arc::new(TestJobDetails("running")),
            )
            .unwrap();
        lease
            .finish(JobState::Completed, Arc::new(TestJobDetails("done")))
            .unwrap();
    }

    repository.fail_checkpoints(false);
    flush_for_idle_shutdown(&manager).await.unwrap();
    manager.shutdown().await.unwrap();
    lifecycle.join().await.unwrap();
    for registry in &registries {
        assert!(matches!(
            registry.start(
                JobKind::Bash,
                "too late",
                Arc::new(TestJobDetails("stopped")),
            ),
            Err(JobRegistryError::ShuttingDown)
        ));
    }
}
