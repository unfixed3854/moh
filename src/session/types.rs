//! Stable session-domain values shared by storage and higher-level actors.

use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{SessionTitle, TitleSource};
use crate::{
    harness::{Message, RunFailure, RunFailureKind, RunStage},
    runtime::rig::ReasoningLevel,
    tools::{JobKind, JobSnapshot, JobState},
};

const SESSION_ID_PREFIX: &str = "session-";
const MAX_SESSION_NAME_SCALARS: usize = 64;
const MAX_PLAN_STEP_SCALARS: usize = 256;

/// One of the five canonical states for an ordered execution-plan step.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// The step has not started.
    Pending,
    /// The one step currently being worked on.
    InProgress,
    /// The step finished successfully.
    Completed,
    /// The step cannot continue without outside resolution.
    Blocked,
    /// The step was intentionally abandoned.
    Cancelled,
}

impl PlanStatus {
    /// Returns this status in its canonical model-visible spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }
}

impl FromStr for PlanStatus {
    type Err = PlanStatusParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "blocked" => Ok(Self::Blocked),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(PlanStatusParseError),
        }
    }
}

/// A status string that is not one of the five canonical plan statuses.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("plan statuses must use one of the five canonical names")]
pub struct PlanStatusParseError;

/// One validated, ordered execution-plan step.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    /// One ordered plan step.
    step: String,
    /// Current status of this plan step.
    status: PlanStatus,
}

impl PlanItem {
    /// Validates and owns plan-step text with its current status.
    pub fn parse(step: impl Into<String>, status: PlanStatus) -> Result<Self, PlanItemError> {
        let item = Self {
            step: step.into(),
            status,
        };
        item.validate()?;
        Ok(item)
    }

    /// Returns the validated step text.
    pub fn step(&self) -> &str {
        &self.step
    }

    /// Returns the step's canonical status.
    pub const fn status(&self) -> PlanStatus {
        self.status
    }

    /// Validates text received through a derived deserializer.
    pub fn validate(&self) -> Result<(), PlanItemError> {
        let scalar_count = self.step.chars().count();
        if scalar_count == 0
            || scalar_count > MAX_PLAN_STEP_SCALARS
            || self.step.trim() != self.step
            || self.step.chars().any(char::is_control)
        {
            return Err(PlanItemError);
        }
        Ok(())
    }
}

/// Plan-step text that is empty, malformed, or outside the supported bounds.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("plan steps must contain 1-256 trimmed Unicode scalars without controls")]
pub struct PlanItemError;

/// Identifies one observer registration within a session actor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AttachmentId(pub u64);

/// A globally stable session identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(u64);

impl SessionId {
    pub(crate) fn from_stored(value: u64) -> Self {
        Self(value)
    }

    /// Returns the positive numeric portion of this identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{SESSION_ID_PREFIX}{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = SessionIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let number = value
            .strip_prefix(SESSION_ID_PREFIX)
            .filter(|number| !number.is_empty())
            .ok_or(SessionIdParseError)?;
        if number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SessionIdParseError);
        }
        let number = number.parse::<u64>().map_err(|_| SessionIdParseError)?;
        if number == 0 {
            return Err(SessionIdParseError);
        }
        Ok(Self(number))
    }
}

/// A malformed or non-canonical stable session identifier.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("session identifiers must use the canonical session-N form")]
pub struct SessionIdParseError;

/// A user-selected session name scoped to one working directory.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionName(String);

impl SessionName {
    /// Validates and owns a session name.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionNameParseError> {
        let value = value.into();
        let scalar_count = value.chars().count();
        if scalar_count == 0
            || scalar_count > MAX_SESSION_NAME_SCALARS
            || value.trim() != value
            || value.chars().any(char::is_control)
            || value.starts_with(SESSION_ID_PREFIX)
        {
            return Err(SessionNameParseError);
        }
        Ok(Self(value))
    }

    /// Returns the validated name text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A session name that violates the length or namespace rules.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error(
    "session names must contain 1-64 scalars without surrounding whitespace, controls, or the session- prefix"
)]
pub struct SessionNameParseError;

