//! Authoritative reduction of durable session records and live session events.

use thiserror::Error;

use super::{
    ActiveRunSnapshot, JobSnapshotDto, ModelCatalogState, PlanItem, SessionEvent,
    SessionEventEnvelope, SessionRecord, SessionSettings, SessionSnapshot, SessionSummary,
    TranscriptItem,
};

/// An invalid attempt to reduce a session event.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProjectionError {
    /// The event sequence cannot advance without overflowing.
    #[error("session event sequence is exhausted")]
    SequenceExhausted,
    /// A run-specific event arrived when no run is active.
    #[error("received run event {received} without an active run")]
    NoActiveRun {
        /// The event's harness run identifier.
        received: u64,
    },
    /// A run-specific event belongs to a different active run.
    #[error("received run event {received} for active run {active}")]
    RunIdMismatch {
        /// The currently active harness run identifier.
        active: u64,
        /// The event's harness run identifier.
        received: u64,
    },
    /// A new run was received while another run is active.
    #[error("received new run while run {active} is active")]
    RunAlreadyActive {
        /// The currently active harness run identifier.
        active: u64,
    },
}

/// The complete in-memory state required to make an attachment snapshot.
#[derive(Clone, Debug)]
pub struct SessionProjection {
    summary: SessionSummary,
    transcript: Vec<TranscriptItem>,
    active_run: Option<ActiveRunSnapshot>,
    settings: SessionSettings,
    catalog: ModelCatalogState,
    plan: Vec<PlanItem>,
    persistence_warning: Option<String>,
    sequence: u64,
}

impl SessionProjection {
    /// Reconstructs a live projection from the durable transcript and settings.
    pub fn from_record(record: SessionRecord, catalog: ModelCatalogState) -> Self {
        Self {
            summary: SessionSummary {
                id: record.id,
                title: record.title,
                title_revision: record.title_revision,
                cwd_display: String::from_utf8_lossy(&record.cwd).into_owned(),
                cwd: record.cwd,
                running_jobs: 0,
                running: false,
                busy: false,
                attached_clients: 0,
                last_activity: record.last_activity,
            },
            transcript: record.transcript,
            active_run: None,
            settings: record.settings,
            catalog,
            plan: record.plan,
            persistence_warning: None,
            sequence: 0,
        }
    }

    /// Returns an authoritative attachment snapshot with caller-supplied registry jobs.
    pub fn snapshot(&self, jobs: Vec<JobSnapshotDto>) -> SessionSnapshot {
        let busy = self.active_run.is_some();
        debug_assert_eq!(self.summary.busy, busy);
        SessionSnapshot {
            summary: self.summary.clone(),
            transcript: self.transcript.clone(),
            active_run: self.active_run.clone(),
            settings: self.settings.clone(),
            catalog: self.catalog.clone(),
            plan: self.plan.clone(),
            jobs,
            persistence_warning: self.persistence_warning.clone(),
            sequence: self.sequence,
            busy,
        }
    }

    #[cfg(test)]
    pub(crate) fn exhaust_sequence_for_test(&mut self) {
        self.sequence = u64::MAX;
    }

    /// Validates and atomically applies one state change, assigning its next sequence.
    pub fn apply(&mut self, event: SessionEvent) -> Result<SessionEventEnvelope, ProjectionError> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(ProjectionError::SequenceExhausted)?;
        self.validate_event(&event)?;

        self.sequence = sequence;
        self.reduce(&event);

