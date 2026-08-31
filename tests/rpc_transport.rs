#![cfg(unix)]

mod support;

use std::{cell::RefCell, collections::VecDeque, future, rc::Rc, sync::Arc, time::Duration};

use chrono::Utc;
use futures::AsyncReadExt;
use moh::{
    backend::ActivityTracker,
    harness::EngineEvent,
    moh_capnp,
    rpc::{
        client::{RpcBackendClient, RpcClientError, RpcSessionClient, RpcStartup, SessionUpdate},
        convert::{
            CommandError, ErrorCode, MAX_RPC_CWD_BYTES, MAX_RPC_IDENTIFIER_BYTES,
            MAX_RPC_PROMPT_BYTES, MAX_RPC_TITLE_BYTES, MaterializeSuccess, OpenResult, OpenSuccess,
            ProtocolInfo, StartupResult, StartupSuccess, read_attachment_id, read_command_result,
            read_event_envelope, read_job_list_result, read_job_result, read_materialize_result,
            read_open_result, read_protocol_info, read_session_selector, read_submit_result,
            write_detach_result, write_event_envelope, write_open_result, write_protocol_info,
            write_startup_result,
        },
        server::{BackendContext, RpcServerError, serve_connection},
    },
    runtime::rig::ReasoningLevel,
    session::{
        AttachmentId, ConnectionId, ModelCatalogState, SessionCommandError, SessionEvent,
        SessionEventEnvelope, SessionId, SessionListScope, SessionManagerError,
        SessionManagerHandle, SessionManagerLifecycle, SessionRepository, SessionSelector,
        SessionSettings, SessionSnapshot, SessionStore, SessionSummary, SessionTitle,
    },
};
use tempfile::{TempDir, tempdir};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::{Semaphore, mpsc},
    task::{JoinHandle, LocalSet},
};
use tokio_util::compat::TokioAsyncReadCompatExt;

use support::{ControlledEngineControl, ControlledEngineFactory, FailingRepository};

const RPC_TIMEOUT: Duration = Duration::from_secs(2);

macro_rules! rpc_error {
    ($promise:expr) => {
        match $promise.await {
            Ok(_) => panic!("oversized RPC request unexpectedly succeeded"),
            Err(error) => error,
        }
    };
}

struct RpcFixture {
    _directory: TempDir,
    manager: SessionManagerHandle,
    lifecycle: SessionManagerLifecycle,
    factory: ControlledEngineFactory,
    activity: ActivityTracker,
}

impl RpcFixture {
    async fn new() -> Self {
        let directory = tempdir().unwrap();
        let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
            .await
            .unwrap();
        let repository: Arc<dyn SessionRepository> = Arc::new(opened.store);
        Self::build(directory, repository, ControlledEngineFactory::new())
    }

    fn with_repository(
        repository: Arc<dyn SessionRepository>,
        factory: ControlledEngineFactory,
    ) -> Self {
        Self::build(tempdir().unwrap(), repository, factory)
    }

    fn build(
        directory: TempDir,
        repository: Arc<dyn SessionRepository>,
        factory: ControlledEngineFactory,
    ) -> Self {
        let activity = ActivityTracker::new();
        let (manager, lifecycle) =
            SessionManagerHandle::spawn(repository, factory.clone(), activity.clone());
        Self {
            _directory: directory,
            manager,
            lifecycle,
            factory,
            activity,
        }
    }

    fn context(&self) -> BackendContext {
        self.context_with_info(ProtocolInfo::v2("fixture-instance".into(), vec![]))
    }

    fn context_with_info(&self, protocol_info: ProtocolInfo) -> BackendContext {
        BackendContext::new(self.manager.clone(), self.activity.clone(), protocol_info)
    }

    fn engine(&self) -> ControlledEngineControl {
        self.factory
            .controls()
            .into_iter()
            .next()
            .expect("opening a session must create an engine")
    }

    async fn shutdown(self) {
        self.manager.shutdown().await.unwrap();
        self.lifecycle.join().await.unwrap();
    }
}

struct RpcClient {
    backend: moh_capnp::backend::Client,
    disconnector: capnp_rpc::Disconnector<capnp_rpc::rpc_twoparty_capnp::Side>,
    task: JoinHandle<capnp::Result<()>>,
}

fn start_client(stream: UnixStream) -> RpcClient {
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
    let task = tokio::task::spawn_local(rpc);
    RpcClient {
        backend,
        disconnector,
        task,
    }
}

fn start_pair(
    fixture: &RpcFixture,
    connection_id: ConnectionId,
) -> (
    RpcClient,
    JoinHandle<Result<(), moh::rpc::server::RpcServerError>>,
) {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let server = serve_connection(server_stream, connection_id, fixture.context());
    (start_client(client_stream), server)
}

fn start_raw_pair(
    fixture: &RpcFixture,
    connection_id: ConnectionId,
) -> (UnixStream, JoinHandle<Result<(), RpcServerError>>) {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let server = serve_connection(server_stream, connection_id, fixture.context());
    (client_stream, server)
}

fn start_raw_pair_with_info(
    fixture: &RpcFixture,
    connection_id: ConnectionId,
    protocol_info: ProtocolInfo,
) -> (UnixStream, JoinHandle<Result<(), RpcServerError>>) {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let server = serve_connection(
        server_stream,
        connection_id,
        fixture.context_with_info(protocol_info),
    );
    (client_stream, server)
}

async fn disconnect(
    client: RpcClient,
    server: JoinHandle<Result<(), RpcServerError>>,
) -> Result<(), RpcServerError> {
    let RpcClient {
        backend,
        disconnector,
        task,
    } = client;
    drop(backend);
    tokio::time::timeout(RPC_TIMEOUT, disconnector)
        .await
        .expect("client disconnector timed out")
        .expect("client disconnector failed");
    tokio::time::timeout(RPC_TIMEOUT, task)
        .await
        .expect("client RPC system did not stop after disconnect")
        .expect("client RPC task panicked")
        .expect("client RPC system failed after graceful disconnect");
    await_server(server).await
}

async fn await_server(
    server: JoinHandle<Result<(), RpcServerError>>,
) -> Result<(), RpcServerError> {
    tokio::time::timeout(RPC_TIMEOUT, server)
        .await
        .expect("server RPC system did not observe disconnect")
        .expect("server task panicked")
}

async fn disconnect_typed(
    client: RpcBackendClient,
    server: JoinHandle<Result<(), RpcServerError>>,
) {
    tokio::time::timeout(RPC_TIMEOUT, client.disconnect())
        .await
        .expect("typed client disconnector timed out")
        .expect("typed client disconnect failed");
    await_server(server)
        .await
        .expect("server must complete after typed client disconnect");
}

struct RecordingObserver {
    events: mpsc::UnboundedSender<moh::session::SessionEventEnvelope>,
}

impl moh_capnp::observer::Server for RecordingObserver {
    async fn publish(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::observer::PublishParams,
        _: moh_capnp::observer::PublishResults,
    ) -> capnp::Result<()> {
        let event = read_event_envelope(params.get()?.get_event()?)
            .map_err(|error| capnp::Error::failed(error.to_string()))?;
        self.events
            .send(event)
            .map_err(|_| capnp::Error::failed("recording observer is closed".into()))
    }
}

fn recording_observer() -> (
    moh_capnp::observer::Client,
    mpsc::UnboundedReceiver<moh::session::SessionEventEnvelope>,
) {
    let (events, receiver) = mpsc::unbounded_channel();
    (
        capnp_rpc::new_client(RecordingObserver { events }),
        receiver,
    )
}

struct FailingObserver {
    entered: mpsc::UnboundedSender<()>,
}

impl moh_capnp::observer::Server for FailingObserver {
    async fn publish(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::observer::PublishParams,
        _: moh_capnp::observer::PublishResults,
    ) -> capnp::Result<()> {
        let _ = self.entered.send(());
        Err(capnp::Error::failed("observer rejected event".into()))
    }
}

fn failing_observer() -> (moh_capnp::observer::Client, mpsc::UnboundedReceiver<()>) {
    let (entered, receiver) = mpsc::unbounded_channel();
    (capnp_rpc::new_client(FailingObserver { entered }), receiver)
}

