#![cfg(unix)]

mod support;

use std::{
    fs,
    future::Future,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::{PermissionsExt, symlink},
    pin::Pin,
    sync::{Arc, Mutex, mpsc as std_mpsc},
    thread,
    time::Duration,
};

use moh::{
    backend::{
        ActivityTracker, BackendError, BackendOptions, BackendRuntimeFactory,
        ConnectionIdAllocator, ShutdownReason, run_backend,
    },
    harness::EngineEvent,
    local::{
        BackendCommand, LocalLaunchError, LocalPathError, LocalPaths, MohConfig, PathRoots,
        connect_or_spawn,
    },
    rpc::{
        convert::ProtocolInfo,
        server::{BackendContext, RpcServerError, serve_connection},
    },
    runtime::rig::{AgentConfig, ReasoningLevel},
    server::{CodexBackendRuntimeFactory, detached_server_command, foreground_server_command},
    session::{
        ConnectionId, ErrorCode, ModelCatalogState, ModelInfoDto, SessionCommandError,
        SessionEngineFactory, SessionManagerHandle, SessionRepository, SessionStore, StoreWarning,
    },
    tools::{JobDetails, JobKind, JobRegistryError, JobState, ReadConfig, ReadServiceFactory},
};
use nix::unistd::Uid;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

use support::{ControlledEngineFactory, FailingRepository};

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

type RuntimeFuture =
    Pin<Box<dyn Future<Output = Result<ControlledEngineFactory, std::io::Error>> + 'static>>;

struct DeferredRuntimeFactory {
    factory: ControlledEngineFactory,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<Result<(), std::io::Error>>,
}

impl BackendRuntimeFactory for DeferredRuntimeFactory {
    type SessionFactory = ControlledEngineFactory;
    type Error = std::io::Error;
    type Future = RuntimeFuture;

    fn initialize(self) -> Self::Future {
        Box::pin(async move {
            self.entered.send(()).unwrap();
            self.release.await.map_err(std::io::Error::other)??;
            Ok(self.factory)
        })
    }
}

struct DeferredBackend {
    paths: LocalPaths,
    factory: ControlledEngineFactory,
    entered: oneshot::Receiver<()>,
    release: oneshot::Sender<Result<(), std::io::Error>>,
    task: JoinHandle<Result<ShutdownReason, BackendError>>,
}

fn spawn_deferred_backend(
    directory: &TempDir,
    repository: Arc<dyn SessionRepository>,
    idle_timeout: Duration,
) -> DeferredBackend {
    spawn_deferred_backend_with_factory(
        directory,
        repository,
        idle_timeout,
        ControlledEngineFactory::new(),
    )
}

fn spawn_deferred_backend_with_factory(
    directory: &TempDir,
    repository: Arc<dyn SessionRepository>,
    idle_timeout: Duration,
    factory: ControlledEngineFactory,
) -> DeferredBackend {
    let paths = local_paths(directory);
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let runtime_factory = DeferredRuntimeFactory {
        factory: factory.clone(),
        entered,
        release: release_rx,
    };
    let task = tokio::task::spawn_local(run_backend(BackendOptions {
        paths: paths.clone(),
        config: moh::local::ServerConfig { idle_timeout },
        runtime_factory,
        repository,
    }));
    DeferredBackend {
        paths,
        factory,
        entered: entered_rx,
        release,
        task,
    }
}

async fn connect_typed(paths: &LocalPaths) -> moh::rpc::client::RpcBackendClient {
    let stream = UnixStream::connect(paths.socket_path()).await.unwrap();
    moh::rpc::client::RpcBackendClient::connect(stream)
        .await
        .unwrap()
}

async fn open_when_ready(
    client: &moh::rpc::client::RpcBackendClient,
) -> moh::rpc::client::RpcSessionClient {
    tokio::time::resume();
    let opened = tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            match client.startup(b"/work/moh".to_vec()).await {
                Ok(moh::rpc::client::RpcStartup::Attached(session)) => return *session,
                Ok(moh::rpc::client::RpcStartup::Draft(defaults)) => {
                    let materialized = client
                        .materialize(
                            defaults.cwd,
                            "local launch bootstrap".into(),
                            defaults.settings,
                        )
                        .await
                        .unwrap();
                    materialized.session.cancel().await.unwrap();
                    return materialized.session;
                }
                Err(moh::rpc::client::RpcClientError::Command(SessionCommandError::Reported {
                    code: ErrorCode::BackendStarting,
                    ..
                })) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected open failure after release: {error}"),
            }
        }
    })
    .await
    .expect("backend did not become ready");
    tokio::time::pause();
    opened
}

#[derive(Debug)]
struct TestJobDetails(&'static str);

impl JobDetails for TestJobDetails {
    fn render(&self) -> String {
        self.0.into()
    }
}

fn local_paths(directory: &TempDir) -> LocalPaths {
    LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(directory.path().join("runtime")),
        temp_dir: directory.path().join("tmp"),
        config_dir: directory.path().join("config"),
        state_dir: directory.path().join("state"),
        effective_uid: Uid::effective().as_raw(),
    })
}