        Ok(SessionEventEnvelope { sequence, event })
    }

    pub(crate) fn apply_unsequenced(
        &mut self,
        event: &SessionEvent,
    ) -> Result<(), ProjectionError> {
        self.validate_event(event)?;
        self.reduce(event);
        Ok(())
    }

    pub(crate) fn install_persisted_started(
        &mut self,
        run_id: u64,
        prompt: String,
    ) -> Result<(), ProjectionError> {
        self.validate_event(&SessionEvent::Started {
            run_id,
            prompt: prompt.clone(),
        })?;
        self.active_run = Some(ActiveRunSnapshot {
            run_id,
            prompt,
            assistant_text: String::new(),
        });
        self.summary.busy = true;
        Ok(())
    }

    fn reduce(&mut self, event: &SessionEvent) {
        match &event {
            SessionEvent::TitleChanged {
                title,
                title_revision,
            } => {
                self.summary.title = title.clone();
                self.summary.title_revision = *title_revision;
            }
            SessionEvent::Started { run_id, prompt } => {
                self.transcript.push(TranscriptItem::User(prompt.clone()));
                self.active_run = Some(ActiveRunSnapshot {
                    run_id: *run_id,
                    prompt: prompt.clone(),
                    assistant_text: String::new(),
                });
                self.summary.busy = true;
            }
            SessionEvent::AssistantDelta { text, .. } => {
                self.active_run
                    .as_mut()
                    .expect("validated active run")
                    .assistant_text
                    .push_str(text);
            }
            SessionEvent::ContextUsage {
                input_tokens,
                last_activity,
                ..
            } => {
                self.settings.context_tokens = *input_tokens;
                self.summary.last_activity = *last_activity;
            }
            SessionEvent::ToolStarted {
                run_id,
                call_id,
                name,
                arguments,
            } => self.transcript.push(TranscriptItem::ToolStarted {
                run_id: *run_id,
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            SessionEvent::ToolFinished { .. } => {}
            SessionEvent::Completed {
                response,
                last_activity,
                ..
            } => {
                self.transcript
                    .push(TranscriptItem::Assistant(response.clone()));
                self.active_run = None;
                self.summary.busy = false;
                self.summary.last_activity = *last_activity;
            }
            SessionEvent::Failed { run_id, failure } => {
                self.transcript.push(TranscriptItem::Failed {
                    run_id: *run_id,
                    failure: failure.clone(),
                });
                self.active_run = None;
                self.summary.busy = false;
            }
            SessionEvent::Cancelled { run_id } => {
                self.transcript
                    .push(TranscriptItem::Cancelled { run_id: *run_id });
                self.active_run = None;
                self.summary.busy = false;
            }
            SessionEvent::SettingsChanged {
                settings,
                last_activity,
            } => {
                self.settings = settings.clone();
                self.summary.last_activity = *last_activity;
            }
            SessionEvent::PlanChanged(plan) => self.plan.clone_from(plan),
            SessionEvent::JobsChanged(_) => {}
            SessionEvent::CatalogChanged(catalog) => self.catalog = catalog.clone(),
            SessionEvent::PersistenceWarning(warning) => {
                self.persistence_warning = warning.clone();
            }
            SessionEvent::Deleted { .. } => {}
        }
    }

    fn validate_event(&self, event: &SessionEvent) -> Result<(), ProjectionError> {
        match event {
            SessionEvent::Started { .. } => match &self.active_run {
                Some(active_run) => Err(ProjectionError::RunAlreadyActive {
                    active: active_run.run_id,
                }),
                None => Ok(()),
            },
            SessionEvent::AssistantDelta { run_id, .. }
            | SessionEvent::ContextUsage { run_id, .. }
            | SessionEvent::ToolStarted { run_id, .. }
            | SessionEvent::ToolFinished { run_id, .. }
            | SessionEvent::Completed { run_id, .. }
            | SessionEvent::Failed { run_id, .. }
            | SessionEvent::Cancelled { run_id } => self.validate_run_id(*run_id),
            SessionEvent::TitleChanged { .. }
            | SessionEvent::SettingsChanged { .. }
            | SessionEvent::PlanChanged(_)
            | SessionEvent::JobsChanged(_)
            | SessionEvent::CatalogChanged(_)
            | SessionEvent::PersistenceWarning(_)
            | SessionEvent::Deleted { .. } => Ok(()),
        }
    }

    fn validate_run_id(&self, received: u64) -> Result<(), ProjectionError> {
        match &self.active_run {
            Some(active_run) if active_run.run_id == received => Ok(()),
            Some(active_run) => Err(ProjectionError::RunIdMismatch {
                active: active_run.run_id,
                received,
            }),
            None => Err(ProjectionError::NoActiveRun { received }),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{ProjectionError, SessionProjection};
    use crate::{
        runtime::rig::ReasoningLevel,
        session::{ModelCatalogState, SessionEvent, SessionId, SessionRecord, SessionSettings},
    };

    fn record() -> SessionRecord {
        SessionRecord {
            id: SessionId::from_stored(1),
            title: crate::session::fallback_title(""),
            title_source: crate::session::TitleSource::Fallback,
            title_revision: 0,
            cwd: b"/work/moh".to_vec(),
            settings: SessionSettings {
                model: "gpt-5.6-terra".into(),
                reasoning: ReasoningLevel::Medium,
                context_tokens: 12,
            },
            transcript: vec![],
            turns: vec![],
            history: vec![],
            plan: vec![],
            created_at: Utc.with_ymd_and_hms(2026, 8, 26, 9, 0, 0).unwrap(),
            last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 9, 1, 0).unwrap(),
        }
    }

    #[test]
    fn exhausted_sequence_rejects_event_without_mutating_snapshot() {
        let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);
        projection.sequence = u64::MAX;
        let before = projection.snapshot(vec![]);

        let error = projection
            .apply(SessionEvent::PersistenceWarning(Some(
                "checkpoint failed".into(),
            )))
            .unwrap_err();

        assert_eq!(error, ProjectionError::SequenceExhausted);
        assert_eq!(projection.snapshot(vec![]), before);
    }
}
