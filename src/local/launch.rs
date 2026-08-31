//! Safe local backend discovery and launch coordination.

use std::{
    ffi::OsString,
    fmt, fs, io,
    os::unix::fs::MetadataExt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio::{net::UnixStream, time::MissedTickBehavior};

use nix::{
    fcntl::{OFlag, open},
    sys::stat::{Mode, fchmod},
};

use crate::rpc::client::{RpcBackendClient, RpcClientError};

use super::{LocalPathError, LocalPaths, paths::SocketCandidateIdentity};

type InjectedSpawn = dyn Fn(&LocalPaths) -> io::Result<()> + Send + Sync + 'static;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

enum ConnectAttempt {
    Connected(RpcBackendClient),
    Absent,
    Reset(RpcClientError),
}

/// Injectable detached-backend command used by connect-or-spawn.
#[derive(Clone)]
pub struct BackendCommand {
    kind: BackendCommandKind,
}

#[derive(Clone)]
enum BackendCommandKind {
    Injected(Arc<InjectedSpawn>),
    Detached(DetachedCommand),
}

#[derive(Clone)]
struct DetachedCommand {
    program: PathBuf,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
}

impl fmt::Debug for BackendCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackendCommand")
            .finish_non_exhaustive()
    }
}

impl BackendCommand {
    /// Builds a command from an injected synchronous spawn boundary.
    pub fn injected(spawn: Arc<InjectedSpawn>) -> Self {
        Self {
            kind: BackendCommandKind::Injected(spawn),
        }
    }

    /// Builds a detached process command without inheriting any terminal handle.
    pub fn detached(program: impl Into<PathBuf>) -> Self {
        Self {
            kind: BackendCommandKind::Detached(DetachedCommand {
                program: program.into(),
                arguments: Vec::new(),
                environment: Vec::new(),
            }),
        }
    }

    /// Appends command-line arguments to a detached process command.
    #[must_use]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        if let BackendCommandKind::Detached(command) = &mut self.kind {
            command
                .arguments
                .extend(arguments.into_iter().map(Into::into));
        }
        self
    }

    /// Adds one environment entry to a detached process command.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        if let BackendCommandKind::Detached(command) = &mut self.kind {
            command.environment.push((key.into(), value.into()));
        }
        self
    }

    /// Returns the detached executable, or `None` for an injected test command.
    pub fn program(&self) -> Option<&Path> {
        match &self.kind {
            BackendCommandKind::Injected(_) => None,
            BackendCommandKind::Detached(command) => Some(&command.program),
        }
    }

    /// Returns detached arguments, or `None` for an injected test command.
    pub fn arguments(&self) -> Option<&[OsString]> {
        match &self.kind {
            BackendCommandKind::Injected(_) => None,
            BackendCommandKind::Detached(command) => Some(&command.arguments),
        }
    }

    /// Starts the injected or detached backend command.
    pub fn spawn(&self, paths: &LocalPaths) -> Result<(), LocalLaunchError> {
        match &self.kind {
            BackendCommandKind::Injected(spawn) => {
                spawn(paths).map_err(|source| spawn_error(paths, source))
            }
            BackendCommandKind::Detached(command) => spawn_detached(command, paths),
        }
    }
}

fn spawn_detached(
    specification: &DetachedCommand,
    paths: &LocalPaths,
) -> Result<(), LocalLaunchError> {
    paths.prepare_state_dir()?;
    let log = open(
        paths.server_log_path(),
        OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_APPEND | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|source| spawn_error(paths, source.into()))?;
    fchmod(&log, Mode::from_bits_truncate(0o600))
        .map_err(|source| spawn_error(paths, source.into()))?;
    let log: fs::File = log.into();
    let stderr = log
        .try_clone()
        .map_err(|source| spawn_error(paths, source))?;
    let mut command = Command::new(&specification.program);
    command
        .args(&specification.arguments)
        .envs(
            specification
                .environment
                .iter()
                .map(|(key, value)| (key, value)),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // SAFETY: after fork and before exec, the closure calls only the async-signal-safe `setsid`
    // syscall and converts its errno without touching application locks or allocated state.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|error| io::Error::from_raw_os_error(error as i32))
        });
    }
    let _child = command
        .spawn()
        .map_err(|source| spawn_error(paths, source))?;
    Ok(())
}

fn spawn_error(paths: &LocalPaths, source: io::Error) -> LocalLaunchError {
    LocalLaunchError::Spawn {
        socket: paths.socket_path().to_path_buf(),
        lock: paths.spawn_lock_path().to_path_buf(),
        log: paths.server_log_path().to_path_buf(),
        source,
    }
}

