//! Unix-stream Cap'n Proto service boundary for backend sessions.

use std::{cell::RefCell, rc::Rc, str::FromStr};

use futures::AsyncReadExt;
use thiserror::Error;
use tokio::{net::UnixStream, sync::mpsc, task::JoinHandle};
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::{
    backend::ActivityTracker,
    moh_capnp,
    rpc::convert::{
        self, CommandError, ErrorCode, MAX_RPC_CWD_BYTES, MAX_RPC_IDENTIFIER_BYTES,
        MAX_RPC_PROMPT_BYTES, MAX_RPC_TITLE_BYTES, MaterializeSuccess, OpenResult, OpenSuccess,
        ProtocolInfo, RpcConversionError, StartupSuccess,
    },
    session::{
        ConnectionId, ManagedSession, SessionCommandError, SessionId, SessionManagerError,
        SessionManagerHandle, SessionStoreError, SessionTitle,
    },
};

/// Shared dependencies used by every RPC implementation on one backend process.
#[derive(Clone)]
pub struct BackendContext {
    readiness: BackendReadiness,
    activity: ActivityTracker,
    protocol_info: ProtocolInfo,
}

impl BackendContext {
    /// Creates a transport context from the process-wide session and activity owners.
    pub fn new(
        manager: SessionManagerHandle,
        activity: ActivityTracker,
        protocol_info: ProtocolInfo,
    ) -> Self {
        Self {
            readiness: BackendReadiness::ready(manager),
            activity,
            protocol_info,
        }
    }

    /// Creates a context that negotiates protocol metadata while session services initialize.
    pub fn starting(
        activity: ActivityTracker,
        protocol_info: ProtocolInfo,
    ) -> (Self, BackendReadiness) {
        let readiness = BackendReadiness::starting();
        (
            Self {
                readiness: readiness.clone(),
                activity,
                protocol_info,
            },
            readiness,
        )
    }

    fn manager(&self) -> Result<SessionManagerHandle, CommandError> {
        self.readiness.manager()
    }
}

#[derive(Clone)]
enum ReadinessState {
    Starting,
    Ready(SessionManagerHandle),
    Failed,
}

/// Shared session-service readiness installed after backend runtime initialization.
#[derive(Clone)]
pub struct BackendReadiness {
    state: Rc<RefCell<ReadinessState>>,
}

impl BackendReadiness {
    fn starting() -> Self {
        Self {
            state: Rc::new(RefCell::new(ReadinessState::Starting)),
        }
    }

    fn ready(manager: SessionManagerHandle) -> Self {
        Self {
            state: Rc::new(RefCell::new(ReadinessState::Ready(manager))),
        }
    }

    /// Installs the initialized manager for subsequent session operations.
    pub fn install(&self, manager: SessionManagerHandle) {
        *self.state.borrow_mut() = ReadinessState::Ready(manager);
    }

    /// Marks startup as failed without exposing initializer details to RPC clients.
    pub fn fail(&self) {
        *self.state.borrow_mut() = ReadinessState::Failed;
    }

    fn manager(&self) -> Result<SessionManagerHandle, CommandError> {
        match &*self.state.borrow() {
            ReadinessState::Starting => Err(CommandError {
                code: ErrorCode::BackendStarting,
                message: "backend is still starting".into(),
                ids: Vec::new(),
            }),
            ReadinessState::Ready(manager) => Ok(manager.clone()),
            ReadinessState::Failed => Err(CommandError {
                code: ErrorCode::BackendUnavailable,
                message: "backend is unavailable".into(),
                ids: Vec::new(),
            }),
        }
    }
}