struct RpcServerFixture {
    ready: oneshot::Receiver<()>,
    accepted: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

fn spawn_rpc_server(
    paths: LocalPaths,
    connections: usize,
    start: Option<oneshot::Receiver<()>>,
    handshake_gate: Option<Arc<Semaphore>>,
) -> RpcServerFixture {
    spawn_rpc_server_with_protocol_major(
        paths,
        connections,
        start,
        handshake_gate,
        moh::rpc::moh_capnp::PROTOCOL_MAJOR,
    )
}

fn spawn_rpc_server_with_protocol_major(
    paths: LocalPaths,
    connections: usize,
    start: Option<oneshot::Receiver<()>>,
    handshake_gate: Option<Arc<Semaphore>>,
    protocol_major: u16,
) -> RpcServerFixture {
    let (ready, ready_rx) = oneshot::channel();
    let (accepted, accepted_rx) = mpsc::unbounded_channel();
    let task = tokio::task::spawn_local(async move {
        if let Some(start) = start {
            start.await.unwrap();
        }
        paths.prepare_runtime_dir().unwrap();
        let listener = UnixListener::bind(paths.socket_path()).unwrap();
        let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
        let activity = ActivityTracker::new();
        let (manager, lifecycle) = SessionManagerHandle::spawn(
            repository,
            ControlledEngineFactory::new(),
            activity.clone(),
        );
        let mut protocol_info = ProtocolInfo::v2("fixture-instance".into(), vec![]);
        protocol_info.major = protocol_major;
        let context = BackendContext::new(manager.clone(), activity, protocol_info);
        ready.send(()).unwrap();

        let mut connection_tasks = Vec::new();
        for id in 1..=connections {
            let (stream, _) = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            if let Some(gate) = &handshake_gate {
                gate.acquire().await.unwrap().forget();
            }
            connection_tasks.push(serve_connection(
                stream,
                ConnectionId(u64::try_from(id).unwrap()),
                context.clone(),
            ));
        }
        drop(listener);
        for task in connection_tasks {
            task.await.unwrap().unwrap();
        }
        manager.shutdown().await.unwrap();
        lifecycle.join().await.unwrap();
    });
    RpcServerFixture {
        ready: ready_rx,
        accepted: accepted_rx,
        task,
    }
}

async fn await_server(server: RpcServerFixture) {
    tokio::time::timeout(TEST_TIMEOUT, server.task)
        .await
        .expect("RPC fixture did not stop")
        .expect("RPC fixture panicked");
}

#[test]
fn connection_ids_issue_the_maximum_once_then_report_exhaustion() {
    let mut ids = ConnectionIdAllocator::starting_at(ConnectionId(u64::MAX));

    assert_eq!(ids.allocate().unwrap(), ConnectionId(u64::MAX));
    assert!(ids.allocate().is_err());
    assert!(ids.allocate().is_err());
}

#[tokio::test]
async fn a_regular_file_at_the_endpoint_is_rejected_and_left_byte_identical() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            let original = b"not a socket\nwith retained bytes";
            fs::write(paths.socket_path(), original).unwrap();
            let spawn = BackendCommand::injected(Arc::new(|_| {
                panic!("a wrong-type endpoint must be rejected before spawning")
            }));

            let error = connect_or_spawn(paths.clone(), spawn).await.unwrap_err();

            assert!(matches!(error, LocalLaunchError::Endpoint(_)));
            assert_eq!(fs::read(paths.socket_path()).unwrap(), original);
        })
        .await;
}

#[tokio::test]
async fn a_symlink_at_the_endpoint_is_rejected_without_touching_it_or_its_target() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            let target = directory.path().join("target");
            let original = b"target bytes stay intact";
            fs::write(&target, original).unwrap();
            symlink(&target, paths.socket_path()).unwrap();
            let original_link = fs::read_link(paths.socket_path()).unwrap();
            let spawn = BackendCommand::injected(Arc::new(|_| {
                panic!("a symlink endpoint must be rejected before spawning")
            }));

            let error = connect_or_spawn(paths.clone(), spawn).await.unwrap_err();

            assert!(matches!(error, LocalLaunchError::Endpoint(_)));
            assert_eq!(fs::read_link(paths.socket_path()).unwrap(), original_link);
            assert_eq!(fs::read(target).unwrap(), original);
        })
        .await;
}

