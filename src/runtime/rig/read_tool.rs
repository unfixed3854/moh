//! Rig adapter for the async text-file read service.

use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
use schemars::schema_for;

use crate::tools::{ReadArgs, ReadService, ReadToolError};

pub(super) const RUNTIME_ERROR_CODE: &str = "MOH_READ_RUNTIME";

/// Rig tool adapter for one cwd-bound async read service.
#[derive(Clone)]
pub struct RigReadTool {
    service: ReadService,
}

impl RigReadTool {
    /// Wraps a cwd-bound async read service for Rig.
    pub fn new(service: ReadService) -> Self {
        Self { service }
    }
}

impl PortableTool for RigReadTool {
    const NAME: &'static str = "read";

    type Error = ReadToolError;
    type Args = ReadArgs;
    type Output = ToolOutput;

    fn description(&self) -> String {
        ReadService::description().to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(ReadArgs)).expect("derived tool schema must serialize")
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            ReadToolError::Worker(error) => ToolExecutionError::other("read tool worker failed")
                .with_code(RUNTIME_ERROR_CODE)
                .with_source(error),
            error => ToolExecutionError::from_error(error),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.service.read(args).await
    }
}
