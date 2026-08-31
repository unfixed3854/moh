//! Per-session run-engine construction boundary.

use std::sync::Arc;

use crate::{
    harness::{RunEngine, RunFailure},
    runtime::rig::{ActiveModel, ActiveReasoning},
    session::SessionTitleGenerator,
    tools::{JobRegistry, PlanUpdateReceiver},
};

use super::{ModelCatalogState, SessionSettings};

/// Runtime components whose mutable state belongs to exactly one session actor.
pub struct SessionEngineBundle<E> {
    /// Model-neutral engine used by the session harness.
    pub engine: E,
    /// Shared model selector read when a future run starts.
    pub active_model: ActiveModel,
    /// Shared reasoning selector read when a future run starts.
    pub active_reasoning: ActiveReasoning,
    /// Process-local job registry isolated to this session.
    pub jobs: JobRegistry,
    /// Actor-owned receiver for model-visible complete plan replacements.
    pub plans: PlanUpdateReceiver,
}

/// Builds an isolated runtime bundle from durable session settings.
pub trait SessionEngineFactory: Clone + Send + Sync + 'static {
    /// Model-neutral engine created for each session.
    type Engine: RunEngine;

    /// Returns the catalog state installed in newly materialized session projections.
    fn catalog(&self) -> ModelCatalogState {
        ModelCatalogState::Loading
    }

    /// Returns the durable settings used when a new session identity is created.
    fn default_settings(&self) -> SessionSettings;

    /// Returns the shared generator used for independent session-title requests.
    fn title_generator(&self) -> Arc<dyn SessionTitleGenerator>;

    /// Creates runtime state initialized from `settings`.
    fn create(
        &self,
        settings: &SessionSettings,
    ) -> Result<SessionEngineBundle<Self::Engine>, RunFailure>;
}