/// Failure returned after an RPC system and its mandatory connection cleanup complete.
#[derive(Debug, Error)]
pub enum RpcServerError {
    /// The Cap'n Proto RPC system failed while the connection was live.
    #[error("Cap'n Proto RPC system failed: {0}")]
    Rpc(capnp::Error),
    /// Detaching the connection from the live session registry failed.
    #[error("RPC connection cleanup failed: {0}")]
    Cleanup(SessionManagerError),
    /// Both transport completion and mandatory cleanup failed.
    #[error("Cap'n Proto RPC system failed: {rpc}; RPC connection cleanup failed: {cleanup}")]
    RpcAndCleanup {
        /// Transport failure observed first.
        rpc: Box<capnp::Error>,
        /// Cleanup failure observed after transport completion.
        cleanup: Box<SessionManagerError>,
    },
}

/// Serves one Unix-stream client on the current-thread runtime's active `LocalSet`.
///
/// The connection activity key is set before the spawned RPC system can be polled. The returned
/// task owns transport completion and always performs connection-wide observer cleanup before it
/// clears that key.
pub fn serve_connection(
    stream: UnixStream,
    connection_id: ConnectionId,
    context: BackendContext,
) -> JoinHandle<Result<(), RpcServerError>> {
    context.activity.set_connection(connection_id, true);

    let cleanup_readiness = context.readiness.clone();
    let activity = context.activity.clone();
    let (reader, writer) = TokioAsyncReadCompatExt::compat(stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let bootstrap: moh_capnp::backend::Client =
        capnp_rpc::new_client(BackendImpl::new(context, connection_id));
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(bootstrap.client));

    tokio::task::spawn_local(async move {
        let rpc_result = rpc.await;
        let cleanup_result = match cleanup_readiness.manager() {
            Ok(manager) => manager.detach_connection(connection_id).await,
            Err(_) => Ok(()),
        };
        activity.set_connection(connection_id, false);
        combine_completion(rpc_result, cleanup_result)
    })
}

fn combine_completion(
    rpc: capnp::Result<()>,
    cleanup: Result<(), SessionManagerError>,
) -> Result<(), RpcServerError> {
    match (rpc, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(rpc), Ok(())) => Err(RpcServerError::Rpc(rpc)),
        (Ok(()), Err(cleanup)) => Err(RpcServerError::Cleanup(cleanup)),
        (Err(rpc), Err(cleanup)) => Err(RpcServerError::RpcAndCleanup {
            rpc: Box::new(rpc),
            cleanup: Box::new(cleanup),
        }),
    }
}

struct BackendImpl {
    context: BackendContext,
    connection_id: ConnectionId,
}

impl BackendImpl {
    fn new(context: BackendContext, connection_id: ConnectionId) -> Self {
        Self {
            context,
            connection_id,
        }
    }
}

impl moh_capnp::backend::Server for BackendImpl {
    async fn get_info(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::backend::GetInfoParams,
        mut results: moh_capnp::backend::GetInfoResults,
    ) -> capnp::Result<()> {
        convert::write_protocol_info(results.get().init_info(), &self.context.protocol_info)
            .map_err(conversion_failure)
    }

    async fn draft_defaults(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::DraftDefaultsParams,
        mut results: moh_capnp::backend::DraftDefaultsResults,
    ) -> capnp::Result<()> {
        let cwd = convert::read_inbound_data(params.get()?.get_cwd(), MAX_RPC_CWD_BYTES, "cwd")
            .map_err(conversion_failure)?;
        let defaults = match self.context.manager() {
            Ok(manager) => manager
                .draft_defaults(cwd)
                .await
                .map_err(|error| manager_command_error(&error)),
            Err(error) => Err(error),
        };
        convert::write_draft_defaults_result(results.get().init_result(), &defaults)
            .map_err(conversion_failure)
    }