/// Failure to validate, discover, launch, or negotiate the local backend.
#[derive(Debug, Error)]
pub enum LocalLaunchError {
    /// Secure local-path preparation or endpoint validation failed.
    #[error(transparent)]
    Endpoint(#[from] LocalPathError),
    /// The exact startup lock could not be opened or acquired.
    #[error("could not acquire local backend startup lock {path}: {source}")]
    StartupLock {
        /// Exact advisory lock path.
        path: PathBuf,
        /// Filesystem or locking failure.
        #[source]
        source: io::Error,
    },
    /// A validated stale socket could not be removed.
    #[error("could not remove stale local backend socket {path}: {source}")]
    RemoveStaleSocket {
        /// Exact validated socket path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: io::Error,
    },
    /// The detached backend command could not be started.
    #[error(
        "could not start local backend for socket {socket}; startup lock {lock}; diagnostic log {log}"
    )]
    Spawn {
        /// Exact local socket path.
        socket: PathBuf,
        /// Exact startup lock path.
        lock: PathBuf,
        /// Exact private diagnostic log path.
        log: PathBuf,
        /// Process creation failure.
        #[source]
        source: io::Error,
    },
    /// The spawned backend did not become reachable and compatible in time.
    #[error(
        "local backend did not become ready at {socket} within five seconds; startup lock {lock}; diagnostic log {log}"
    )]
    StartupTimeout {
        /// Exact local socket path.
        socket: PathBuf,
        /// Exact startup lock path.
        lock: PathBuf,
        /// Exact private diagnostic log path.
        log: PathBuf,
    },
    /// A connected endpoint could not complete the typed protocol handshake.
    #[error(transparent)]
    Rpc(#[from] RpcClientError),
}

/// Connects to a compatible local backend, preparing for safe launch when absent.
pub async fn connect_or_spawn(
    paths: LocalPaths,
    command: BackendCommand,
) -> Result<RpcBackendClient, LocalLaunchError> {
    paths.prepare_runtime_dir()?;
    let original_socket = paths.socket_candidate_identity()?;
    if let ConnectAttempt::Connected(client) = try_connect(&paths).await? {
        return Ok(client);
    }

    let lock = open_startup_lock(&paths)?;
    acquire_startup_lock(&paths, &lock).await?;
    match try_connect(&paths).await? {
        ConnectAttempt::Connected(client) => return Ok(client),
        ConnectAttempt::Reset(error) => return Err(LocalLaunchError::Rpc(error)),
        ConnectAttempt::Absent => {}
    }

    if !remove_stale_socket(&paths, original_socket)? {
        return wait_for_backend(&paths).await;
    }
    command.spawn(&paths)?;
    wait_for_backend(&paths).await
}

