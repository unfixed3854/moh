//! Rig adapter for cwd-bound Bash execution.

use std::sync::Arc;

use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
use schemars::schema_for;
use thiserror::Error;

use crate::tools::{BashArgs, BashService, BashToolError};

pub(super) const RUNTIME_ERROR_CODE: &str = "MOH_BASH_RUNTIME";

/// Errors raised while adapting Bash execution to Rig.
#[derive(Debug, Error)]
pub enum RigBashError {
    /// An error returned by the Bash service.
    #[error(transparent)]
    Domain(#[from] BashToolError),
}

/// Rig adapter for one cwd-bound Bash service.
#[derive(Clone)]
pub struct RigBashTool {
    service: Arc<BashService>,
}

impl RigBashTool {
    /// Wraps a cwd-bound Bash service for Rig.
    pub fn new(service: BashService) -> Self {
        Self {
            service: Arc::new(service),
        }
    }
}

impl PortableTool for RigBashTool {
    const NAME: &'static str = "bash";
    type Error = RigBashError;
    type Args = BashArgs;
    type Output = ToolOutput;

    fn description(&self) -> String {
        BashService::description().to_owned()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(BashArgs)).expect("derived tool schema must serialize")
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            RigBashError::Domain(error @ (BashToolError::Runtime | BashToolError::Output)) => {
                ToolExecutionError::other(error.to_string())
                    .with_code(RUNTIME_ERROR_CODE)
                    .with_source(error)
            }
            RigBashError::Domain(error) => {
                ToolExecutionError::other(error.to_string()).with_source(error)
            }
        }
    }
    async fn call(&self, args: BashArgs) -> Result<Self::Output, Self::Error> {
        self.service.bash(args).await.map_err(RigBashError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::{RUNTIME_ERROR_CODE, RigBashError, RigBashTool};
    use crate::tools::{BashServiceFactory, BashToolError, JobRegistry};
    use rig::tool::PortableTool;

    #[test]
    fn registry_runtime_error_has_fatal_runtime_code() {
        let tool =
            RigBashTool::new(BashServiceFactory::new(JobRegistry::new()).for_cwd(".".into()));
        let mapped = tool.map_error(RigBashError::Domain(BashToolError::Runtime));
        assert_eq!(mapped.code(), Some(RUNTIME_ERROR_CODE));
    }

    #[test]
    fn output_error_has_fatal_runtime_code_but_busy_does_not() {
        let tool =
            RigBashTool::new(BashServiceFactory::new(JobRegistry::new()).for_cwd(".".into()));
        let output = tool.map_error(RigBashError::Domain(BashToolError::Output));
        assert_eq!(output.code(), Some(RUNTIME_ERROR_CODE));
        let busy = tool.map_error(RigBashError::Domain(BashToolError::Busy));
        assert_eq!(busy.code(), None);
    }
}