/// A stable session lookup selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionSelector {
    /// Resolve globally by stable identifier.
    Id(SessionId),
    /// Resolve an exact display title within the supplied working directory.
    Title(SessionTitle),
}

impl fmt::Display for SessionSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => id.fmt(formatter),
            Self::Title(title) => title.fmt(formatter),
        }
    }
}

/// Durable model settings and latest context usage for a session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSettings {
    /// Provider model identifier.
    pub model: String,
    /// Requested reasoning effort.
    pub reasoning: ReasoningLevel,
    /// Latest known input-token context usage.
    pub context_tokens: u64,
}

/// Persistence scope used when listing durable sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionListScope {
    /// Sessions associated with one canonical working directory.
    Project(Vec<u8>),
    /// Every persisted session, regardless of working directory.
    All,
}

/// Complete durable state needed to atomically accept a first prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializeSession {
    /// Canonical working-directory bytes.
    pub cwd: Vec<u8>,
    /// Initial fallback title derived from the prompt.
    pub title: SessionTitle,
    /// Initial model settings.
    pub settings: SessionSettings,
    /// First accepted user prompt.
    pub prompt: String,
    /// Harness run identifier for the first turn.
    pub run_id: u64,
    /// Time at which the first prompt was accepted.
    pub created_at: DateTime<Utc>,
}

/// Durable lifecycle state for one submitted turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStatus {
    /// The model request or one of its tools is still active.
    Running,
    /// The turn completed with a committed assistant response.
    Completed,
    /// The turn ended with a run failure.
    Failed,
    /// The user explicitly cancelled the turn.
    Cancelled,
    /// The backend stopped while the turn was running.
    Interrupted,
}

/// Durable identity and prompt linkage for one submitted turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTurn {
    /// Zero-based turn order within the session.
    pub ordinal: u64,
    /// Harness run identifier associated with the turn.
    pub run_id: u64,
    /// Transcript position of the turn's user prompt.
    pub prompt_position: u64,
    /// Latest durable lifecycle state.
    pub status: TurnStatus,
}

/// Complete durable state for one session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecord {
    /// Globally stable identity.
    pub id: SessionId,
    /// Mutable non-unique display title.
    pub title: SessionTitle,
    /// Mechanism that most recently selected the title.
    pub title_source: TitleSource,
    /// Monotonic title revision used by compare-and-set updates.
    pub title_revision: u64,
    /// Canonical working-directory bytes.
    pub cwd: Vec<u8>,
    /// Durable model settings and context usage.
    pub settings: SessionSettings,
    /// Ordered visible transcript, including non-success terminal state.
    pub transcript: Vec<TranscriptItem>,
    /// Submitted turns and their durable lifecycle states.
    pub turns: Vec<DurableTurn>,
    /// Successfully committed user/assistant exchanges in order.
    pub history: Vec<Message>,
    /// Complete ordered execution plan for the session.
    pub plan: Vec<PlanItem>,
    /// Time at which the session identity was created.
    pub created_at: DateTime<Utc>,
    /// Latest durable session activity time.
    pub last_activity: DateTime<Utc>,
}

/// List-facing session state, ready for live actor overlays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    /// Globally stable identity.
    pub id: SessionId,
    /// Mutable non-unique display title.
    pub title: SessionTitle,
    /// Monotonic title revision.
    pub title_revision: u64,
    /// Canonical working-directory bytes.
    pub cwd: Vec<u8>,
    /// Lossy display form of the working directory for non-Rust clients.
    pub cwd_display: String,
    /// Number of active background jobs owned by this session.
    pub running_jobs: u32,
    /// Whether a model request or background job is active.
    pub running: bool,
    /// Whether the live session actor is running a request.
    pub busy: bool,
    /// Number of client attachments currently registered with the live session actor.
    pub attached_clients: u32,
    /// Latest durable or live session activity time.
    pub last_activity: DateTime<Utc>,
}

