//! Rig adapter for asynchronous hash-anchored edits.

use std::sync::Arc;

use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
use schemars::schema_for;
use thiserror::Error;

use crate::tools::{EditArgs, EditService, EditToolError};

pub(super) const RUNTIME_ERROR_CODE: &str = "MOH_EDIT_RUNTIME";

/// Errors raised while adapting an edit service to Rig.
#[derive(Debug, Error)]
pub enum RigEditError {
    /// A model-visible domain error returned by the edit service.
    #[error(transparent)]
    Domain(#[from] EditToolError),
    /// Tokio could not join the blocking worker that performed the edit.
    #[error("[E_RUNTIME] edit tool worker failed")]
    Runtime(#[source] tokio::task::JoinError),
}

/// Rig tool adapter for asynchronous edits.
#[derive(Clone)]
pub struct RigEditTool {
    service: Arc<EditService>,
}

impl RigEditTool {
    /// Wraps a cwd-bound edit service for Rig.
    pub fn new(service: EditService) -> Self {
        Self {
            service: Arc::new(service),
        }
    }
}

impl PortableTool for RigEditTool {
    const NAME: &'static str = "edit";

    type Error = RigEditError;
    type Args = EditArgs;
    type Output = ToolOutput;

    fn description(&self) -> String {
        EditService::description().to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(EditArgs)).expect("derived tool schema must serialize")
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            RigEditError::Domain(error) => ToolExecutionError::from_error(error),
            RigEditError::Runtime(error) => ToolExecutionError::other("edit tool worker failed")
                .with_code(RUNTIME_ERROR_CODE)
                .with_source(error),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.service.edit(args).await.map_err(|error| match error {
            EditToolError::Worker(source) => RigEditError::Runtime(source),
            error => RigEditError::Domain(error),
        })
    }
}
