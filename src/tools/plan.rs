//! Strict model-facing replacement requests for session-owned execution plans.

use schemars::JsonSchema;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::session::{PlanItem, PlanStatus};

const PLAN_UPDATE_CHANNEL_CAPACITY: usize = 8;
const MAX_PLAN_ITEMS: usize = 32;

/// Strict arguments accepted by the model-visible `update_plan` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanArgs {
    /// Optional reason for changing the plan.
    pub explanation: Option<String>,
    /// Complete ordered replacement plan.
    #[schemars(length(max = 32))]
    pub plan: Vec<PlanItem>,
}

impl UpdatePlanArgs {
    /// Returns the model-facing update-plan tool description.
    pub fn description() -> &'static str {
        "Replace the current ordered execution plan with the supplied complete plan."
    }

    /// Validates complete-plan invariants before the actor observes a request.
    pub fn validate(&self) -> Result<(), PlanToolError> {
        if self.plan.len() > MAX_PLAN_ITEMS {
            return Err(PlanToolError::InvalidArgument(
                "plan must contain at most 32 items",
            ));
        }
        if self.plan.iter().any(|item| item.validate().is_err()) {
            return Err(PlanToolError::InvalidArgument(
                "plan steps must contain 1-256 trimmed Unicode scalars without controls",
            ));
        }
        if self
            .plan
            .iter()
            .filter(|item| item.status() == PlanStatus::InProgress)
            .nth(1)
            .is_some()
        {
            return Err(PlanToolError::InvalidArgument(
                "plan may contain at most one in_progress item",
            ));
        }
        Ok(())
    }
}

/// Stable model-visible failures returned by the update-plan tool.
#[derive(Debug, Error)]
pub enum PlanToolError {
    /// Request arguments did not satisfy the plan contract.
    #[error("[E_INVALID_ARGUMENT] {0}")]
    InvalidArgument(&'static str),
    /// The session actor could not accept or settle the update request.
    #[error("[E_RUNTIME] plan tool state is unavailable")]
    Runtime,
}

/// Cloneable client that sends updates to one authoritative session actor.
#[derive(Clone)]
pub struct PlanUpdateClient {
    sender: mpsc::Sender<PlanUpdateRequest>,
}

impl PlanUpdateClient {
    /// Validates, sends, and waits for one authoritative plan replacement.
    pub async fn replace(&self, args: UpdatePlanArgs) -> Result<PlanUpdateOutcome, PlanToolError> {
        args.validate()?;
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(PlanUpdateRequest {
                plan: args.plan,
                explanation: args.explanation,
                response,
            })
            .await
            .map_err(|_| PlanToolError::Runtime)?;
        receiver.await.map_err(|_| PlanToolError::Runtime)?
    }
}

/// The actor-owned receiving side of a bounded plan-update request port.
pub struct PlanUpdateReceiver {
    receiver: mpsc::Receiver<PlanUpdateRequest>,
}

impl PlanUpdateReceiver {
    /// Waits for the next validated request, or returns `None` after all clients close.
    pub async fn recv(&mut self) -> Option<PlanUpdateRequest> {
        self.receiver.recv().await
    }
}

/// One validated request whose response can be settled only by the session actor.
pub struct PlanUpdateRequest {
    plan: Vec<PlanItem>,
    explanation: Option<String>,
    response: oneshot::Sender<Result<PlanUpdateOutcome, PlanToolError>>,
}

impl PlanUpdateRequest {
    /// Returns the requested complete replacement plan.
    pub fn plan(&self) -> &[PlanItem] {
        &self.plan
    }

    /// Returns the optional model-visible context for this update.
    pub fn explanation(&self) -> Option<&str> {
        self.explanation.as_deref()
    }

    /// Settles this request with an accepted authoritative outcome.
    pub fn succeed(self, outcome: PlanUpdateOutcome) {
        let _ = self.response.send(Ok(outcome));
    }

    /// Settles this request with a model-visible tool failure.
    pub fn fail(self, error: PlanToolError) {
        let _ = self.response.send(Err(error));
    }
}

/// The authoritative replacement outcome returned to the model for one update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanUpdateOutcome {
    plan: Vec<PlanItem>,
    explanation: Option<String>,
    durable: bool,
}

impl PlanUpdateOutcome {
    /// Creates an authoritative outcome with its persistence result.
    pub fn new(plan: Vec<PlanItem>, explanation: Option<String>, durable: bool) -> Self {
        Self {
            plan,
            explanation,
            durable,
        }
    }

    /// Creates an outcome whose plan was durably checkpointed.
    pub fn durable(plan: Vec<PlanItem>, explanation: Option<String>) -> Self {
        Self::new(plan, explanation, true)
    }

    /// Returns the accepted complete ordered plan.
    pub fn plan(&self) -> &[PlanItem] {
        &self.plan
    }

    /// Returns whether the accepted plan has been durably checkpointed.
    pub const fn is_durable(&self) -> bool {
        self.durable
    }

    /// Renders the canonical model-visible summary of the accepted plan.
    pub fn render(&self) -> String {
        let completed = self
            .plan
            .iter()
            .filter(|item| item.status() == PlanStatus::Completed)
            .count();
        let in_progress = self
            .plan
            .iter()
            .filter(|item| item.status() == PlanStatus::InProgress)
            .count();
        let pending = self
            .plan
            .iter()
            .filter(|item| item.status() == PlanStatus::Pending)
            .count();
        let blocked = self
            .plan
            .iter()
            .filter(|item| item.status() == PlanStatus::Blocked)
            .count();
        let cancelled = self
            .plan
            .iter()
            .filter(|item| item.status() == PlanStatus::Cancelled)
            .count();
        let mut lines = vec![format!(
            "Plan updated: {completed} completed, {in_progress} in progress, {pending} pending, {blocked} blocked, {cancelled} cancelled."
        )];
        if let Some(explanation) = &self.explanation {
            lines.push(format!("Explanation: {explanation}"));
        }
        lines.extend(self.plan.iter().enumerate().map(|(index, item)| {
            format!(
                "{}. [{}] {}",
                index + 1,
                item.status().as_str(),
                item.step()
            )
        }));
        if !self.durable {
            lines.push("Plan persistence is pending; the live session retains this update.".into());
        }
        lines.join("\n")
    }
}

/// Creates the paired model client and actor receiver for one session.
pub fn plan_update_channel() -> (PlanUpdateClient, PlanUpdateReceiver) {
    let (sender, receiver) = mpsc::channel(PLAN_UPDATE_CHANNEL_CAPACITY);
    (PlanUpdateClient { sender }, PlanUpdateReceiver { receiver })
}
