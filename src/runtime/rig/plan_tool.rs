//! Rig adapter for authoritative execution-plan replacements.

use rig::tool::{PortableTool, ToolExecutionError, ToolOutput};
use schemars::schema_for;
use thiserror::Error;

use crate::tools::{PlanToolError, PlanUpdateClient, UpdatePlanArgs};

pub(super) const RUNTIME_ERROR_CODE: &str = "MOH_PLAN_RUNTIME";

/// Errors raised while adapting plan replacements to Rig.
#[derive(Debug, Error)]
pub enum RigUpdatePlanError {
    /// An error returned by the authoritative plan-update client.
    #[error(transparent)]
    Domain(#[from] PlanToolError),
}

/// Rig tool adapter for an actor-owned execution plan.
#[derive(Clone)]
pub struct RigUpdatePlanTool {
    plans: PlanUpdateClient,
}

impl RigUpdatePlanTool {
    /// Wraps the client for one session's authoritative plan actor.
    pub fn new(plans: PlanUpdateClient) -> Self {
        Self { plans }
    }
}

impl PortableTool for RigUpdatePlanTool {
    const NAME: &'static str = "update_plan";

    type Error = RigUpdatePlanError;
    type Args = UpdatePlanArgs;
    type Output = ToolOutput;

    fn description(&self) -> String {
        UpdatePlanArgs::description().to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(UpdatePlanArgs))
            .expect("derived tool schema must serialize")
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            RigUpdatePlanError::Domain(error @ PlanToolError::Runtime) => {
                ToolExecutionError::other("plan tool state is unavailable")
                    .with_code(RUNTIME_ERROR_CODE)
                    .with_source(error)
            }
            RigUpdatePlanError::Domain(error) => {
                ToolExecutionError::other(error.to_string()).with_source(error)
            }
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.plans
            .replace(args)
            .await
            .map(|outcome| ToolOutput::text(outcome.render()))
            .map_err(RigUpdatePlanError::from)
    }
}

#[cfg(test)]
mod tests {
    use rig::tool::PortableTool;

    use super::{RUNTIME_ERROR_CODE, RigUpdatePlanError, RigUpdatePlanTool};
    use crate::tools::{PlanToolError, plan_update_channel};

    #[test]
    fn unavailable_actor_has_the_runtime_failure_code() {
        let (client, _receiver) = plan_update_channel();
        let tool = RigUpdatePlanTool::new(client);

        assert_eq!(
            tool.map_error(RigUpdatePlanError::Domain(PlanToolError::Runtime))
                .code(),
            Some(RUNTIME_ERROR_CODE)
        );
    }
}
