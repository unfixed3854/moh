#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use futures::{StreamExt, future::BoxFuture, stream};
use moh::{
    harness::{EngineEvent, RunEngine, RunFailure, RunRequest, RunStream},
    runtime::rig::{ActiveModel, ActiveReasoning, ReasoningLevel},
    session::{
        DurableTurn, MaterializeSession, ModelCatalogState, SessionEngineBundle,
        SessionEngineFactory, SessionId, SessionListScope, SessionRecord, SessionRepository,
        SessionSelector, SessionSettings, SessionStoreError, SessionSummary, SessionTitle,
        SessionTitleGenerator, TitleGenerationError, TitleRequest, TitleSource, TranscriptItem,
        TurnStatus,
    },
    tools::{
        JobRegistry, PlanToolError, PlanUpdateClient, PlanUpdateOutcome, PlanUpdateReceiver,
        UpdatePlanArgs, plan_update_channel,
    },
};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

type ControlledStreamSenders =
    Arc<Mutex<Vec<mpsc::UnboundedSender<Result<EngineEvent, RunFailure>>>>>;

struct PendingPlanUpdate {
    args: UpdatePlanArgs,
    response: oneshot::Sender<Result<PlanUpdateOutcome, PlanToolError>>,
}

#[derive(Clone)]
pub struct ControlledEngine {
    requests: Arc<Mutex<Vec<RunRequest>>>,
    request_version: watch::Sender<u64>,
    streams: ControlledStreamSenders,
    consumed: Arc<AtomicU64>,
    consumed_version: watch::Sender<u64>,
    plans: PlanUpdateClient,
    plan_receiver: Arc<Mutex<Option<PlanUpdateReceiver>>>,
    pending_plan_update: Arc<Mutex<Option<PendingPlanUpdate>>>,
    panic_on_poll: bool,
}

#[derive(Clone)]
pub struct ControlledEngineControl {
    requests: Arc<Mutex<Vec<RunRequest>>>,
    request_version: watch::Sender<u64>,
    streams: ControlledStreamSenders,
    consumed: Arc<AtomicU64>,
    consumed_version: watch::Sender<u64>,
    plans: PlanUpdateClient,
    pending_plan_update: Arc<Mutex<Option<PendingPlanUpdate>>>,
}

#[derive(Clone)]
pub struct ControlledEngineFactory {
    controls: Arc<Mutex<Vec<ControlledEngineControl>>>,
    registries: Arc<Mutex<Vec<JobRegistry>>>,
    defaults: SessionSettings,
    catalog: ModelCatalogState,
    panicking_engine: Option<u64>,
    engine_count: Arc<AtomicU64>,
    created_version: watch::Sender<u64>,
    title_generator: Arc<ScriptedTitleGenerator>,
}

pub struct ScriptedTitleGenerator {
    requests: Mutex<Vec<TitleRequest>>,
    responses: Mutex<VecDeque<ScriptedTitleResponse>>,
}

enum ScriptedTitleResponse {
    Ready(Result<String, TitleGenerationError>),
    Blocked(oneshot::Receiver<Result<String, TitleGenerationError>>),
}

impl Default for ScriptedTitleGenerator {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
        }
    }
}

impl ScriptedTitleGenerator {
    pub fn push(&self, response: Result<String, TitleGenerationError>) {
        self.responses
            .lock()
            .unwrap()
            .push_back(ScriptedTitleResponse::Ready(response));
    }

    pub fn block_next(&self) -> oneshot::Sender<Result<String, TitleGenerationError>> {
        let (sender, receiver) = oneshot::channel();
        self.responses
            .lock()
            .unwrap()
            .push_back(ScriptedTitleResponse::Blocked(receiver));
        sender
    }

    pub fn requests(&self) -> Vec<TitleRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl SessionTitleGenerator for ScriptedTitleGenerator {
    fn generate(
        &self,
        request: TitleRequest,
    ) -> BoxFuture<'static, Result<String, TitleGenerationError>> {
        self.requests.lock().unwrap().push(request);
        let response =
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ScriptedTitleResponse::Ready(Err(
                    TitleGenerationError::Completion,
                )));
        Box::pin(async move {
            match response {
                ScriptedTitleResponse::Ready(response) => response,
                ScriptedTitleResponse::Blocked(receiver) => receiver
                    .await
                    .unwrap_or(Err(TitleGenerationError::Completion)),
            }
        })
    }
}