#[tokio::test]
async fn a_symlink_to_a_reachable_compatible_listener_is_still_rejected() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let target_directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let target_paths = local_paths(&target_directory);
            paths.prepare_runtime_dir().unwrap();
            let mut server = spawn_rpc_server(target_paths.clone(), 1, None, None);
            (&mut server.ready).await.unwrap();
            symlink(target_paths.socket_path(), paths.socket_path()).unwrap();
            let spawn = BackendCommand::injected(Arc::new(|_| {
                panic!("a symlink endpoint must be rejected before spawning")
            }));

            let error = connect_or_spawn(paths.clone(), spawn).await.unwrap_err();

            assert!(matches!(error, LocalLaunchError::Endpoint(_)));
            assert_eq!(
                fs::read_link(paths.socket_path()).unwrap(),
                target_paths.socket_path()
            );
            let target_client = connect_typed(&target_paths).await;
            target_client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn a_reachable_compatible_listener_is_reused_without_spawning() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let mut server = spawn_rpc_server(paths.clone(), 1, None, None);
            (&mut server.ready).await.unwrap();
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                Ok(())
            }));

            let client = connect_or_spawn(paths, command).await.unwrap();

            assert_eq!(client.info().instance_id, "fixture-instance");
            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 0);
            client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn a_reachable_protocol_v1_backend_is_rejected_without_spawning_or_unlinking() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let mut server = spawn_rpc_server_with_protocol_major(paths.clone(), 1, None, None, 1);
            (&mut server.ready).await.unwrap();
            let socket_metadata = fs::symlink_metadata(paths.socket_path()).unwrap();
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                Ok(())
            }));

            let error = connect_or_spawn(paths.clone(), command).await.unwrap_err();

            assert!(matches!(
                error,
                LocalLaunchError::Rpc(moh::rpc::client::RpcClientError::IncompatibleProtocol {
                    client: 2,
                    server: 1
                })
            ));
            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 0);
            let retained = fs::symlink_metadata(paths.socket_path()).unwrap();
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                (retained.dev(), retained.ino()),
                (socket_metadata.dev(), socket_metadata.ino())
            );
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn connect_returns_only_after_the_compatible_handshake_completes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let gate = Arc::new(Semaphore::new(0));
            let mut server = spawn_rpc_server(paths.clone(), 1, None, Some(Arc::clone(&gate)));
            (&mut server.ready).await.unwrap();
            let command = BackendCommand::injected(Arc::new(|_| {
                panic!("a reachable listener must not spawn")
            }));
            let client = tokio::task::spawn_local(connect_or_spawn(paths, command));

            tokio::time::timeout(TEST_TIMEOUT, server.accepted.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(!client.is_finished());
            gate.add_permits(1);
            let client = tokio::time::timeout(TEST_TIMEOUT, client)
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn a_stale_owned_socket_is_removed_and_one_backend_is_spawned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            drop(UnixListener::bind(paths.socket_path()).unwrap());
            let stale = fs::symlink_metadata(paths.socket_path()).unwrap();
            let (start, start_rx) = oneshot::channel();
            let server = spawn_rpc_server(paths.clone(), 1, Some(start_rx), None);
            let start = Arc::new(std::sync::Mutex::new(Some(start)));
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                start.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok(())
            }));

            let client = connect_or_spawn(paths.clone(), command).await.unwrap();

            let live = fs::symlink_metadata(paths.socket_path()).unwrap();
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                (stale.dev(), stale.ino(), stale.ctime(), stale.ctime_nsec()),
                (live.dev(), live.ino(), live.ctime(), live.ctime_nsec())
            );
            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 1);
            client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn a_transient_reset_is_retried_before_the_backend_is_spawned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            let resetter = UnixListener::bind(paths.socket_path()).unwrap();
            let reset_task = tokio::task::spawn_local(async move {
                let (stream, _) = resetter.accept().await.unwrap();
                drop(stream);
                drop(resetter);
            });
            let (start, start_rx) = oneshot::channel();
            let server = spawn_rpc_server(paths.clone(), 1, Some(start_rx), None);
            let start = Arc::new(std::sync::Mutex::new(Some(start)));
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                start.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok(())
            }));

            let client = connect_or_spawn(paths, command).await.unwrap();

            reset_task.await.unwrap();
            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 1);
            client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn repeated_handshake_resets_do_not_authorize_unlinking_a_live_listener() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            let listener = UnixListener::bind(paths.socket_path()).unwrap();
            let reset_task = tokio::task::spawn_local(async move {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    drop(stream);
                }
                std::future::pending::<()>().await;
                drop(listener);
            });
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                Err(std::io::Error::other("must not spawn"))
            }));

            let error = connect_or_spawn(paths.clone(), command).await.unwrap_err();

            assert!(matches!(error, LocalLaunchError::Rpc(_)));
            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 0);
            assert!(paths.socket_path().exists());
            reset_task.abort();
        })
        .await;
}