struct SlowObserver {
    entered: mpsc::UnboundedSender<()>,
}

impl moh_capnp::observer::Server for SlowObserver {
    async fn publish(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::observer::PublishParams,
        _: moh_capnp::observer::PublishResults,
    ) -> capnp::Result<()> {
        let _ = self.entered.send(());
        future::pending().await
    }
}

fn slow_observer() -> (moh_capnp::observer::Client, mpsc::UnboundedReceiver<()>) {
    let (entered, receiver) = mpsc::unbounded_channel();
    (capnp_rpc::new_client(SlowObserver { entered }), receiver)
}

struct DummySession {
    detach_failure: bool,
    attached_clients: u32,
}

impl moh_capnp::session::Server for DummySession {
    async fn detach(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::session::DetachParams,
        mut results: moh_capnp::session::DetachResults,
    ) -> capnp::Result<()> {
        if self.detach_failure {
            return Err(capnp::Error::failed("scripted old detach failed".into()));
        }
        write_detach_result(results.get().init_result(), &Ok(self.attached_clients))
            .map_err(|error| capnp::Error::failed(error.to_string()))
    }
}

struct ActorDetachSession {
    manager: SessionManagerHandle,
    session_id: SessionId,
    connection_id: ConnectionId,
}

impl moh_capnp::session::Server for ActorDetachSession {
    async fn detach(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::session::DetachParams,
        mut results: moh_capnp::session::DetachResults,
    ) -> capnp::Result<()> {
        let attachment_id = read_attachment_id(params.get()?.get_attachment_id())
            .map_err(|error| capnp::Error::failed(error.to_string()))?;
        let attached_clients = self
            .manager
            .detach(self.session_id, self.connection_id, attachment_id)
            .await
            .map_err(|_| capnp::Error::failed("actor detach failed".into()))?;
        write_detach_result(results.get().init_result(), &Ok(attached_clients))
            .map_err(|error| capnp::Error::failed(error.to_string()))
    }
}

struct ActorBackedRecoveryBackend {
    manager: SessionManagerHandle,
    session_id: SessionId,
    connection_id: ConnectionId,
    observers: mpsc::UnboundedSender<moh_capnp::observer::Client>,
    retained_events: RefCell<Vec<mpsc::Receiver<SessionEventEnvelope>>>,
}

impl ActorBackedRecoveryBackend {
    async fn attach(
        &self,
        attachment_id: AttachmentId,
        observer: moh_capnp::observer::Client,
    ) -> capnp::Result<OpenSuccess> {
        let attached = self
            .manager
            .open(
                SessionSelector::Id(self.session_id),
                Vec::new(),
                self.connection_id,
                attachment_id,
            )
            .await
            .map_err(|_| capnp::Error::failed("actor open failed".into()))?;
        self.retained_events.borrow_mut().push(attached.events);
        self.observers
            .send(observer)
            .map_err(|_| capnp::Error::failed("observer receiver closed".into()))?;
        Ok(OpenSuccess {
            session: capnp_rpc::new_client(ActorDetachSession {
                manager: self.manager.clone(),
                session_id: self.session_id,
                connection_id: self.connection_id,
            }),
            snapshot: attached.snapshot,
        })
    }
}

impl moh_capnp::backend::Server for ActorBackedRecoveryBackend {
    async fn get_info(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::backend::GetInfoParams,
        mut results: moh_capnp::backend::GetInfoResults,
    ) -> capnp::Result<()> {
        write_protocol_info(
            results.get().init_info(),
            &ProtocolInfo::v2("actor-recovery".into(), vec![]),
        )
        .map_err(|error| capnp::Error::failed(error.to_string()))
    }

    async fn startup(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::StartupParams,
        mut results: moh_capnp::backend::StartupResults,
    ) -> capnp::Result<()> {
        let params = params.get()?;
        let attached = self
            .attach(
                read_attachment_id(params.get_attachment_id())
                    .map_err(|error| capnp::Error::failed(error.to_string()))?,
                params.get_observer()?,
            )
            .await?;
        write_startup_result(
            results.get().init_result(),
            &Ok(StartupSuccess::Attached(Box::new(attached))),
        )
        .map_err(|error| capnp::Error::failed(error.to_string()))
    }

    async fn open_session(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::OpenSessionParams,
        mut results: moh_capnp::backend::OpenSessionResults,
    ) -> capnp::Result<()> {
        let params = params.get()?;
        let selector = read_session_selector(params.get_selector()?)
            .map_err(|error| capnp::Error::failed(error.to_string()))?;
        if selector != SessionSelector::Id(self.session_id) {
            let result: OpenResult = Err(CommandError {
                code: ErrorCode::SessionNotFound,
                message: "unexpected recovery selector".into(),
                ids: Vec::new(),
            });
            return write_open_result(results.get().init_result(), &result)
                .map_err(|error| capnp::Error::failed(error.to_string()));
        }
        let attached = self
            .attach(
                read_attachment_id(params.get_attachment_id())
                    .map_err(|error| capnp::Error::failed(error.to_string()))?,
                params.get_observer()?,
            )
            .await?;
        write_open_result(results.get().init_result(), &Ok(attached))
            .map_err(|error| capnp::Error::failed(error.to_string()))
    }
}

#[derive(Clone)]
struct ScriptedOpenGate {
    entered: Rc<Semaphore>,
    release: Rc<Semaphore>,
}

impl ScriptedOpenGate {
    fn new() -> Self {
        Self {
            entered: Rc::new(Semaphore::new(0)),
            release: Rc::new(Semaphore::new(0)),
        }
    }

    async fn wait_until_entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }

    async fn enter_and_wait(&self) {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

struct ScriptedBackend {
    info: ProtocolInfo,
    opens: RefCell<VecDeque<Result<SessionSnapshot, moh::rpc::convert::CommandError>>>,
    observers: mpsc::UnboundedSender<moh_capnp::observer::Client>,
    reopened: mpsc::UnboundedSender<(SessionSelector, Vec<u8>)>,
    reopen_gate: Option<ScriptedOpenGate>,
    detach_failure: bool,
}

impl ScriptedBackend {
    fn next_open(&self, observer: moh_capnp::observer::Client) -> capnp::Result<OpenResult> {
        let opened = self
            .opens
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| capnp::Error::failed("scripted opens exhausted".into()))?;
        match opened {
            Ok(snapshot) => {
                let attached_clients = snapshot.summary.attached_clients;
                self.observers.send(observer).map_err(|_| {
                    capnp::Error::failed("scripted observer receiver closed".into())
                })?;
                let session = capnp_rpc::new_client(DummySession {
                    detach_failure: self.detach_failure,
                    attached_clients,
                });
                Ok(Ok(OpenSuccess { session, snapshot }))
            }
            Err(error) => Ok(Err(error)),
        }
    }
}

impl moh_capnp::backend::Server for ScriptedBackend {
    async fn get_info(
        self: capnp::capability::Rc<Self>,
        _: moh_capnp::backend::GetInfoParams,
        mut results: moh_capnp::backend::GetInfoResults,
    ) -> capnp::Result<()> {
        write_protocol_info(results.get().init_info(), &self.info)
            .map_err(|error| capnp::Error::failed(error.to_string()))
    }

    async fn startup(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::StartupParams,
        mut results: moh_capnp::backend::StartupResults,
    ) -> capnp::Result<()> {
        let observer = params.get()?.get_observer()?;
        let result = match self.next_open(observer)? {
            Ok(opened) => StartupResult::Ok(StartupSuccess::Attached(Box::new(opened))),
            Err(error) => StartupResult::Err(error),
        };
        write_startup_result(results.get().init_result(), &result)
            .map_err(|error| capnp::Error::failed(error.to_string()))
    }

    async fn open_session(
        self: capnp::capability::Rc<Self>,
        params: moh_capnp::backend::OpenSessionParams,
        mut results: moh_capnp::backend::OpenSessionResults,
    ) -> capnp::Result<()> {
        let params = params.get()?;
        let selector = read_session_selector(params.get_selector()?)
            .map_err(|error| capnp::Error::failed(error.to_string()))?;
        let cwd_for_title = params.get_cwd_for_title()?.to_vec();
        let observer = params.get_observer()?;
        self.reopened
            .send((selector, cwd_for_title))
            .map_err(|_| capnp::Error::failed("scripted reattach receiver closed".into()))?;
        if let Some(gate) = &self.reopen_gate {
            gate.enter_and_wait().await;
        }
        let result = self.next_open(observer)?;
        write_open_result(results.get().init_result(), &result)
            .map_err(|error| capnp::Error::failed(error.to_string()))
    }
}