impl ControlledEngineFactory {
    pub fn new() -> Self {
        let (created_version, _) = watch::channel(0);
        Self {
            controls: Arc::new(Mutex::new(Vec::new())),
            registries: Arc::new(Mutex::new(Vec::new())),
            defaults: SessionSettings {
                model: "gpt-5.6-terra".into(),
                reasoning: ReasoningLevel::Medium,
                context_tokens: 0,
            },
            catalog: ModelCatalogState::Loading,
            panicking_engine: None,
            engine_count: Arc::new(AtomicU64::new(0)),
            created_version,
            title_generator: Arc::new(ScriptedTitleGenerator::default()),
        }
    }

    pub fn with_catalog(mut self, catalog: ModelCatalogState) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn with_panicking_engine(mut self, creation_index: u64) -> Self {
        self.panicking_engine = Some(creation_index);
        self
    }

    pub fn controls(&self) -> Vec<ControlledEngineControl> {
        self.controls.lock().unwrap().clone()
    }

    pub fn registries(&self) -> Vec<JobRegistry> {
        self.registries.lock().unwrap().clone()
    }

    pub fn title_generator(&self) -> Arc<ScriptedTitleGenerator> {
        Arc::clone(&self.title_generator)
    }

    pub async fn wait_for_control(&self, index: usize) -> ControlledEngineControl {
        let mut created = self.created_version.subscribe();
        loop {
            if let Some(control) = self.controls.lock().unwrap().get(index).cloned() {
                return control;
            }
            created
                .changed()
                .await
                .expect("controlled engine factory must remain alive while waiting");
        }
    }

    pub async fn wait_for_registry(&self, index: usize) -> JobRegistry {
        let mut created = self.created_version.subscribe();
        loop {
            if let Some(registry) = self.registries.lock().unwrap().get(index).cloned() {
                return registry;
            }
            created
                .changed()
                .await
                .expect("controlled engine factory must remain alive while waiting");
        }
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
    ) -> Result<SessionEngineBundle<Self::Engine>, RunFailure> {
        let (mut engine, control) = controlled_engine();
        let creation_index = self.engine_count.fetch_add(1, Ordering::AcqRel);
        engine.panic_on_poll = self.panicking_engine == Some(creation_index);
        self.controls.lock().unwrap().push(control);
        let bundle = engine_bundle(engine, settings);
        self.registries.lock().unwrap().push(bundle.jobs.clone());
        self.created_version
            .send_replace(creation_index.saturating_add(1));
        Ok(bundle)
    }
}

pub fn controlled_engine() -> (ControlledEngine, ControlledEngineControl) {
    let engine = ControlledEngine::default();
    let control = ControlledEngineControl {
        requests: Arc::clone(&engine.requests),
        request_version: engine.request_version.clone(),
        streams: Arc::clone(&engine.streams),
        consumed: Arc::clone(&engine.consumed),
        consumed_version: engine.consumed_version.clone(),
        plans: engine.plans.clone(),
        pending_plan_update: Arc::clone(&engine.pending_plan_update),
    };
    (engine, control)
}

impl Default for ControlledEngine {
    fn default() -> Self {
        let (request_version, _) = watch::channel(0);
        let (consumed_version, _) = watch::channel(0);
        let (plans, receiver) = plan_update_channel();
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            request_version,
            streams: Arc::new(Mutex::new(Vec::new())),
            consumed: Arc::new(AtomicU64::new(0)),
            consumed_version,
            plans,
            plan_receiver: Arc::new(Mutex::new(Some(receiver))),
            pending_plan_update: Arc::new(Mutex::new(None)),
            panic_on_poll: false,
        }
    }
}

pub fn engine_bundle(
    engine: ControlledEngine,
    settings: &SessionSettings,
) -> SessionEngineBundle<ControlledEngine> {
    let plans = engine
        .plan_receiver
        .lock()
        .unwrap()
        .take()
        .expect("controlled engine bundle must be built once");
    SessionEngineBundle {
        engine,
        active_model: ActiveModel::new(settings.model.clone()),
        active_reasoning: ActiveReasoning::new(settings.reasoning),
        jobs: JobRegistry::new(),
        plans,
    }
}

impl ControlledEngineControl {
    pub fn invoke_plan_on_next_run(
        &self,
        args: UpdatePlanArgs,
    ) -> oneshot::Receiver<Result<PlanUpdateOutcome, PlanToolError>> {
        let (response, receiver) = oneshot::channel();
        let previous = self
            .pending_plan_update
            .lock()
            .unwrap()
            .replace(PendingPlanUpdate { args, response });
        assert!(
            previous.is_none(),
            "only one controlled plan update may be queued"
        );
        receiver
    }

