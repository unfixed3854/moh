use futures::StreamExt;

use super::{
    EngineEvent, HarnessError, Message, Role, RunContext, RunEngine, RunEvent, RunFailure,
    RunFailureKind, RunId, RunRequest, RunStage, RunStream,
};

/// Owns committed history and the lifecycle of at most one active model run.
pub struct Harness<E> {
    engine: E,
    history: Vec<Message>,
    active: Option<ActiveRun>,
    next_run_id: u64,
    ids_exhausted: bool,
}

struct ActiveRun {
    id: RunId,
    prompt: String,
    stream: RunStream,
}

impl<E: RunEngine> Harness<E> {
    /// Creates an empty harness backed by `engine`.
    pub fn new(engine: E) -> Self {
        Self::with_history(engine, Vec::new())
    }

    /// Creates a harness with existing successful text-only history.
    pub fn with_history(engine: E, history: Vec<Message>) -> Self {
        Self {
            engine,
            history,
            active: None,
            next_run_id: 0,
            ids_exhausted: false,
        }
    }

    /// Starts a run and returns its unpolled [`RunEvent::Started`] event.
    pub fn submit(
        &mut self,
        prompt: impl Into<String>,
        context: RunContext,
    ) -> Result<RunEvent, HarnessError> {
        if self.active.is_some() {
            return Err(HarnessError::Busy);
        }
        if self.ids_exhausted {
            return Err(HarnessError::RunIdExhausted);
        }

        let id = RunId::new(self.next_run_id);
        match self.next_run_id.checked_add(1) {
            Some(next_run_id) => self.next_run_id = next_run_id,
            None => self.ids_exhausted = true,
        }

        let prompt = prompt.into();
        let stream = self.engine.start(RunRequest {
            prompt: prompt.clone(),
            history: self.history.clone(),
            context,
        });
        self.active = Some(ActiveRun { id, prompt, stream });

        Ok(RunEvent::Started { run_id: id })
    }

    /// Awaits and projects the next event for the active run.
    ///
    /// Returns `None` only when no run is active when called.
    pub async fn next_event(&mut self) -> Option<RunEvent> {
        let (id, engine_event) = {
            let active = self.active.as_mut()?;
            (active.id, active.stream.next().await)
        };

        match engine_event {
            Some(Ok(EngineEvent::AssistantDelta(text))) => {
                Some(RunEvent::AssistantDelta { run_id: id, text })
            }
            Some(Ok(EngineEvent::ContextUsage { input_tokens })) => Some(RunEvent::ContextUsage {
                run_id: id,
                input_tokens,
            }),
            Some(Ok(EngineEvent::ToolStarted {
                call_id,
                name,
                arguments,
            })) => Some(RunEvent::ToolStarted {
                run_id: id,
                call_id,
                name,
                arguments,
            }),
            Some(Ok(EngineEvent::ToolFinished { call_id, name })) => Some(RunEvent::ToolFinished {
                run_id: id,
                call_id,
                name,
            }),
            Some(Ok(EngineEvent::Completed(response))) if response.trim().is_empty() => {
                self.active.take();
                Some(RunEvent::Failed {
                    run_id: id,
                    failure: RunFailure::new(
                        RunStage::Finalization,
                        RunFailureKind::EmptyResponse,
                        false,
                        "engine completed with an empty response",
                    ),
                })
            }
            Some(Ok(EngineEvent::Completed(response))) => {
                let active = self.active.take().expect("active run was polled");
                self.history.push(Message::new(Role::User, active.prompt));
                self.history
                    .push(Message::new(Role::Assistant, response.clone()));
                Some(RunEvent::Completed {
                    run_id: id,
                    response,
                })
            }
            Some(Err(failure)) => {
                self.active.take();
                Some(RunEvent::Failed {
                    run_id: id,
                    failure,
                })
            }
            None => {
                self.active.take();
                Some(RunEvent::Failed {
                    run_id: id,
                    failure: RunFailure::new(
                        RunStage::Finalization,
                        RunFailureKind::Protocol,
                        false,
                        "engine stream ended before completion",
                    ),
                })
            }
        }
    }

    /// Cancels the active run after dropping its engine stream.
    pub fn cancel(&mut self) -> Result<RunEvent, HarnessError> {
        let active = self.active.take().ok_or(HarnessError::NotRunning)?;
        let id = active.id;
        drop(active);
        Ok(RunEvent::Cancelled { run_id: id })
    }

    /// Returns the successful text-only history committed by this harness.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Returns whether an engine stream is currently active.
    pub const fn is_running(&self) -> bool {
        self.active.is_some()
    }
}
