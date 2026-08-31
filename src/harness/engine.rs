use std::pin::Pin;

use futures::Stream;

use super::{EngineEvent, RunFailure, RunRequest};

/// The asynchronous event stream returned for one started run.
pub type RunStream = Pin<Box<dyn Stream<Item = Result<EngineEvent, RunFailure>> + Send + 'static>>;

/// A model-neutral adapter that starts requests and produces lifecycle events.
pub trait RunEngine: Send + Sync + 'static {
    /// Starts a request and returns its event stream without polling it.
    fn start(&self, request: RunRequest) -> RunStream;
}
