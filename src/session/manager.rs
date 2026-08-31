//! Lazy process-wide registry of independently serialized session actors.

use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::backend::ActivityTracker;
use crate::harness::{RunFailure, RunFailureKind, RunStage};
use crate::runtime::rig::ReasoningLevel;
use crate::tools::JobState;

use super::{
    AttachmentId, ConnectionId, DraftDefaults, DurableTurn, MaterializeSession, RunFailureSnapshot,
    SessionActorOutcome, SessionAttachment, SessionCommandError, SessionEngineFactory,
    SessionHandle, SessionId, SessionListScope, SessionProjection, SessionRecord,
    SessionRepository, SessionSelector, SessionSnapshot, SessionStoreError, SessionSummary,
    TitleGenerationError, TitleRequest, TranscriptItem, TurnStatus, fallback_title,
};

const COMMAND_CAPACITY: usize = 128;

/// A live actor command handle paired with its atomic attachment state.
pub struct ManagedSession {
    /// Commands targeting the one authoritative session actor.
    pub handle: SessionHandle,
    /// Complete state at the attachment sequence.
    pub snapshot: SessionSnapshot,
    /// Bounded events sequenced after `snapshot`.
    pub events: mpsc::Receiver<super::SessionEventEnvelope>,
}

/// Result of atomically selecting current project work or preparing a new draft.
pub enum StartupResult {
    /// No project session is running, so no durable row or actor was created.
    Draft(DraftDefaults),
    /// The newest running project session was selected and attached.
    Attached(Box<ManagedSession>),
}

/// A newly durable session, its first active run, and its atomic requester attachment.
pub struct MaterializedSession {
    /// The authoritative actor attachment created for the requester.
    pub session: ManagedSession,
    /// Harness run identifier assigned to the first submitted prompt.
    pub run_id: u64,
}

impl ManagedSession {
    fn new(handle: SessionHandle, attachment: SessionAttachment) -> Self {
        Self {
            handle,
            snapshot: attachment.snapshot,
            events: attachment.events,
        }
    }
}

