//! Rig adapter for asynchronous whole-file writes.

use std::sync::Arc;

use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
use schemars::schema_for;
use thiserror::Error;

use crate::tools::{WriteArgs, WriteService, WriteToolError};

pub(super) const RUNTIME_ERROR_CODE: &str = "MOH_WRITE_RUNTIME";

/// Errors raised while adapting a write service to Rig.
#[derive(Debug, Error)]
pub enum RigWriteError {
    /// A model-visible domain error returned by the write service.
    #[error(transparent)]
    Domain(#[from] WriteToolError),
    /// Tokio could not join the blocking worker that performed the write.
    #[error("[E_RUNTIME] write tool worker failed")]
    Runtime(#[source] tokio::task::JoinError),
}

/// Rig tool adapter for asynchronous writes.
#[derive(Clone)]
pub struct RigWriteTool {
    service: Arc<WriteService>,
}

impl RigWriteTool {
    /// Wraps a cwd-bound write service for Rig.
    pub fn new(service: WriteService) -> Self {
        Self {
            service: Arc::new(service),
        }
    }
}

impl PortableTool for RigWriteTool {
    const NAME: &'static str = "write";

    type Error = RigWriteError;
    type Args = WriteArgs;
    type Output = ToolOutput;

    fn description(&self) -> String {
        WriteService::description().to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(WriteArgs)).expect("derived tool schema must serialize")
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            RigWriteError::Domain(error) => ToolExecutionError::from_error(error),
            RigWriteError::Runtime(error) => ToolExecutionError::other("write tool worker failed")
                .with_code(RUNTIME_ERROR_CODE)
                .with_source(error),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.service.write(args).await.map_err(|error| match error {
            WriteToolError::Worker(source) => RigWriteError::Runtime(source),
            error => RigWriteError::Domain(error),
        })
    }
}