    async fn startup(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::StartupParams,
        mut results: moh_capnp::backend::StartupResults,
    ) -> capnp::Result<()> {
        let (cwd, attachment_id, observer) = {
            let params = params.get()?;
            (
                convert::read_inbound_data(params.get_cwd(), MAX_RPC_CWD_BYTES, "cwd")
                    .map_err(conversion_failure)?,
                convert::read_attachment_id(params.get_attachment_id())
                    .map_err(conversion_failure)?,
                params.get_observer()?,
            )
        };
        let manager = match self.context.manager() {
            Ok(manager) => manager,
            Err(error) => {
                return PreparedStartup::from_result(
                    Err(error),
                    observer,
                    None,
                    self.connection_id,
                )
                .write(results.get().init_result());
            }
        };
        let started = manager
            .startup(cwd, self.connection_id, attachment_id)
            .await
            .map_err(|error| manager_command_error(&error));
        PreparedStartup::from_result(started, observer, Some(manager), self.connection_id)
            .write(results.get().init_result())
    }

    async fn materialize(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::MaterializeParams,
        mut results: moh_capnp::backend::MaterializeResults,
    ) -> capnp::Result<()> {
        let (cwd, prompt, settings, attachment_id, observer) = {
            let params = params.get()?;
            (
                convert::read_inbound_data(params.get_cwd(), MAX_RPC_CWD_BYTES, "cwd")
                    .map_err(conversion_failure)?,
                convert::read_inbound_text(params.get_prompt(), MAX_RPC_PROMPT_BYTES, "prompt")
                    .map_err(conversion_failure)?,
                convert::read_session_settings(params.get_settings()?)
                    .map_err(conversion_failure)?,
                convert::read_attachment_id(params.get_attachment_id())
                    .map_err(conversion_failure)?,
                params.get_observer()?,
            )
        };
        let manager = match self.context.manager() {
            Ok(manager) => manager,
            Err(error) => {
                return PreparedMaterialize::from_result(
                    Err(error),
                    observer,
                    None,
                    self.connection_id,
                )
                .write(results.get().init_result());
            }
        };
        let materialized = manager
            .materialize_and_submit(cwd, prompt, settings, self.connection_id, attachment_id)
            .await
            .map_err(|error| manager_command_error(&error));
        PreparedMaterialize::from_result(materialized, observer, Some(manager), self.connection_id)
            .write(results.get().init_result())
    }

    async fn open_session(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::OpenSessionParams,
        mut results: moh_capnp::backend::OpenSessionResults,
    ) -> capnp::Result<()> {
        let (selector, cwd_for_title, attachment_id, observer) = {
            let params = params.get()?;
            (
                convert::read_session_selector(params.get_selector()?)
                    .map_err(conversion_failure)?,
                convert::read_inbound_data(
                    params.get_cwd_for_title(),
                    MAX_RPC_CWD_BYTES,
                    "cwdForTitle",
                )
                .map_err(conversion_failure)?,
                convert::read_attachment_id(params.get_attachment_id())
                    .map_err(conversion_failure)?,
                params.get_observer()?,
            )
        };
        let manager = match self.context.manager() {
            Ok(manager) => manager,
            Err(error) => {
                return PreparedOpen::from_result(Err(error), observer, None, self.connection_id)
                    .write(results.get().init_result());
            }
        };
        let opened = manager
            .open(selector, cwd_for_title, self.connection_id, attachment_id)
            .await
            .map_err(|error| manager_command_error(&error));
        PreparedOpen::from_result(opened, observer, Some(manager), self.connection_id)
            .write(results.get().init_result())
    }

    async fn list_sessions(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::ListSessionsParams,
        mut results: moh_capnp::backend::ListSessionsResults,
    ) -> capnp::Result<()> {
        let scope = {
            let params = params.get()?;
            convert::read_session_list_scope(params.get_scope(), params.get_cwd())
                .map_err(conversion_failure)?
        };
        let listed = match self.context.manager() {
            Ok(manager) => manager
                .list(scope)
                .await
                .map_err(|error| manager_command_error(&error)),
            Err(error) => Err(error),
        };
        convert::write_session_list_result(results.get().init_result(), &listed)
            .map_err(conversion_failure)
    }