    pub async fn update_plan(
        &self,
        args: UpdatePlanArgs,
    ) -> Result<PlanUpdateOutcome, PlanToolError> {
        self.plans.replace(args).await
    }

    pub fn emit(&self, event: Result<EngineEvent, RunFailure>) {
        self.streams
            .lock()
            .unwrap()
            .last()
            .expect("a controlled stream must be started before emitting")
            .send(event)
            .expect("the controlled stream must still be active");
    }

    pub fn requests(&self) -> Vec<RunRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub async fn wait_for_request_count(&self, count: usize) -> Vec<RunRequest> {
        let mut version = self.request_version.subscribe();
        loop {
            let requests = self.requests();
            if requests.len() >= count {
                return requests;
            }
            version
                .changed()
                .await
                .expect("controlled engine must remain alive while waiting for requests");
        }
    }

    pub async fn wait_for_consumed_count(&self, count: u64) -> u64 {
        let mut version = self.consumed_version.subscribe();
        loop {
            let consumed = self.consumed.load(Ordering::Acquire);
            if consumed >= count {
                return consumed;
            }
            version
                .changed()
                .await
                .expect("controlled engine must remain alive while waiting for event consumption");
        }
    }
}

impl RunEngine for ControlledEngine {
    fn start(&self, request: RunRequest) -> RunStream {
        let request_count = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            u64::try_from(requests.len()).unwrap_or(u64::MAX)
        };
        self.request_version.send_replace(request_count);
        if self.panic_on_poll {
            return Box::pin(stream::poll_fn(|_| {
                panic!("controlled engine stream panic")
            }));
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        self.streams.lock().unwrap().push(sender);
        let consumed = Arc::clone(&self.consumed);
        let consumed_version = self.consumed_version.clone();
        let tail = stream::unfold(
            (receiver, consumed, consumed_version),
            |(mut receiver, consumed, consumed_version)| async move {
                receiver.recv().await.map(|event| {
                    let count = consumed.fetch_add(1, Ordering::AcqRel).saturating_add(1);
                    consumed_version.send_replace(count);
                    (event, (receiver, consumed, consumed_version))
                })
            },
        );
        let pending = self.pending_plan_update.lock().unwrap().take();
        if let Some(pending) = pending {
            let plans = self.plans.clone();
            let started = stream::iter([Ok(EngineEvent::ToolStarted {
                call_id: "controlled-plan".into(),
                name: "update_plan".into(),
                arguments: serde_json::json!({}),
            })]);
            let finished = stream::once(async move {
                let result = plans.replace(pending.args).await;
                let event = match result.as_ref() {
                    Ok(_) => Ok(EngineEvent::ToolFinished {
                        call_id: "controlled-plan".into(),
                        name: "update_plan".into(),
                    }),
                    Err(_) => Err(RunFailure::new(
                        moh::harness::RunStage::ToolExecution,
                        moh::harness::RunFailureKind::ToolInfrastructure,
                        true,
                        "controlled plan update failed",
                    )),
                };
                let _ = pending.response.send(result);
                event
            });
            return Box::pin(started.chain(finished).chain(tail));
        }
        Box::pin(tail)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryWriteOperation {
    Checkpoint,
    UpdateMetadata,
}

#[derive(Clone)]
pub struct FailingRepository {
    records: Arc<Mutex<HashMap<SessionId, SessionRecord>>>,
    next_id: Arc<AtomicU64>,
    fail_checkpoints: Arc<AtomicBool>,
    fail_deletes: Arc<AtomicBool>,
    write_operations: Arc<Mutex<Vec<RepositoryWriteOperation>>>,
    checkpoint_attempts: Arc<Mutex<Vec<SessionId>>>,
    checkpoint_gate: Arc<Mutex<Option<RepositoryCheckpointGate>>>,
    materialize_gate: Arc<Mutex<Option<RepositoryMaterializeGate>>>,
    final_drop: Option<Arc<FinalDropProbe>>,
}

struct FinalDropProbe {
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Drop for FinalDropProbe {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.lock().unwrap().take() {
            callback();
        }
    }
}

#[derive(Clone)]
pub struct RepositoryCheckpointGate {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct RepositoryMaterializeGate {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl RepositoryCheckpointGate {
    pub async fn wait_until_entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }
}

impl RepositoryMaterializeGate {
    pub async fn wait_until_entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }
}

impl Default for FailingRepository {
    fn default() -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            fail_checkpoints: Arc::new(AtomicBool::new(false)),
            fail_deletes: Arc::new(AtomicBool::new(false)),
            write_operations: Arc::new(Mutex::new(Vec::new())),
            checkpoint_attempts: Arc::new(Mutex::new(Vec::new())),
            checkpoint_gate: Arc::new(Mutex::new(None)),
            materialize_gate: Arc::new(Mutex::new(None)),
            final_drop: None,
        }
    }
}

