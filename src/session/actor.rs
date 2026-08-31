//! Serialized ownership of one live session and its observers.

use std::{ffi::OsString, future::Future, path::PathBuf, str::FromStr, sync::Arc};

use chrono::Utc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use crate::{
    backend::ActivityTracker,
    harness::{Harness, HarnessError, RunContext, RunEngine, RunEvent},
    runtime::rig::{ActiveModel, ActiveReasoning, ReasoningLevel},
    tools::{
        JobId, JobRegistry, JobRegistryError, PlanToolError, PlanUpdateReceiver, PlanUpdateRequest,
    },
};

use super::{
    AttachmentId, DurableTurn, JobSnapshotDto, ModelCatalogState, ProjectionError,
    RunFailureSnapshot, SessionEngineBundle, SessionEvent, SessionEventEnvelope, SessionProjection,
    SessionRecord, SessionRepository, SessionSnapshot, SessionStoreError, SessionTitle,
    TitleGenerationError, TranscriptItem, TurnStatus, sanitize_generated_title,
};

const COMMAND_CAPACITY: usize = 128;
const OBSERVER_CAPACITY: usize = 128;
const TERMINAL_EVENT_RESERVE: usize = 1;

/// Identifies all observer registrations owned by one client connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConnectionId(pub u64);

/// Stable command failure category independent of actor and transport implementation details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// A run already owns the session.
    Busy,
    /// No run is available for cancellation.
    NotRunning,
    /// A session selector did not resolve.
    SessionNotFound,
    /// An exact title matched more than one session.
    AmbiguousTitle,
    /// A CWD-scoped session name is already in use.
    SessionNameConflict,
    /// A caller argument is invalid.
    InvalidArgument,
    /// The selected model does not exist.
    ModelNotFound,
    /// The selected reasoning level is unsupported.
    UnsupportedReasoning,
    /// A job selector did not resolve.
    JobNotFound,
    /// The backend has not completed startup.
    BackendStarting,
    /// The backend cannot accept commands.
    BackendUnavailable,
    /// Durable state could not be written.
    Persistence,
    /// The selected session is irreversibly quiescing for deletion.
    SessionDeleting,
    /// The selected session was durably deleted.
    SessionDeleted,
    /// An internal invariant or implementation failed.
    Internal,
}

/// An authoritative snapshot paired with events sequenced after it.
pub struct SessionAttachment {
    /// Complete state at the attachment sequence.
    pub snapshot: SessionSnapshot,
    /// Bounded live-event receiver for this attachment.
    pub events: mpsc::Receiver<SessionEventEnvelope>,
}

/// Typed failures returned by session actor commands.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionCommandError {
    /// A backend-reported result-union failure with its sanitized display message intact.
    #[error("{message}")]
    Reported {
        /// Stable machine-readable failure category.
        code: ErrorCode,
        /// Sanitized human-readable backend description.
        message: String,
    },
    /// Another run already owns this session's harness.
    #[error("a run is already active")]
    Busy,
    /// Cancellation was requested while the harness was idle.
    #[error("there is no active run")]
    NotRunning,
    /// The harness cannot allocate another monotonic run identifier.
    #[error("run identifier space is exhausted")]
    RunIdExhausted,
    /// A ready model catalog does not contain the requested identifier.
    #[error("model {model} was not found")]
    ModelNotFound {
        /// Requested provider model identifier.
        model: String,
    },
    /// The active catalog model does not advertise the requested effort.
    #[error("model {model} does not support reasoning effort {reasoning}")]
    UnsupportedReasoning {
        /// Active provider model identifier.
        model: String,
        /// Requested reasoning effort.
        reasoning: &'static str,
    },
    /// A job identifier was not in canonical `job-N` form.
    #[error("job identifier {id} is malformed")]
    InvalidJobId {
        /// Rejected job identifier.
        id: String,
    },
    /// The first prompt of a draft contains no non-whitespace text.
    #[error("the first session prompt must contain non-whitespace text")]
    InvalidPrompt,
    /// The session-local registry has no retained job with this identifier.
    #[error("job {id} was not found")]
    JobNotFound {
        /// Requested canonical job identifier.
        id: String,
    },
    /// Session state could not be written durably.
    #[error("session persistence failed: {message}")]
    Persistence {
        /// Sanitized repository diagnostic.
        message: String,
    },
    /// The isolated job registry could not complete a command.
    #[error("session job command failed: {message}")]
    Job {
        /// Sanitized registry diagnostic.
        message: String,
    },
    /// The authoritative projection rejected an actor-owned event.
    #[error("session projection failed: {message}")]
    Projection {
        /// Projection invariant diagnostic.
        message: String,
    },
    /// The session is irreversibly quiescing for deletion.
    #[error("session is being deleted")]
    Deleting,
    /// The actor is shutting down or is no longer available.
    #[error("session actor is unavailable")]
    Unavailable,
}

/// Terminal result returned after a deleting actor has fully exited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionActorOutcome {
    /// Durable deletion succeeded and observers received their terminal event.
    Deleted,
    /// Durable deletion failed and the quiesced actor exited without a deleted event.
    DeleteAborted,
}

/// Cloneable command boundary for one serialized session actor.
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<SessionCommand>,
}

impl SessionHandle {
    /// Starts an actor from durable state, an authoritative projection, and isolated runtime state.
    pub fn spawn<E>(
        repository: Arc<dyn SessionRepository>,
        record: SessionRecord,
        projection: SessionProjection,
        bundle: SessionEngineBundle<E>,
        activity: ActivityTracker,
    ) -> Self
    where
        E: RunEngine,
    {
        let SessionEngineBundle {
            engine,
            active_model,
            active_reasoning,
            jobs,
            plans,
        } = bundle;
        let harness = Harness::with_history(engine, record.history.clone());
        Self::spawn_actor(SessionActorInit {
            repository,
            record,
            projection,
            harness,
            active_model,
            active_reasoning,
            jobs,
            plans,
            activity,
        })
    }

