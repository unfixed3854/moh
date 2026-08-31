use std::error::Error as StdError;

use thiserror::Error;

type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// The lifecycle stage where a run failure occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStage {
    /// The engine could not be started.
    Startup,
    /// The engine could not complete a model request.
    ModelRequest,
    /// A tool could not execute.
    ToolExecution,
    /// The engine could not produce a valid final response.
    Finalization,
}

/// A model-neutral classification of a run failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunFailureKind {
    /// Authentication could not be established or refreshed.
    Authentication,
    /// A network transport operation failed.
    Transport,
    /// A remote endpoint rejected an HTTP request.
    HttpRejected {
        /// The rejected HTTP status code.
        status: u16,
    },
    /// The engine violated its event-stream protocol.
    Protocol,
    /// The engine completed with a blank response.
    EmptyResponse,
    /// The runtime exhausted its allowed model-call budget.
    BudgetExhausted,
    /// Shared runtime infrastructure failed.
    RuntimeInfrastructure,
    /// Tool-specific infrastructure failed.
    ToolInfrastructure,
}

/// Structured information about an unsuccessful run.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct RunFailure {
    stage: RunStage,
    kind: RunFailureKind,
    retryable: bool,
    message: String,
    #[source]
    source: Option<BoxError>,
}

impl RunFailure {
    /// Creates a failure without an underlying source error.
    pub fn new(
        stage: RunStage,
        kind: RunFailureKind,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            kind,
            retryable,
            message: message.into(),
            source: None,
        }
    }

    /// Attaches an underlying source error to this failure.
    pub fn with_source<E>(mut self, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        self.source = Some(Box::new(source));
        self
    }

    /// Returns the lifecycle stage where the failure occurred.
    pub const fn stage(&self) -> RunStage {
        self.stage
    }

    /// Returns the model-neutral failure classification.
    pub fn kind(&self) -> &RunFailureKind {
        &self.kind
    }

    /// Returns whether retrying this run may succeed.
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Returns the human-readable failure description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Errors returned by harness commands before an engine event is available.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum HarnessError {
    /// A run is already active.
    #[error("a run is already active")]
    Busy,
    /// There is no active run to cancel.
    #[error("there is no active run")]
    NotRunning,
    /// The monotonic run identifier space has been exhausted.
    #[error("run identifier space is exhausted")]
    RunIdExhausted,
}