    async fn rename_session(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::RenameSessionParams,
        mut results: moh_capnp::backend::RenameSessionResults,
    ) -> capnp::Result<()> {
        let (session_id, title) = {
            let params = params.get()?;
            let session_id = SessionId::from_str(
                &convert::read_inbound_text(params.get_id(), MAX_RPC_IDENTIFIER_BYTES, "id")
                    .map_err(conversion_failure)?,
            )
            .map_err(|_| conversion_failure(RpcConversionError::InvalidSessionId))?;
            let title = SessionTitle::parse(
                convert::read_inbound_text(params.get_title(), MAX_RPC_TITLE_BYTES, "title")
                    .map_err(conversion_failure)?,
            )
            .map_err(|_| conversion_failure(RpcConversionError::InvalidSessionTitle))?;
            (session_id, title)
        };
        let renamed = match self.context.manager() {
            Ok(manager) => manager
                .rename(session_id, title)
                .await
                .map_err(|error| manager_command_error(&error)),
            Err(error) => Err(error),
        };
        convert::write_command_result(results.get().init_result(), &renamed)
            .map_err(conversion_failure)
    }

    async fn delete_session(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::DeleteSessionParams,
        mut results: moh_capnp::backend::DeleteSessionResults,
    ) -> capnp::Result<()> {
        let session_id = SessionId::from_str(
            &convert::read_inbound_text(params.get()?.get_id(), MAX_RPC_IDENTIFIER_BYTES, "id")
                .map_err(conversion_failure)?,
        )
        .map_err(|_| conversion_failure(RpcConversionError::InvalidSessionId))?;
        let deleted = match self.context.manager() {
            Ok(manager) => manager
                .delete(session_id)
                .await
                .map_err(|error| manager_command_error(&error)),
            Err(error) => Err(error),
        };
        convert::write_command_result(results.get().init_result(), &deleted)
            .map_err(conversion_failure)
    }
}

struct PreparedOpen {
    value: OpenResult,
    pump: Option<ObserverPump>,
}

impl PreparedOpen {
    fn from_result(
        opened: Result<ManagedSession, CommandError>,
        observer: moh_capnp::observer::Client,
        manager: Option<SessionManagerHandle>,
        connection_id: ConnectionId,
    ) -> Self {
        match opened {
            Ok(opened) => {
                let (success, pump) = prepare_managed(
                    opened,
                    observer,
                    manager.expect("successful open must retain its manager"),
                    connection_id,
                );
                Self {
                    value: Ok(success),
                    pump: Some(pump),
                }
            }
            Err(error) => Self {
                value: Err(error),
                pump: None,
            },
        }
    }

    fn write(self, builder: moh_capnp::open_result::Builder<'_>) -> capnp::Result<()> {
        convert::write_open_result(builder, &self.value).map_err(conversion_failure)?;
        if let Some(pump) = self.pump {
            tokio::task::spawn_local(pump.run());
        }
        Ok(())
    }
}

struct PreparedStartup {
    value: convert::StartupResult,
    pump: Option<ObserverPump>,
}

impl PreparedStartup {
    fn from_result(
        started: Result<crate::session::StartupResult, CommandError>,
        observer: moh_capnp::observer::Client,
        manager: Option<SessionManagerHandle>,
        connection_id: ConnectionId,
    ) -> Self {
        match started {
            Ok(crate::session::StartupResult::Draft(draft)) => Self {
                value: Ok(StartupSuccess::Draft(draft)),
                pump: None,
            },
            Ok(crate::session::StartupResult::Attached(opened)) => {
                let (success, pump) = prepare_managed(
                    *opened,
                    observer,
                    manager.expect("attached startup must retain its manager"),
                    connection_id,
                );
                Self {
                    value: Ok(StartupSuccess::Attached(Box::new(success))),
                    pump: Some(pump),
                }
            }
            Err(error) => Self {
                value: Err(error),
                pump: None,
            },
        }
    }

    fn write(self, builder: moh_capnp::startup_result::Builder<'_>) -> capnp::Result<()> {
        convert::write_startup_result(builder, &self.value).map_err(conversion_failure)?;
        if let Some(pump) = self.pump {
            tokio::task::spawn_local(pump.run());
        }
        Ok(())
    }
}