    /// Starts an actor for a session whose first prompt and running turn are already durable.
    pub fn spawn_materialized<E>(
        repository: Arc<dyn SessionRepository>,
        record: SessionRecord,
        mut projection: SessionProjection,
        bundle: SessionEngineBundle<E>,
        first_prompt: String,
        activity: ActivityTracker,
    ) -> Result<Self, SessionCommandError>
    where
        E: RunEngine,
    {
        let SessionEngineBundle {
            engine,
            active_model,
            active_reasoning,
            jobs,
            plans,
        } = bundle;
        let mut harness = Harness::with_history(engine, record.history.clone());
        let context = RunContext {
            cwd: cwd_path(&record.cwd),
            plan: record.plan.clone(),
        };
        let RunEvent::Started { run_id } = harness
            .submit(first_prompt.clone(), context)
            .map_err(map_harness_error)?
        else {
            unreachable!("harness submission returns Started")
        };
        validate_materialized_start(&record, run_id.get(), &first_prompt)?;
        projection
            .install_persisted_started(run_id.get(), first_prompt)
            .map_err(map_projection_error)?;
        activity.set_run(record.id, true);
        Ok(Self::spawn_actor(SessionActorInit {
            repository,
            record,
            projection,
            harness,
            active_model,
            active_reasoning,
            jobs,
            plans,
            activity,
        }))
    }

    fn spawn_actor<E>(init: SessionActorInit<E>) -> Self
    where
        E: RunEngine,
    {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (job_events, job_event_rx) = mpsc::channel(1);
        let (job_monitor_stop, mut job_monitor_stop_rx) = tokio::sync::watch::channel(false);
        let mut job_changes = init.jobs.subscribe_changes();
        let monitored_jobs = init.jobs.clone();
        let session_id = init.record.id;
        match monitored_jobs.running_count() {
            Ok(running_jobs) => init.activity.set_running_jobs(session_id, running_jobs),
            Err(_) => init.activity.set_running_jobs(session_id, 1),
        }
        let monitor_activity = init.activity.clone();
        let job_monitor = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    changed = job_monitor_stop_rx.changed() => {
                        if changed.is_err() || *job_monitor_stop_rx.borrow() {
                            break;
                        }
                    }
                    changed = job_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let Ok(running_jobs) = monitored_jobs.running_count() else {
                            monitor_activity.set_running_jobs(session_id, 1);
                            break;
                        };
                        monitor_activity.set_running_jobs(session_id, running_jobs);
                        match job_events.try_send(()) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                            Err(mpsc::error::TrySendError::Closed(())) => break,
                        }
                    }
                }
            }
        });
        let actor = SessionActor::new(
            init,
            JobMonitor {
                events: job_event_rx,
                stop: job_monitor_stop,
                task: job_monitor,
            },
        );
        tokio::spawn(send_terminal_after_run(actor.run(command_rx)));
        Self { commands }
    }

    /// Registers one bounded observer and returns an atomic attachment snapshot.
    pub async fn attach(
        &self,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<SessionAttachment, SessionCommandError> {
        let (events, event_rx) = mpsc::channel(OBSERVER_CAPACITY + TERMINAL_EVENT_RESERVE);
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Attach {
            connection_id,
            attachment_id,
            events,
            response,
        })
        .await?;
        let snapshot = response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)??;
        Ok(SessionAttachment {
            snapshot,
            events: event_rx,
        })
    }

    /// Removes one exact observer registration without cancelling work.
    pub async fn detach(
        &self,
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
    ) -> Result<u32, SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Detach {
            connection_id,
            attachment_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)
    }

    /// Removes every observer registered by `connection_id` without cancelling work.
    pub async fn detach_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::DetachConnection {
            connection_id,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)
    }

    /// Returns the current authoritative state without registering an observer.
    pub async fn snapshot(&self) -> Result<SessionSnapshot, SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Snapshot { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Starts one run and returns its actor-local run identifier.
    pub async fn submit(&self, prompt: String) -> Result<u64, SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Submit { prompt, response })
            .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Explicitly cancels the active run.
    pub async fn cancel(&self) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Cancel { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Applies a user-selected title through the actor's ordered event stream.
    pub async fn rename(&self, title: SessionTitle) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Rename { title, response })
            .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Applies one asynchronous generated-title result when its revision is still current.
    pub async fn apply_generated_title(
        &self,
        expected_revision: u64,
        generated: Result<String, TitleGenerationError>,
    ) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::ApplyGeneratedTitle {
            expected_revision,
            generated,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Irreversibly quiesces model and job work and persists its terminal visible state.
    pub async fn prepare_delete(&self) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::PrepareDelete { response })
            .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Emits the terminal deletion event and waits until the actor has fully exited.
    pub async fn finish_delete(&self) -> Result<SessionActorOutcome, SessionCommandError> {
        let (completion, completion_rx) = oneshot::channel();
        self.send(SessionCommand::FinishDelete { completion })
            .await?;
        completion_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Closes a quiesced actor without a deletion event and waits until it has fully exited.
    pub async fn abort_delete(&self) -> Result<SessionActorOutcome, SessionCommandError> {
        let (completion, completion_rx) = oneshot::channel();
        self.send(SessionCommand::AbortDelete { completion })
            .await?;
        completion_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Selects the model read when future engine streams start.
    pub async fn select_model(&self, model: String) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::SelectModel { model, response })
            .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Selects a reasoning effort advertised for the active catalog model.
    pub async fn select_reasoning(
        &self,
        reasoning: ReasoningLevel,
    ) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::SelectReasoning {
            reasoning,
            response,
        })
        .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Lists point-in-time snapshots from this session's isolated job registry.
    pub async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::ListJobs { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Cancels one job without blocking actor event reduction while the producer settles.
    pub async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::CancelJob { id, response })
            .await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Retries an outstanding full durable checkpoint.
    pub async fn flush(&self) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Flush { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    /// Flushes durable state, shuts down jobs, and stops this actor.
    pub async fn shutdown(&self) -> Result<(), SessionCommandError> {
        let (response, response_rx) = oneshot::channel();
        self.send(SessionCommand::Shutdown { response }).await?;
        response_rx
            .await
            .map_err(|_| SessionCommandError::Unavailable)?
    }

    async fn send(&self, command: SessionCommand) -> Result<(), SessionCommandError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| SessionCommandError::Unavailable)
    }
}

