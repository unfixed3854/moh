//! Binary-private session boundary consumed by the terminal presentation.

use std::{error::Error, fmt, future::Future, pin::Pin};

use moh::{
    rpc::client::{RpcClientError, SessionUpdate},
    runtime::rig::ReasoningLevel,
    session::{
        DraftDefaults, ErrorCode, JobSnapshotDto, ModelCatalogState, SessionCommandError,
        SessionId, SessionListScope, SessionSelector, SessionSettings, SessionSnapshot,
        SessionSummary, SessionTitle,
    },
};

/// Startup intent passed from the CLI composition root to the workspace controller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LaunchMode {
    Startup,
    NewDraft,
    Session(SessionSelector),
}

/// Ephemeral chat state that has not created durable session identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DraftState {
    pub(crate) cwd: Vec<u8>,
    pub(crate) settings: SessionSettings,
    pub(crate) catalog: ModelCatalogState,
}

impl From<DraftDefaults> for DraftState {
    fn from(defaults: DraftDefaults) -> Self {
        Self {
            cwd: defaults.cwd,
            settings: defaults.settings,
            catalog: defaults.catalog,
        }
    }
}

/// The one chat currently presented by the terminal client.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChatProjection {
    Draft(DraftState),
    Session(Box<SessionSnapshot>),
}

impl ChatProjection {
    pub(crate) fn session(snapshot: SessionSnapshot) -> Self {
        Self::Session(Box::new(snapshot))
    }
}

/// A controller update that distinguishes ordinary session events from deletion fallback.
#[allow(dead_code)] // Consumed by the draft-aware event loop in Task 12.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WorkspaceUpdate {
    Session(SessionUpdate),
    Deleted { session_id: SessionId, cwd: Vec<u8> },
    Warning(String),
}

pub(crate) type SessionListFuture =
    Pin<Box<dyn Future<Output = Result<Vec<SessionSummary>, ClientSessionError>> + 'static>>;

/// A sanitized client-session failure safe to surface after terminal restoration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClientSessionError {
    message: String,
    backend_starting: bool,
}

impl ClientSessionError {
    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            backend_starting: false,
        }
    }

    pub(crate) fn is_backend_starting(&self) -> bool {
        self.backend_starting
    }

    #[cfg(test)]
    pub(crate) fn scripted(message: impl Into<String>) -> Self {
        Self::message(message)
    }
}

/// Workspace operations consumed by draft-aware terminal presentation.
#[allow(dead_code)] // Task 11 defines the boundary before Task 12 switches the event loop to it.
pub(crate) trait WorkspaceClient {
    fn current_projection(&self) -> &ChatProjection;
    async fn next_update(&mut self) -> Result<WorkspaceUpdate, ClientSessionError>;
    async fn submit(&mut self, prompt: &str) -> Result<u64, ClientSessionError>;
    async fn cancel(&self) -> Result<(), ClientSessionError>;
    async fn select_model(&mut self, model: String) -> Result<(), ClientSessionError>;
    async fn select_reasoning(
        &mut self,
        reasoning: ReasoningLevel,
    ) -> Result<(), ClientSessionError>;
    async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, ClientSessionError>;
    async fn cancel_job(&self, id: String) -> Result<JobSnapshotDto, ClientSessionError>;
    async fn new_draft(&mut self) -> Result<(), ClientSessionError>;
    fn list_sessions(&self, scope: SessionListScope) -> SessionListFuture;
    async fn switch_session(&mut self, id: SessionId) -> Result<(), ClientSessionError>;
    async fn rename_session(
        &self,
        id: SessionId,
        title: SessionTitle,
    ) -> Result<(), ClientSessionError>;
    async fn delete_session(&mut self, id: SessionId) -> Result<(), ClientSessionError>;
    async fn startup_fallback(&mut self, cwd: Vec<u8>) -> Result<(), ClientSessionError>;
}

impl fmt::Display for ClientSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ClientSessionError {}

impl From<RpcClientError> for ClientSessionError {
    fn from(error: RpcClientError) -> Self {
        let backend_starting = matches!(
            &error,
            RpcClientError::Command(SessionCommandError::Reported {
                code: ErrorCode::BackendStarting,
                ..
            })
        );
        Self {
            message: error.to_string(),
            backend_starting,
        }
    }
}