fn open_startup_lock(paths: &LocalPaths) -> Result<fs::File, LocalLaunchError> {
    let path = paths.spawn_lock_path();
    let descriptor = open(
        path,
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|source| LocalLaunchError::StartupLock {
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    fchmod(&descriptor, Mode::from_bits_truncate(0o600)).map_err(|source| {
        LocalLaunchError::StartupLock {
            path: path.to_path_buf(),
            source: source.into(),
        }
    })?;
    Ok(descriptor.into())
}

async fn acquire_startup_lock(paths: &LocalPaths, lock: &fs::File) -> Result<(), LocalLaunchError> {
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match fs::File::try_lock(lock) {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(source)) => {
                return Err(LocalLaunchError::StartupLock {
                    path: paths.spawn_lock_path().to_path_buf(),
                    source,
                });
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(LocalLaunchError::StartupTimeout {
                socket: paths.socket_path().to_path_buf(),
                lock: paths.spawn_lock_path().to_path_buf(),
                log: paths.server_log_path().to_path_buf(),
            });
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

async fn try_connect(paths: &LocalPaths) -> Result<ConnectAttempt, LocalLaunchError> {
    try_connect_until(paths, tokio::time::Instant::now() + CONNECT_TIMEOUT).await
}

async fn try_connect_until(
    paths: &LocalPaths,
    deadline: tokio::time::Instant,
) -> Result<ConnectAttempt, LocalLaunchError> {
    let candidate = paths.socket_candidate_identity()?;
    let stream =
        match tokio::time::timeout_at(deadline, UnixStream::connect(paths.socket_path())).await {
            Err(_) => {
                return Err(LocalLaunchError::StartupTimeout {
                    socket: paths.socket_path().to_path_buf(),
                    lock: paths.spawn_lock_path().to_path_buf(),
                    log: paths.server_log_path().to_path_buf(),
                });
            }
            Ok(result) => match result {
                Ok(stream) => stream,
                Err(source)
                    if matches!(
                        source.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(ConnectAttempt::Absent);
                }
                Err(source) => {
                    return Err(LocalLaunchError::Spawn {
                        socket: paths.socket_path().to_path_buf(),
                        lock: paths.spawn_lock_path().to_path_buf(),
                        log: paths.server_log_path().to_path_buf(),
                        source,
                    });
                }
            },
        };
    if paths.socket_candidate_identity()? != candidate {
        return Ok(ConnectAttempt::Absent);
    }
    match tokio::time::timeout_at(deadline, RpcBackendClient::connect(stream)).await {
        Ok(Ok(connected)) => Ok(ConnectAttempt::Connected(connected)),
        Ok(Err(error @ RpcClientError::Connection(_))) => Ok(ConnectAttempt::Reset(error)),
        Ok(Err(error)) => Err(LocalLaunchError::Rpc(error)),
        Err(_) => Err(LocalLaunchError::StartupTimeout {
            socket: paths.socket_path().to_path_buf(),
            lock: paths.spawn_lock_path().to_path_buf(),
            log: paths.server_log_path().to_path_buf(),
        }),
    }
}

fn remove_stale_socket(
    paths: &LocalPaths,
    original: Option<SocketCandidateIdentity>,
) -> Result<bool, LocalLaunchError> {
    let path = paths.socket_path();
    let current = paths.socket_candidate_identity()?;
    match (original, current) {
        (None, None) | (Some(_), None) => return Ok(true),
        (None, Some(_)) => return Ok(false),
        (Some(original), Some(current)) if original != current => return Ok(false),
        (Some(_), Some(_)) => {}
    };
    let current =
        fs::symlink_metadata(path).map_err(|source| LocalLaunchError::RemoveStaleSocket {
            path: path.to_path_buf(),
            source,
        })?;
    let original = original.expect("matched candidates retain an original identity");
    if (current.dev(), current.ino()) != (original.device, original.inode) {
        return Ok(false);
    }
    fs::remove_file(path).map_err(|source| LocalLaunchError::RemoveStaleSocket {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(true)
}

async fn wait_for_backend(paths: &LocalPaths) -> Result<RpcBackendClient, LocalLaunchError> {
    let start = tokio::time::Instant::now();
    let deadline = start + CONNECT_TIMEOUT;
    let mut retry = tokio::time::interval_at(start, RETRY_INTERVAL);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut observed_reset = false;
    loop {
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => {
                return Err(LocalLaunchError::StartupTimeout {
                    socket: paths.socket_path().to_path_buf(),
                    lock: paths.spawn_lock_path().to_path_buf(),
                    log: paths.server_log_path().to_path_buf(),
                });
            }
            _ = retry.tick() => {
                match try_connect_until(paths, deadline).await? {
                    ConnectAttempt::Connected(client) => return Ok(client),
                    ConnectAttempt::Absent => {}
                    ConnectAttempt::Reset(error) if observed_reset => {
                        return Err(LocalLaunchError::Rpc(error));
                    }
                    ConnectAttempt::Reset(_) => observed_reset = true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener};

    use nix::unistd::Uid;

    use super::remove_stale_socket;
    use crate::local::{LocalPaths, PathRoots};

    #[test]
    fn stale_cleanup_preserves_a_replacement_socket_with_a_different_identity() {
        let directory = tempfile::tempdir().unwrap();
        let paths = LocalPaths::from_roots(PathRoots {
            runtime_dir: Some(directory.path().join("runtime")),
            temp_dir: directory.path().join("tmp"),
            config_dir: directory.path().join("config"),
            state_dir: directory.path().join("state"),
            effective_uid: Uid::effective().as_raw(),
        });
        paths.prepare_runtime_dir().unwrap();
        let original_listener = UnixListener::bind(paths.socket_path()).unwrap();
        let original = paths.socket_candidate_identity().unwrap();
        drop(original_listener);
        fs::remove_file(paths.socket_path()).unwrap();
        let replacement = UnixListener::bind(paths.socket_path()).unwrap();

        assert!(!remove_stale_socket(&paths, original).unwrap());
        assert!(paths.socket_path().exists());

        drop(replacement);
    }
}