enum SessionCommand {
    Attach {
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
        events: mpsc::Sender<SessionEventEnvelope>,
        response: oneshot::Sender<Result<SessionSnapshot, SessionCommandError>>,
    },
    Detach {
        connection_id: ConnectionId,
        attachment_id: AttachmentId,
        response: oneshot::Sender<u32>,
    },
    DetachConnection {
        connection_id: ConnectionId,
        response: oneshot::Sender<()>,
    },
    Snapshot {
        response: oneshot::Sender<Result<SessionSnapshot, SessionCommandError>>,
    },
    Submit {
        prompt: String,
        response: oneshot::Sender<Result<u64, SessionCommandError>>,
    },
    Cancel {
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    Rename {
        title: SessionTitle,
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    ApplyGeneratedTitle {
        expected_revision: u64,
        generated: Result<String, TitleGenerationError>,
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    PrepareDelete {
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    FinishDelete {
        completion: oneshot::Sender<Result<SessionActorOutcome, SessionCommandError>>,
    },
    AbortDelete {
        completion: oneshot::Sender<Result<SessionActorOutcome, SessionCommandError>>,
    },
    SelectModel {
        model: String,
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    SelectReasoning {
        reasoning: ReasoningLevel,
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    ListJobs {
        response: oneshot::Sender<Result<Vec<JobSnapshotDto>, SessionCommandError>>,
    },
    CancelJob {
        id: String,
        response: oneshot::Sender<Result<JobSnapshotDto, SessionCommandError>>,
    },
    Flush {
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
    Shutdown {
        response: oneshot::Sender<Result<(), SessionCommandError>>,
    },
}

struct Observer {
    connection_id: ConnectionId,
    attachment_id: AttachmentId,
    events: mpsc::Sender<SessionEventEnvelope>,
}

struct JobMonitor {
    events: mpsc::Receiver<()>,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

struct SessionActorInit<E> {
    repository: Arc<dyn SessionRepository>,
    record: SessionRecord,
    projection: SessionProjection,
    harness: Harness<E>,
    active_model: ActiveModel,
    active_reasoning: ActiveReasoning,
    jobs: JobRegistry,
    plans: PlanUpdateReceiver,
    activity: ActivityTracker,
}

struct SessionActor<E> {
    repository: Arc<dyn SessionRepository>,
    record: SessionRecord,
    projection: SessionProjection,
    harness: Harness<E>,
    active_model: ActiveModel,
    active_reasoning: ActiveReasoning,
    jobs: JobRegistry,
    plans: PlanUpdateReceiver,
    activity: ActivityTracker,
    job_monitor: JobMonitor,
    observers: Vec<Observer>,
    dirty: Option<SessionRecord>,
    shutting_down: bool,
    deleting: bool,
}

struct TerminalCompletion {
    response: oneshot::Sender<Result<SessionActorOutcome, SessionCommandError>>,
    outcome: SessionActorOutcome,
}

enum ActorExit {
    Shutdown,
    Delete(TerminalCompletion),
}

async fn send_terminal_after_run<F>(run: F)
where
    F: Future<Output = Option<TerminalCompletion>>,
{
    if let Some(terminal) = run.await {
        let _ = terminal.response.send(Ok(terminal.outcome));
    }
}

enum ActorInput {
    Command(SessionCommand),
    Run(Option<RunEvent>),
    JobsChanged,
    PlanUpdate(PlanUpdateRequest),
}

impl<E: RunEngine> SessionActor<E> {
    fn new(init: SessionActorInit<E>, job_monitor: JobMonitor) -> Self {
        Self {
            repository: init.repository,
            record: init.record,
            projection: init.projection,
            harness: init.harness,
            active_model: init.active_model,
            active_reasoning: init.active_reasoning,
            jobs: init.jobs,
            plans: init.plans,
            activity: init.activity,
            job_monitor,
            observers: Vec::new(),
            dirty: None,
            shutting_down: false,
            deleting: false,
        }
    }

    async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<SessionCommand>,
    ) -> Option<TerminalCompletion> {
        let terminal = loop {
            if command_rx.is_closed() && !self.harness.is_running() {
                if self.jobs.shutdown().await.is_ok() {
                    self.clear_activity();
                }
                break None;
            }

            let input = if self.harness.is_running() {
                tokio::select! {
                    Some(command) = command_rx.recv() => ActorInput::Command(command),
                    event = self.harness.next_event() => ActorInput::Run(event),
                    Some(()) = self.job_monitor.events.recv() => ActorInput::JobsChanged,
                    Some(request) = self.plans.recv() => ActorInput::PlanUpdate(request),
                }
            } else {
                tokio::select! {
                    Some(command) = command_rx.recv() => ActorInput::Command(command),
                    Some(()) = self.job_monitor.events.recv() => ActorInput::JobsChanged,
                    Some(request) = self.plans.recv() => ActorInput::PlanUpdate(request),
                }
            };

            match input {
                ActorInput::Command(command) => {
                    if let Some(exit) = self.handle_command(command).await {
                        break Some(exit);
                    }
                }
                ActorInput::Run(Some(event)) => self.handle_run_event(event).await,
                ActorInput::Run(None) => {}
                ActorInput::JobsChanged => self.handle_jobs_changed(),
                ActorInput::PlanUpdate(request) => self.handle_plan_update(request).await,
            }
        };
        self.stop_job_monitor().await;
        self.observers.clear();
        match terminal {
            Some(ActorExit::Delete(terminal)) => Some(terminal),
            Some(ActorExit::Shutdown) | None => None,
        }
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Option<ActorExit> {
        match command {
            SessionCommand::Attach {
                connection_id,
                attachment_id,
                events,
                response,
            } => {
                if self.deleting {
                    let _ = response.send(Err(SessionCommandError::Deleting));
                    return None;
                }
                self.observers.push(Observer {
                    connection_id,
                    attachment_id,
                    events,
                });
                let result = self.attachment_snapshot();
                if result.is_err() {
                    self.observers.pop();
                }
                let _ = response.send(result);
            }
            SessionCommand::Detach {
                connection_id,
                attachment_id,
                response,
            } => {
                self.observers.retain(|observer| {
                    (observer.connection_id, observer.attachment_id)
                        != (connection_id, attachment_id)
                });
                let _ = response.send(self.attached_client_count());
            }
            SessionCommand::DetachConnection {
                connection_id,
                response,
            } => {
                self.observers
                    .retain(|observer| observer.connection_id != connection_id);
                let _ = response.send(());
            }
            SessionCommand::Snapshot { response } => {
                let _ = response.send(self.attachment_snapshot());
            }
            SessionCommand::Submit { prompt, response } => {
                let result = self.submit(prompt).await;
                let _ = response.send(result);
            }
            SessionCommand::Cancel { response } => {
                let result = self.cancel().await;
                let _ = response.send(result);
            }
            SessionCommand::Rename { title, response } => {
                let result = self.rename(title).await;
                let _ = response.send(result);
            }
            SessionCommand::ApplyGeneratedTitle {
                expected_revision,
                generated,
                response,
            } => {
                let result = self
                    .apply_generated_title(expected_revision, generated)
                    .await;
                let _ = response.send(result);
            }
            SessionCommand::PrepareDelete { response } => {
                let result = self.prepare_delete().await;
                let _ = response.send(result);
            }
            SessionCommand::FinishDelete { completion } => match self.finish_delete() {
                Ok(()) => {
                    return Some(ActorExit::Delete(TerminalCompletion {
                        response: completion,
                        outcome: SessionActorOutcome::Deleted,
                    }));
                }
                Err(error) => {
                    let _ = completion.send(Err(error));
                }
            },
            SessionCommand::AbortDelete { completion } => match self.abort_delete() {
                Ok(()) => {
                    return Some(ActorExit::Delete(TerminalCompletion {
                        response: completion,
                        outcome: SessionActorOutcome::DeleteAborted,
                    }));
                }
                Err(error) => {
                    let _ = completion.send(Err(error));
                }
            },
            SessionCommand::SelectModel { model, response } => {
                let result = self.select_model(model).await;
                let _ = response.send(result);
            }
            SessionCommand::SelectReasoning {
                reasoning,
                response,
            } => {
                let result = self.select_reasoning(reasoning).await;
                let _ = response.send(result);
            }
            SessionCommand::ListJobs { response } => {
                let result = self.ensure_mutable().and_then(|()| self.job_snapshots());
                let _ = response.send(result);
            }
            SessionCommand::CancelJob { id, response } => {
                if let Err(error) = self.ensure_mutable() {
                    let _ = response.send(Err(error));
                } else {
                    self.cancel_job(id, response);
                }
            }
            SessionCommand::Flush { response } => {
                let result = match self.ensure_not_deleting() {
                    Ok(()) => self.flush().await,
                    Err(error) => Err(error),
                };
                let _ = response.send(result);
            }
            SessionCommand::Shutdown { response } => {
                if let Err(error) = self.ensure_not_deleting() {
                    let _ = response.send(Err(error));
                    return None;
                }
                self.shutting_down = true;
                let result = self.shutdown().await;
                let stop = result.is_ok();
                let _ = response.send(result);
                if stop {
                    return Some(ActorExit::Shutdown);
                }
            }
        }
        None
    }

    fn ensure_mutable(&self) -> Result<(), SessionCommandError> {
        self.ensure_not_deleting()?;
        if self.shutting_down {
            Err(SessionCommandError::Unavailable)
        } else {
            Ok(())
        }
    }

    fn ensure_not_deleting(&self) -> Result<(), SessionCommandError> {
        if self.deleting {
            Err(SessionCommandError::Deleting)
        } else {
            Ok(())
        }
    }

    async fn rename(&mut self, title: SessionTitle) -> Result<(), SessionCommandError> {
        self.ensure_mutable()?;
        let persisted = self
            .repository
            .rename(self.record.id, title)
            .await
            .map_err(map_store_error)?;
        self.install_persisted_title(persisted)
    }

    async fn apply_generated_title(
        &mut self,
        expected_revision: u64,
        generated: Result<String, TitleGenerationError>,
    ) -> Result<(), SessionCommandError> {
        self.ensure_mutable()?;
        let Ok(generated) = generated else {
            return Ok(());
        };
        let Some(title) = sanitize_generated_title(&generated) else {
            return Ok(());
        };
        let Some(persisted) = self
            .repository
            .compare_and_set_generated_title(self.record.id, expected_revision, title)
            .await
            .map_err(map_store_error)?
        else {
            return Ok(());
        };
        self.install_persisted_title(persisted)
    }

    fn install_persisted_title(
        &mut self,
        persisted: SessionRecord,
    ) -> Result<(), SessionCommandError> {
        self.record.title = persisted.title.clone();
        self.record.title_source = persisted.title_source;
        self.record.title_revision = persisted.title_revision;
        if self.dirty.is_some() {
            self.dirty = Some(self.record.clone());
        }
        let envelope = self.project(SessionEvent::TitleChanged {
            title: persisted.title,
            title_revision: persisted.title_revision,
        })?;
        if let Some(envelope) = envelope {
            self.broadcast(envelope);
        }
        Ok(())
    }

    async fn prepare_delete(&mut self) -> Result<(), SessionCommandError> {
        self.ensure_mutable()?;
        self.deleting = true;

        let mut first_error = None;
        let mut cancellation = None;
        if self.harness.is_running() {
            match self
                .harness
                .cancel()
                .map(map_run_event)
                .map_err(map_harness_error)
            {
                Ok(event) => {
                    self.activity.set_run(self.record.id, false);
                    let mut updated_record = self.record.clone();
                    match apply_durable_event(&mut updated_record, &event) {
                        Ok(()) => {
                            self.record = updated_record;
                            match self.project(event) {
                                Ok(envelope) => cancellation = envelope,
                                Err(error) => first_error = Some(error),
                            }
                        }
                        Err(error) => first_error = Some(error),
                    }
                }
                Err(error) => first_error = Some(error),
            }
        }

        if let Err(error) = self.jobs.shutdown().await.map_err(map_job_error)
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        let persistence_transition = match self.repository.checkpoint(self.record.clone()).await {
            Ok(()) => {
                self.dirty = None;
                self.projection
                    .snapshot(Vec::new())
                    .persistence_warning
                    .as_ref()
                    .map(|_| None)
            }
            Err(error) => {
                self.dirty = Some(self.record.clone());
                if first_error.is_none() {
                    first_error = Some(map_store_error(error));
                }
                None
            }
        };

        if let Some(cancellation) = cancellation {
            self.broadcast(cancellation);
        }
        self.broadcast_persistence_transition(persistence_transition);

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn finish_delete(&mut self) -> Result<(), SessionCommandError> {
        self.ensure_delete_prepared()?;
        let envelope = self.project(SessionEvent::Deleted {
            session_id: self.record.id,
        })?;
        if let Some(envelope) = envelope {
            self.broadcast_terminal(envelope);
        }
        self.clear_activity();
        Ok(())
    }

    fn abort_delete(&mut self) -> Result<(), SessionCommandError> {
        self.ensure_delete_prepared()?;
        self.clear_activity();
        Ok(())
    }

    fn ensure_delete_prepared(&self) -> Result<(), SessionCommandError> {
        if self.deleting {
            Ok(())
        } else {
            Err(durable_invariant("session deletion was not prepared"))
        }
    }

    async fn submit(&mut self, prompt: String) -> Result<u64, SessionCommandError> {
        self.ensure_mutable()?;
        let context = RunContext {
            cwd: cwd_path(&self.record.cwd),
            plan: self.record.plan.clone(),
        };
        let event = self
            .harness
            .submit(prompt.clone(), context)
            .map_err(map_harness_error)?;
        let RunEvent::Started { run_id } = event else {
            unreachable!("harness submission returns Started")
        };
        let event = SessionEvent::Started {
            run_id: run_id.get(),
            prompt,
        };
        let mut updated_record = self.record.clone();
        if let Err(error) = apply_durable_event(&mut updated_record, &event) {
            let _ = self.harness.cancel();
            return Err(error);
        }
        let envelope = match self.project(event) {
            Ok(envelope) => envelope,
            Err(error) => {
                let _ = self.harness.cancel();
                return Err(error);
            }
        };
        self.record = updated_record;
        let warning = self.persist_checkpoint().await;
        if let Some(envelope) = envelope {
            self.broadcast(envelope);
        }
        self.broadcast_persistence_transition(warning);
        self.activity.set_run(self.record.id, true);
        Ok(run_id.get())
    }

    async fn cancel(&mut self) -> Result<(), SessionCommandError> {
        self.ensure_mutable()?;
        let event = self.harness.cancel().map_err(map_harness_error)?;
        self.activity.set_run(self.record.id, false);
        let session_event = map_run_event(event);
        let mut updated_record = self.record.clone();
        apply_durable_event(&mut updated_record, &session_event)?;
        let envelope = self.project(session_event)?;
        self.record = updated_record;
        let warning = self.persist_checkpoint().await;
        if let Some(envelope) = envelope {
            self.broadcast(envelope);
        }
        self.broadcast_persistence_transition(warning);
        Ok(())
    }

    async fn select_model(&mut self, model: String) -> Result<(), SessionCommandError> {
        self.ensure_mutable()?;
        let catalog = self.projection.snapshot(Vec::new()).catalog;
        if let ModelCatalogState::Ready(models) = catalog
            && !models.iter().any(|candidate| candidate.id == model)
        {
            return Err(SessionCommandError::ModelNotFound { model });
        }
        self.active_model.select(model.clone());
        self.record.settings.model = model;
        let last_activity = Utc::now();
        self.record.last_activity = last_activity;
        let event = self.project(SessionEvent::SettingsChanged {
            settings: self.record.settings.clone(),
            last_activity,
        })?;
        let warning = self.persist_metadata().await;
        if let Some(event) = event {
            self.broadcast(event);
        }
        self.broadcast_persistence_transition(warning);
        Ok(())
    }

    async fn select_reasoning(
        &mut self,
        reasoning: ReasoningLevel,
    ) -> Result<(), SessionCommandError> {
        self.ensure_mutable()?;
        let catalog = self.projection.snapshot(Vec::new()).catalog;
        let supported = match &catalog {
            ModelCatalogState::Ready(models) => models
                .iter()
                .find(|model| model.id == self.record.settings.model)
                .is_some_and(|model| model.reasoning_efforts.contains(&reasoning)),
            ModelCatalogState::Loading | ModelCatalogState::Failed(_) => false,
        };
        if !supported {
            return Err(SessionCommandError::UnsupportedReasoning {
                model: self.record.settings.model.clone(),
                reasoning: reasoning.as_str(),
            });
        }
        self.active_reasoning.select(reasoning);
        self.record.settings.reasoning = reasoning;
        let last_activity = Utc::now();
        self.record.last_activity = last_activity;
        let event = self.project(SessionEvent::SettingsChanged {
            settings: self.record.settings.clone(),
            last_activity,
        })?;
        let warning = self.persist_metadata().await;
        if let Some(event) = event {
            self.broadcast(event);
        }
        self.broadcast_persistence_transition(warning);
        Ok(())
    }

    fn cancel_job(
        &self,
        id: String,
        response: oneshot::Sender<Result<JobSnapshotDto, SessionCommandError>>,
    ) {
        let job_id = match JobId::from_str(&id) {
            Ok(job_id) => job_id,
            Err(JobRegistryError::MalformedId) => {
                let _ = response.send(Err(SessionCommandError::InvalidJobId { id }));
                return;
            }
            Err(error) => {
                let _ = response.send(Err(map_job_error(error)));
                return;
            }
        };
        let jobs = self.jobs.clone();
        tokio::spawn(async move {
            let result = jobs
                .cancel(job_id)
                .await
                .map(|snapshot| JobSnapshotDto::from(&snapshot))
                .map_err(map_job_error);
            let _ = response.send(result);
        });
    }

    async fn handle_run_event(&mut self, event: RunEvent) {
        if matches!(
            &event,
            RunEvent::Completed { .. } | RunEvent::Failed { .. } | RunEvent::Cancelled { .. }
        ) {
            self.activity.set_run(self.record.id, false);
        }
        match event {
            RunEvent::ContextUsage {
                run_id,
                input_tokens,
            } => {
                self.record.settings.context_tokens = input_tokens;
                let last_activity = Utc::now();
                self.record.last_activity = last_activity;
                let applied = self.project(SessionEvent::ContextUsage {
                    run_id: run_id.get(),
                    input_tokens,
                    last_activity,
                });
                if let Ok(envelope) = applied {
                    let warning = self.persist_metadata().await;
                    if let Some(envelope) = envelope {
                        self.broadcast(envelope);
                    }
                    self.broadcast_persistence_transition(warning);
                }
            }
            RunEvent::Completed { run_id, response } => {
                let last_activity = Utc::now();
                let event = SessionEvent::Completed {
                    run_id: run_id.get(),
                    response,
                    last_activity,
                };
                self.persist_durable_run_event(event, true).await;
            }
            RunEvent::ToolStarted { .. } | RunEvent::Failed { .. } | RunEvent::Cancelled { .. } => {
                self.persist_durable_run_event(map_run_event(event), false)
                    .await;
            }
            event => {
                if let Ok(Some(envelope)) = self.project(map_run_event(event)) {
                    self.broadcast(envelope);
                }
            }
        }
    }

    async fn persist_durable_run_event(&mut self, event: SessionEvent, commit_history: bool) {
        let mut updated_record = self.record.clone();
        if apply_durable_event(&mut updated_record, &event).is_err() {
            return;
        }
        let applied = self.project(event);
        if applied.is_err() {
            return;
        }
        self.record = updated_record;
        if commit_history {
            self.record.history = self.harness.history().to_vec();
        }
        let warning = self.persist_checkpoint().await;
        if let Ok(Some(envelope)) = applied {
            self.broadcast(envelope);
        }
        self.broadcast_persistence_transition(warning);
    }

    async fn handle_plan_update(&mut self, request: PlanUpdateRequest) {
        let previous = std::mem::replace(&mut self.record.plan, request.plan().to_vec());
        let explanation = request.explanation().map(str::to_owned);
        let event = match self.project_strict(SessionEvent::PlanChanged(self.record.plan.clone())) {
            Ok(event) => event,
            Err(_) => {
                self.record.plan = previous;
                request.fail(PlanToolError::Runtime);
                return;
            }
        };
        let warning = self.persist_checkpoint().await;
        let durable = !matches!(warning, Some(Some(_)));
        self.broadcast(event);
        self.broadcast_persistence_transition(warning);
        request.succeed(crate::tools::PlanUpdateOutcome::new(
            self.record.plan.clone(),
            explanation,
            durable,
        ));
    }

    fn attachment_snapshot(&self) -> Result<SessionSnapshot, SessionCommandError> {
        let mut snapshot = self.projection.snapshot(self.job_snapshots()?);
        snapshot.summary.attached_clients = self.attached_client_count();
        Ok(snapshot)
    }

    fn attached_client_count(&self) -> u32 {
        u32::try_from(self.observers.len()).unwrap_or(u32::MAX)
    }

    fn job_snapshots(&self) -> Result<Vec<JobSnapshotDto>, SessionCommandError> {
        self.jobs
            .status(None)
            .map(|jobs| jobs.iter().map(JobSnapshotDto::from).collect())
            .map_err(map_job_error)
    }

    fn handle_jobs_changed(&mut self) {
        if self.deleting {
            return;
        }
        let Ok(jobs) = self.job_snapshots() else {
            return;
        };
        if let Ok(Some(envelope)) = self.project(SessionEvent::JobsChanged(jobs)) {
            self.broadcast(envelope);
        }
    }

    fn project(
        &mut self,
        event: SessionEvent,
    ) -> Result<Option<SessionEventEnvelope>, SessionCommandError> {
        match self.projection.apply(event.clone()) {
            Ok(envelope) => Ok(Some(envelope)),
            Err(ProjectionError::SequenceExhausted) => {
                self.projection
                    .apply_unsequenced(&event)
                    .map_err(map_projection_error)?;
                self.observers.clear();
                Ok(None)
            }
            Err(error) => Err(map_projection_error(error)),
        }
    }

    fn project_strict(
        &mut self,
        event: SessionEvent,
    ) -> Result<SessionEventEnvelope, SessionCommandError> {
        self.projection.apply(event).map_err(map_projection_error)
    }

    fn broadcast(&mut self, envelope: SessionEventEnvelope) {
        self.observers.retain(|observer| {
            observer.events.capacity() > TERMINAL_EVENT_RESERVE
                && observer.events.try_send(envelope.clone()).is_ok()
        });
    }

    fn broadcast_terminal(&mut self, envelope: SessionEventEnvelope) {
        self.observers.retain(|observer| {
            let result = observer.events.try_send(envelope.clone());
            debug_assert!(
                !matches!(result, Err(mpsc::error::TrySendError::Full(_))),
                "surviving observers retain one terminal event slot"
            );
            result.is_ok()
        });
    }

    async fn persist_metadata(&mut self) -> Option<Option<String>> {
        let result = if self.dirty.is_some() {
            self.repository.checkpoint(self.record.clone()).await
        } else {
            self.repository.update_metadata(self.record.clone()).await
        };
        self.persistence_result(result)
    }

    async fn persist_checkpoint(&mut self) -> Option<Option<String>> {
        let result = self.repository.checkpoint(self.record.clone()).await;
        self.persistence_result(result)
    }

    fn persistence_result(
        &mut self,
        result: Result<(), SessionStoreError>,
    ) -> Option<Option<String>> {
        match result {
            Ok(()) => {
                self.dirty = None;
                self.projection
                    .snapshot(Vec::new())
                    .persistence_warning
                    .as_ref()
                    .map(|_| None)
            }
            Err(error) => {
                self.dirty = Some(self.record.clone());
                Some(Some(error.to_string()))
            }
        }
    }

    fn broadcast_persistence_transition(&mut self, transition: Option<Option<String>>) {
        let Some(warning) = transition else {
            return;
        };
        if let Ok(Some(envelope)) = self.project(SessionEvent::PersistenceWarning(warning)) {
            self.broadcast(envelope);
        }
    }

    async fn flush(&mut self) -> Result<(), SessionCommandError> {
        let Some(record) = self.dirty.clone() else {
            return Ok(());
        };
        match self.repository.checkpoint(record).await {
            Ok(()) => {
                self.dirty = None;
                self.broadcast_persistence_transition(Some(None));
                Ok(())
            }
            Err(error) => {
                self.dirty = Some(self.record.clone());
                self.broadcast_persistence_transition(Some(Some(error.to_string())));
                Err(map_store_error(error))
            }
        }
    }

    async fn shutdown(&mut self) -> Result<(), SessionCommandError> {
        self.flush().await?;
        self.jobs.shutdown().await.map_err(map_job_error)?;
        self.clear_activity();
        Ok(())
    }

    fn clear_activity(&self) {
        self.activity.set_run(self.record.id, false);
        self.activity.set_running_jobs(self.record.id, 0);
    }

    async fn stop_job_monitor(&mut self) {
        self.job_monitor.stop.send_replace(true);
        let _ = (&mut self.job_monitor.task).await;
    }
}

fn validate_materialized_start(
    record: &SessionRecord,
    run_id: u64,
    first_prompt: &str,
) -> Result<(), SessionCommandError> {
    let running = record
        .turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Running)
        .collect::<Vec<_>>();
    let [turn] = running.as_slice() else {
        return Err(durable_invariant(
            "materialized session must contain exactly one running turn",
        ));
    };
    if turn.run_id != run_id {
        return Err(durable_invariant(format!(
            "materialized run {} does not match harness run {run_id}",
            turn.run_id
        )));
    }
    let prompt_position = usize::try_from(turn.prompt_position)
        .map_err(|_| durable_invariant("materialized prompt position is out of range"))?;
    if record.transcript.get(prompt_position) != Some(&TranscriptItem::User(first_prompt.into())) {
        return Err(durable_invariant(
            "materialized running turn does not reference the first prompt",
        ));
    }
    Ok(())
}

fn apply_durable_event(
    record: &mut SessionRecord,
    event: &SessionEvent,
) -> Result<(), SessionCommandError> {
    match event {
        SessionEvent::Started { run_id, prompt } => {
            if record
                .turns
                .iter()
                .any(|turn| turn.status == TurnStatus::Running)
            {
                return Err(durable_invariant(
                    "cannot append a durable turn while another turn is running",
                ));
            }
            let prompt_position = u64::try_from(record.transcript.len())
                .map_err(|_| durable_invariant("durable transcript position is exhausted"))?;
            let ordinal = u64::try_from(record.turns.len())
                .map_err(|_| durable_invariant("durable turn ordinal is exhausted"))?;
            record.transcript.push(TranscriptItem::User(prompt.clone()));
            record.turns.push(DurableTurn {
                ordinal,
                run_id: *run_id,
                prompt_position,
                status: TurnStatus::Running,
            });
        }
        SessionEvent::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => {
            running_turn_mut(record, *run_id)?;
            record.transcript.push(TranscriptItem::ToolStarted {
                run_id: *run_id,
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
        }
        SessionEvent::Completed {
            run_id,
            response,
            last_activity,
        } => {
            running_turn_mut(record, *run_id)?.status = TurnStatus::Completed;
            record
                .transcript
                .push(TranscriptItem::Assistant(response.clone()));
            record.last_activity = *last_activity;
        }
        SessionEvent::Failed { run_id, failure } => {
            running_turn_mut(record, *run_id)?.status = TurnStatus::Failed;
            record.transcript.push(TranscriptItem::Failed {
                run_id: *run_id,
                failure: failure.clone(),
            });
        }
        SessionEvent::Cancelled { run_id } => {
            running_turn_mut(record, *run_id)?.status = TurnStatus::Cancelled;
            record
                .transcript
                .push(TranscriptItem::Cancelled { run_id: *run_id });
        }
        SessionEvent::AssistantDelta { .. }
        | SessionEvent::ContextUsage { .. }
        | SessionEvent::ToolFinished { .. }
        | SessionEvent::TitleChanged { .. }
        | SessionEvent::SettingsChanged { .. }
        | SessionEvent::PlanChanged(_)
        | SessionEvent::JobsChanged(_)
        | SessionEvent::CatalogChanged(_)
        | SessionEvent::PersistenceWarning(_)
        | SessionEvent::Deleted { .. } => {}
    }
    Ok(())
}

fn running_turn_mut(
    record: &mut SessionRecord,
    run_id: u64,
) -> Result<&mut DurableTurn, SessionCommandError> {
    record
        .turns
        .iter_mut()
        .find(|turn| turn.run_id == run_id && turn.status == TurnStatus::Running)
        .ok_or_else(|| durable_invariant(format!("durable running turn {run_id} was not found")))
}

fn durable_invariant(message: impl Into<String>) -> SessionCommandError {
    SessionCommandError::Projection {
        message: message.into(),
    }
}

fn map_run_event(event: RunEvent) -> SessionEvent {
    match event {
        RunEvent::Started { .. } => unreachable!("Started events are enriched by submit"),
        RunEvent::AssistantDelta { run_id, text } => SessionEvent::AssistantDelta {
            run_id: run_id.get(),
            text,
        },
        RunEvent::ContextUsage {
            run_id: _,
            input_tokens: _,
        } => unreachable!("ContextUsage events are enriched by the actor"),
        RunEvent::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => SessionEvent::ToolStarted {
            run_id: run_id.get(),
            call_id,
            name,
            arguments,
        },
        RunEvent::ToolFinished {
            run_id,
            call_id,
            name,
        } => SessionEvent::ToolFinished {
            run_id: run_id.get(),
            call_id,
            name,
        },
        RunEvent::Completed {
            run_id: _,
            response: _,
        } => unreachable!("Completed events are enriched by the actor"),
        RunEvent::Failed { run_id, failure } => SessionEvent::Failed {
            run_id: run_id.get(),
            failure: RunFailureSnapshot::from(&failure),
        },
        RunEvent::Cancelled { run_id } => SessionEvent::Cancelled {
            run_id: run_id.get(),
        },
    }
}

fn map_harness_error(error: HarnessError) -> SessionCommandError {
    match error {
        HarnessError::Busy => SessionCommandError::Busy,
        HarnessError::NotRunning => SessionCommandError::NotRunning,
        HarnessError::RunIdExhausted => SessionCommandError::RunIdExhausted,
    }
}

fn map_projection_error(error: ProjectionError) -> SessionCommandError {
    SessionCommandError::Projection {
        message: error.to_string(),
    }
}

fn map_store_error(error: SessionStoreError) -> SessionCommandError {
    SessionCommandError::Persistence {
        message: error.to_string(),
    }
}

fn map_job_error(error: JobRegistryError) -> SessionCommandError {
    match error {
        JobRegistryError::MalformedId => SessionCommandError::InvalidJobId { id: String::new() },
        JobRegistryError::NotFound(id) => SessionCommandError::JobNotFound { id: id.to_string() },
        error => SessionCommandError::Job {
            message: error.to_string(),
        },
    }
}

#[cfg(unix)]
fn cwd_path(cwd: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(cwd.to_vec()))
}

#[cfg(not(unix))]
fn cwd_path(cwd: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(cwd).into_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use futures::stream;
    use tokio::sync::{mpsc, oneshot};

    use super::{
        ConnectionId, SessionActorOutcome, SessionHandle, TerminalCompletion,
        send_terminal_after_run,
    };
    use crate::{
        backend::ActivityTracker,
        harness::{EngineEvent, RunEngine, RunFailure, RunRequest, RunStream},
        runtime::rig::{ActiveModel, ActiveReasoning, ReasoningLevel},
        session::{
            AttachmentId, MaterializeSession, ModelCatalogState, PlanItem, PlanStatus,
            SessionEngineBundle, SessionProjection, SessionRepository, SessionSettings,
            SessionStore, TranscriptItem, TurnStatus, fallback_title,
        },
        tools::{JobRegistry, PlanToolError, UpdatePlanArgs, plan_update_channel},
    };

    type ControlledSender =
        Arc<Mutex<Option<mpsc::UnboundedSender<Result<EngineEvent, RunFailure>>>>>;

    #[derive(Clone, Default)]
    struct ControlledEngine {
        stream: ControlledSender,
    }

    impl ControlledEngine {
        fn emit(&self, event: EngineEvent) {
            self.stream
                .lock()
                .unwrap()
                .as_ref()
                .expect("a stream must be active")
                .send(Ok(event))
                .expect("the stream must still be polled");
        }
    }

    impl RunEngine for ControlledEngine {
        fn start(&self, _request: RunRequest) -> RunStream {
            let (sender, receiver) = mpsc::unbounded_channel();
            *self.stream.lock().unwrap() = Some(sender);
            Box::pin(stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            }))
        }
    }

    struct RunReturnProbe(Arc<AtomicBool>);

    impl Drop for RunReturnProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn spawn_wrapper_sends_terminal_completion_after_run_returns() {
        let returned = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&returned);
        let (response, response_rx) = oneshot::channel();
        let run = async move {
            let _probe = RunReturnProbe(observed);
            Some(TerminalCompletion {
                response,
                outcome: SessionActorOutcome::Deleted,
            })
        };

        send_terminal_after_run(run).await;

        assert!(returned.load(Ordering::Acquire));
        assert_eq!(
            response_rx.await.unwrap().unwrap(),
            SessionActorOutcome::Deleted
        );
    }

    #[tokio::test]
    async fn sequence_exhaustion_detaches_observers_without_losing_completion() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
            .await
            .unwrap()
            .store;
        let settings = SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Medium,
            context_tokens: 0,
        };
        let mut record = store
            .materialize(MaterializeSession {
                cwd: b"/work/moh".to_vec(),
                title: fallback_title("seed durable actor"),
                settings: settings.clone(),
                prompt: "seed durable actor".into(),
                run_id: 41,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        record.turns[0].status = TurnStatus::Completed;
        store.checkpoint(record.clone()).await.unwrap();
        let mut projection =
            SessionProjection::from_record(record.clone(), ModelCatalogState::Loading);
        projection.exhaust_sequence_for_test();
        let engine = ControlledEngine::default();
        let (_, plans) = plan_update_channel();
        let bundle = SessionEngineBundle {
            engine: engine.clone(),
            active_model: ActiveModel::new(settings.model),
            active_reasoning: ActiveReasoning::new(settings.reasoning),
            jobs: JobRegistry::new(),
            plans,
        };
        let repository: Arc<dyn SessionRepository> = Arc::new(store.clone());
        let handle = SessionHandle::spawn(
            repository,
            record.clone(),
            projection,
            bundle,
            ActivityTracker::new(),
        );
        let mut exhausted = handle
            .attach(ConnectionId(1), AttachmentId(1))
            .await
            .unwrap();

        assert_eq!(
            handle
                .submit("persist despite overflow".into())
                .await
                .unwrap(),
            0
        );
        assert_eq!(exhausted.events.recv().await, None);
        engine.emit(EngineEvent::Completed("durable answer".into()));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if store.load(record.id).await.unwrap().history.len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completion was not checkpointed");
        let later = handle
            .attach(ConnectionId(2), AttachmentId(2))
            .await
            .unwrap();
        assert!(!later.snapshot.busy);
        assert_eq!(later.snapshot.sequence, u64::MAX);
        assert!(later.snapshot.transcript.iter().any(|item| {
            matches!(item, TranscriptItem::Assistant(response) if response == "durable answer")
        }));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_sequence_rejects_plan_update_without_checkpointing_live_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
            .await
            .unwrap()
            .store;
        let settings = SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Medium,
            context_tokens: 0,
        };
        let record = store
            .materialize(MaterializeSession {
                cwd: b"/work/moh".to_vec(),
                title: fallback_title("initial prompt"),
                settings: settings.clone(),
                prompt: "initial prompt".into(),
                run_id: 0,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let mut projection =
            SessionProjection::from_record(record.clone(), ModelCatalogState::Loading);
        projection.exhaust_sequence_for_test();
        let engine = ControlledEngine::default();
        let (plans, receiver) = plan_update_channel();
        let bundle = SessionEngineBundle {
            engine,
            active_model: ActiveModel::new(settings.model),
            active_reasoning: ActiveReasoning::new(settings.reasoning),
            jobs: JobRegistry::new(),
            plans: receiver,
        };
        let repository: Arc<dyn SessionRepository> = Arc::new(store.clone());
        let handle = SessionHandle::spawn(
            repository,
            record.clone(),
            projection,
            bundle,
            ActivityTracker::new(),
        );

        let result = plans
            .replace(UpdatePlanArgs {
                explanation: None,
                plan: vec![PlanItem::parse("Verify", PlanStatus::InProgress).unwrap()],
            })
            .await;
        assert!(matches!(result, Err(PlanToolError::Runtime)));
        assert!(handle.snapshot().await.unwrap().plan.is_empty());
        assert!(store.load(record.id).await.unwrap().plan.is_empty());

        handle.shutdown().await.unwrap();
    }
}
