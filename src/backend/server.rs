//! Backend listener composition and checked connection identity allocation.

use std::{
    collections::HashMap,
    convert::Infallible,
    error::Error,
    fs,
    future::{Future, Ready, ready},
    io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        io::AsFd,
        net::UnixStream as StdUnixStream,
    },
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};

use futures::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use thiserror::Error;
use tokio::net::UnixListener;

use crate::{
    local::{LocalPathError, LocalPaths, ServerConfig},
    providers::codex::AuthFile,
    rpc::{
        convert::ProtocolInfo,
        server::{BackendContext, RpcServerError, serve_connection},
    },
    session::{
        ConnectionId, SessionEngineFactory, SessionManagerError, SessionManagerHandle,
        SessionManagerLifecycleError, SessionRepository,
    },
};

use super::{ActivitySnapshot, ActivityTracker, flush_for_idle_shutdown, wait_for_idle};

/// Failure to allocate another process-local RPC connection identifier.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("backend connection identifier space is exhausted")]
pub struct ConnectionIdExhausted;

/// Checked monotonic allocator for process-local RPC connection identifiers.
pub struct ConnectionIdAllocator {
    next: Option<ConnectionId>,
}

impl Default for ConnectionIdAllocator {
    fn default() -> Self {
        Self::starting_at(ConnectionId(1))
    }
}

impl ConnectionIdAllocator {
    /// Creates an allocator whose first issued identifier is `first`.
    pub fn starting_at(first: ConnectionId) -> Self {
        Self { next: Some(first) }
    }

    /// Issues the next identifier without wrapping or reusing one after exhaustion.
    pub fn allocate(&mut self) -> Result<ConnectionId, ConnectionIdExhausted> {
        let current = self.next.ok_or(ConnectionIdExhausted)?;
        self.next = current.0.checked_add(1).map(ConnectionId);
        Ok(current)
    }
}

/// Asynchronously prepares the per-session runtime factory after the listener is bound.
pub trait BackendRuntimeFactory: 'static {
    /// Ready synchronous factory consumed by the session manager.
    type SessionFactory: SessionEngineFactory;
    /// Typed initialization failure retained behind the backend error boundary.
    type Error: Error + Send + Sync + 'static;
    /// Owned initialization operation polled concurrently with socket accepts.
    type Future: Future<Output = Result<Self::SessionFactory, Self::Error>> + 'static;

    /// Starts runtime initialization without requiring the backend to delay binding.
    fn initialize(self) -> Self::Future;
}

impl<F> BackendRuntimeFactory for F
where
    F: SessionEngineFactory,
{
    type SessionFactory = F;
    type Error = Infallible;
    type Future = Ready<Result<F, Infallible>>;

    fn initialize(self) -> Self::Future {
        ready(Ok(self))
    }
}

/// Dependencies supplied to one local backend process.
pub struct BackendOptions<F> {
    /// Resolved secure local endpoint and state paths.
    pub paths: LocalPaths,
    /// Backend lifecycle configuration.
    pub config: ServerConfig,
    /// Deferred runtime initializer polled after the listener is bound.
    pub runtime_factory: F,
    /// Durable repository shared by every lazy session actor.
    pub repository: Arc<dyn SessionRepository>,
}

/// Successful reason the backend stopped accepting local clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    /// One unchanged eligible activity generation remained idle for the full timeout.
    Idle,
    /// The process received an explicit interrupt or termination signal.
    Signal,
}

