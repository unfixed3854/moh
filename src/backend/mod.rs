//! Backend-global lifecycle coordination.

mod activity;
#[cfg(unix)]
mod server;

use thiserror::Error;

use crate::session::SessionManagerHandle;

pub use activity::{ActivitySnapshot, ActivityTracker, IdleDeadline, wait_for_idle};
#[cfg(unix)]
pub use server::{
    BackendError, BackendOptions, BackendRuntimeFactory, ConnectionIdAllocator,
    ConnectionIdExhausted, ShutdownReason, run_backend,
};

/// Reason automatic idle shutdown must leave the backend running.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ShutdownVeto {
    /// At least one live session's committed state remains unpersisted.
    #[error("one or more dirty sessions could not be persisted")]
    DirtySessions,
}

/// Flushes all live actors before automatic idle shutdown may proceed.
pub async fn flush_for_idle_shutdown(manager: &SessionManagerHandle) -> Result<(), ShutdownVeto> {
    manager
        .flush_all()
        .await
        .map_err(|_| ShutdownVeto::DirtySessions)
}
