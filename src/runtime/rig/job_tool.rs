//! Rig adapters for shared job lifecycle operations.

use crate::tools::{JobCancelArgs, JobService, JobStatusArgs, JobToolError, JobWaitArgs};
use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
use schemars::schema_for;
use std::sync::Arc;

pub(super) const RUNTIME_ERROR_CODE: &str = "MOH_JOB_RUNTIME";

fn map_error(error: JobToolError) -> ToolExecutionError {
    match error {
        JobToolError::Runtime => {
            ToolExecutionError::other("job registry is unavailable").with_code(RUNTIME_ERROR_CODE)
        }
        error => ToolExecutionError::other(error.to_string()).with_source(error),
    }
}

macro_rules! job_tool {
    ($name:ident, $docs:literal, $tool_name:literal, $args:ty, $description:literal, $method:ident) => {
        #[derive(Clone)]
        #[doc = $docs]
        pub struct $name {
            service: Arc<JobService>,
        }
        impl $name {
            /// Wraps the shared job lifecycle service for Rig.
            pub fn new(service: Arc<JobService>) -> Self {
                Self { service }
            }
        }
        impl PortableTool for $name {
            const NAME: &'static str = $tool_name;
            type Error = JobToolError;
            type Args = $args;
            type Output = ToolOutput;
            fn description(&self) -> String {
                $description.to_owned()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::to_value(schema_for!($args))
                    .expect("derived tool schema must serialize")
            }
            fn map_error(&self, error: Self::Error) -> ToolExecutionError {
                map_error(error)
            }
            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                self.service.$method(args).await
            }
        }
    };
}

job_tool!(
    RigJobStatusTool,
    "Rig adapter for job status queries.",
    "job_status",
    JobStatusArgs,
    "List retained jobs or show one job's current status.",
    status
);
job_tool!(
    RigJobWaitTool,
    "Rig adapter for waiting on jobs.",
    "job_wait",
    JobWaitArgs,
    "Wait for one or more jobs to reach a terminal state.",
    wait
);
job_tool!(
    RigJobCancelTool,
    "Rig adapter for cancelling jobs.",
    "job_cancel",
    JobCancelArgs,
    "Cancel a running job and return its final status.",
    cancel
);

#[cfg(test)]
mod tests {
    use super::{RUNTIME_ERROR_CODE, map_error};
    use crate::tools::JobToolError;
    #[test]
    fn registry_runtime_error_has_fatal_runtime_code() {
        assert_eq!(
            map_error(JobToolError::Runtime).code(),
            Some(RUNTIME_ERROR_CODE)
        );
    }
}