#[tokio::test]
async fn concurrent_callers_share_the_exact_lock_and_spawn_once() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let (start, start_rx) = oneshot::channel();
            let server = spawn_rpc_server(paths.clone(), 2, Some(start_rx), None);
            let start = Arc::new(std::sync::Mutex::new(Some(start)));
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                start.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok(())
            }));

            let (first, second) = tokio::join!(
                connect_or_spawn(paths.clone(), command.clone()),
                connect_or_spawn(paths.clone(), command),
            );
            let first = first.unwrap();
            let second = second.unwrap();

            assert_eq!(first.info().instance_id, second.info().instance_id);
            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 1);
            assert_eq!(
                fs::metadata(paths.spawn_lock_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            first.disconnect().await.unwrap();
            second.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test]
async fn lock_owner_reconnects_before_stale_cleanup_or_spawning() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            drop(UnixListener::bind(paths.socket_path()).unwrap());
            let held_lock = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(paths.spawn_lock_path())
                .unwrap();
            held_lock.lock().unwrap();
            let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let spawn_count = Arc::clone(&spawns);
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                Ok(())
            }));
            let client = tokio::task::spawn_local(connect_or_spawn(paths.clone(), command));
            tokio::task::yield_now().await;
            assert!(!client.is_finished());

            fs::remove_file(paths.socket_path()).unwrap();
            let mut server = spawn_rpc_server(paths, 1, None, None);
            (&mut server.ready).await.unwrap();
            held_lock.unlock().unwrap();
            let client = tokio::time::timeout(TEST_TIMEOUT, client)
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            assert_eq!(spawns.load(std::sync::atomic::Ordering::Acquire), 0);
            client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn spawn_readiness_retries_every_twenty_five_milliseconds() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let (spawned, spawned_rx) = oneshot::channel();
            let spawned = Arc::new(std::sync::Mutex::new(Some(spawned)));
            let command = BackendCommand::injected(Arc::new(move |_| {
                spawned.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok(())
            }));
            let client = tokio::task::spawn_local(connect_or_spawn(paths.clone(), command));
            spawned_rx.await.unwrap();

            tokio::time::advance(Duration::from_millis(24)).await;
            assert!(!client.is_finished());
            let mut server = spawn_rpc_server(paths, 1, None, None);
            (&mut server.ready).await.unwrap();
            assert!(!client.is_finished());
            tokio::time::advance(Duration::from_millis(1)).await;
            let client = client.await.unwrap().unwrap();

            client.disconnect().await.unwrap();
            await_server(server).await;
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn spawn_readiness_stops_after_five_seconds() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let command = BackendCommand::injected(Arc::new(|_| Ok(())));
            let started = tokio::time::Instant::now();
            let client = tokio::task::spawn_local(connect_or_spawn(paths, command));
            tokio::task::yield_now().await;

            tokio::time::advance(Duration::from_millis(4_999)).await;
            assert!(!client.is_finished());
            tokio::time::advance(Duration::from_millis(1)).await;
            let error = client.await.unwrap().unwrap_err();

            assert!(matches!(error, LocalLaunchError::StartupTimeout { .. }));
            assert_eq!(
                tokio::time::Instant::now() - started,
                Duration::from_secs(5)
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_late_listener_that_never_handshakes_cannot_extend_the_global_deadline() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let command = BackendCommand::injected(Arc::new(|_| Ok(())));
            let started = tokio::time::Instant::now();
            let client = tokio::task::spawn_local(connect_or_spawn(paths.clone(), command));
            tokio::task::yield_now().await;

            tokio::time::advance(Duration::from_millis(4_950)).await;
            let listener = UnixListener::bind(paths.socket_path()).unwrap();
            let held = tokio::task::spawn_local(async move {
                let (stream, _) = listener.accept().await.unwrap();
                std::future::pending::<()>().await;
                drop(stream);
            });
            tokio::time::advance(Duration::from_millis(49)).await;
            assert!(!client.is_finished());
            tokio::time::advance(Duration::from_millis(1)).await;
            let error = client.await.unwrap().unwrap_err();

            assert!(matches!(error, LocalLaunchError::StartupTimeout { .. }));
            assert_eq!(
                tokio::time::Instant::now() - started,
                Duration::from_secs(5)
            );
            held.abort();
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn backend_binds_before_runtime_readiness_then_serves_and_idles_cleanly() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();
            let metadata = fs::symlink_metadata(backend.paths.socket_path()).unwrap();
            use std::os::unix::fs::FileTypeExt;
            assert!(metadata.file_type().is_socket());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

            let client = connect_typed(&backend.paths).await;
            let error = client.startup(b"/work/moh".to_vec()).await.unwrap_err();
            assert!(matches!(
                error,
                moh::rpc::client::RpcClientError::Command(SessionCommandError::Reported {
                    code: ErrorCode::BackendStarting,
                    ..
                })
            ));

            backend.release.send(Ok(())).unwrap();
            let session = open_when_ready(&client).await;
            assert_eq!(session.snapshot().summary.cwd, b"/work/moh");
            drop(session);
            client.disconnect().await.unwrap();
            tokio::time::advance(timeout).await;

            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
            assert!(!backend.paths.socket_path().exists());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn ready_runtime_catalog_is_installed_in_new_session_snapshots() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let catalog = ModelCatalogState::Ready(vec![ModelInfoDto {
                id: "gpt-5.6-terra".into(),
                display_name: "GPT Test".into(),
                description: "fixture model".into(),
                reasoning_efforts: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
                default_reasoning: Some(ReasoningLevel::Medium),
            }]);
            let factory = ControlledEngineFactory::new().with_catalog(catalog.clone());
            let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
            let timeout = Duration::from_secs(1);
            let mut backend =
                spawn_deferred_backend_with_factory(&directory, repository, timeout, factory);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            let client = connect_typed(&backend.paths).await;
            let session = open_when_ready(&client).await;

            assert_eq!(session.snapshot().catalog, catalog);

            drop(session);
            client.disconnect().await.unwrap();
            tokio::time::advance(timeout).await;
            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn quarantined_store_path_is_reported_as_a_startup_warning() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("sessions.sqlite");
            fs::write(&database, b"not sqlite").unwrap();
            let opened = SessionStore::open_at(&database).await.unwrap();
            let [StoreWarning::CorruptDatabaseQuarantined { path: quarantine }] =
                opened.warnings.as_slice()
            else {
                panic!("expected one quarantine warning");
            };
            let expected = format!(
                "corrupt session store was quarantined at {}",
                quarantine.display()
            );
            let repository: Arc<dyn SessionRepository> = Arc::new(opened.store);
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();

            let client = connect_typed(&backend.paths).await;
            assert_eq!(client.info().startup_warnings, [expected]);

            client.disconnect().await.unwrap();
            backend.release.send(Ok(())).unwrap();
            tokio::time::advance(timeout).await;
            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn connections_runs_and_jobs_each_veto_idle_then_manager_shutdown_joins_jobs() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            let client = connect_typed(&backend.paths).await;
            let session = open_when_ready(&client).await;

            tokio::time::advance(timeout).await;
            assert!(
                !backend.task.is_finished(),
                "connected client must veto idle"
            );
            session.submit("keep running".into()).await.unwrap();
            let control = backend.factory.controls()[0].clone();
            let registry = backend.factory.registries()[0].clone();
            drop(session);
            client.disconnect().await.unwrap();
            tokio::time::advance(timeout).await;
            assert!(!backend.task.is_finished(), "active run must veto idle");

            control.emit(Ok(EngineEvent::Completed("done".into())));
            tokio::task::yield_now().await;
            let lease = registry
                .start(
                    JobKind::Bash,
                    "retained job",
                    Arc::new(TestJobDetails("running")),
                )
                .unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(timeout).await;
            assert!(!backend.task.is_finished(), "running job must veto idle");

            lease
                .finish(JobState::Completed, Arc::new(TestJobDetails("done")))
                .unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(timeout).await;
            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
            assert!(matches!(
                registry.start(
                    JobKind::Bash,
                    "too late",
                    Arc::new(TestJobDetails("stopped")),
                ),
                Err(JobRegistryError::ShuttingDown)
            ));
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn dirty_flush_failure_vetoes_idle_and_a_later_deadline_retries() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository = FailingRepository::default();
            let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository_boundary, timeout);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            let client = connect_typed(&backend.paths).await;
            let mut session = open_when_ready(&client).await;
            repository.fail_checkpoints(true);
            session.submit("persist me".into()).await.unwrap();
            backend.factory.controls()[0].emit(Ok(EngineEvent::Completed("answer".into())));
            loop {
                let moh::rpc::client::SessionUpdate::Event(event) =
                    session.next_update().await.unwrap()
                else {
                    continue;
                };
                if matches!(event.event, moh::session::SessionEvent::Completed { .. }) {
                    break;
                }
            }
            assert!(!repository.take_checkpoint_attempts().is_empty());
            drop(session);
            client.disconnect().await.unwrap();
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            tokio::time::advance(timeout).await;
            tokio::task::yield_now().await;
            tokio::time::advance(timeout).await;
            tokio::task::yield_now().await;
            assert!(!backend.task.is_finished());
            assert!(!repository.take_checkpoint_attempts().is_empty());
            drop(
                UnixStream::connect(backend.paths.socket_path())
                    .await
                    .unwrap(),
            );
            tokio::task::yield_now().await;
            repository.fail_checkpoints(false);
            tokio::time::advance(timeout).await;

            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn idle_shutdown_leaves_a_replacement_endpoint_byte_identical() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            tokio::task::yield_now().await;
            fs::remove_file(backend.paths.socket_path()).unwrap();
            let replacement = b"replacement must survive shutdown";
            fs::write(backend.paths.socket_path(), replacement).unwrap();

            tokio::time::advance(timeout).await;

            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
            assert_eq!(fs::read(backend.paths.socket_path()).unwrap(), replacement);
        })
        .await;
}