struct ScriptedPair {
    stream: UnixStream,
    server: JoinHandle<capnp::Result<()>>,
    observers: mpsc::UnboundedReceiver<moh_capnp::observer::Client>,
    reopened: mpsc::UnboundedReceiver<(SessionSelector, Vec<u8>)>,
}

fn start_scripted_pair(snapshots: Vec<SessionSnapshot>) -> ScriptedPair {
    start_scripted_pair_inner(snapshots.into_iter().map(Ok).collect(), None)
}

fn start_scripted_pair_with_gate(
    snapshots: Vec<SessionSnapshot>,
) -> (ScriptedPair, ScriptedOpenGate) {
    let gate = ScriptedOpenGate::new();
    let pair =
        start_scripted_pair_inner(snapshots.into_iter().map(Ok).collect(), Some(gate.clone()));
    (pair, gate)
}

fn start_scripted_pair_with_results(
    opens: Vec<Result<SessionSnapshot, moh::rpc::convert::CommandError>>,
) -> ScriptedPair {
    start_scripted_pair_inner(opens, None)
}

fn start_scripted_pair_with_detach_failure(snapshots: Vec<SessionSnapshot>) -> ScriptedPair {
    start_scripted_pair_inner_with_detach(snapshots.into_iter().map(Ok).collect(), None, true)
}

fn start_scripted_pair_inner(
    opens: Vec<Result<SessionSnapshot, moh::rpc::convert::CommandError>>,
    reopen_gate: Option<ScriptedOpenGate>,
) -> ScriptedPair {
    start_scripted_pair_inner_with_detach(opens, reopen_gate, false)
}

fn start_scripted_pair_inner_with_detach(
    opens: Vec<Result<SessionSnapshot, moh::rpc::convert::CommandError>>,
    reopen_gate: Option<ScriptedOpenGate>,
    detach_failure: bool,
) -> ScriptedPair {
    let (server_stream, stream) = UnixStream::pair().unwrap();
    let (observers, observer_rx) = mpsc::unbounded_channel();
    let (reopened, reopened_rx) = mpsc::unbounded_channel();
    let backend: moh_capnp::backend::Client = capnp_rpc::new_client(ScriptedBackend {
        info: ProtocolInfo::v2("scripted-instance".into(), vec![]),
        opens: RefCell::new(opens.into()),
        observers,
        reopened,
        reopen_gate,
        detach_failure,
    });
    let (reader, writer) = TokioAsyncReadCompatExt::compat(server_stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(backend.client));
    ScriptedPair {
        stream,
        server: tokio::task::spawn_local(rpc),
        observers: observer_rx,
        reopened: reopened_rx,
    }
}

fn start_actor_recovery_pair(
    manager: SessionManagerHandle,
    session_id: SessionId,
    connection_id: ConnectionId,
) -> (
    UnixStream,
    JoinHandle<capnp::Result<()>>,
    mpsc::UnboundedReceiver<moh_capnp::observer::Client>,
) {
    let (server_stream, stream) = UnixStream::pair().unwrap();
    let (observers, observer_rx) = mpsc::unbounded_channel();
    let backend: moh_capnp::backend::Client = capnp_rpc::new_client(ActorBackedRecoveryBackend {
        manager,
        session_id,
        connection_id,
        observers,
        retained_events: RefCell::new(Vec::new()),
    });
    let (reader, writer) = TokioAsyncReadCompatExt::compat(server_stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(backend.client));
    (stream, tokio::task::spawn_local(rpc), observer_rx)
}

fn scripted_snapshot(sequence: u64, cwd: Vec<u8>) -> SessionSnapshot {
    SessionSnapshot {
        summary: SessionSummary {
            id: "session-42".parse::<SessionId>().unwrap(),
            title: SessionTitle::parse("scripted").unwrap(),
            title_revision: 0,
            cwd_display: String::from_utf8_lossy(&cwd).into_owned(),
            cwd,
            running_jobs: 0,
            running: false,
            busy: false,
            attached_clients: 1,
            last_activity: Utc::now(),
        },
        transcript: Vec::new(),
        active_run: None,
        settings: SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Medium,
            context_tokens: 0,
        },
        catalog: ModelCatalogState::Loading,
        plan: Vec::new(),
        jobs: Vec::new(),
        persistence_warning: None,
        sequence,
        busy: false,
    }
}

async fn publish_event(
    observer: &moh_capnp::observer::Client,
    event: &SessionEventEnvelope,
) -> capnp::Result<()> {
    let mut request = observer.publish_request();
    write_event_envelope(request.get().init_event(), event)
        .map_err(|error| capnp::Error::failed(error.to_string()))?;
    request.send().promise.await?;
    Ok(())
}

async fn disconnect_scripted(client: RpcBackendClient, server: JoinHandle<capnp::Result<()>>) {
    tokio::time::timeout(RPC_TIMEOUT, client.disconnect())
        .await
        .expect("typed scripted client disconnector timed out")
        .expect("typed scripted client disconnect failed");
    tokio::time::timeout(RPC_TIMEOUT, server)
        .await
        .expect("scripted server did not observe disconnect")
        .expect("scripted server task panicked")
        .expect("scripted server RPC system failed");
}

async fn startup_attached(backend: &RpcBackendClient, cwd: Vec<u8>) -> RpcSessionClient {
    match backend.startup(cwd).await.unwrap() {
        RpcStartup::Attached(session) => *session,
        RpcStartup::Draft(_) => panic!("scripted startup must return an attachment"),
    }
}

async fn materialize_raw(
    backend: &moh_capnp::backend::Client,
    cwd: &[u8],
    prompt: &str,
    attachment_id: u64,
    observer: moh_capnp::observer::Client,
) -> MaterializeSuccess {
    let mut request = backend.materialize_request();
    {
        let mut params = request.get();
        params.set_cwd(cwd);
        params.set_prompt(prompt);
        params.set_attachment_id(attachment_id);
        let mut settings = params.reborrow().init_settings();
        settings.set_model("gpt-5.6-terra");
        settings.set_reasoning(moh_capnp::ReasoningLevel::Medium);
        settings.set_context_tokens(0);
        params.set_observer(observer);
    }
    let response = request.send().promise.await.unwrap();
    read_materialize_result(response.get().unwrap().get_result().unwrap())
        .unwrap()
        .unwrap()
}

async fn open_raw(
    backend: &moh_capnp::backend::Client,
    session_id: SessionId,
    cwd_for_title: &[u8],
    attachment_id: u64,
    observer: moh_capnp::observer::Client,
) -> OpenSuccess {
    let mut request = backend.open_session_request();
    request
        .get()
        .reborrow()
        .init_selector()
        .set_id(session_id.to_string());
    request.get().set_cwd_for_title(cwd_for_title);
    request.get().set_attachment_id(attachment_id);
    request.get().set_observer(observer);
    let response = request.send().promise.await.unwrap();
    read_open_result(response.get().unwrap().get_result().unwrap())
        .unwrap()
        .unwrap()
}

async fn next_event(
    events: &mut mpsc::UnboundedReceiver<moh::session::SessionEventEnvelope>,
) -> moh::session::SessionEventEnvelope {
    tokio::time::timeout(RPC_TIMEOUT, events.recv())
        .await
        .expect("observer callback timed out")
        .expect("observer callback channel closed")
}

async fn attached_clients(fixture: &RpcFixture, cwd: &[u8]) -> (u32, bool) {
    let summary = fixture
        .manager
        .list(SessionListScope::Project(cwd.to_vec()))
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the opened session must be listed");
    (summary.attached_clients, summary.busy)
}