struct PreparedMaterialize {
    value: convert::MaterializeResult,
    pump: Option<ObserverPump>,
}

impl PreparedMaterialize {
    fn from_result(
        materialized: Result<crate::session::MaterializedSession, CommandError>,
        observer: moh_capnp::observer::Client,
        manager: Option<SessionManagerHandle>,
        connection_id: ConnectionId,
    ) -> Self {
        match materialized {
            Ok(materialized) => {
                let (session, pump) = prepare_managed(
                    materialized.session,
                    observer,
                    manager.expect("successful materialization must retain its manager"),
                    connection_id,
                );
                Self {
                    value: Ok(MaterializeSuccess {
                        session: session.session,
                        snapshot: session.snapshot,
                        run_id: materialized.run_id,
                    }),
                    pump: Some(pump),
                }
            }
            Err(error) => Self {
                value: Err(error),
                pump: None,
            },
        }
    }

    fn write(self, builder: moh_capnp::materialize_result::Builder<'_>) -> capnp::Result<()> {
        convert::write_materialize_result(builder, &self.value).map_err(conversion_failure)?;
        if let Some(pump) = self.pump {
            tokio::task::spawn_local(pump.run());
        }
        Ok(())
    }
}

fn prepare_managed(
    opened: ManagedSession,
    observer: moh_capnp::observer::Client,
    manager: SessionManagerHandle,
    connection_id: ConnectionId,
) -> (OpenSuccess, ObserverPump) {
    let ManagedSession {
        handle,
        snapshot,
        events,
    } = opened;
    let session_id = snapshot.summary.id;
    let session = capnp_rpc::new_client(SessionImpl {
        handle,
        manager,
        session_id,
        connection_id,
    });
    (
        OpenSuccess { session, snapshot },
        ObserverPump { events, observer },
    )
}

struct ObserverPump {
    events: mpsc::Receiver<crate::session::SessionEventEnvelope>,
    observer: moh_capnp::observer::Client,
}

impl ObserverPump {
    async fn run(mut self) {
        while let Some(event) = self.events.recv().await {
            let terminal = matches!(event.event, crate::session::SessionEvent::Deleted { .. });
            let mut request = self.observer.publish_request();
            if convert::write_event_envelope(request.get().init_event(), &event).is_err() {
                break;
            }
            if request.send().promise.await.is_err() {
                break;
            }
            if terminal {
                break;
            }
        }
    }
}

struct SessionImpl {
    handle: crate::session::SessionHandle,
    manager: SessionManagerHandle,
    session_id: SessionId,
    connection_id: ConnectionId,
}

impl moh_capnp::session::Server for SessionImpl {
    async fn submit(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::session::SubmitParams,
        mut results: moh_capnp::session::SubmitResults,
    ) -> capnp::Result<()> {
        let prompt =
            convert::read_inbound_text(params.get()?.get_prompt(), MAX_RPC_PROMPT_BYTES, "prompt")
                .map_err(conversion_failure)?;
        let result = self
            .handle
            .submit(prompt)
            .await
            .map_err(|error| CommandError::from(&error));
        convert::write_submit_result(results.get().init_result(), &result)
            .map_err(conversion_failure)
    }

    async fn cancel(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::session::CancelParams,
        mut results: moh_capnp::session::CancelResults,
    ) -> capnp::Result<()> {
        let result = self
            .handle
            .cancel()
            .await
            .map_err(|error| CommandError::from(&error));
        convert::write_command_result(results.get().init_result(), &result)
            .map_err(conversion_failure)
    }

    async fn select_model(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::session::SelectModelParams,
        mut results: moh_capnp::session::SelectModelResults,
    ) -> capnp::Result<()> {
        let model = convert::read_inbound_text(
            params.get()?.get_model_id(),
            MAX_RPC_IDENTIFIER_BYTES,
            "modelId",
        )
        .map_err(conversion_failure)?;
        let result = self
            .handle
            .select_model(model)
            .await
            .map_err(|error| CommandError::from(&error));
        convert::write_command_result(results.get().init_result(), &result)
            .map_err(conversion_failure)
    }