#[tokio::test]
async fn runtime_initialization_failure_shuts_down_and_unlinks_the_bound_socket() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
            let mut backend =
                spawn_deferred_backend(&directory, repository, Duration::from_secs(60));
            (&mut backend.entered).await.unwrap();
            let socket = backend.paths.socket_path().to_path_buf();
            backend
                .release
                .send(Err(std::io::Error::other("secret initializer detail")))
                .unwrap();

            let error = backend.task.await.unwrap().unwrap_err();

            assert!(matches!(error, BackendError::RuntimeInitialization { .. }));
            assert_eq!(error.to_string(), "backend runtime initialization failed");
            assert!(!socket.exists());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn final_repository_owner_is_dropped_before_the_owned_socket_is_unlinked() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let socket_path = paths.socket_path().to_path_buf();
            let socket_existed_at_drop = Arc::new(Mutex::new(None));
            let observed = Arc::clone(&socket_existed_at_drop);
            let observed_path = socket_path.clone();
            let repository: Arc<dyn SessionRepository> =
                Arc::new(FailingRepository::default().on_final_drop(move || {
                    *observed.lock().unwrap() = Some(observed_path.exists());
                }));
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();

            tokio::time::advance(timeout).await;

            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
            assert_eq!(*socket_existed_at_drop.lock().unwrap(), Some(true));
            assert!(!socket_path.exists());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn malformed_connection_is_reaped_without_disrupting_a_healthy_client() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository: Arc<dyn SessionRepository> = Arc::new(FailingRepository::default());
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            let client = connect_typed(&backend.paths).await;
            let session = open_when_ready(&client).await;
            let cwd = session.snapshot().summary.cwd.clone();
            let session_id = session.snapshot().summary.id;
            let mut malformed = UnixStream::connect(backend.paths.socket_path())
                .await
                .unwrap();
            malformed.write_all(&[0xff; 8]).await.unwrap();
            malformed.shutdown().await.unwrap();
            let mut discarded = Vec::new();
            let _ = malformed.read_to_end(&mut discarded).await;
            tokio::task::yield_now().await;

            assert!(!backend.task.is_finished());
            let summaries = client
                .list_sessions(moh::session::SessionListScope::Project(cwd))
                .await
                .unwrap();
            assert!(summaries.iter().any(|summary| summary.id == session_id));

            drop(session);
            client.disconnect().await.unwrap();
            tokio::time::advance(timeout).await;

            assert_eq!(backend.task.await.unwrap().unwrap(), ShutdownReason::Idle);
            assert!(!backend.paths.socket_path().exists());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn failed_manager_shutdown_does_not_hold_backend_cleanup_open() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let repository = FailingRepository::default();
            let repository_boundary: Arc<dyn SessionRepository> = Arc::new(repository.clone());
            let factory = ControlledEngineFactory::new().with_panicking_engine(1);
            let mut backend = spawn_deferred_backend_with_factory(
                &directory,
                repository_boundary,
                Duration::from_secs(60),
                factory,
            );
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            let client = connect_typed(&backend.paths).await;
            let mut session = open_when_ready(&client).await;
            repository.fail_checkpoints(true);
            session.submit("remain dirty".into()).await.unwrap();
            backend.factory.controls()[0].emit(Ok(EngineEvent::Completed("answer".into())));
            loop {
                let moh::rpc::client::SessionUpdate::Event(event) =
                    session.next_update().await.unwrap()
                else {
                    continue;
                };
                if matches!(event.event, moh::session::SessionEvent::Completed { .. }) {
                    break;
                }
            }
            let moh::rpc::client::RpcStartup::Draft(defaults) =
                client.startup(b"/work/moh".to_vec()).await.unwrap()
            else {
                panic!("completed session must not be selected by startup");
            };
            let panicking_session = match client
                .materialize(defaults.cwd, "stop this actor".into(), defaults.settings)
                .await
            {
                Ok(materialized) => Some(materialized.session),
                Err(moh::rpc::client::RpcClientError::Command(SessionCommandError::Reported {
                    code: ErrorCode::BackendUnavailable,
                    ..
                })) => None,
                Err(error) => panic!("unexpected panicking materialization error: {error}"),
            };
            tokio::time::resume();
            if let Some(panicking_session) = panicking_session {
                tokio::time::timeout(TEST_TIMEOUT, async {
                    loop {
                        if panicking_session.list_jobs().await.is_err() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("panicking actor remained reachable");
                drop(panicking_session);
            }

            drop(session);
            let _ = client.disconnect().await;

            let error = tokio::time::timeout(TEST_TIMEOUT, backend.task)
                .await
                .expect("failed manager shutdown held lifecycle open")
                .unwrap()
                .unwrap_err();

            assert!(matches!(
                error,
                BackendError::Connection(
                    RpcServerError::Cleanup(_) | RpcServerError::RpcAndCleanup { .. }
                )
            ));
            assert!(!backend.paths.socket_path().exists());
        })
        .await;
}

