//! Typed client ownership for the local Cap'n Proto backend and session capabilities.

use std::{cell::Cell, fmt, future::Future, pin::Pin, rc::Rc};

use futures::AsyncReadExt;
use thiserror::Error;
use tokio::{
    net::UnixStream,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::{
    moh_capnp,
    rpc::convert::{self, CommandError, ProtocolInfo, REQUIRED_FEATURES, RpcConversionError},
    runtime::rig::ReasoningLevel,
    session::{
        AttachmentId, DraftDefaults, JobSnapshotDto, SessionCommandError, SessionEvent,
        SessionEventEnvelope, SessionId, SessionListScope, SessionSelector, SessionSettings,
        SessionSnapshot, SessionSummary, SessionTitle,
    },
};

const OBSERVER_CAPACITY: usize = 128;

/// Client-facing metadata established before any session can be opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcBackendInfo {
    /// Protocol major version negotiated by this connection.
    pub protocol_major: u16,
    /// Additive protocol minor version advertised by the backend.
    pub protocol_minor: u16,
    /// Stable identifier for the connected backend process.
    pub instance_id: String,
    /// Sanitized non-fatal diagnostics from backend startup.
    pub startup_warnings: Vec<String>,
    /// RPC methods advertised by this backend.
    pub features: Vec<String>,
}

impl From<ProtocolInfo> for RpcBackendInfo {
    fn from(info: ProtocolInfo) -> Self {
        Self {
            protocol_major: info.major,
            protocol_minor: info.minor,
            instance_id: info.instance_id,
            startup_warnings: info.startup_warnings,
            features: info.features,
        }
    }
}

