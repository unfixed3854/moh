//! Durable session domain types and repository implementations.

mod actor;
mod manager;
mod projection;
mod runtime;
mod store;
mod title;
mod types;

pub use actor::{
    ConnectionId, ErrorCode, SessionActorOutcome, SessionAttachment, SessionCommandError,
    SessionHandle,
};
pub use manager::{
    ManagedSession, MaterializedSession, SessionManagerError, SessionManagerHandle,
    SessionManagerLifecycle, SessionManagerLifecycleError, StartupResult,
};
pub use projection::{ProjectionError, SessionProjection};
pub use runtime::{SessionEngineBundle, SessionEngineFactory};
pub use store::{
    OpenedSessionStore, SessionRepository, SessionStore, SessionStoreError, StoreWarning,
};
pub use title::{
    MAX_SESSION_TITLE_SCALARS, SessionTitle, SessionTitleGenerator, SessionTitleParseError,
    TitleGenerationError, TitleRequest, TitleSource, fallback_title, sanitize_generated_title,
};
pub use types::{
    ActiveRunSnapshot, AttachmentId, DraftDefaults, DurableTurn, JobSnapshotDto,
    MaterializeSession, ModelCatalogState, ModelInfoDto, PlanItem, PlanItemError, PlanStatus,
    PlanStatusParseError, RunFailureSnapshot, SessionEvent, SessionEventEnvelope, SessionId,
    SessionIdParseError, SessionListScope, SessionName, SessionNameParseError, SessionRecord,
    SessionSelector, SessionSettings, SessionSnapshot, SessionSummary, TranscriptItem, TurnStatus,
};