#[test]
fn foreground_and_detached_server_commands_use_the_internal_server_modes() {
    let executable = std::path::PathBuf::from("/opt/moh/bin/moh");
    let foreground = foreground_server_command(&executable);
    let detached = detached_server_command(&executable);

    assert_eq!(foreground.get_program(), executable.as_os_str());
    assert_eq!(
        foreground.get_args().collect::<Vec<_>>(),
        [std::ffi::OsStr::new("server")]
    );
    assert_eq!(detached.program(), Some(executable.as_path()));
    assert_eq!(
        detached.arguments().unwrap(),
        [
            std::ffi::OsStr::new("server"),
            std::ffi::OsStr::new("--internal-detached"),
        ]
    );
}

#[test]
#[ignore]
fn detached_child_entry() {
    let Some(marker) = std::env::var_os("MOH_DETACHED_TEST_MARKER") else {
        return;
    };
    let mut byte = [0_u8; 1];
    let stdin_bytes = std::io::stdin().read(&mut byte).unwrap();
    let isolated_session = nix::unistd::getsid(None).unwrap() == nix::unistd::getpid();
    println!("detached stdout marker");
    eprintln!("detached stderr marker");
    fs::write(
        marker,
        format!("stdin_bytes={stdin_bytes}\nisolated_session={isolated_session}\n"),
    )
    .unwrap();
}

#[test]
fn restrictive_umask_first_run_remains_private_and_operational() {
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "restrictive_umask_child_entry",
            "--nocapture",
        ])
        .env("MOH_HIGH_UMASK_TEST_ROOT", directory.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore]