/// A transport-safe snapshot of a run failure without its error source chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunFailureSnapshot {
    /// The lifecycle stage where the failure occurred.
    pub stage: RunStage,
    /// The model-neutral failure classification.
    pub kind: RunFailureKind,
    /// Whether retrying the run may succeed.
    pub retryable: bool,
    /// The failure's sanitized display message.
    pub message: String,
}

impl From<&RunFailure> for RunFailureSnapshot {
    fn from(failure: &RunFailure) -> Self {
        Self {
            stage: failure.stage(),
            kind: failure.kind().clone(),
            retryable: failure.retryable(),
            message: failure.message().into(),
        }
    }
}

/// A transport-safe copy of a process-local job snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSnapshotDto {
    /// The rendered stable job identifier.
    pub id: String,
    /// The producer family that owns the job.
    pub kind: JobKind,
    /// The job's current lifecycle state.
    pub state: JobState,
    /// The producer-provided job title.
    pub title: String,
    /// The UTC time when the job started.
    pub started_at: DateTime<Utc>,
    /// The UTC time when the job became terminal, if it has.
    pub completed_at: Option<DateTime<Utc>>,
    /// Rendered producer details, without producer implementation state.
    pub details: String,
}

impl From<&JobSnapshot> for JobSnapshotDto {
    fn from(snapshot: &JobSnapshot) -> Self {
        Self {
            id: snapshot.id().to_string(),
            kind: snapshot.kind(),
            state: snapshot.state(),
            title: snapshot.title().into(),
            started_at: snapshot.started_at(),
            completed_at: snapshot.completed_at(),
            details: snapshot.details().render(),
        }
    }
}

/// One selectable model exposed to presentation and transport clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelInfoDto {
    /// Stable model identifier selected by settings and commands.
    pub id: String,
    /// Human-readable model name.
    pub display_name: String,
    /// Human-readable model description.
    pub description: String,
    /// Reasoning levels this model accepts.
    pub reasoning_efforts: Vec<ReasoningLevel>,
    /// The model's default reasoning level, when the catalog provides one.
    pub default_reasoning: Option<ReasoningLevel>,
}

/// The asynchronous model catalog state visible to session clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCatalogState {
    /// The catalog is still being loaded.
    Loading,
    /// The catalog is ready for model selection.
    Ready(Vec<ModelInfoDto>),
    /// Loading failed with a sanitized display message.
    Failed(String),
}

/// Factory-provided values used by a client before its first prompt is durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftDefaults {
    /// Canonical working-directory bytes selected by the client.
    pub cwd: Vec<u8>,
    /// Initial model settings selected for the first prompt.
    pub settings: SessionSettings,
    /// Current model catalog used to validate those settings.
    pub catalog: ModelCatalogState,
}

/// A committed or process-local transcript entry with no presentation details.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    /// A user prompt, whether restored from history or accepted for an active run.
    User(String),
    /// A successfully committed assistant response.
    Assistant(String),
    /// A tool invocation started during a run.
    ToolStarted {
        /// The active harness run identifier.
        run_id: u64,
        /// The engine-provided tool call identifier.
        call_id: String,
        /// The tool name.
        name: String,
        /// Model-provided JSON arguments.
        arguments: serde_json::Value,
    },
    /// A run that ended unsuccessfully.
    Failed {
        /// The active harness run identifier.
        run_id: u64,
        /// Transport-safe failure information.
        failure: RunFailureSnapshot,
    },
    /// A run that was explicitly cancelled.
    Cancelled {
        /// The cancelled harness run identifier.
        run_id: u64,
    },
}

/// Process-local detail for the one currently active harness run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRunSnapshot {
    /// The harness-assigned run identifier.
    pub run_id: u64,
    /// The prompt accepted for this run.
    pub prompt: String,
    /// Assistant deltas accumulated so far, before successful completion.
    pub assistant_text: String,
}