impl FailingRepository {
    pub fn on_final_drop(mut self, callback: impl FnOnce() + Send + 'static) -> Self {
        self.final_drop = Some(Arc::new(FinalDropProbe {
            callback: Mutex::new(Some(Box::new(callback))),
        }));
        self
    }

    pub fn new(record: SessionRecord) -> Self {
        let next_id = record.id.get().checked_add(1).unwrap();
        Self {
            records: Arc::new(Mutex::new(HashMap::from([(record.id, record)]))),
            next_id: Arc::new(AtomicU64::new(next_id)),
            fail_checkpoints: Arc::new(AtomicBool::new(false)),
            fail_deletes: Arc::new(AtomicBool::new(false)),
            write_operations: Arc::new(Mutex::new(Vec::new())),
            checkpoint_attempts: Arc::new(Mutex::new(Vec::new())),
            checkpoint_gate: Arc::new(Mutex::new(None)),
            materialize_gate: Arc::new(Mutex::new(None)),
            final_drop: None,
        }
    }

    pub fn fail_checkpoints(&self, fail: bool) {
        self.fail_checkpoints.store(fail, Ordering::Release);
    }

    pub fn fail_deletes(&self, fail: bool) {
        self.fail_deletes.store(fail, Ordering::Release);
    }

    pub fn write_operations(&self) -> Vec<RepositoryWriteOperation> {
        self.write_operations.lock().unwrap().clone()
    }

    pub fn take_checkpoint_attempts(&self) -> Vec<SessionId> {
        std::mem::take(&mut *self.checkpoint_attempts.lock().unwrap())
    }