async fn restrictive_umask_child_entry() {
    let Some(root) = std::env::var_os("MOH_HIGH_UMASK_TEST_ROOT").map(std::path::PathBuf::from)
    else {
        return;
    };
    let runtime_home = root.join("runtime-home");
    let runtime_dir = runtime_home.join("moh");
    fs::create_dir(&runtime_home).unwrap();
    fs::create_dir(&runtime_dir).unwrap();
    fs::set_permissions(&runtime_home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let state_home = root.join("missing-state-home");
    let codex_home = root.join("missing-codex-home");
    // SAFETY: the parent starts this test alone in a dedicated child process, before any test task
    // is spawned, so neither these environment mutations nor the umask can race another test.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_home);
        std::env::set_var("XDG_STATE_HOME", &state_home);
        std::env::set_var("CODEX_HOME", &codex_home);
    }
    let previous_umask = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o777));

    let paths = LocalPaths::platform_default().unwrap();
    assert_eq!(paths.state_dir(), state_home.join("moh"));
    let error = moh::server::run(paths.clone(), MohConfig::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            moh::server::ServerRunError::Backend(BackendError::RuntimeInitialization { .. })
        ),
        "foreground server stopped before deferred runtime: {error:?}"
    );
    for path in [state_home, paths.state_dir().to_path_buf()] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} must remain private under umask 0777",
            path.display()
        );
    }
    for path in [
        paths.state_dir().join("sessions.sqlite"),
        paths.state_dir().join("sessions.sqlite.lock"),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{} must remain private under umask 0777",
            path.display()
        );
    }

    let detached_root = root.join("missing-detached-state");
    let detached_paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("unused-detached-runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: detached_root.join("nested/moh"),
        effective_uid: Uid::effective().as_raw(),
    });
    let marker = root.join("detached-child.ready");
    fs::write(&marker, []).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let command = BackendCommand::detached(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "detached_child_entry",
            "--nocapture",
        ])
        .env("MOH_DETACHED_TEST_MARKER", &marker);

    command.spawn(&detached_paths).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if fs::read_to_string(&marker)
                .is_ok_and(|report| report == "stdin_bytes=0\nisolated_session=true\n")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached child did not report readiness");

    for path in [
        detached_root.clone(),
        detached_root.join("nested"),
        detached_paths.state_dir().to_path_buf(),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} must remain private under umask 0777",
            path.display()
        );
    }
    assert_eq!(
        fs::metadata(detached_paths.server_log_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    nix::sys::stat::umask(previous_umask);
}

#[tokio::test]
async fn detached_launch_nulls_stdin_appends_private_logs_and_starts_a_new_session() {
    let directory = tempfile::tempdir().unwrap();
    let paths = local_paths(&directory);
    let executable = std::env::current_exe().unwrap();
    fs::create_dir(paths.state_dir()).unwrap();
    fs::set_permissions(paths.state_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(paths.server_log_path(), b"existing log prefix\n").unwrap();

    for index in 0..2 {
        let marker = directory.path().join(format!("child-{index}.ready"));
        let command = BackendCommand::detached(&executable)
            .args([
                "--ignored",
                "--exact",
                "detached_child_entry",
                "--nocapture",
            ])
            .env("MOH_DETACHED_TEST_MARKER", &marker);
        command.spawn(&paths).unwrap();
        let report = tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if let Ok(report) = fs::read_to_string(&marker)
                    && report == "stdin_bytes=0\nisolated_session=true\n"
                {
                    break report;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached child did not report readiness");
        assert_eq!(report, "stdin_bytes=0\nisolated_session=true\n");
    }

    let log = fs::read_to_string(paths.server_log_path()).unwrap();
    assert!(log.starts_with("existing log prefix\n"));
    assert_eq!(log.matches("detached stdout marker").count(), 2);
    assert_eq!(log.matches("detached stderr marker").count(), 2);
    assert_eq!(
        fs::metadata(paths.server_log_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(paths.state_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test]
async fn detached_launch_securely_creates_missing_state_ancestors() {
    let directory = tempfile::tempdir().unwrap();
    let state_root = directory.path().join("missing-state-root");
    let paths = LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(directory.path().join("runtime")),
        temp_dir: directory.path().join("tmp"),
        config_dir: directory.path().join("config"),
        state_dir: state_root.join("nested/moh"),
        effective_uid: Uid::effective().as_raw(),
    });
    let marker = directory.path().join("child.ready");
    let command = BackendCommand::detached(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "detached_child_entry",
            "--nocapture",
        ])
        .env("MOH_DETACHED_TEST_MARKER", &marker);

    command.spawn(&paths).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if fs::read_to_string(&marker)
                .is_ok_and(|report| report == "stdin_bytes=0\nisolated_session=true\n")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached child did not report readiness");

    assert_eq!(
        fs::metadata(paths.server_log_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    for path in [
        state_root.clone(),
        state_root.join("nested"),
        paths.state_dir().to_path_buf(),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} must remain private",
            path.display()
        );
    }
}

#[tokio::test]
async fn production_runtime_initializer_defers_auth_loading_until_it_is_polled() {
    let directory = tempfile::tempdir().unwrap();
    let auth_path = directory.path().join("auth.json");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", "99.99.99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{
                "slug": "gpt-test",
                "display_name": "GPT Test",
                "description": "Test model",
                "visibility": "list",
                "supported_reasoning_levels": [
                    {"effort": "low"},
                    {"effort": "medium"}
                ],
                "default_reasoning_level": "medium"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let initializer = CodexBackendRuntimeFactory::new(
        auth_path.clone(),
        moh::providers::codex::CodexConfig {
            api_base: server.uri(),
            refresh_url: format!("{}/oauth/token", server.uri()),
        },
        AgentConfig::default(),
        ReadServiceFactory::new(ReadConfig::at(directory.path().join("anchors.sqlite"))),
    );
    fs::write(
        &auth_path,
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "test-access",
                "refresh_token": "test-refresh",
                "account_id": "test-account"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let factory = initializer.initialize().await.unwrap();

    let defaults = factory.default_settings();
    assert_eq!(defaults.model, moh::runtime::rig::DEFAULT_MODEL);
    assert_eq!(defaults.reasoning, ReasoningLevel::Medium);
    assert_eq!(defaults.context_tokens, 0);
    assert_eq!(
        factory.catalog(),
        ModelCatalogState::Ready(vec![ModelInfoDto {
            id: "gpt-test".into(),
            display_name: "GPT Test".into(),
            description: "Test model".into(),
            reasoning_efforts: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_reasoning: Some(ReasoningLevel::Medium),
        }])
    );
}

#[tokio::test]
async fn startup_failure_reports_only_sanitized_paths_not_injected_details() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            let command = BackendCommand::injected(Arc::new(|_| {
                Err(std::io::Error::other("SECRET_ENV_VALUE"))
            }));

            let error = connect_or_spawn(paths.clone(), command).await.unwrap_err();
            let diagnostic = error.to_string();

            assert!(matches!(error, LocalLaunchError::Spawn { .. }));
            assert!(diagnostic.contains(&paths.socket_path().display().to_string()));
            assert!(diagnostic.contains(&paths.spawn_lock_path().display().to_string()));
            assert!(diagnostic.contains(&paths.server_log_path().display().to_string()));
            assert!(!diagnostic.contains("SECRET_ENV_VALUE"));
        })
        .await;
}

#[tokio::test]
async fn a_symlink_at_the_exact_startup_lock_is_rejected_without_touching_its_target() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let directory = tempfile::tempdir().unwrap();
            let paths = local_paths(&directory);
            paths.prepare_runtime_dir().unwrap();
            let target = directory.path().join("lock-target");
            let original = b"lock target remains unchanged";
            fs::write(&target, original).unwrap();
            symlink(&target, paths.spawn_lock_path()).unwrap();
            let command = BackendCommand::injected(Arc::new(|_| {
                panic!("a symlink lock must be rejected before spawning")
            }));

            let error = connect_or_spawn(paths.clone(), command).await.unwrap_err();

            assert!(matches!(error, LocalLaunchError::StartupLock { .. }));
            assert_eq!(fs::read_link(paths.spawn_lock_path()).unwrap(), target);
            assert_eq!(fs::read(target).unwrap(), original);
        })
        .await;
}

