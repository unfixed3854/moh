//! Terminal-client composition over the typed backend session boundary.

pub(super) mod app;
mod session;
mod terminal;
mod ui;
mod workspace;

use moh::rpc::client::RpcBackendClient;

// Task 12 consumes the full workspace projection surface; Task 11 wires only launch ownership.
#[allow(unused_imports)]
pub(super) use session::{
    ChatProjection, ClientSessionError, DraftState, LaunchMode, WorkspaceClient, WorkspaceUpdate,
};
use workspace::RpcWorkspaceController;

pub(super) async fn run(
    backend: RpcBackendClient,
    cwd: Vec<u8>,
    mode: LaunchMode,
) -> Result<(), app::AppError> {
    let mut workspace = RpcWorkspaceController::launch(backend, cwd, mode).await?;
    let application = app::run(&mut workspace).await;
    let shutdown = workspace.shutdown().await.map_err(app::AppError::from);
    application.and(shutdown)
}