    pub fn gate_checkpoints(&self) -> RepositoryCheckpointGate {
        let gate = RepositoryCheckpointGate {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        *self.checkpoint_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    pub fn gate_materializations(&self) -> RepositoryMaterializeGate {
        let gate = RepositoryMaterializeGate {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        *self.materialize_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    fn allocate_id(&self) -> Result<SessionId, SessionStoreError> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        format!("session-{id}")
            .parse()
            .map_err(|_| SessionStoreError::InvalidStoredData {
                field: "session id",
                reason: "fake repository exhausted session identifiers".into(),
            })
    }

    fn failed_write(operation: &'static str) -> SessionStoreError {
        SessionStoreError::Database {
            operation,
            source: rusqlite::Error::InvalidQuery,
        }
    }
}

impl SessionRepository for FailingRepository {
    fn resolve(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            let records = repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
            match &selector {
                SessionSelector::Id(id) => {
                    records
                        .get(id)
                        .cloned()
                        .ok_or_else(|| SessionStoreError::NotFound {
                            selector: selector.to_string(),
                        })
                }
                SessionSelector::Title(title) => {
                    let mut matches = records
                        .values()
                        .filter(|record| record.cwd == cwd_for_title && &record.title == title)
                        .cloned()
                        .collect::<Vec<_>>();
                    matches.sort_by_key(|record| record.id);
                    match matches.as_slice() {
                        [] => Err(SessionStoreError::NotFound {
                            selector: selector.to_string(),
                        }),
                        [record] => Ok(record.clone()),
                        _ => Err(SessionStoreError::AmbiguousTitle {
                            title: title.clone(),
                            ids: matches.iter().map(|record| record.id).collect(),
                        }),
                    }
                }
            }
        })
    }

    fn load(&self, id: SessionId) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?
                .get(&id)
                .cloned()
                .ok_or_else(|| SessionStoreError::NotFound {
                    selector: id.to_string(),
                })
        })
    }

    fn materialize(
        &self,
        request: MaterializeSession,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            let gate = repository.materialize_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.add_permits(1);
                gate.release.acquire().await.unwrap().forget();
            }
            let id = repository.allocate_id()?;
            let record = SessionRecord {
                id,
                title: request.title,
                title_source: TitleSource::Fallback,
                title_revision: 0,
                cwd: request.cwd,
                settings: request.settings,
                transcript: vec![TranscriptItem::User(request.prompt)],
                turns: vec![DurableTurn {
                    ordinal: 0,
                    run_id: request.run_id,
                    prompt_position: 0,
                    status: TurnStatus::Running,
                }],
                history: Vec::new(),
                plan: Vec::new(),
                created_at: request.created_at,
                last_activity: request.created_at,
            };
            repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?
                .insert(id, record.clone());
            Ok(record)
        })
    }

    fn list(
        &self,
        scope: SessionListScope,
    ) -> BoxFuture<'static, Result<Vec<SessionSummary>, SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            let records = repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
            let mut summaries = records
                .values()
                .filter(|record| match &scope {
                    SessionListScope::Project(cwd) => &record.cwd == cwd,
                    SessionListScope::All => true,
                })
                .map(|record| SessionSummary {
                    id: record.id,
                    title: record.title.clone(),
                    title_revision: record.title_revision,
                    cwd: record.cwd.clone(),
                    cwd_display: String::from_utf8_lossy(&record.cwd).into_owned(),
                    running_jobs: 0,
                    running: false,
                    busy: false,
                    attached_clients: 0,
                    last_activity: record.last_activity,
                })
                .collect::<Vec<_>>();
            summaries.sort_by_key(|summary| (summary.last_activity, summary.id));
            summaries.reverse();
            Ok(summaries)
        })
    }

    fn rename(
        &self,
        id: SessionId,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            let mut records = repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
            let record = records
                .get_mut(&id)
                .ok_or_else(|| SessionStoreError::NotFound {
                    selector: id.to_string(),
                })?;
            record.title_revision =
                record
                    .title_revision
                    .checked_add(1)
                    .ok_or(SessionStoreError::ValueOutOfRange {
                        field: "title revision",
                    })?;
            record.title = title;
            record.title_source = TitleSource::Manual;
            Ok(record.clone())
        })
    }

    fn compare_and_set_generated_title(
        &self,
        id: SessionId,
        expected_revision: u64,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<Option<SessionRecord>, SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            let mut records = repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
            let record = records
                .get_mut(&id)
                .ok_or_else(|| SessionStoreError::NotFound {
                    selector: id.to_string(),
                })?;
            if record.title_source == TitleSource::Manual
                || record.title_revision != expected_revision
            {
                return Ok(None);
            }
            record.title_revision =
                record
                    .title_revision
                    .checked_add(1)
                    .ok_or(SessionStoreError::ValueOutOfRange {
                        field: "title revision",
                    })?;
            record.title = title;
            record.title_source = TitleSource::Generated;
            Ok(Some(record.clone()))
        })
    }

    fn delete(&self, id: SessionId) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            if repository.fail_deletes.load(Ordering::Acquire) {
                return Err(Self::failed_write("delete failpoint"));
            }
            repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?
                .remove(&id)
                .map(|_| ())
                .ok_or_else(|| SessionStoreError::NotFound {
                    selector: id.to_string(),
                })
        })
    }

    fn checkpoint(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            repository
                .checkpoint_attempts
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?
                .push(record.id);
            repository
                .write_operations
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?
                .push(RepositoryWriteOperation::Checkpoint);
            let gate = repository.checkpoint_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.entered.add_permits(1);
                gate.release.acquire().await.unwrap().forget();
            }
            if repository.fail_checkpoints.load(Ordering::Acquire) {
                return Err(Self::failed_write("checkpoint failpoint"));
            }
            let mut records = repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
            let stored =
                records
                    .get_mut(&record.id)
                    .ok_or_else(|| SessionStoreError::NotFound {
                        selector: record.id.to_string(),
                    })?;
            *stored = record;
            Ok(())
        })
    }

    fn update_metadata(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        let repository = self.clone();
        Box::pin(async move {
            repository
                .write_operations
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?
                .push(RepositoryWriteOperation::UpdateMetadata);
            if repository.fail_checkpoints.load(Ordering::Acquire) {
                return Err(Self::failed_write("metadata checkpoint failpoint"));
            }
            let mut records = repository
                .records
                .lock()
                .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
            let stored =
                records
                    .get_mut(&record.id)
                    .ok_or_else(|| SessionStoreError::NotFound {
                        selector: record.id.to_string(),
                    })?;
            stored.settings = record.settings;
            stored.last_activity = record.last_activity;
            Ok(())
        })
    }
}