#[tokio::test]
async fn detached_launch_rejects_a_symlink_log_without_touching_its_target() {
    let directory = tempfile::tempdir().unwrap();
    let paths = local_paths(&directory);
    fs::create_dir_all(paths.state_dir()).unwrap();
    let target = directory.path().join("log-target");
    let original = b"log target remains unchanged";
    fs::write(&target, original).unwrap();
    symlink(&target, paths.server_log_path()).unwrap();
    let command = BackendCommand::detached(std::env::current_exe().unwrap()).args([
        "--ignored",
        "--exact",
        "detached_child_entry",
    ]);

    let error = command.spawn(&paths).unwrap_err();

    assert_eq!(fs::read_link(paths.server_log_path()).unwrap(), target);
    assert_eq!(fs::read(target).unwrap(), original);
    assert!(matches!(error, LocalLaunchError::Spawn { .. }));
}

#[tokio::test]
async fn detached_launch_rejects_a_symlink_state_directory_before_opening_a_log() {
    let directory = tempfile::tempdir().unwrap();
    let paths = local_paths(&directory);
    let target = directory.path().join("state-target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&target, paths.state_dir()).unwrap();
    let command = BackendCommand::detached(std::env::current_exe().unwrap()).args([
        "--ignored",
        "--exact",
        "detached_child_entry",
    ]);

    let error = command.spawn(&paths).unwrap_err();

    assert!(matches!(
        error,
        LocalLaunchError::Endpoint(LocalPathError::OpenStateDirectory { .. })
    ));
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!target.join("server.log").exists());
}

#[tokio::test(start_paused = true)]
async fn backend_shutdown_drains_auth_before_repository_drop_and_socket_unlink() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let (received, received_rx) = std_mpsc::channel();
            let (release, release_rx) = std_mpsc::channel();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                received.send(()).unwrap();
                release_rx.recv().unwrap();
                let body = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh"}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });

            let directory = tempfile::tempdir().unwrap();
            let auth_path = directory.path().join("auth.json");
            fs::write(
                &auth_path,
                serde_json::to_vec(&serde_json::json!({
                    "auth_mode": "chatgpt",
                    "tokens": {
                        "access_token": "original-access",
                        "refresh_token": "original-refresh",
                        "account_id": "test-account"
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            let mut auth = moh::providers::codex::AuthFile::load(auth_path.clone())
                .await
                .unwrap();
            let refresh = tokio::spawn(async move { auth.refresh(&endpoint).await });
            tokio::task::spawn_blocking(move || received_rx.recv().unwrap())
                .await
                .unwrap();
            refresh.abort();

            let auth_drained_at_repository_drop = Arc::new(Mutex::new(None));
            let observed = Arc::clone(&auth_drained_at_repository_drop);
            let observed_auth_path = auth_path.clone();
            let repository: Arc<dyn SessionRepository> = Arc::new(
                FailingRepository::default().on_final_drop(move || {
                    let stored: serde_json::Value = serde_json::from_slice(
                        &fs::read(observed_auth_path).unwrap(),
                    )
                    .unwrap();
                    *observed.lock().unwrap() =
                        Some(stored["tokens"]["access_token"] == "rotated-access");
                }),
            );
            let timeout = Duration::from_secs(1);
            let mut backend = spawn_deferred_backend(&directory, repository, timeout);
            (&mut backend.entered).await.unwrap();
            backend.release.send(Ok(())).unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(timeout).await;
            assert!(!backend.task.is_finished());
            assert!(backend.paths.socket_path().exists());

            release.send(()).unwrap();
            tokio::time::resume();
            let reason = tokio::time::timeout(TEST_TIMEOUT, backend.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            tokio::time::pause();

            assert_eq!(reason, ShutdownReason::Idle);
            server.join().unwrap();
            let stored: serde_json::Value =
                serde_json::from_slice(&fs::read(auth_path).unwrap()).unwrap();
            assert_eq!(stored["tokens"]["access_token"], "rotated-access");
            assert_eq!(*auth_drained_at_repository_drop.lock().unwrap(), Some(true));
            assert!(!backend.paths.socket_path().exists());
        })
        .await;
}