    async fn select_reasoning(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::session::SelectReasoningParams,
        mut results: moh_capnp::session::SelectReasoningResults,
    ) -> capnp::Result<()> {
        let reasoning =
            convert::read_reasoning_level(params.get()?.get_level()).map_err(conversion_failure)?;
        let result = self
            .handle
            .select_reasoning(reasoning)
            .await
            .map_err(|error| CommandError::from(&error));
        convert::write_command_result(results.get().init_result(), &result)
            .map_err(conversion_failure)
    }

    async fn list_jobs(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::session::ListJobsParams,
        mut results: moh_capnp::session::ListJobsResults,
    ) -> capnp::Result<()> {
        let result = self
            .handle
            .list_jobs()
            .await
            .map_err(|error| CommandError::from(&error));
        convert::write_job_list_result(results.get().init_result(), &result)
            .map_err(conversion_failure)
    }

    async fn cancel_job(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::session::CancelJobParams,
        mut results: moh_capnp::session::CancelJobResults,
    ) -> capnp::Result<()> {
        let id = convert::read_inbound_text(
            params.get()?.get_job_id(),
            MAX_RPC_IDENTIFIER_BYTES,
            "jobId",
        )
        .map_err(conversion_failure)?;
        let result = self
            .handle
            .cancel_job(id)
            .await
            .map_err(|error| CommandError::from(&error));
        convert::write_job_result(results.get().init_result(), &result).map_err(conversion_failure)
    }

    async fn detach(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::session::DetachParams,
        mut results: moh_capnp::session::DetachResults,
    ) -> capnp::Result<()> {
        let attachment_id = convert::read_attachment_id(params.get()?.get_attachment_id())
            .map_err(conversion_failure)?;
        let detached = self
            .manager
            .detach(self.session_id, self.connection_id, attachment_id)
            .await
            .map_err(|error| manager_command_error(&error));
        convert::write_detach_result(results.get().init_result(), &detached)
            .map_err(conversion_failure)
    }
}

fn conversion_failure(error: RpcConversionError) -> capnp::Error {
    capnp::Error::failed(error.to_string())
}

fn manager_command_error(error: &SessionManagerError) -> CommandError {
    match error {
        SessionManagerError::Store(SessionStoreError::NotFound { .. }) => CommandError {
            code: ErrorCode::SessionNotFound,
            message: error.to_string(),
            ids: Vec::new(),
        },
        SessionManagerError::Store(SessionStoreError::AmbiguousTitle { ids, .. }) => CommandError {
            code: ErrorCode::AmbiguousTitle,
            message: error.to_string(),
            ids: ids.clone(),
        },
        SessionManagerError::Store(SessionStoreError::NameConflict { .. }) => CommandError {
            code: ErrorCode::SessionNameConflict,
            message: error.to_string(),
            ids: Vec::new(),
        },
        SessionManagerError::Store(SessionStoreError::ValueOutOfRange { .. }) => CommandError {
            code: ErrorCode::InvalidArgument,
            message: error.to_string(),
            ids: Vec::new(),
        },
        SessionManagerError::Store(_) => CommandError {
            code: ErrorCode::Persistence,
            message: "session persistence failed".into(),
            ids: Vec::new(),
        },
        SessionManagerError::Runtime(_) => CommandError {
            code: ErrorCode::Internal,
            message: "session runtime could not be initialized".into(),
            ids: Vec::new(),
        },
        SessionManagerError::Session(error) => actor_command_error(error),
        SessionManagerError::Unavailable => CommandError {
            code: ErrorCode::BackendUnavailable,
            message: "session manager is unavailable".into(),
            ids: Vec::new(),
        },
    }
}

fn actor_command_error(error: &SessionCommandError) -> CommandError {
    CommandError::from(error)
}