/// Failure observed while composing, serving, or completely shutting down the backend.
#[derive(Debug, Error)]
pub enum BackendError {
    /// Secure endpoint preparation or validation failed.
    #[error(transparent)]
    Paths(#[from] LocalPathError),
    /// The Unix listener could not bind its exact endpoint.
    #[error("could not bind local backend socket {path}: {source}")]
    Bind {
        /// Exact socket path.
        path: PathBuf,
        /// Operating-system bind failure.
        #[source]
        source: io::Error,
    },
    /// The bound socket could not be restricted to its owner.
    #[error("could not restrict local backend socket {path} to owner-only access: {source}")]
    SocketPermissions {
        /// Exact socket path.
        path: PathBuf,
        /// Permission update failure.
        #[source]
        source: io::Error,
    },
    /// Runtime initialization failed after the listener became reachable.
    #[error("backend runtime initialization failed")]
    RuntimeInitialization {
        /// Underlying initializer error, intentionally omitted from the top-level display.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// Accepting a queued local connection failed.
    #[error("could not accept a local backend connection: {0}")]
    Accept(#[source] io::Error),
    /// Preparing a retained connection shutdown handle failed.
    #[error("could not retain a local backend connection for orderly shutdown: {0}")]
    RetainConnection(#[source] io::Error),
    /// The checked process-local connection counter was exhausted.
    #[error(transparent)]
    ConnectionId(#[from] ConnectionIdExhausted),
    /// A retained RPC system completed with a mandatory cleanup failure.
    #[error(transparent)]
    Connection(#[from] RpcServerError),
    /// A retained RPC task could not be joined.
    #[error("backend RPC connection task failed: {0}")]
    ConnectionTask(#[source] tokio::task::JoinError),
    /// Shutting down all instantiated session actors/jobs failed.
    #[error("backend session-manager shutdown failed: {0}")]
    ManagerShutdown(#[source] SessionManagerError),
    /// The task owning the session-manager registry failed to join cleanly.
    #[error("backend session-manager lifecycle failed: {0}")]
    ManagerLifecycle(#[source] SessionManagerLifecycleError),
    /// Installing the process signal listener failed.
    #[error("could not install backend shutdown signal handling: {0}")]
    Signal(#[source] io::Error),
    /// The exact socket bound by this backend could not be inspected or unlinked.
    #[error("could not clean up local backend socket {path}: {source}")]
    SocketCleanup {
        /// Exact socket path.
        path: PathBuf,
        /// Inspection or unlink failure.
        #[source]
        source: io::Error,
    },
}

#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

type ConnectionCompletion = LocalBoxFuture<
    'static,
    (
        ConnectionId,
        Result<Result<(), RpcServerError>, tokio::task::JoinError>,
    ),
>;

/// Binds and serves one local backend until an eligible idle deadline or explicit signal.
pub async fn run_backend<F>(options: BackendOptions<F>) -> Result<ShutdownReason, BackendError>
where
    F: BackendRuntimeFactory,
{
    let BackendOptions {
        paths,
        config,
        runtime_factory,
        repository,
    } = options;
    paths.prepare_runtime_dir()?;
    paths.validate_socket_candidate()?;
    let listener =
        UnixListener::bind(paths.socket_path()).map_err(|source| BackendError::Bind {
            path: paths.socket_path().to_path_buf(),
            source,
        })?;
    fs::set_permissions(paths.socket_path(), fs::Permissions::from_mode(0o600)).map_err(
        |source| BackendError::SocketPermissions {
            path: paths.socket_path().to_path_buf(),
            source,
        },
    )?;
    let socket_identity = bound_socket_identity(&paths)?;
    let protocol_info = ProtocolInfo::v2(
        format!(
            "backend-{}-{}-{}",
            std::process::id(),
            socket_identity.device,
            socket_identity.inode
        ),
        repository
            .startup_warnings()
            .iter()
            .map(crate::session::StoreWarning::sanitized_message)
            .collect(),
    );
    let activity = ActivityTracker::new();
    let (context, readiness) = BackendContext::starting(activity.clone(), protocol_info);
    let mut initialization = Box::pin(runtime_factory.initialize());
    let mut initialization_pending = true;
    let mut manager = None;
    let mut lifecycle = None;
    let mut connection_ids = ConnectionIdAllocator::default();
    let mut connection_shutdowns = HashMap::new();
    let mut connection_tasks = FuturesUnordered::new();
    let mut idle_waiter: Pin<Box<dyn Future<Output = super::IdleDeadline>>> =
        Box::pin(std::future::pending());
    let mut signal = Box::pin(shutdown_signal());
    let result = 'serve: loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        if let Err(error) = retain_connection(
                            stream,
                            &mut connection_ids,
                            &context,
                            &mut connection_shutdowns,
                            &mut connection_tasks,
                        ) {
                            break 'serve Err(error);
                        }
                    }
                    Err(source) => break 'serve Err(BackendError::Accept(source)),
                }
            }
            initialized = &mut initialization, if initialization_pending => {
                initialization_pending = false;
                match initialized {
                    Ok(factory) => {
                        let (ready_manager, ready_lifecycle) = SessionManagerHandle::spawn(
                            Arc::clone(&repository),
                            factory,
                            activity.clone(),
                        );
                        readiness.install(ready_manager.clone());
                        manager = Some(ready_manager);
                        lifecycle = Some(ready_lifecycle);
                        idle_waiter = Box::pin(wait_for_idle(
                            activity.subscribe(),
                            config.idle_timeout,
                        ));
                    }
                    Err(source) => {
                        readiness.fail();
                        break 'serve Err(BackendError::RuntimeInitialization {
                            source: Box::new(source),
                        });
                    }
                }
            }
            signal_result = &mut signal => {
                readiness.fail();
                break 'serve signal_result
                    .map(|()| ShutdownReason::Signal)
                    .map_err(BackendError::Signal);
            }
            completed = connection_tasks.next(), if !connection_tasks.is_empty() => {
                let (id, result) = completed.expect("guarded as non-empty");
                connection_shutdowns.remove(&id);
                if let Err(error) = connection_result(result) {
                    break 'serve Err(error);
                }
            }
            deadline = &mut idle_waiter, if manager.is_some() => {
                let queued = match accept_queued_connection(
                    &listener,
                    &mut connection_ids,
                    &context,
                    &mut connection_shutdowns,
                    &mut connection_tasks,
                ) {
                    Ok(queued) => queued,
                    Err(error) => break 'serve Err(error),
                };
                if queued {
                    idle_waiter = Box::pin(wait_for_idle(
                        activity.subscribe(),
                        config.idle_timeout,
                    ));
                    continue 'serve;
                }
                if !same_idle_generation(&activity, deadline.snapshot) {
                    idle_waiter = Box::pin(wait_for_idle(
                        activity.subscribe(),
                        config.idle_timeout,
                    ));
                    continue 'serve;
                }
                let ready_manager = manager.as_ref().expect("guarded as ready");
                if flush_for_idle_shutdown(ready_manager).await.is_err()
                    || !same_idle_generation(&activity, deadline.snapshot)
                {
                    idle_waiter = Box::pin(wait_for_idle(
                        activity.subscribe(),
                        config.idle_timeout,
                    ));
                    continue 'serve;
                }
                let queued = match accept_queued_connection(
                    &listener,
                    &mut connection_ids,
                    &context,
                    &mut connection_shutdowns,
                    &mut connection_tasks,
                ) {
                    Ok(queued) => queued,
                    Err(error) => break 'serve Err(error),
                };
                if queued {
                    idle_waiter = Box::pin(wait_for_idle(
                        activity.subscribe(),
                        config.idle_timeout,
                    ));
                    continue 'serve;
                }
                readiness.fail();
                break 'serve Ok(ShutdownReason::Idle);
            }
        }
    };

    readiness.fail();
    drop(listener);
    let (mut shutdown_error, shutdown_reason) = match result {
        Ok(reason) => (None, Some(reason)),
        Err(error) => (Some(error), None),
    };
    if let Some(ready_manager) = manager.take()
        && let Err(source) = ready_manager.shutdown().await
    {
        record_first(&mut shutdown_error, BackendError::ManagerShutdown(source));
    }
    if let Some(ready_lifecycle) = lifecycle.take()
        && let Err(source) = ready_lifecycle.join().await
    {
        record_first(&mut shutdown_error, BackendError::ManagerLifecycle(source));
    }
    AuthFile::drain_pending_refreshes().await;
    close_and_join_connections(connection_shutdowns, connection_tasks, &mut shutdown_error).await;
    drop(repository);
    if let Err(error) = unlink_bound_socket(&paths, socket_identity) {
        record_first(&mut shutdown_error, error);
    }

    match shutdown_error {
        Some(error) => Err(error),
        None => Ok(shutdown_reason.expect("successful serve outcome retains its reason")),
    }
}

fn bound_socket_identity(paths: &LocalPaths) -> Result<SocketIdentity, BackendError> {
    let metadata = fs::symlink_metadata(paths.socket_path()).map_err(|source| {
        BackendError::SocketCleanup {
            path: paths.socket_path().to_path_buf(),
            source,
        }
    })?;
    if !metadata.file_type().is_socket() {
        return Err(BackendError::SocketCleanup {
            path: paths.socket_path().to_path_buf(),
            source: io::Error::other("bound endpoint is no longer a Unix socket"),
        });
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn retain_connection(
    stream: tokio::net::UnixStream,
    ids: &mut ConnectionIdAllocator,
    context: &BackendContext,
    shutdowns: &mut HashMap<ConnectionId, StdUnixStream>,
    tasks: &mut FuturesUnordered<ConnectionCompletion>,
) -> Result<(), BackendError> {
    let id = ids.allocate()?;
    let shutdown = stream
        .as_fd()
        .try_clone_to_owned()
        .map(StdUnixStream::from)
        .map_err(BackendError::RetainConnection)?;
    let task = serve_connection(stream, id, context.clone());
    shutdowns.insert(id, shutdown);
    tasks.push(async move { (id, task.await) }.boxed_local());
    Ok(())
}

fn accept_queued_connection(
    listener: &UnixListener,
    ids: &mut ConnectionIdAllocator,
    context: &BackendContext,
    shutdowns: &mut HashMap<ConnectionId, StdUnixStream>,
    tasks: &mut FuturesUnordered<ConnectionCompletion>,
) -> Result<bool, BackendError> {
    match listener.accept().now_or_never() {
        Some(Ok((stream, _))) => {
            retain_connection(stream, ids, context, shutdowns, tasks)?;
            Ok(true)
        }
        Some(Err(source)) => Err(BackendError::Accept(source)),
        None => Ok(false),
    }
}

fn same_idle_generation(activity: &ActivityTracker, expected: ActivitySnapshot) -> bool {
    *activity.subscribe().borrow() == expected
        && expected.connections == 0
        && expected.active_runs == 0
        && expected.running_jobs == 0
        && expected.title_tasks == 0
}

async fn close_and_join_connections(
    shutdowns: HashMap<ConnectionId, StdUnixStream>,
    mut tasks: FuturesUnordered<ConnectionCompletion>,
    first_error: &mut Option<BackendError>,
) {
    for shutdown in shutdowns.values() {
        let _ = shutdown.shutdown(std::net::Shutdown::Both);
    }
    while let Some((_, result)) = tasks.next().await {
        if let Err(error) = connection_result(result) {
            record_first(first_error, error);
        }
    }
}

fn connection_result(
    result: Result<Result<(), RpcServerError>, tokio::task::JoinError>,
) -> Result<(), BackendError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(RpcServerError::Rpc(_))) => Ok(()),
        Ok(Err(source @ (RpcServerError::Cleanup(_) | RpcServerError::RpcAndCleanup { .. }))) => {
            Err(BackendError::Connection(source))
        }
        Err(source) => Err(BackendError::ConnectionTask(source)),
    }
}

fn unlink_bound_socket(paths: &LocalPaths, identity: SocketIdentity) -> Result<(), BackendError> {
    let path = paths.socket_path();
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(BackendError::SocketCleanup {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_socket()
        || (metadata.dev(), metadata.ino()) != (identity.device, identity.inode)
    {
        return Ok(());
    }
    fs::remove_file(path).map_err(|source| BackendError::SocketCleanup {
        path: path.to_path_buf(),
        source,
    })
}

fn record_first(first: &mut Option<BackendError>, error: BackendError) {
    if first.is_none() {
        *first = Some(error);
    }
}

async fn shutdown_signal() -> io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}