/// An authoritative, presentation-neutral session state sent during attachment.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    /// List-facing session identity and live status.
    pub summary: SessionSummary,
    /// Committed history plus process-local tool and terminal run records.
    pub transcript: Vec<TranscriptItem>,
    /// The current active run, if any.
    pub active_run: Option<ActiveRunSnapshot>,
    /// Current model settings and context use.
    pub settings: SessionSettings,
    /// Current model-catalog state.
    pub catalog: ModelCatalogState,
    /// Complete ordered execution plan for this session.
    pub plan: Vec<PlanItem>,
    /// Point-in-time job-registry snapshots supplied by the session actor.
    pub jobs: Vec<JobSnapshotDto>,
    /// A pending persistence warning, if the latest checkpoint failed.
    pub persistence_warning: Option<String>,
    /// The last event sequence included in this snapshot.
    pub sequence: u64,
    /// Whether an active run is in progress.
    pub busy: bool,
}

/// A typed state change emitted by a session actor.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// The durable display title changed.
    TitleChanged {
        /// The complete title after the change.
        title: SessionTitle,
        /// The monotonic title revision after the change.
        title_revision: u64,
    },
    /// A run was accepted by the harness.
    Started {
        /// The harness-assigned run identifier.
        run_id: u64,
        /// The prompt accepted by the harness.
        prompt: String,
    },
    /// Incremental assistant text for the active run.
    AssistantDelta {
        /// The harness run identifier.
        run_id: u64,
        /// The incremental assistant text.
        text: String,
    },
    /// Input-token usage reported during the active run.
    ContextUsage {
        /// The harness run identifier.
        run_id: u64,
        /// The latest input-token context usage.
        input_tokens: u64,
        /// Durable activity time atomically associated with this usage update.
        last_activity: DateTime<Utc>,
    },
    /// A tool invocation started during the active run.
    ToolStarted {
        /// The harness run identifier.
        run_id: u64,
        /// The engine-provided call identifier.
        call_id: String,
        /// The tool name.
        name: String,
        /// Model-provided tool arguments.
        arguments: serde_json::Value,
    },
    /// A tool invocation finished during the active run.
    ToolFinished {
        /// The harness run identifier.
        run_id: u64,
        /// The engine-provided call identifier.
        call_id: String,
        /// The tool name.
        name: String,
    },
    /// A run completed and committed its final assistant response.
    Completed {
        /// The harness run identifier.
        run_id: u64,
        /// The final assistant response committed by the harness.
        response: String,
        /// Durable activity time atomically associated with this completion.
        last_activity: DateTime<Utc>,
    },
    /// A run failed without committing assistant text.
    Failed {
        /// The harness run identifier.
        run_id: u64,
        /// Transport-safe failure information.
        failure: RunFailureSnapshot,
    },
    /// A run was cancelled without committing assistant text.
    Cancelled {
        /// The harness run identifier.
        run_id: u64,
    },
    /// The selected model settings changed.
    SettingsChanged {
        /// Complete durable settings after the change.
        settings: SessionSettings,
        /// Durable activity time atomically associated with this settings change.
        last_activity: DateTime<Utc>,
    },
    /// The complete ordered execution plan was replaced.
    PlanChanged(Vec<PlanItem>),
    /// The actor's job registry changed.
    JobsChanged(Vec<JobSnapshotDto>),
    /// The model catalog changed.
    CatalogChanged(ModelCatalogState),
    /// A persistence warning became visible or was cleared.
    PersistenceWarning(Option<String>),
    /// The session was durably deleted and this observer stream is terminating.
    Deleted {
        /// The stable identity that was deleted.
        session_id: SessionId,
    },
}

/// A sequenced event emitted after a successful projection reduction.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEventEnvelope {
    /// The strictly monotonic sequence assigned by the session actor.
    pub sequence: u64,
    /// The state change at this sequence.
    pub event: SessionEvent,
}