#[tokio::test(flavor = "current_thread")]
async fn real_unix_stream_negotiates_protocol_info_and_counts_connection_before_poll() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(1));
            assert_eq!(fixture.activity.subscribe().borrow().connections, 1);

            let response = client
                .backend
                .get_info_request()
                .send()
                .promise
                .await
                .unwrap();
            let info = read_protocol_info(response.get().unwrap().get_info().unwrap()).unwrap();
            assert_eq!(info.major, 2);
            assert_eq!(info.minor, 0);
            assert_eq!(info.instance_id, "fixture-instance");
            assert!(info.features.contains(&"backend.listSessions.all".into()));

            disconnect(client, server)
                .await
                .expect("graceful disconnect must complete without an RPC or cleanup error");
            assert_eq!(fixture.activity.subscribe().borrow().connections, 0);
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn draft_defaults_rpc_is_nonselecting_even_with_running_project_work() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let cwd = b"/work/nonselecting-defaults".to_vec();
            let moh::session::StartupResult::Draft(seed) = fixture
                .manager
                .startup(cwd.clone(), ConnectionId(80), AttachmentId(1))
                .await
                .unwrap()
            else {
                panic!("empty fixture unexpectedly selected a session");
            };
            let running = fixture
                .manager
                .materialize_and_submit(
                    cwd.clone(),
                    "running session".into(),
                    seed.settings,
                    ConnectionId(80),
                    AttachmentId(1),
                )
                .await
                .unwrap();
            let running_id = running.session.snapshot.summary.id;
            let (stream, server) = start_raw_pair(&fixture, ConnectionId(81));
            let backend = RpcBackendClient::connect(stream).await.unwrap();

            let defaults = backend.draft_defaults(cwd.clone()).await.unwrap();

            assert_eq!(defaults.cwd, cwd);
            assert_eq!(defaults.settings.model, "gpt-5.6-terra");
            let summaries = fixture.manager.list(SessionListScope::All).await.unwrap();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].id, running_id);
            assert_eq!(summaries[0].attached_clients, 1);

            disconnect_typed(backend, server).await;
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_backend_inputs_fail_before_manager_dispatch_with_sanitized_errors() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(82));

            let (observer, _) = recording_observer();
            let oversized_cwd = vec![b'x'; MAX_RPC_CWD_BYTES + 1];
            let mut request = client.backend.startup_request();
            request.get().set_cwd(&oversized_cwd);
            request.get().set_attachment_id(1);
            request.get().set_observer(observer);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC cwd field is too long"));

            let (observer, _) = recording_observer();
            let oversized_prompt = "x".repeat(MAX_RPC_PROMPT_BYTES + 1);
            let mut request = client.backend.materialize_request();
            {
                let mut params = request.get();
                params.set_cwd(b"/work/bounds");
                params.set_prompt(&oversized_prompt);
                params.set_attachment_id(2);
                let mut settings = params.reborrow().init_settings();
                settings.set_model("gpt-5.6-terra");
                settings.set_reasoning(moh_capnp::ReasoningLevel::Medium);
                settings.set_context_tokens(0);
                params.set_observer(observer);
            }
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC prompt field is too long"));

            let oversized_id = "x".repeat(MAX_RPC_IDENTIFIER_BYTES + 1);
            let (observer, _) = recording_observer();
            let mut request = client.backend.materialize_request();
            {
                let mut params = request.get();
                params.set_cwd(b"/work/bounds");
                params.set_prompt("bounded prompt");
                params.set_attachment_id(3);
                let mut settings = params.reborrow().init_settings();
                settings.set_model(&oversized_id);
                settings.set_reasoning(moh_capnp::ReasoningLevel::Medium);
                settings.set_context_tokens(0);
                params.set_observer(observer);
            }
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC model field is too long"));

            let oversized_title = "x".repeat(MAX_RPC_TITLE_BYTES + 1);
            let (observer, _) = recording_observer();
            let mut request = client.backend.open_session_request();
            request
                .get()
                .reborrow()
                .init_selector()
                .set_title(&oversized_title);
            request.get().set_cwd_for_title(b"/work/bounds");
            request.get().set_attachment_id(4);
            request.get().set_observer(observer);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC title field is too long"));

            let (observer, _) = recording_observer();
            let mut request = client.backend.open_session_request();
            request
                .get()
                .reborrow()
                .init_selector()
                .set_id(&oversized_id);
            request.get().set_cwd_for_title(b"/work/bounds");
            request.get().set_attachment_id(5);
            request.get().set_observer(observer);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC id field is too long"));

            let mut request = client.backend.rename_session_request();
            request.get().set_id("session-1");
            request.get().set_title(&oversized_title);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC title field is too long"));

            assert!(
                fixture
                    .manager
                    .list(SessionListScope::All)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(fixture.factory.controls().is_empty());

            disconnect(client, server).await.unwrap();
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_session_inputs_fail_before_actor_dispatch_with_sanitized_errors() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(83));
            let (observer, _) = recording_observer();
            let opened = materialize_raw(
                &client.backend,
                b"/work/session-bounds",
                "start",
                1,
                observer,
            )
            .await;

            let oversized_prompt = "x".repeat(MAX_RPC_PROMPT_BYTES + 1);
            let mut request = opened.session.submit_request();
            request.get().set_prompt(&oversized_prompt);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC prompt field is too long"));

            let oversized_id = "x".repeat(MAX_RPC_IDENTIFIER_BYTES + 1);
            let mut request = opened.session.select_model_request();
            request.get().set_model_id(&oversized_id);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC modelId field is too long"));

            let mut request = opened.session.cancel_job_request();
            request.get().set_job_id(&oversized_id);
            let error = rpc_error!(request.send().promise);
            assert!(error.extra.ends_with("RPC jobId field is too long"));

            assert_eq!(fixture.factory.controls().len(), 1);

            drop(opened);
            disconnect(client, server).await.unwrap();
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_frame_reports_rpc_error_and_still_clears_connection_activity() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (mut client_stream, server) = start_raw_pair(&fixture, ConnectionId(10));
            client_stream.write_all(&[0xff; 8]).await.unwrap();
            client_stream.shutdown().await.unwrap();

            let error = await_server(server).await.unwrap_err();
            let RpcServerError::Rpc(error) = error else {
                panic!("malformed frame must return the RPC-only variant: {error:?}");
            };
            assert_eq!(error.kind, capnp::ErrorKind::Failed);
            assert!(error.extra.contains("Too few segments: 0"));
            assert_eq!(fixture.activity.subscribe().borrow().connections, 0);
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn graceful_disconnect_reports_cleanup_error_and_clears_connection_activity() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(11));
            client
                .backend
                .get_info_request()
                .send()
                .promise
                .await
                .unwrap();
            fixture.manager.shutdown().await.unwrap();
            fixture.lifecycle.join().await.unwrap();

            let error = disconnect(client, server).await.unwrap_err();
            assert!(matches!(
                error,
                RpcServerError::Cleanup(SessionManagerError::Unavailable)
            ));
            assert_eq!(fixture.activity.subscribe().borrow().connections, 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_frame_preserves_rpc_and_cleanup_errors_and_clears_activity() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (mut client_stream, server) = start_raw_pair(&fixture, ConnectionId(12));
            fixture.manager.shutdown().await.unwrap();
            fixture.lifecycle.join().await.unwrap();
            client_stream.write_all(&[0xff; 8]).await.unwrap();
            client_stream.shutdown().await.unwrap();

            let error = await_server(server).await.unwrap_err();
            let RpcServerError::RpcAndCleanup { rpc, cleanup } = error else {
                panic!(
                    "malformed frame with unavailable manager must preserve both errors: {error:?}"
                );
            };
            assert_eq!(rpc.kind, capnp::ErrorKind::Failed);
            assert!(rpc.extra.contains("Too few segments: 0"));
            assert!(matches!(*cleanup, SessionManagerError::Unavailable));
            assert_eq!(fixture.activity.subscribe().borrow().connections, 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_supports_session_lifecycle() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (first_stream, first_server) = start_raw_pair(&fixture, ConnectionId(2));
            let backend = RpcBackendClient::connect(first_stream).await.unwrap();
            assert_eq!(backend.info().protocol_major, 2);
            assert_eq!(backend.info().protocol_minor, 0);
            assert_eq!(backend.info().instance_id, "fixture-instance");
            assert!(backend.info().features.contains(&"backend.startup".into()));
            assert!(backend.info().features.contains(&"session.detach".into()));

            let project = b"/work/\xffmoh".to_vec();
            let other_project = b"/work/other".to_vec();
            let RpcStartup::Draft(draft) = backend.startup(project.clone()).await.unwrap() else {
                panic!("startup without a durable row must return draft defaults");
            };
            assert_eq!(draft.cwd, project);
            assert!(
                backend
                    .list_sessions(SessionListScope::Project(project.clone()))
                    .await
                    .unwrap()
                    .is_empty(),
                "draft startup must not create a durable row"
            );

            let first = backend
                .materialize(
                    project.clone(),
                    "first topic".into(),
                    draft.settings.clone(),
                )
                .await
                .unwrap();
            assert_eq!(first.run_id, 0);
            assert_eq!(first.session.snapshot().summary.cwd, project);
            assert_eq!(first.session.snapshot().summary.attached_clients, 1);
            let first_id = first.session.snapshot().summary.id;

            let second = backend
                .materialize(
                    project.clone(),
                    "second topic".into(),
                    draft.settings.clone(),
                )
                .await
                .unwrap();
            let second_id = second.session.snapshot().summary.id;
            let outside = backend
                .materialize(
                    other_project.clone(),
                    "outside topic".into(),
                    draft.settings,
                )
                .await
                .unwrap();
            let outside_id = outside.session.snapshot().summary.id;

            let project_sessions = backend
                .list_sessions(SessionListScope::Project(project.clone()))
                .await
                .unwrap();
            assert_eq!(project_sessions.len(), 2);
            assert!(
                project_sessions
                    .iter()
                    .all(|summary| summary.cwd == project)
            );
            let all_sessions = backend.list_sessions(SessionListScope::All).await.unwrap();
            assert_eq!(all_sessions.len(), 3);
            assert!(all_sessions.iter().any(|summary| summary.id == outside_id));

            let (second_stream, second_server) = start_raw_pair(&fixture, ConnectionId(13));
            let second_backend = RpcBackendClient::connect(second_stream).await.unwrap();
            let mut first_for_second = second_backend
                .open_session(SessionSelector::Id(first_id), project.clone())
                .await
                .unwrap();

            let duplicate_title = SessionTitle::parse("duplicate").unwrap();
            backend
                .rename_session(first_id, duplicate_title.clone())
                .await
                .unwrap();
            let SessionUpdate::Event(renamed) = first_for_second.next_update().await.unwrap()
            else {
                panic!("rename must propagate as a contiguous observer event");
            };
            assert!(matches!(
                renamed.event,
                SessionEvent::TitleChanged { ref title, .. } if title == &duplicate_title
            ));
            backend
                .rename_session(second_id, duplicate_title.clone())
                .await
                .unwrap();

            let ambiguous = backend
                .open_session(SessionSelector::Title(duplicate_title), project.clone())
                .await
                .unwrap_err();
            assert!(matches!(
                ambiguous,
                RpcClientError::AmbiguousTitle {
                    ref ids,
                    ref message,
                } if ids == &vec![first_id, second_id]
                    && message.contains("ambiguous")
            ));

            let mut second_for_second = second_backend
                .open_session(SessionSelector::Id(second_id), project.clone())
                .await
                .unwrap();
            first_for_second.detach().await.unwrap();
            let live = backend.list_sessions(SessionListScope::All).await.unwrap();
            assert_eq!(
                live.iter()
                    .find(|summary| summary.id == first_id)
                    .unwrap()
                    .attached_clients,
                1,
                "exact detach must remove only the old switched attachment"
            );
            assert_eq!(
                live.iter()
                    .find(|summary| summary.id == second_id)
                    .unwrap()
                    .attached_clients,
                2,
                "the new switch target must retain both clients"
            );

            first.session.detach().await.unwrap();
            assert_eq!(
                backend
                    .list_sessions(SessionListScope::All)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|summary| summary.id == first_id)
                    .unwrap()
                    .attached_clients,
                0,
                "the connection must remain usable after exact detach"
            );

            backend.delete_session(second_id).await.unwrap();
            assert!(matches!(
                second_for_second.next_update().await.unwrap(),
                SessionUpdate::Event(SessionEventEnvelope {
                    event: SessionEvent::Cancelled { run_id: 0 },
                    ..
                })
            ));
            assert_eq!(
                second_for_second.next_update().await.unwrap(),
                SessionUpdate::Deleted {
                    session_id: second_id,
                    cwd: project.clone(),
                },
                "remote deletion must arrive before the observer closes"
            );
            assert!(
                backend
                    .list_sessions(SessionListScope::All)
                    .await
                    .unwrap()
                    .iter()
                    .all(|summary| summary.id != second_id)
            );

            drop(second.session);
            drop(second_for_second);
            drop(outside.session);
            disconnect_typed(second_backend, second_server).await;
            disconnect_typed(backend, first_server).await;
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_rejects_incompatible_major_and_missing_required_feature_before_open() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;

            let mut incompatible = ProtocolInfo::v2("major-one".into(), vec![]);
            incompatible.major = 1;
            let (stream, server) =
                start_raw_pair_with_info(&fixture, ConnectionId(14), incompatible);
            let error = RpcBackendClient::connect(stream).await.unwrap_err();
            assert!(matches!(
                error,
                RpcClientError::IncompatibleProtocol {
                    client: 2,
                    server: 1
                }
            ));
            await_server(server).await.unwrap();

            let mut missing = ProtocolInfo::v2("missing-feature".into(), vec![]);
            missing
                .features
                .retain(|feature| feature != "session.detach");
            let (stream, server) = start_raw_pair_with_info(&fixture, ConnectionId(15), missing);
            let error = RpcBackendClient::connect(stream).await.unwrap_err();
            assert!(matches!(
                error,
                RpcClientError::MissingFeature { ref feature }
                    if feature == "session.detach"
            ));
            await_server(server).await.unwrap();

            let mut missing_count = ProtocolInfo::v2("missing-detach-count".into(), vec![]);
            missing_count
                .features
                .retain(|feature| feature != "session.detach.attachedClients");
            let (stream, server) =
                start_raw_pair_with_info(&fixture, ConnectionId(16), missing_count);
            let error = RpcBackendClient::connect(stream).await.unwrap_err();
            assert!(matches!(
                error,
                RpcClientError::MissingFeature { ref feature }
                    if feature == "session.detach.attachedClients"
            ));
            await_server(server).await.unwrap();

            assert_eq!(fixture.activity.subscribe().borrow().connections, 0);
            assert!(
                fixture
                    .manager
                    .list(SessionListScope::Project(b"/work/moh".to_vec()))
                    .await
                    .unwrap()
                    .is_empty()
            );
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_ignores_stale_events_and_replaces_snapshot_on_a_sequence_gap() {
    LocalSet::new()
        .run_until(async {
            let cwd = b"/work/\xffmoh".to_vec();
            let mut pair = start_scripted_pair(vec![
                scripted_snapshot(3, cwd.clone()),
                scripted_snapshot(10, cwd.clone()),
            ]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, cwd.clone()).await;
            let first_observer = pair.observers.recv().await.unwrap();

            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: 3,
                    event: SessionEvent::PersistenceWarning(Some("stale".into())),
                },
            )
            .await
            .unwrap();
            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: 4,
                    event: SessionEvent::PersistenceWarning(Some("ordered".into())),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                session.next_update().await.unwrap(),
                SessionUpdate::Event(SessionEventEnvelope {
                    sequence: 4,
                    event: SessionEvent::PersistenceWarning(Some(ref message)),
                }) if message == "ordered"
            ));

            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: 6,
                    event: SessionEvent::PersistenceWarning(Some("must not leak".into())),
                },
            )
            .await
            .unwrap();
            let replacement = session.next_update().await.unwrap();
            assert!(matches!(
                replacement,
                SessionUpdate::SnapshotReplaced(snapshot) if snapshot.sequence == 10
            ));
            assert_eq!(session.snapshot().sequence, 10);

            let (selector, reattach_cwd) = pair.reopened.recv().await.unwrap();
            assert_eq!(selector, SessionSelector::Id(session.snapshot().summary.id));
            assert_eq!(reattach_cwd, cwd);
            let second_observer = pair.observers.recv().await.unwrap();

            let stale_error = publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: 11,
                    event: SessionEvent::PersistenceWarning(Some("old observer".into())),
                },
            )
            .await
            .unwrap_err();
            assert!(stale_error.extra.contains("closed"));
            publish_event(
                &second_observer,
                &SessionEventEnvelope {
                    sequence: 11,
                    event: SessionEvent::PersistenceWarning(Some("fresh observer".into())),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                session.next_update().await.unwrap(),
                SessionUpdate::Event(SessionEventEnvelope {
                    sequence: 11,
                    event: SessionEvent::PersistenceWarning(Some(ref message)),
                }) if message == "fresh observer"
            ));

            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_resumes_gap_recovery_after_the_waiting_future_is_dropped() {
    LocalSet::new()
        .run_until(async {
            let cwd = b"/work/moh".to_vec();
            let (mut pair, gate) = start_scripted_pair_with_gate(vec![
                scripted_snapshot(3, cwd.clone()),
                scripted_snapshot(10, cwd),
            ]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, b"/work/moh".to_vec()).await;
            let first_observer = pair.observers.recv().await.unwrap();

            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: 5,
                    event: SessionEvent::PersistenceWarning(Some("gap".into())),
                },
            )
            .await
            .unwrap();

            let mut update = Box::pin(session.next_update());
            tokio::select! {
                () = gate.wait_until_entered() => {}
                result = &mut update => panic!("recovery completed before the scripted release: {result:?}"),
            }
            drop(update);
            gate.release();

            let replacement = tokio::time::timeout(RPC_TIMEOUT, session.next_update())
                .await
                .expect("pending recovery must resume without another observer event")
                .unwrap();
            assert!(matches!(
                replacement,
                SessionUpdate::SnapshotReplaced(snapshot) if snapshot.sequence == 10
            ));
            let _second_observer = pair.observers.recv().await.unwrap();
            assert!(matches!(
                pair.reopened.recv().await.unwrap().0,
                SessionSelector::Id(_)
            ));
            assert_eq!(
                pair.reopened.try_recv(),
                Err(mpsc::error::TryRecvError::Empty),
                "dropping next_update must not start a duplicate reattachment"
            );

            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn gap_recovery_exact_detaches_the_old_real_actor_attachment() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let cwd = b"/work/actor-recovery".to_vec();
            let moh::session::StartupResult::Draft(defaults) = fixture
                .manager
                .startup(cwd.clone(), ConnectionId(70), AttachmentId(1))
                .await
                .unwrap()
            else {
                panic!("empty fixture unexpectedly selected a session")
            };
            let materialized = fixture
                .manager
                .materialize_and_submit(
                    cwd.clone(),
                    "recover this attachment".into(),
                    defaults.settings,
                    ConnectionId(70),
                    AttachmentId(1),
                )
                .await
                .unwrap();
            let session_id = materialized.session.snapshot.summary.id;
            fixture
                .manager
                .detach(session_id, ConnectionId(70), AttachmentId(1))
                .await
                .unwrap();

            let connection_id = ConnectionId(71);
            let (stream, server, mut observers) =
                start_actor_recovery_pair(fixture.manager.clone(), session_id, connection_id);
            let backend = RpcBackendClient::connect(stream).await.unwrap();
            let mut session = startup_attached(&backend, cwd.clone()).await;
            let first_observer = observers.recv().await.unwrap();
            assert_eq!(attached_clients(&fixture, &cwd).await.0, 1);

            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: session.snapshot().sequence + 2,
                    event: SessionEvent::PersistenceWarning(Some("force recovery".into())),
                },
            )
            .await
            .unwrap();
            let SessionUpdate::SnapshotReplaced(snapshot) = session.next_update().await.unwrap()
            else {
                panic!("gap recovery must install a replacement snapshot");
            };
            assert_eq!(snapshot.summary.id, session_id);
            assert_eq!(snapshot.summary.attached_clients, 1);
            assert_eq!(session.snapshot().summary.attached_clients, 1);
            let _replacement_observer = observers.recv().await.unwrap();

            assert_eq!(
                attached_clients(&fixture, &cwd).await.0,
                1,
                "successful recovery must replace, not accumulate, the actor attachment"
            );

            session.detach().await.unwrap();
            disconnect_scripted(backend, server).await;
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn evicted_old_observer_recovery_installs_authoritative_post_detach_count() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let cwd = b"/work/actor-eviction-recovery".to_vec();
            let moh::session::StartupResult::Draft(defaults) = fixture
                .manager
                .startup(cwd.clone(), ConnectionId(72), AttachmentId(1))
                .await
                .unwrap()
            else {
                panic!("empty fixture unexpectedly selected a session")
            };
            let materialized = fixture
                .manager
                .materialize_and_submit(
                    cwd.clone(),
                    "evict this attachment".into(),
                    defaults.settings,
                    ConnectionId(72),
                    AttachmentId(1),
                )
                .await
                .unwrap();
            let session_id = materialized.session.snapshot.summary.id;
            fixture
                .manager
                .detach(session_id, ConnectionId(72), AttachmentId(1))
                .await
                .unwrap();

            let connection_id = ConnectionId(73);
            let (stream, server, mut observers) =
                start_actor_recovery_pair(fixture.manager.clone(), session_id, connection_id);
            let backend = RpcBackendClient::connect(stream).await.unwrap();
            let mut session = startup_attached(&backend, cwd.clone()).await;
            let first_observer = observers.recv().await.unwrap();

            for _ in 0..130 {
                fixture
                    .engine()
                    .emit(Ok(EngineEvent::AssistantDelta("x".into())));
            }
            tokio::time::timeout(RPC_TIMEOUT, async {
                loop {
                    if attached_clients(&fixture, &cwd).await.0 == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("bounded actor observer was not evicted");

            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: session.snapshot().sequence + 2,
                    event: SessionEvent::PersistenceWarning(Some("force recovery".into())),
                },
            )
            .await
            .unwrap();
            let SessionUpdate::SnapshotReplaced(snapshot) = session.next_update().await.unwrap()
            else {
                panic!("gap recovery must install a replacement snapshot");
            };
            let manager_count = attached_clients(&fixture, &cwd).await.0;
            assert_eq!(manager_count, 1);
            assert_eq!(snapshot.summary.attached_clients, manager_count);
            assert_eq!(session.snapshot().summary.attached_clients, manager_count);
            let _replacement_observer = observers.recv().await.unwrap();

            session.detach().await.unwrap();
            disconnect_scripted(backend, server).await;
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_keeps_the_replacement_and_warns_when_old_detach_fails() {
    LocalSet::new()
        .run_until(async {
            let cwd = b"/work/recovery-warning".to_vec();
            let mut pair = start_scripted_pair_with_detach_failure(vec![
                scripted_snapshot(3, cwd.clone()),
                scripted_snapshot(10, cwd),
            ]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, b"/work/recovery-warning".to_vec()).await;
            let first_observer = pair.observers.recv().await.unwrap();

            publish_event(
                &first_observer,
                &SessionEventEnvelope {
                    sequence: 5,
                    event: SessionEvent::PersistenceWarning(Some("gap".into())),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                session.next_update().await.unwrap(),
                SessionUpdate::SnapshotReplaced(snapshot) if snapshot.sequence == 10
            ));
            assert!(matches!(
                session.next_update().await.unwrap(),
                SessionUpdate::Warning(ref warning)
                    if warning == "old session attachment could not be detached after recovery: RPC connection failed"
            ));
            assert_eq!(session.snapshot().sequence, 10);

            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_treats_missing_gap_recovery_as_deleted() {
    LocalSet::new()
        .run_until(async {
            let cwd = b"/work/moh".to_vec();
            let session_id = "session-42".parse::<SessionId>().unwrap();
            let mut pair = start_scripted_pair_with_results(vec![
                Ok(scripted_snapshot(3, cwd.clone())),
                Err(moh::rpc::convert::CommandError {
                    code: ErrorCode::SessionNotFound,
                    message: "session session-42 was not found".into(),
                    ids: Vec::new(),
                }),
            ]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, cwd.clone()).await;
            let observer = pair.observers.recv().await.unwrap();

            publish_event(
                &observer,
                &SessionEventEnvelope {
                    sequence: 5,
                    event: SessionEvent::JobsChanged(Vec::new()),
                },
            )
            .await
            .unwrap();

            assert_eq!(
                session.next_update().await.unwrap(),
                SessionUpdate::Deleted {
                    session_id,
                    cwd: cwd.clone(),
                }
            );
            assert!(matches!(
                pair.reopened.recv().await.unwrap().0,
                SessionSelector::Id(id) if id == session_id
            ));
            assert_eq!(
                pair.reopened.try_recv(),
                Err(mpsc::error::TryRecvError::Empty),
                "gap deletion recovery must attempt exactly one stable-ID reattachment"
            );

            drop(observer);
            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_treats_missing_observer_close_recovery_as_deleted() {
    LocalSet::new()
        .run_until(async {
            let cwd = b"/work/moh".to_vec();
            let session_id = "session-42".parse::<SessionId>().unwrap();
            let mut pair = start_scripted_pair_with_results(vec![
                Ok(scripted_snapshot(3, cwd.clone())),
                Err(moh::rpc::convert::CommandError {
                    code: ErrorCode::SessionNotFound,
                    message: "session session-42 was not found".into(),
                    ids: Vec::new(),
                }),
            ]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, cwd.clone()).await;
            let observer = pair.observers.recv().await.unwrap();
            drop(observer);

            let update = tokio::time::timeout(RPC_TIMEOUT, session.next_update())
                .await
                .expect("closed observer recovery timed out")
                .unwrap();
            assert_eq!(
                update,
                SessionUpdate::Deleted {
                    session_id,
                    cwd: cwd.clone(),
                }
            );
            assert!(matches!(
                pair.reopened.recv().await.unwrap().0,
                SessionSelector::Id(id) if id == session_id
            ));
            assert_eq!(
                pair.reopened.try_recv(),
                Err(mpsc::error::TryRecvError::Empty),
                "closed observer recovery must attempt exactly one stable-ID reattachment"
            );

            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn failed_delete_observer_close_rematerializes_the_stable_session() {
    LocalSet::new()
        .run_until(async {
            let repository = FailingRepository::default();
            let fixture = RpcFixture::with_repository(
                Arc::new(repository.clone()),
                ControlledEngineFactory::new(),
            );
            let (stream, server) = start_raw_pair(&fixture, ConnectionId(16));
            let backend = RpcBackendClient::connect(stream).await.unwrap();
            let cwd = b"/work/delete-failure".to_vec();
            let RpcStartup::Draft(draft) = backend.startup(cwd.clone()).await.unwrap() else {
                panic!("empty failing repository must start as a draft");
            };
            let mut session = backend
                .materialize(cwd.clone(), "keep me".into(), draft.settings)
                .await
                .unwrap()
                .session;
            let session_id = session.snapshot().summary.id;

            repository.fail_deletes(true);
            let error = backend.delete_session(session_id).await.unwrap_err();
            assert!(matches!(
                error,
                RpcClientError::Command(SessionCommandError::Reported {
                    code: ErrorCode::Persistence,
                    ..
                })
            ));
            assert!(matches!(
                session.next_update().await.unwrap(),
                SessionUpdate::Event(SessionEventEnvelope {
                    event: SessionEvent::Cancelled { run_id: 0 },
                    ..
                })
            ));
            let replacement = tokio::time::timeout(RPC_TIMEOUT, session.next_update())
                .await
                .expect("failed-delete observer recovery timed out")
                .unwrap();
            assert!(matches!(
                replacement,
                SessionUpdate::SnapshotReplaced(snapshot)
                    if snapshot.summary.id == session_id && snapshot.summary.cwd == cwd
            ));
            assert_eq!(
                fixture
                    .manager
                    .list(SessionListScope::All)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|summary| summary.id == session_id)
                    .unwrap()
                    .attached_clients,
                1,
                "recovery must install one replacement attachment"
            );

            repository.fail_deletes(false);
            drop(session);
            disconnect_typed(backend, server).await;
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_reports_sequence_exhaustion_after_one_fresh_max_snapshot() {
    LocalSet::new()
        .run_until(async {
            let cwd = b"/work/moh".to_vec();
            let mut pair = start_scripted_pair(vec![
                scripted_snapshot(u64::MAX, cwd.clone()),
                scripted_snapshot(u64::MAX, cwd.clone()),
                scripted_snapshot(u64::MAX, cwd),
            ]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, b"/work/moh".to_vec()).await;
            let _first_observer = pair.observers.recv().await.unwrap();

            assert!(matches!(
                session.next_update().await.unwrap(),
                SessionUpdate::SnapshotReplaced(snapshot) if snapshot.sequence == u64::MAX
            ));
            let _second_observer = pair.observers.recv().await.unwrap();
            assert!(matches!(
                pair.reopened.recv().await.unwrap().0,
                SessionSelector::Id(_)
            ));
            assert!(matches!(
                session.next_update().await.unwrap_err(),
                RpcClientError::SequenceExhausted
            ));
            assert_eq!(
                pair.reopened.try_recv(),
                Err(mpsc::error::TryRecvError::Empty),
                "terminal sequence exhaustion must not start another reattachment"
            );
            assert!(
                matches!(
                    pair.observers.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ),
                "terminal sequence exhaustion must not install another observer"
            );

            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn typed_client_surfaces_observer_wire_conversion_failure_for_that_attachment() {
    LocalSet::new()
        .run_until(async {
            let mut pair = start_scripted_pair(vec![scripted_snapshot(3, b"/work/moh".to_vec())]);
            let backend = RpcBackendClient::connect(pair.stream).await.unwrap();
            let mut session = startup_attached(&backend, b"/work/moh".to_vec()).await;
            let observer = pair.observers.recv().await.unwrap();

            let mut request = observer.publish_request();
            let mut envelope = request.get().init_event();
            envelope.set_sequence(4);
            let mut event = envelope.init_context_usage();
            event.set_run_id(1);
            event.set_input_tokens(10);
            event.set_last_activity("not-a-timestamp");
            let callback = request.send().promise;
            let (callback, update) = tokio::join!(callback, session.next_update());

            let callback = match callback {
                Ok(_) => panic!("invalid observer event must fail its callback"),
                Err(error) => error,
            };
            assert!(callback.extra.contains("lastActivity"));
            assert!(matches!(
                update.unwrap_err(),
                RpcClientError::Conversion(
                    moh::rpc::convert::RpcConversionError::InvalidTimestamp {
                        field: "lastActivity"
                    }
                )
            ));

            drop(session);
            disconnect_scripted(backend, pair.server).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn disconnect_detaches_observers_without_cancelling_the_active_run() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(3));
            let (observer, _events) = recording_observer();
            let opened =
                materialize_raw(&client.backend, b"/work/moh", "keep running", 1, observer).await;

            drop(opened);
            disconnect(client, server)
                .await
                .expect("graceful disconnect must complete without an RPC or cleanup error");
            assert_eq!(attached_clients(&fixture, b"/work/moh").await, (0, true));
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn deleted_callback_is_delivered_before_observer_pump_closes() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(17));
            let (observer, mut events) = recording_observer();
            let opened = materialize_raw(
                &client.backend,
                b"/work/delete-order",
                "delete me",
                1,
                observer,
            )
            .await;
            let session_id = opened.snapshot.summary.id;

            let mut request = client.backend.delete_session_request();
            request.get().set_id(session_id.to_string());
            let response = request.send().promise.await.unwrap();
            assert!(
                read_command_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .is_ok()
            );
            assert!(matches!(
                next_event(&mut events).await.event,
                SessionEvent::Cancelled { run_id: 0 }
            ));
            assert!(matches!(
                next_event(&mut events).await.event,
                SessionEvent::Deleted { session_id: deleted } if deleted == session_id
            ));
            assert_eq!(
                tokio::time::timeout(RPC_TIMEOUT, events.recv())
                    .await
                    .expect("observer pump did not close after Deleted"),
                None
            );

            drop(opened);
            disconnect(client, server)
                .await
                .expect("deleted-session connection must disconnect cleanly");
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn backend_rejects_zero_attachment_ids_before_manager_calls() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(4));
            let cwd = b"/work/\xffmoh";

            let (observer, _events) = recording_observer();
            let mut request = client.backend.startup_request();
            request.get().set_cwd(cwd);
            request.get().set_attachment_id(0);
            request.get().set_observer(observer);
            let error = match request.send().promise.await {
                Ok(_) => panic!("zero startup attachment ID must fail the RPC call"),
                Err(error) => error,
            };
            assert!(error.extra.contains("attachment identifier"));

            let (observer, _events) = recording_observer();
            let mut request = client.backend.materialize_request();
            {
                let mut params = request.get();
                params.set_cwd(cwd);
                params.set_prompt("must not persist");
                params.set_attachment_id(0);
                let mut settings = params.reborrow().init_settings();
                settings.set_model("gpt-5.6-terra");
                settings.set_reasoning(moh_capnp::ReasoningLevel::Medium);
                settings.set_context_tokens(0);
                params.set_observer(observer);
            }
            let error = match request.send().promise.await {
                Ok(_) => panic!("zero materialize attachment ID must fail the RPC call"),
                Err(error) => error,
            };
            assert!(error.extra.contains("attachment identifier"));
            assert!(
                fixture
                    .manager
                    .list(SessionListScope::All)
                    .await
                    .unwrap()
                    .is_empty(),
                "zero must be rejected before materialization reaches the manager"
            );

            let (observer, _events) = recording_observer();
            let mut request = client.backend.open_session_request();
            request.get().reborrow().init_selector().set_id("session-1");
            request.get().set_cwd_for_title(cwd);
            request.get().set_attachment_id(0);
            request.get().set_observer(observer);
            let error = match request.send().promise.await {
                Ok(_) => panic!("zero open attachment ID must fail the RPC call"),
                Err(error) => error,
            };
            assert!(error.extra.contains("attachment identifier"));

            let (observer, _events) = recording_observer();
            let opened = materialize_raw(&client.backend, cwd, "persist", 1, observer).await;
            let mut request = opened.session.detach_request();
            request.get().set_attachment_id(0);
            let error = match request.send().promise.await {
                Ok(_) => panic!("zero detach attachment ID must fail the RPC call"),
                Err(error) => error,
            };
            assert!(error.extra.contains("attachment identifier"));
            assert_eq!(attached_clients(&fixture, cwd).await.0, 1);

            drop(opened);
            disconnect(client, server)
                .await
                .expect("graceful disconnect must complete without an RPC or cleanup error");
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn every_session_method_uses_result_unions_for_ordinary_domain_outcomes() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (client, server) = start_pair(&fixture, ConnectionId(5));
            let (observer, _events) = recording_observer();
            let opened =
                materialize_raw(&client.backend, b"/work/moh", "materialize", 1, observer).await;

            let response = opened
                .session
                .cancel_request()
                .send()
                .promise
                .await
                .unwrap();
            assert!(
                read_command_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .is_ok()
            );

            let response = opened
                .session
                .cancel_request()
                .send()
                .promise
                .await
                .unwrap();
            assert_eq!(
                read_command_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .unwrap_err()
                    .code,
                ErrorCode::NotRunning
            );

            let mut request = opened.session.select_model_request();
            request.get().set_model_id("missing");
            let response = request.send().promise.await.unwrap();
            assert!(
                read_command_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .is_ok()
            );

            let mut request = opened.session.select_reasoning_request();
            request.get().set_level(moh_capnp::ReasoningLevel::Max);
            let response = request.send().promise.await.unwrap();
            assert_eq!(
                read_command_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .unwrap_err()
                    .code,
                ErrorCode::UnsupportedReasoning
            );

            let response = opened
                .session
                .list_jobs_request()
                .send()
                .promise
                .await
                .unwrap();
            assert!(
                read_job_list_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .unwrap()
                    .is_empty()
            );

            let mut request = opened.session.cancel_job_request();
            request.get().set_job_id("bad");
            let response = request.send().promise.await.unwrap();
            assert_eq!(
                read_job_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidArgument
            );

            let mut request = opened.session.submit_request();
            request.get().set_prompt("first");
            let response = request.send().promise.await.unwrap();
            assert_eq!(
                read_submit_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .unwrap(),
                1
            );
            let mut request = opened.session.submit_request();
            request.get().set_prompt("second");
            let response = request.send().promise.await.unwrap();
            assert_eq!(
                read_submit_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .unwrap_err()
                    .code,
                ErrorCode::Busy
            );

            let response = opened
                .session
                .cancel_request()
                .send()
                .promise
                .await
                .unwrap();
            assert!(
                read_command_result(response.get().unwrap().get_result().unwrap())
                    .unwrap()
                    .is_ok()
            );

            drop(opened);
            disconnect(client, server)
                .await
                .expect("graceful disconnect must complete without an RPC or cleanup error");
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn failed_callback_detaches_only_its_attachment() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (failed_client, failed_server) = start_pair(&fixture, ConnectionId(6));
            let (recording_client, recording_server) = start_pair(&fixture, ConnectionId(7));
            assert_eq!(fixture.activity.subscribe().borrow().connections, 2);
            let (failed, mut failed_entered) = failing_observer();
            let failed_opened =
                materialize_raw(&failed_client.backend, b"/work/moh", "stream", 1, failed).await;
            let (recording, mut events) = recording_observer();
            let recording_opened = open_raw(
                &recording_client.backend,
                failed_opened.snapshot.summary.id,
                b"/work/moh",
                1,
                recording,
            )
            .await;
            fixture
                .engine()
                .emit(Ok(EngineEvent::AssistantDelta("first".into())));
            let _ = next_event(&mut events).await;
            tokio::time::timeout(RPC_TIMEOUT, failed_entered.recv())
                .await
                .expect("failing callback was not invoked")
                .expect("failing callback signal closed");

            fixture
                .engine()
                .emit(Ok(EngineEvent::AssistantDelta("still live".into())));
            let delta = next_event(&mut events).await;
            assert!(matches!(delta.event, SessionEvent::AssistantDelta { .. }));
            assert_eq!(attached_clients(&fixture, b"/work/moh").await, (1, true));

            drop(failed_opened);
            drop(recording_opened);
            disconnect(failed_client, failed_server)
                .await
                .expect("failed-observer connection must disconnect cleanly");
            disconnect(recording_client, recording_server)
                .await
                .expect("recording-observer connection must disconnect cleanly");
            fixture.shutdown().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn slow_callback_is_bounded_and_does_not_stall_another_client_or_the_run() {
    LocalSet::new()
        .run_until(async {
            let fixture = RpcFixture::new().await;
            let (slow_client, slow_server) = start_pair(&fixture, ConnectionId(8));
            let (recording_client, recording_server) = start_pair(&fixture, ConnectionId(9));
            assert_eq!(fixture.activity.subscribe().borrow().connections, 2);
            let (slow, mut slow_entered) = slow_observer();
            let slow_opened =
                materialize_raw(&slow_client.backend, b"/work/moh", "stream", 1, slow).await;
            let (recording, mut events) = recording_observer();
            let recording_opened = open_raw(
                &recording_client.backend,
                slow_opened.snapshot.summary.id,
                b"/work/moh",
                1,
                recording,
            )
            .await;
            fixture
                .engine()
                .emit(Ok(EngineEvent::AssistantDelta("first".into())));
            let _ = next_event(&mut events).await;
            tokio::time::timeout(RPC_TIMEOUT, slow_entered.recv())
                .await
                .expect("slow callback was not invoked")
                .expect("slow callback signal closed");

            for _ in 0..129 {
                fixture
                    .engine()
                    .emit(Ok(EngineEvent::AssistantDelta("x".into())));
                let delta = next_event(&mut events).await;
                assert!(matches!(delta.event, SessionEvent::AssistantDelta { .. }));
            }
            assert_eq!(attached_clients(&fixture, b"/work/moh").await, (1, true));

            drop(slow_opened);
            drop(recording_opened);
            disconnect(slow_client, slow_server)
                .await
                .expect("slow-observer connection must disconnect cleanly");
            disconnect(recording_client, recording_server)
                .await
                .expect("recording-observer connection must disconnect cleanly");
            fixture.shutdown().await;
        })
        .await;
}