/// A typed backend command, conversion, compatibility, or connection failure.
#[derive(Debug, Error)]
pub enum RpcClientError {
    /// The server uses an incompatible protocol major version.
    #[error("incompatible RPC protocol major: client {client}, server {server}")]
    IncompatibleProtocol {
        /// Protocol major implemented by this client.
        client: u16,
        /// Protocol major advertised by the server.
        server: u16,
    },
    /// The compatible server omitted an RPC method this client calls.
    #[error("RPC server does not advertise required feature {feature}")]
    MissingFeature {
        /// Missing stable method feature name.
        feature: String,
    },
    /// An exact project-scoped title matched more than one durable session.
    #[error("{message}")]
    AmbiguousTitle {
        /// Sanitized backend description of the ambiguity.
        message: String,
        /// Stable matching identifiers in ascending order.
        ids: Vec<SessionId>,
    },
    /// A result union reported an ordinary backend command failure.
    #[error(transparent)]
    Command(#[from] SessionCommandError),
    /// A checked wire/domain conversion failed.
    #[error(transparent)]
    Conversion(#[from] RpcConversionError),
    /// The Cap'n Proto request or connection failed.
    #[error("RPC connection failed")]
    Connection(#[source] capnp::Error),
    /// The task driving the local Cap'n Proto system could not be joined.
    #[error("RPC connection task failed")]
    ConnectionTask(#[source] tokio::task::JoinError),
    /// The connection ended while waiting for an observer callback.
    #[error("RPC connection closed while waiting for a session update")]
    ConnectionClosed,
    /// The local observer capability was closed.
    #[error("RPC session observer closed")]
    ObserverClosed,
    /// The process-local attachment identifier counter cannot advance.
    #[error("RPC attachment identifier space is exhausted")]
    AttachmentIdExhausted,
    /// A fresh attachment also reported the maximum event sequence.
    #[error("RPC session event sequence is exhausted")]
    SequenceExhausted,
}

impl RpcClientError {
    fn connection(error: capnp::Error) -> Self {
        Self::Connection(error)
    }
}

/// A sequenced callback or an authoritative replacement after sequence recovery.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionUpdate {
    /// The next contiguous event after the locally observed sequence.
    Event(SessionEventEnvelope),
    /// A fresh attachment snapshot replacing all prior local projection state.
    SnapshotReplaced(Box<SessionSnapshot>),
    /// A nonfatal recovery cleanup problem after the replacement was installed.
    Warning(String),
    /// The attached session was durably deleted and the observer is terminal.
    Deleted {
        /// Stable identity removed by the backend.
        session_id: SessionId,
        /// Canonical project bytes used by the caller's startup fallback.
        cwd: Vec<u8>,
    },
}

/// Result of backend startup selection for one project.
#[derive(Debug)]
pub enum RpcStartup {
    /// No running durable session exists, so the caller remains on a local draft.
    Draft(DraftDefaults),
    /// The most recently active running session was atomically attached.
    Attached(Box<RpcSessionClient>),
}

/// A first prompt made durable and started together with its new attachment.
#[derive(Debug)]
pub struct MaterializedSession {
    /// Typed commands and observer state for the newly durable session.
    pub session: RpcSessionClient,
    /// Harness run identifier assigned to the first prompt.
    pub run_id: u64,
}

struct RecoveredAttachment {
    opened: convert::OpenSuccess,
    attachment: LocalAttachment,
    detach_warning: Option<String>,
}

type RecoveryFuture =
    Pin<Box<dyn Future<Output = Result<RecoveredAttachment, RpcClientError>> + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryReason {
    SequenceGap,
    SequenceOverflow,
    ObserverClosed,
}

struct PendingRecovery {
    reason: RecoveryReason,
    future: RecoveryFuture,
}

/// Typed client for the backend bootstrap capability and its connection lifecycle.
pub struct RpcBackendClient {
    backend: moh_capnp::backend::Client,
    info: RpcBackendInfo,
    attachment_ids: Rc<AttachmentIds>,
    connection_closed: watch::Receiver<bool>,
    shutdown: oneshot::Sender<()>,
    connection_task: JoinHandle<capnp::Result<()>>,
}

impl fmt::Debug for RpcBackendClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcBackendClient")
            .field("backend", &"<capability>")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl RpcBackendClient {
    /// Connects a Unix stream and completes version/feature negotiation before returning.
    pub async fn connect(stream: UnixStream) -> Result<Self, RpcClientError> {
        let (reader, writer) = TokioAsyncReadCompatExt::compat(stream).split();
        let network = capnp_rpc::twoparty::VatNetwork::new(
            futures::io::BufReader::new(reader),
            futures::io::BufWriter::new(writer),
            capnp_rpc::rpc_twoparty_capnp::Side::Client,
            Default::default(),
        );
        let mut rpc = capnp_rpc::RpcSystem::new(Box::new(network), None);
        let backend = rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
        let disconnector = rpc.get_disconnector();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (connection_state, connection_closed) = watch::channel(false);
        let connection_task = tokio::task::spawn_local(async move {
            tokio::pin!(rpc);
            let result = tokio::select! {
                result = rpc.as_mut() => result,
                _ = shutdown_rx => {
                    tokio::pin!(disconnector);
                    tokio::select! {
                        result = rpc.as_mut() => result,
                        result = disconnector.as_mut() => {
                            result?;
                            rpc.await
                        }
                    }
                }
            };
            let _ = connection_state.send(true);
            result
        });

        let info = request_info(&backend).await;
        let info = match info {
            Ok(info) => info,
            Err(error) => {
                let _ = finish_connection(shutdown, connection_task).await;
                return Err(error);
            }
        };
        if let Err(error) = validate_protocol_major(moh_capnp::PROTOCOL_MAJOR, info.major) {
            let _ = finish_connection(shutdown, connection_task).await;
            return Err(error);
        }
        if let Some(feature) = REQUIRED_FEATURES
            .iter()
            .find(|required| !info.features.iter().any(|feature| feature == **required))
        {
            let error = RpcClientError::MissingFeature {
                feature: (*feature).into(),
            };
            let _ = finish_connection(shutdown, connection_task).await;
            return Err(error);
        }

        Ok(Self {
            backend,
            info: info.into(),
            attachment_ids: Rc::new(AttachmentIds::default()),
            connection_closed,
            shutdown,
            connection_task,
        })
    }

    /// Returns the immutable metadata negotiated by [`Self::connect`].
    pub fn info(&self) -> &RpcBackendInfo {
        &self.info
    }

    /// Returns fresh non-durable defaults without selecting or attaching running work.
    pub async fn draft_defaults(
        &self,
        cwd: Vec<u8>,
    ) -> Result<crate::session::DraftDefaults, RpcClientError> {
        convert::validate_inbound_field_length(cwd.len(), convert::MAX_RPC_CWD_BYTES, "cwd")?;
        let mut request = self.backend.draft_defaults_request();
        request.get().set_cwd(&cwd);
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_draft_defaults_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Selects current project work or returns non-durable draft defaults.
    pub async fn startup(&self, cwd: Vec<u8>) -> Result<RpcStartup, RpcClientError> {
        convert::validate_inbound_field_length(cwd.len(), convert::MAX_RPC_CWD_BYTES, "cwd")?;
        let mut attachment = self.new_attachment()?;
        let mut request = self.backend.startup_request();
        request.get().set_cwd(&cwd);
        request
            .get()
            .set_attachment_id(convert::write_attachment_id(AttachmentId(attachment.id))?);
        request.get().set_observer(attachment.take_observer());
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_startup_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        match reported_result(result)? {
            convert::StartupSuccess::Draft(draft) => Ok(RpcStartup::Draft(draft)),
            convert::StartupSuccess::Attached(opened) => Ok(RpcStartup::Attached(Box::new(
                self.session_from_open(*opened, attachment),
            ))),
        }
    }

    /// Durably accepts the first prompt, starts its run, and attaches the caller.
    pub async fn materialize(
        &self,
        cwd: Vec<u8>,
        prompt: String,
        settings: SessionSettings,
    ) -> Result<MaterializedSession, RpcClientError> {
        convert::validate_inbound_field_length(cwd.len(), convert::MAX_RPC_CWD_BYTES, "cwd")?;
        convert::validate_inbound_field_length(
            prompt.len(),
            convert::MAX_RPC_PROMPT_BYTES,
            "prompt",
        )?;
        let mut attachment = self.new_attachment()?;
        let mut request = self.backend.materialize_request();
        {
            let mut params = request.get();
            params.set_cwd(&cwd);
            params.set_prompt(&prompt);
            convert::write_session_settings(params.reborrow().init_settings(), &settings)?;
            params.set_attachment_id(convert::write_attachment_id(AttachmentId(attachment.id))?);
            params.set_observer(attachment.take_observer());
        }
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let materialized = reported_result(convert::read_materialize_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?)?;
        let session = self.session_from_open(
            convert::OpenSuccess {
                session: materialized.session,
                snapshot: materialized.snapshot,
            },
            attachment,
        );
        Ok(MaterializedSession {
            session,
            run_id: materialized.run_id,
        })
    }

    /// Opens a stable ID globally or an unambiguous exact title in one project.
    pub async fn open_session(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> Result<RpcSessionClient, RpcClientError> {
        let (opened, attachment) = request_open_session(
            &self.backend,
            &self.attachment_ids,
            &selector,
            &cwd_for_title,
        )
        .await?;
        Ok(self.session_from_open(opened, attachment))
    }

    /// Lists live-overlay session summaries in one project or across all projects.
    pub fn list_sessions(
        &self,
        scope: SessionListScope,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SessionSummary>, RpcClientError>> + 'static>> {
        let backend = self.backend.clone();
        Box::pin(async move {
            let (wire_scope, cwd) = convert::write_session_list_scope(&scope)?;
            let mut request = backend.list_sessions_request();
            request.get().set_scope(wire_scope);
            request.get().set_cwd(cwd);
            let response = request
                .send()
                .promise
                .await
                .map_err(RpcClientError::connection)?;
            let result = convert::read_session_list_result(
                response
                    .get()
                    .map_err(RpcClientError::connection)?
                    .get_result()
                    .map_err(RpcClientError::connection)?,
            )?;
            reported_result(result)
        })
    }

    /// Applies a validated manual title to one stable session identity.
    pub async fn rename_session(
        &self,
        session_id: SessionId,
        title: SessionTitle,
    ) -> Result<(), RpcClientError> {
        let session_id = session_id.to_string();
        convert::validate_inbound_field_length(
            session_id.len(),
            convert::MAX_RPC_IDENTIFIER_BYTES,
            "id",
        )?;
        convert::validate_inbound_field_length(
            title.as_str().len(),
            convert::MAX_RPC_TITLE_BYTES,
            "title",
        )?;
        let mut request = self.backend.rename_session_request();
        request.get().set_id(&session_id);
        request.get().set_title(title.as_str());
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_command_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Permanently deletes one stable session identity.
    pub async fn delete_session(&self, session_id: SessionId) -> Result<(), RpcClientError> {
        let session_id = session_id.to_string();
        convert::validate_inbound_field_length(
            session_id.len(),
            convert::MAX_RPC_IDENTIFIER_BYTES,
            "id",
        )?;
        let mut request = self.backend.delete_session_request();
        request.get().set_id(&session_id);
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_command_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Gracefully disconnects and joins the task driving this Cap'n Proto connection.
    pub async fn disconnect(self) -> Result<(), RpcClientError> {
        let Self {
            backend,
            shutdown,
            connection_task,
            ..
        } = self;
        drop(backend);
        finish_connection(shutdown, connection_task).await
    }

    fn new_attachment(&self) -> Result<LocalAttachment, RpcClientError> {
        LocalAttachment::new(&self.attachment_ids)
    }

    fn session_from_open(
        &self,
        opened: convert::OpenSuccess,
        attachment: LocalAttachment,
    ) -> RpcSessionClient {
        RpcSessionClient::new(
            self.backend.clone(),
            Rc::clone(&self.attachment_ids),
            self.connection_closed.clone(),
            opened,
            attachment,
        )
    }
}

fn validate_protocol_major(client: u16, server: u16) -> Result<(), RpcClientError> {
    if client == server {
        Ok(())
    } else {
        Err(RpcClientError::IncompatibleProtocol { client, server })
    }
}

/// Typed client for one attached session capability and its observer stream.
pub struct RpcSessionClient {
    backend: moh_capnp::backend::Client,
    session: moh_capnp::session::Client,
    attachment_ids: Rc<AttachmentIds>,
    reattach_selector: SessionSelector,
    attachment: LocalAttachment,
    snapshot: SessionSnapshot,
    expected_sequence: u64,
    pending_recovery: Option<PendingRecovery>,
    pending_warning: Option<String>,
    sequence_exhausted: bool,
    connection_closed: watch::Receiver<bool>,
}

impl fmt::Debug for RpcSessionClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcSessionClient")
            .field("session", &"<capability>")
            .field("reattach_selector", &self.reattach_selector)
            .field("snapshot", &self.snapshot)
            .field("expected_sequence", &self.expected_sequence)
            .field("recovery_pending", &self.pending_recovery.is_some())
            .field("sequence_exhausted", &self.sequence_exhausted)
            .finish_non_exhaustive()
    }
}

impl RpcSessionClient {
    fn new(
        backend: moh_capnp::backend::Client,
        attachment_ids: Rc<AttachmentIds>,
        connection_closed: watch::Receiver<bool>,
        opened: convert::OpenSuccess,
        attachment: LocalAttachment,
    ) -> Self {
        let reattach_selector = SessionSelector::Id(opened.snapshot.summary.id);
        let expected_sequence = opened.snapshot.sequence;
        Self {
            backend,
            session: opened.session,
            attachment_ids,
            reattach_selector,
            attachment,
            snapshot: opened.snapshot,
            expected_sequence,
            pending_recovery: None,
            pending_warning: None,
            sequence_exhausted: false,
            connection_closed,
        }
    }

    /// Returns the authoritative snapshot installed by the latest attachment.
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    /// Waits for the next contiguous callback, recovering gaps with a fresh attachment snapshot.
    pub async fn next_update(&mut self) -> Result<SessionUpdate, RpcClientError> {
        loop {
            if let Some(warning) = self.pending_warning.take() {
                return Ok(SessionUpdate::Warning(warning));
            }
            if self.pending_recovery.is_some() {
                return self.resume_recovery().await;
            }
            let Some(next_sequence) = self.expected_sequence.checked_add(1) else {
                if self.sequence_exhausted {
                    return Err(RpcClientError::SequenceExhausted);
                }
                self.begin_recovery(RecoveryReason::SequenceOverflow);
                return self.resume_recovery().await;
            };
            if *self.connection_closed.borrow() {
                return Err(RpcClientError::ConnectionClosed);
            }
            let message = tokio::select! {
                message = self.attachment.events.recv() => message,
                changed = self.connection_closed.changed() => {
                    let _ = changed;
                    return Err(RpcClientError::ConnectionClosed);
                }
            };
            let Some(message) = message else {
                self.begin_recovery(RecoveryReason::ObserverClosed);
                return self.resume_recovery().await;
            };
            if message.attachment_id != self.attachment.id {
                continue;
            }
            let event = message.event?;
            if let SessionEvent::Deleted { session_id } = &event.event {
                return Ok(SessionUpdate::Deleted {
                    session_id: *session_id,
                    cwd: self.snapshot.summary.cwd.clone(),
                });
            }
            if event.sequence <= self.expected_sequence {
                continue;
            }
            if event.sequence == next_sequence {
                self.expected_sequence = event.sequence;
                return Ok(SessionUpdate::Event(event));
            }
            self.begin_recovery(RecoveryReason::SequenceGap);
            return self.resume_recovery().await;
        }
    }

    /// Submits a prompt and returns the backend-assigned run identifier.
    pub async fn submit(&self, prompt: String) -> Result<u64, RpcClientError> {
        convert::validate_inbound_field_length(
            prompt.len(),
            convert::MAX_RPC_PROMPT_BYTES,
            "prompt",
        )?;
        let mut request = self.session.submit_request();
        request.get().set_prompt(&prompt);
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_submit_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Explicitly cancels the active run without detaching the session.
    pub async fn cancel(&self) -> Result<(), RpcClientError> {
        let response = self
            .session
            .cancel_request()
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_command_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Selects the model used by future requests in this session.
    pub async fn select_model(&self, model: String) -> Result<(), RpcClientError> {
        convert::validate_inbound_field_length(
            model.len(),
            convert::MAX_RPC_IDENTIFIER_BYTES,
            "modelId",
        )?;
        let mut request = self.session.select_model_request();
        request.get().set_model_id(&model);
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_command_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Selects the reasoning effort used by future requests in this session.
    pub async fn select_reasoning(&self, reasoning: ReasoningLevel) -> Result<(), RpcClientError> {
        let mut request = self.session.select_reasoning_request();
        request
            .get()
            .set_level(convert::write_reasoning_level(reasoning));
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_command_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Lists the latest snapshots for this session's process-local jobs.
    pub async fn list_jobs(&self) -> Result<Vec<JobSnapshotDto>, RpcClientError> {
        let response = self
            .session
            .list_jobs_request()
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_job_list_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Cancels a retained session-local job and returns its terminal snapshot.
    pub async fn cancel_job(&self, job_id: String) -> Result<JobSnapshotDto, RpcClientError> {
        convert::validate_inbound_field_length(
            job_id.len(),
            convert::MAX_RPC_IDENTIFIER_BYTES,
            "jobId",
        )?;
        let mut request = self.session.cancel_job_request();
        request.get().set_job_id(&job_id);
        let response = request
            .send()
            .promise
            .await
            .map_err(RpcClientError::connection)?;
        let result = convert::read_job_result(
            response
                .get()
                .map_err(RpcClientError::connection)?
                .get_result()
                .map_err(RpcClientError::connection)?,
        )?;
        reported_result(result)
    }

    /// Detaches this exact attachment without closing the backend connection or cancelling work.
    pub async fn detach(self) -> Result<(), RpcClientError> {
        detach_attachment(&self.session, self.attachment.id)
            .await
            .map(|_| ())
    }

    fn begin_recovery(&mut self, reason: RecoveryReason) {
        debug_assert!(self.pending_recovery.is_none());
        let backend = self.backend.clone();
        let attachment_ids = Rc::clone(&self.attachment_ids);
        let selector = self.reattach_selector.clone();
        let cwd = self.snapshot.summary.cwd.clone();
        let old_session = self.session.clone();
        let old_attachment_id = self.attachment.id;
        let future = Box::pin(async move {
            let (mut opened, attachment) =
                request_open_session(&backend, &attachment_ids, &selector, &cwd).await?;
            let detach_warning = match detach_attachment(&old_session, old_attachment_id).await {
                Ok(attached_clients) => {
                    opened.snapshot.summary.attached_clients = attached_clients;
                    None
                }
                Err(error) => Some(format!(
                    "old session attachment could not be detached after recovery: {error}"
                )),
            };
            Ok(RecoveredAttachment {
                opened,
                attachment,
                detach_warning,
            })
        });
        self.pending_recovery = Some(PendingRecovery { reason, future });
    }

    async fn resume_recovery(&mut self) -> Result<SessionUpdate, RpcClientError> {
        let result = self
            .pending_recovery
            .as_mut()
            .expect("recovery must be installed before it is resumed")
            .future
            .as_mut()
            .await;
        let pending = self
            .pending_recovery
            .take()
            .expect("completed recovery must still be installed");
        let recovered = match result {
            Ok(recovered) => recovered,
            Err(error) if is_session_not_found(&error) => {
                return Ok(SessionUpdate::Deleted {
                    session_id: self.snapshot.summary.id,
                    cwd: self.snapshot.summary.cwd.clone(),
                });
            }
            Err(error) => return Err(error),
        };
        let RecoveredAttachment {
            opened,
            attachment,
            detach_warning,
        } = recovered;
        self.sequence_exhausted = pending.reason == RecoveryReason::SequenceOverflow
            && opened.snapshot.sequence == u64::MAX;
        self.session = opened.session;
        self.expected_sequence = opened.snapshot.sequence;
        self.snapshot = opened.snapshot;
        self.attachment = attachment;
        self.pending_warning = detach_warning;
        Ok(SessionUpdate::SnapshotReplaced(Box::new(
            self.snapshot.clone(),
        )))
    }
}

async fn detach_attachment(
    session: &moh_capnp::session::Client,
    attachment_id: u64,
) -> Result<u32, RpcClientError> {
    let mut request = session.detach_request();
    request
        .get()
        .set_attachment_id(convert::write_attachment_id(AttachmentId(attachment_id))?);
    let response = request
        .send()
        .promise
        .await
        .map_err(RpcClientError::connection)?;
    let result = convert::read_detach_result(
        response
            .get()
            .map_err(RpcClientError::connection)?
            .get_result()
            .map_err(RpcClientError::connection)?,
    )?;
    reported_result(result)
}

#[derive(Default)]
struct AttachmentIds {
    last: Cell<u64>,
}

impl AttachmentIds {
    fn allocate(&self) -> Result<u64, RpcClientError> {
        let id = self
            .last
            .get()
            .checked_add(1)
            .ok_or(RpcClientError::AttachmentIdExhausted)?;
        self.last.set(id);
        Ok(id)
    }
}

struct LocalAttachment {
    id: u64,
    observer: Option<moh_capnp::observer::Client>,
    events: mpsc::Receiver<ObserverMessage>,
}

impl LocalAttachment {
    fn new(ids: &AttachmentIds) -> Result<Self, RpcClientError> {
        let id = ids.allocate()?;
        let (events, receiver) = mpsc::channel(OBSERVER_CAPACITY);
        let observer = capnp_rpc::new_client(ObserverImpl {
            attachment_id: id,
            events,
        });
        Ok(Self {
            id,
            observer: Some(observer),
            events: receiver,
        })
    }

    fn take_observer(&mut self) -> moh_capnp::observer::Client {
        self.observer
            .take()
            .expect("a local observer capability is sent exactly once")
    }
}

struct ObserverMessage {
    attachment_id: u64,
    event: Result<SessionEventEnvelope, RpcConversionError>,
}

struct ObserverImpl {
    attachment_id: u64,
    events: mpsc::Sender<ObserverMessage>,
}

impl moh_capnp::observer::Server for ObserverImpl {
    async fn publish(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::observer::PublishParams,
        _: moh_capnp::observer::PublishResults,
    ) -> capnp::Result<()> {
        match convert::read_event_envelope(params.get()?.get_event()?) {
            Ok(event) => self
                .events
                .send(ObserverMessage {
                    attachment_id: self.attachment_id,
                    event: Ok(event),
                })
                .await
                .map_err(|_| capnp::Error::failed("RPC session observer is closed".into())),
            Err(error) => {
                let message = error.to_string();
                let _ = self
                    .events
                    .send(ObserverMessage {
                        attachment_id: self.attachment_id,
                        event: Err(error),
                    })
                    .await;
                Err(capnp::Error::failed(message))
            }
        }
    }
}

async fn request_info(
    backend: &moh_capnp::backend::Client,
) -> Result<ProtocolInfo, RpcClientError> {
    let response = backend
        .get_info_request()
        .send()
        .promise
        .await
        .map_err(RpcClientError::connection)?;
    Ok(convert::read_protocol_info(
        response
            .get()
            .map_err(RpcClientError::connection)?
            .get_info()
            .map_err(RpcClientError::connection)?,
    )?)
}

async fn request_open_session(
    backend: &moh_capnp::backend::Client,
    attachment_ids: &AttachmentIds,
    selector: &SessionSelector,
    cwd_for_title: &[u8],
) -> Result<(convert::OpenSuccess, LocalAttachment), RpcClientError> {
    convert::validate_inbound_field_length(
        cwd_for_title.len(),
        convert::MAX_RPC_CWD_BYTES,
        "cwdForTitle",
    )?;
    let mut attachment = LocalAttachment::new(attachment_ids)?;
    let mut request = backend.open_session_request();
    convert::write_session_selector(request.get().reborrow().init_selector(), selector)?;
    request.get().set_cwd_for_title(cwd_for_title);
    request
        .get()
        .set_attachment_id(convert::write_attachment_id(AttachmentId(attachment.id))?);
    request.get().set_observer(attachment.take_observer());
    let response = request
        .send()
        .promise
        .await
        .map_err(RpcClientError::connection)?;
    let opened = convert::read_open_result(
        response
            .get()
            .map_err(RpcClientError::connection)?
            .get_result()
            .map_err(RpcClientError::connection)?,
    )?;
    Ok((reported_result(opened)?, attachment))
}

fn reported_result<T>(result: Result<T, CommandError>) -> Result<T, RpcClientError> {
    result.map_err(|error| match error.code {
        crate::session::ErrorCode::AmbiguousTitle => RpcClientError::AmbiguousTitle {
            message: error.message,
            ids: error.ids,
        },
        _ => RpcClientError::Command(SessionCommandError::Reported {
            code: error.code,
            message: error.message,
        }),
    })
}

fn is_session_not_found(error: &RpcClientError) -> bool {
    matches!(
        error,
        RpcClientError::Command(SessionCommandError::Reported {
            code: crate::session::ErrorCode::SessionNotFound,
            ..
        })
    )
}

async fn finish_connection(
    shutdown: oneshot::Sender<()>,
    connection_task: JoinHandle<capnp::Result<()>>,
) -> Result<(), RpcClientError> {
    let _ = shutdown.send(());
    connection_task
        .await
        .map_err(RpcClientError::ConnectionTask)?
        .map_err(RpcClientError::connection)
}

#[cfg(test)]
mod tests {
    use super::{AttachmentIds, RpcClientError, validate_protocol_major};

    #[test]
    fn protocol_major_boundary_rejects_both_cross_version_directions() {
        let current = crate::moh_capnp::PROTOCOL_MAJOR;
        assert_eq!(current, 2);
        assert!(matches!(
            validate_protocol_major(current, 1),
            Err(RpcClientError::IncompatibleProtocol {
                client: 2,
                server: 1
            })
        ));
        assert!(matches!(
            validate_protocol_major(1, current),
            Err(RpcClientError::IncompatibleProtocol {
                client: 1,
                server: 2
            })
        ));
    }

    #[test]
    fn attachment_ids_fail_instead_of_wrapping() {
        let ids = AttachmentIds::default();
        ids.last.set(u64::MAX - 1);
        assert_eq!(ids.allocate().unwrap(), u64::MAX);
        assert!(matches!(
            ids.allocate().unwrap_err(),
            RpcClientError::AttachmentIdExhausted
        ));
    }
}