/// Typed failures returned by manager commands.
#[derive(Debug, Error)]
pub enum SessionManagerError {
    /// Durable identity resolution or persistence failed.
    #[error(transparent)]
    Store(#[from] SessionStoreError),
    /// An isolated runtime bundle could not be constructed.
    #[error(transparent)]
    Runtime(#[from] RunFailure),
    /// A live actor rejected or could not complete the command.
    #[error(transparent)]
    Session(#[from] SessionCommandError),
    /// The manager task is shutting down or no longer available.
    #[error("session manager is unavailable")]
    Unavailable,
}

/// Failure observed while joining the task that owns the live session registry.
#[derive(Debug, Error)]
pub enum SessionManagerLifecycleError {
    /// The manager completed with an actor or persistence failure.
    #[error(transparent)]
    Manager(#[from] SessionManagerError),
    /// Tokio could not join the manager task.
    #[error("session manager task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Owned completion boundary for the task that retains all live session actors.
pub struct SessionManagerLifecycle {
    task: JoinHandle<Result<(), SessionManagerError>>,
}

impl SessionManagerLifecycle {
    /// Waits for manager completion and surfaces channel-closure shutdown failures.
    pub async fn join(self) -> Result<(), SessionManagerLifecycleError> {
        self.task.await??;
        Ok(())
    }
}

/// Cloneable command boundary for the process-wide lazy actor registry.
#[derive(Clone)]
pub struct SessionManagerHandle {
    commands: mpsc::Sender<ManagerCommand>,
}

impl SessionManagerHandle {
    /// Starts a serialized manager using durable storage and an isolated engine factory.
    pub fn spawn<F>(
        repository: Arc<dyn SessionRepository>,
        factory: F,
        activity: ActivityTracker,
    ) -> (Self, SessionManagerLifecycle)
    where
        F: SessionEngineFactory,
    {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let task = tokio::spawn(SessionManager::new(repository, factory, activity).run(command_rx));
        (Self { commands }, SessionManagerLifecycle { task })
    }

    /// Atomically attaches to the newest running project session or returns draft defaults.
    pub async fn startup(
        &self,
        cwd: Vec<u8>,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<StartupResult, SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::Startup {
            cwd,
            connection_id,
            attachment_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Returns fresh draft defaults without selecting or attaching a durable session.
    pub async fn draft_defaults(&self, cwd: Vec<u8>) -> Result<DraftDefaults, SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::DraftDefaults { cwd, response })
            .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Persists a first prompt, starts run zero, and atomically attaches its requester.
    pub async fn materialize_and_submit(
        &self,
        cwd: Vec<u8>,
        prompt: String,
        settings: super::SessionSettings,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<MaterializedSession, SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::MaterializeAndSubmit {
            cwd,
            prompt,
            settings,
            connection_id,
            attachment_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Resolves and lazily opens a global ID or CWD-scoped name.
    pub async fn open(
        &self,
        selector: SessionSelector,
        cwd_for_name: Vec<u8>,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<ManagedSession, SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::Open {
            selector,
            cwd_for_name,
            connection_id,
            attachment_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Lists durable sessions in one project or globally with live actor state overlaid.
    pub async fn list(
        &self,
        scope: SessionListScope,
    ) -> Result<Vec<SessionSummary>, SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::List { scope, response }).await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Applies a manual title through a live actor or directly to cold storage.
    pub async fn rename(
        &self,
        session_id: SessionId,
        title: super::SessionTitle,
    ) -> Result<(), SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::Rename {
            session_id,
            title,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Deletes one cold record or coordinates terminal deletion with its live actor.
    pub async fn delete(&self, session_id: SessionId) -> Result<(), SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::Delete {
            session_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Detaches one exact observer from one live session actor.
    pub async fn detach(
        &self,
        session_id: SessionId,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<u32, SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::Detach {
            session_id,
            connection_id,
            attachment_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Detaches every observer owned by one connection from every live actor.
    pub async fn detach_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<(), SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::DetachConnection {
            connection_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Retries every live actor's outstanding durable checkpoint.
    pub async fn flush_all(&self) -> Result<(), SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::FlushAll { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    /// Flushes and shuts down every live actor and then stops the manager.
    pub async fn shutdown(&self) -> Result<(), SessionManagerError> {
        let (response, response_rx) = oneshot::channel();
        self.send(ManagerCommand::Shutdown { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionManagerError::Unavailable)?
    }

    async fn send(&self, command: ManagerCommand) -> Result<(), SessionManagerError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| SessionManagerError::Unavailable)
    }
}

enum ManagerCommand {
    Startup {
        cwd: Vec<u8>,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
        response: oneshot::Sender<Result<StartupResult, SessionManagerError>>,
    },
    DraftDefaults {
        cwd: Vec<u8>,
        response: oneshot::Sender<Result<DraftDefaults, SessionManagerError>>,
    },
    MaterializeAndSubmit {
        cwd: Vec<u8>,
        prompt: String,
        settings: super::SessionSettings,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
        response: oneshot::Sender<Result<MaterializedSession, SessionManagerError>>,
    },
    Open {
        selector: SessionSelector,
        cwd_for_name: Vec<u8>,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
        response: oneshot::Sender<Result<ManagedSession, SessionManagerError>>,
    },
    List {
        scope: SessionListScope,
        response: oneshot::Sender<Result<Vec<SessionSummary>, SessionManagerError>>,
    },
    Rename {
        session_id: SessionId,
        title: super::SessionTitle,
        response: oneshot::Sender<Result<(), SessionManagerError>>,
    },
    Delete {
        session_id: SessionId,
        response: oneshot::Sender<Result<(), SessionManagerError>>,
    },
    Detach {
        session_id: SessionId,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
        response: oneshot::Sender<Result<u32, SessionManagerError>>,
    },
    DetachConnection {
        connection_id: ConnectionId,
        response: oneshot::Sender<Result<(), SessionManagerError>>,
    },
    FlushAll {
        response: oneshot::Sender<Result<(), SessionManagerError>>,
    },
    Shutdown {
        response: oneshot::Sender<Result<(), SessionManagerError>>,
    },
}

struct SessionManager<F: SessionEngineFactory> {
    repository: Arc<dyn SessionRepository>,
    factory: F,
    actors: HashMap<SessionId, SessionHandle>,
    activity: ActivityTracker,
    title_tasks: JoinSet<TitleCompletion>,
}

struct TitleCompletion {
    session_id: SessionId,
    expected_revision: u64,
    generated: Result<String, TitleGenerationError>,
}

impl<F: SessionEngineFactory> SessionManager<F> {
    fn new(repository: Arc<dyn SessionRepository>, factory: F, activity: ActivityTracker) -> Self {
        Self {
            repository,
            factory,
            actors: HashMap::new(),
            activity,
            title_tasks: JoinSet::new(),
        }
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<ManagerCommand>,
    ) -> Result<(), SessionManagerError> {
        loop {
            tokio::select! {
                completion = self.title_tasks.join_next(), if !self.title_tasks.is_empty() => {
                    if let Some(Ok(completion)) = completion {
                        self.route_title_completion(completion).await;
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        self.drain_title_tasks().await;
                        return self.shutdown_actors().await;
                    };
                    if self.handle_command(command).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn handle_command(&mut self, command: ManagerCommand) -> bool {
        match command {
            ManagerCommand::Startup {
                cwd,
                connection_id,
                attachment_id,
                response,
            } => {
                let _ = response.send(self.startup(cwd, connection_id, attachment_id).await);
            }
            ManagerCommand::DraftDefaults { cwd, response } => {
                let _ = response.send(Ok(self.draft_defaults(cwd)));
            }
            ManagerCommand::MaterializeAndSubmit {
                cwd,
                prompt,
                settings,
                connection_id,
                attachment_id,
                response,
            } => {
                let result = self
                    .materialize_and_submit(cwd, prompt, settings, connection_id, attachment_id)
                    .await;
                let _ = response.send(result);
            }
            ManagerCommand::Open {
                selector,
                cwd_for_name,
                connection_id,
                attachment_id,
                response,
            } => {
                let _ = response.send(
                    self.open(selector, cwd_for_name, connection_id, attachment_id)
                        .await,
                );
            }
            ManagerCommand::List { scope, response } => {
                let _ = response.send(self.list(scope).await);
            }
            ManagerCommand::Rename {
                session_id,
                title,
                response,
            } => {
                let _ = response.send(self.rename(session_id, title).await);
            }
            ManagerCommand::Delete {
                session_id,
                response,
            } => {
                let _ = response.send(self.delete(session_id).await);
            }
            ManagerCommand::Detach {
                session_id,
                connection_id,
                attachment_id,
                response,
            } => {
                let _ = response.send(self.detach(session_id, connection_id, attachment_id).await);
            }
            ManagerCommand::DetachConnection {
                connection_id,
                response,
            } => {
                let _ = response.send(self.detach_connection(connection_id).await);
            }
            ManagerCommand::FlushAll { response } => {
                let _ = response.send(self.flush_actors().await);
            }
            ManagerCommand::Shutdown { response } => {
                self.drain_title_tasks().await;
                match self.shutdown_actors().await {
                    Ok(()) => {
                        let _ = response.send(Ok(()));
                        return true;
                    }
                    Err(error) => {
                        let _ = response.send(Err(error));
                    }
                }
            }
        }
        false
    }

    async fn open(
        &mut self,
        selector: SessionSelector,
        cwd_for_name: Vec<u8>,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<ManagedSession, SessionManagerError> {
        let id = self.resolve_id(selector, cwd_for_name).await?;
        let handle = if let Some(handle) = self.actors.get(&id) {
            handle.clone()
        } else {
            let record = self.repository.load(id).await?;
            let bundle = self.factory.create(&record.settings)?;
            let projection = SessionProjection::from_record(record.clone(), self.factory.catalog());
            let handle = SessionHandle::spawn(
                Arc::clone(&self.repository),
                record,
                projection,
                bundle,
                self.activity.clone(),
            );
            self.actors.insert(id, handle.clone());
            handle
        };
        let attachment = handle.attach(connection_id, attachment_id).await?;
        Ok(ManagedSession::new(handle, attachment))
    }

    async fn resolve_id(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> Result<SessionId, SessionStoreError> {
        let title = match selector {
            SessionSelector::Id(id) => return Ok(id),
            SessionSelector::Title(title) => title,
        };
        let mut ids = self
            .repository
            .list(SessionListScope::Project(cwd_for_title))
            .await?
            .into_iter()
            .filter(|summary| summary.title == title)
            .map(|summary| summary.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        match ids.as_slice() {
            [] => Err(SessionStoreError::NotFound {
                selector: title.to_string(),
            }),
            [id] => Ok(*id),
            _ => Err(SessionStoreError::AmbiguousTitle { title, ids }),
        }
    }

    async fn startup(
        &self,
        cwd: Vec<u8>,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<StartupResult, SessionManagerError> {
        let summaries = self.list(SessionListScope::Project(cwd.clone())).await?;
        for summary in summaries.into_iter().filter(|summary| summary.running) {
            let Some(handle) = self.actors.get(&summary.id).cloned() else {
                continue;
            };
            let attachment = handle.attach(connection_id, attachment_id).await?;
            return Ok(StartupResult::Attached(Box::new(ManagedSession::new(
                handle, attachment,
            ))));
        }
        Ok(StartupResult::Draft(self.draft_defaults(cwd)))
    }

    fn draft_defaults(&self, cwd: Vec<u8>) -> DraftDefaults {
        DraftDefaults {
            cwd,
            settings: self.factory.default_settings(),
            catalog: self.factory.catalog(),
        }
    }

    async fn materialize_and_submit(
        &mut self,
        cwd: Vec<u8>,
        prompt: String,
        settings: super::SessionSettings,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<MaterializedSession, SessionManagerError> {
        if prompt.trim().is_empty() {
            return Err(SessionCommandError::InvalidPrompt.into());
        }
        let catalog = self.factory.catalog();
        validate_materialization_settings(&settings, &catalog)?;
        let title_reasoning = title_reasoning(&settings, &catalog);
        let run_id = 0;
        let record = self
            .repository
            .materialize(MaterializeSession {
                cwd,
                title: fallback_title(&prompt),
                settings: settings.clone(),
                prompt: prompt.clone(),
                run_id,
                created_at: Utc::now(),
            })
            .await?;
        let bundle = match self.factory.create(&settings) {
            Ok(bundle) => bundle,
            Err(error) => {
                let failure = startup_failure(error.message(), error.retryable());
                self.persist_failed_materialization(record, prompt, run_id, failure)
                    .await?;
                return Err(error.into());
            }
        };
        let projection = SessionProjection::from_record(record.clone(), catalog);
        let handle = match SessionHandle::spawn_materialized(
            Arc::clone(&self.repository),
            record.clone(),
            projection,
            bundle,
            prompt.clone(),
            self.activity.clone(),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                let failure = startup_failure(&error.to_string(), false);
                self.persist_failed_materialization(record, prompt, run_id, failure)
                    .await?;
                return Err(error.into());
            }
        };
        self.actors.insert(record.id, handle.clone());
        let attachment = handle.attach(connection_id, attachment_id).await?;
        self.dispatch_title_task(TitleRequest {
            session_id: record.id,
            model: settings.model,
            reasoning: title_reasoning,
            first_message: prompt,
            expected_revision: record.title_revision,
        });
        Ok(MaterializedSession {
            session: ManagedSession::new(handle, attachment),
            run_id,
        })
    }

    async fn persist_failed_materialization(
        &self,
        mut record: SessionRecord,
        prompt: String,
        run_id: u64,
        failure: RunFailureSnapshot,
    ) -> Result<(), SessionStoreError> {
        let first_prompt = TranscriptItem::User(prompt);
        record.transcript.truncate(1);
        if record.transcript.first() != Some(&first_prompt) {
            record.transcript.clear();
            record.transcript.push(first_prompt);
        }
        record
            .transcript
            .push(TranscriptItem::Failed { run_id, failure });
        record.turns.truncate(1);
        match record.turns.first_mut() {
            Some(turn) => {
                turn.ordinal = 0;
                turn.run_id = run_id;
                turn.prompt_position = 0;
                turn.status = TurnStatus::Failed;
            }
            None => record.turns.push(DurableTurn {
                ordinal: 0,
                run_id,
                prompt_position: 0,
                status: TurnStatus::Failed,
            }),
        }
        record.history.clear();
        let session_id = record.id;
        if let Err(error) = self.repository.checkpoint(record).await {
            let _ = self.repository.delete(session_id).await;
            return Err(error);
        }
        Ok(())
    }

    fn dispatch_title_task(&mut self, request: TitleRequest) {
        let generator = self.factory.title_generator();
        let title_guard = self.activity.begin_title_task();
        let generated = generator.generate(request.clone());
        self.title_tasks.spawn(async move {
            let _title_guard = title_guard;
            TitleCompletion {
                session_id: request.session_id,
                expected_revision: request.expected_revision,
                generated: generated.await,
            }
        });
    }

    async fn route_title_completion(&self, completion: TitleCompletion) {
        if let Some(handle) = self.actors.get(&completion.session_id) {
            let _ = handle
                .apply_generated_title(completion.expected_revision, completion.generated)
                .await;
        }
    }

    async fn drain_title_tasks(&mut self) {
        while let Some(completion) = self.title_tasks.join_next().await {
            if let Ok(completion) = completion {
                self.route_title_completion(completion).await;
            }
        }
    }

    async fn list(
        &self,
        scope: SessionListScope,
    ) -> Result<Vec<SessionSummary>, SessionManagerError> {
        let mut summaries = self.repository.list(scope).await?;
        for summary in &mut summaries {
            if let Some(handle) = self.actors.get(&summary.id) {
                let snapshot = handle.snapshot().await?;
                summary.clone_from(&snapshot.summary);
                summary.running_jobs = u32::try_from(
                    snapshot
                        .jobs
                        .iter()
                        .filter(|job| job.state == JobState::Running)
                        .count(),
                )
                .unwrap_or(u32::MAX);
                summary.running = snapshot.busy || summary.running_jobs > 0;
            }
        }
        summaries.sort_by_key(|summary| (summary.last_activity, summary.id));
        summaries.reverse();
        Ok(summaries)
    }

    async fn rename(
        &self,
        session_id: SessionId,
        title: super::SessionTitle,
    ) -> Result<(), SessionManagerError> {
        if let Some(handle) = self.actors.get(&session_id) {
            handle.rename(title).await?;
        } else {
            self.repository.rename(session_id, title).await?;
        }
        Ok(())
    }

    async fn delete(&mut self, session_id: SessionId) -> Result<(), SessionManagerError> {
        let Some(handle) = self.actors.get(&session_id).cloned() else {
            self.repository.delete(session_id).await?;
            return Ok(());
        };

        if let Err(error) = handle.prepare_delete().await {
            self.abort_delete_actor(session_id, &handle).await;
            return Err(error.into());
        }
        if let Err(error) = self.repository.delete(session_id).await {
            self.abort_delete_actor(session_id, &handle).await;
            return Err(error.into());
        }
        let outcome = handle.finish_delete().await;
        self.actors.remove(&session_id);
        match outcome? {
            SessionActorOutcome::Deleted => Ok(()),
            SessionActorOutcome::DeleteAborted => Err(SessionCommandError::Projection {
                message: "delete finish returned an aborted outcome".into(),
            }
            .into()),
        }
    }

    async fn abort_delete_actor(&mut self, session_id: SessionId, handle: &SessionHandle) {
        let _outcome = handle.abort_delete().await;
        self.actors.remove(&session_id);
    }

    async fn detach(
        &self,
        session_id: SessionId,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<u32, SessionManagerError> {
        if let Some(handle) = self.actors.get(&session_id) {
            return Ok(handle.detach(connection_id, attachment_id).await?);
        }
        Ok(0)
    }

    async fn detach_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<(), SessionManagerError> {
        let mut first_error = None;
        for handle in self.actors.values() {
            if let Err(error) = handle.detach_connection(connection_id).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.into()))
    }

    async fn shutdown_actors(&mut self) -> Result<(), SessionManagerError> {
        let actors = self
            .actors
            .iter()
            .map(|(id, handle)| (*id, handle.clone()))
            .collect::<Vec<_>>();
        let mut first_error = None;
        for (id, handle) in actors {
            match handle.shutdown().await {
                Ok(()) => {
                    self.actors.remove(&id);
                }
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.into()))
    }

    async fn flush_actors(&self) -> Result<(), SessionManagerError> {
        let mut first_error = None;
        for handle in self.actors.values() {
            if let Err(error) = handle.flush().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.into()))
    }
}

fn validate_materialization_settings(
    settings: &super::SessionSettings,
    catalog: &super::ModelCatalogState,
) -> Result<(), SessionCommandError> {
    let super::ModelCatalogState::Ready(models) = catalog else {
        return Ok(());
    };
    let Some(model) = models.iter().find(|model| model.id == settings.model) else {
        return Err(SessionCommandError::ModelNotFound {
            model: settings.model.clone(),
        });
    };
    if !model.reasoning_efforts.contains(&settings.reasoning) {
        return Err(SessionCommandError::UnsupportedReasoning {
            model: settings.model.clone(),
            reasoning: settings.reasoning.as_str(),
        });
    }
    Ok(())
}

fn title_reasoning(
    settings: &super::SessionSettings,
    catalog: &super::ModelCatalogState,
) -> ReasoningLevel {
    let super::ModelCatalogState::Ready(models) = catalog else {
        return settings.reasoning;
    };
    let model = models
        .iter()
        .find(|model| model.id == settings.model)
        .expect("validated ready catalog must contain the selected model");
    [
        ReasoningLevel::None,
        ReasoningLevel::Minimal,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::Xhigh,
        ReasoningLevel::Max,
    ]
    .into_iter()
    .find(|reasoning| model.reasoning_efforts.contains(reasoning))
    .expect("validated ready catalog model must advertise a reasoning effort")
}

fn startup_failure(message: &str, retryable: bool) -> RunFailureSnapshot {
    let message = sanitize_failure_message(message);
    RunFailureSnapshot {
        stage: RunStage::Startup,
        kind: RunFailureKind::RuntimeInfrastructure,
        retryable,
        message,
    }
}

fn sanitize_failure_message(message: &str) -> String {
    let mut characters = message.chars().peekable();
    let mut plain = String::new();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => consume_escape_sequence(&mut characters),
            '\u{009b}' => consume_control_sequence(&mut characters),
            character if character.is_whitespace() => plain.push(' '),
            character if character.is_control() => {}
            character => plain.push(character),
        }
    }
    let sanitized = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if sanitized.is_empty() {
        "session runtime failed to start".into()
    } else {
        sanitized
    }
}

fn consume_escape_sequence(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match characters.peek() {
        Some('[') => {
            characters.next();
            consume_control_sequence(characters);
        }
        Some(']') => {
            characters.next();
            while let Some(character) = characters.next() {
                if character == '\u{7}'
                    || character == '\u{1b}' && characters.next_if_eq(&'\\').is_some()
                {
                    break;
                }
            }
        }
        Some(_) => {
            characters.next();
        }
        None => {}
    }
}

fn consume_control_sequence(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for character in characters.by_ref() {
        if ('@'..='~').contains(&character) {
            break;
        }
    }
}
