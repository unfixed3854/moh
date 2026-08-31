//! Moh's executable entry point and terminal application wiring.

#[cfg(unix)]
mod client;

use std::{ffi::OsString, process::ExitCode};

use moh::cli::{self, CliMode};

#[cfg(unix)]
use std::{
    env,
    future::Future,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use moh::{
    local::{LocalLaunchError, LocalPathError, LocalPaths, MohConfig, connect_or_spawn},
    rpc::client::{RpcBackendClient, RpcClientError},
    server::ServerRunError,
    session::{ErrorCode, SessionCommandError, SessionSummary},
};

#[cfg(unix)]
const BACKEND_READY_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error("{0}")]
    Cli(#[from] clap::Error),
    #[cfg(unix)]
    #[error(transparent)]
    Paths(#[from] LocalPathError),
    #[cfg(unix)]
    #[error(transparent)]
    Config(#[from] moh::local::ConfigError),
    #[cfg(unix)]
    #[error("could not resolve the current working directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[cfg(unix)]
    #[error("could not canonicalize working directory {path}: {source}")]
    CanonicalDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[cfg(unix)]
    #[error("could not resolve the current executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[cfg(unix)]
    #[error("could not start the asynchronous runtime: {0}")]
    AsyncRuntime(#[source] io::Error),
    #[cfg(unix)]
    #[error(transparent)]
    Launch(#[from] LocalLaunchError),
    #[cfg(unix)]
    #[error(transparent)]
    Rpc(#[from] RpcClientError),
    #[cfg(unix)]
    #[error(transparent)]
    App(#[from] client::app::AppError),
    #[cfg(unix)]
    #[error(transparent)]
    Server(#[from] ServerRunError),
    #[cfg(any(not(unix), test))]
    #[error("local backend transport is not supported on this platform")]
    UnsupportedPlatform,
}

#[cfg(unix)]
fn run_with_current_thread_runtime<F, Fut, T>(operation: F) -> io::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    Ok(runtime.block_on(local.run_until(operation())))
}

#[cfg(unix)]
fn canonical_cwd_bytes() -> Result<Vec<u8>, MainError> {
    let cwd = env::current_dir().map_err(MainError::CurrentDirectory)?;
    let canonical = cwd
        .canonicalize()
        .map_err(|source| MainError::CanonicalDirectory { path: cwd, source })?;
    Ok(canonical.as_os_str().as_bytes().to_vec())
}

#[cfg(unix)]
async fn list_when_ready(
    backend: &RpcBackendClient,
    cwd: &[u8],
) -> Result<Vec<SessionSummary>, RpcClientError> {
    loop {
        match backend
            .list_sessions(moh::session::SessionListScope::Project(cwd.to_vec()))
            .await
        {
            Err(error) if is_backend_starting(&error) => {
                tokio::time::sleep(BACKEND_READY_INTERVAL).await;
            }
            result => return result,
        }
    }
}

#[cfg(unix)]
fn is_backend_starting(error: &RpcClientError) -> bool {
    matches!(
        error,
        RpcClientError::Command(SessionCommandError::Reported {
            code: ErrorCode::BackendStarting,
            ..
        })
    )
}

#[cfg(unix)]
fn format_sessions(mut sessions: Vec<SessionSummary>) -> Vec<String> {
    sessions.sort_by(|left, right| {
        right
            .last_activity
            .cmp(&left.last_activity)
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
        .into_iter()
        .map(|session| {
            let state = if session.running { "running" } else { "idle" };
            format!(
                "{}\t{}\t{}\tclients={}\t{}",
                session.id,
                session.title,
                state,
                session.attached_clients,
                session.last_activity.to_rfc3339()
            )
        })
        .collect()
}

#[cfg(unix)]
fn print_sessions(sessions: Vec<SessionSummary>) {
    for line in format_sessions(sessions) {
        println!("{line}");
    }
}

#[cfg(unix)]
async fn run_client_mode(mode: CliMode, paths: LocalPaths, cwd: Vec<u8>) -> Result<(), MainError> {
    let executable = env::current_exe().map_err(MainError::CurrentExecutable)?;
    let command = moh::server::detached_server_command(executable);
    let backend = connect_or_spawn(paths, command).await?;

    if mode == CliMode::Sessions {
        let operation = list_when_ready(&backend, &cwd)
            .await
            .map(print_sessions)
            .map_err(MainError::from);
        let disconnect = backend.disconnect().await.map_err(MainError::from);
        return operation.and(disconnect);
    }

    let launch = match mode {
        CliMode::Default => client::LaunchMode::Startup,
        CliMode::New => client::LaunchMode::NewDraft,
        CliMode::Session { selector } => client::LaunchMode::Session(selector),
        CliMode::Sessions | CliMode::Server { .. } => {
            unreachable!("noninteractive modes dispatch before terminal startup")
        }
    };
    client::run(backend, cwd, launch)
        .await
        .map_err(MainError::from)
}

#[cfg(unix)]
trait DispatchRunner {
    async fn run_server(&self, paths: LocalPaths, config: MohConfig) -> Result<(), MainError>;
    async fn run_client(
        &self,
        mode: CliMode,
        paths: LocalPaths,
        cwd: Vec<u8>,
    ) -> Result<(), MainError>;
}

#[cfg(unix)]
struct ProductionDispatchRunner;

#[cfg(unix)]
impl DispatchRunner for ProductionDispatchRunner {
    async fn run_server(&self, paths: LocalPaths, config: MohConfig) -> Result<(), MainError> {
        moh::server::run(paths, config).await?;
        Ok(())
    }

    async fn run_client(
        &self,
        mode: CliMode,
        paths: LocalPaths,
        cwd: Vec<u8>,
    ) -> Result<(), MainError> {
        run_client_mode(mode, paths, cwd).await
    }
}

#[cfg(unix)]
async fn dispatch_with<R, ResolvePaths, LoadConfig, ResolveCwd>(
    mode: CliMode,
    resolve_paths: ResolvePaths,
    load_config: LoadConfig,
    resolve_cwd: ResolveCwd,
    runner: &R,
) -> Result<(), MainError>
where
    R: DispatchRunner,
    ResolvePaths: FnOnce() -> Result<LocalPaths, MainError>,
    LoadConfig: FnOnce(&Path) -> Result<MohConfig, MainError>,
    ResolveCwd: FnOnce() -> Result<Vec<u8>, MainError>,
{
    let paths = resolve_paths()?;
    match mode {
        CliMode::Server { .. } => {
            let config = load_config(paths.config_path())?;
            runner.run_server(paths, config).await
        }
        client_mode => {
            let cwd = resolve_cwd()?;
            runner.run_client(client_mode, paths, cwd).await
        }
    }
}

#[cfg(unix)]
async fn dispatch(mode: CliMode) -> Result<(), MainError> {
    dispatch_with(
        mode,
        || LocalPaths::platform_default().map_err(MainError::from),
        |path| MohConfig::load(path).map_err(MainError::from),
        canonical_cwd_bytes,
        &ProductionDispatchRunner,
    )
    .await
}

fn parse_platform_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<CliMode, MainError> {
    cli::parse(arguments).map_err(MainError::from)
}

#[cfg(any(not(unix), test))]
fn unsupported_platform(_mode: CliMode) -> Result<(), MainError> {
    Err(MainError::UnsupportedPlatform)
}

fn format_main_diagnostic(error: &MainError) -> String {
    format!("moh: {error}")
}

#[cfg(unix)]
fn platform_main(arguments: impl IntoIterator<Item = OsString>) -> Result<(), MainError> {
    let mode = parse_platform_arguments(arguments)?;
    run_with_current_thread_runtime(|| dispatch(mode)).map_err(MainError::AsyncRuntime)?
}

#[cfg(not(unix))]
fn platform_main(arguments: impl IntoIterator<Item = OsString>) -> Result<(), MainError> {
    unsupported_platform(parse_platform_arguments(arguments)?)
}

fn main() -> ExitCode {
    match platform_main(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(MainError::Cli(error)) => error.exit(),
        Err(error) => {
            eprintln!("{}", format_main_diagnostic(&error));
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{cell::RefCell, ffi::OsString, rc::Rc};

    use chrono::{TimeZone, Utc};
    use moh::{
        cli::CliMode,
        local::{LocalPaths, MohConfig, PathRoots},
        session::SessionSummary,
    };

    use super::{DispatchRunner, MainError, dispatch_with, format_sessions, is_backend_starting};

    type ClientRun = (CliMode, Vec<u8>);

    #[derive(Clone, Default)]
    struct ExistingBackendRunner {
        server_runs: Rc<RefCell<usize>>,
        client_runs: Rc<RefCell<Vec<ClientRun>>>,
    }

    impl DispatchRunner for ExistingBackendRunner {
        async fn run_server(
            &self,
            _paths: LocalPaths,
            _config: MohConfig,
        ) -> Result<(), MainError> {
            *self.server_runs.borrow_mut() += 1;
            Ok(())
        }

        async fn run_client(
            &self,
            mode: CliMode,
            _paths: LocalPaths,
            cwd: Vec<u8>,
        ) -> Result<(), MainError> {
            self.client_runs.borrow_mut().push((mode, cwd));
            Ok(())
        }
    }

    fn paths_in(directory: &tempfile::TempDir) -> LocalPaths {
        LocalPaths::from_roots(PathRoots {
            runtime_dir: Some(directory.path().join("runtime")),
            temp_dir: directory.path().join("tmp"),
            config_dir: directory.path().join("config"),
            state_dir: directory.path().join("state"),
            effective_uid: nix::unistd::Uid::effective().as_raw(),
        })
    }

    #[test]
    fn backend_starting_retry_is_keyed_only_by_the_stable_error_code() {
        let starting = moh::rpc::client::RpcClientError::Command(
            moh::session::SessionCommandError::Reported {
                code: moh::session::ErrorCode::BackendStarting,
                message: "safe startup message".into(),
            },
        );
        let busy = moh::rpc::client::RpcClientError::Command(
            moh::session::SessionCommandError::Reported {
                code: moh::session::ErrorCode::Busy,
                message: "busy".into(),
            },
        );

        assert!(is_backend_starting(&starting));
        assert!(!is_backend_starting(&busy));
    }

    #[test]
    fn list_output_orders_by_most_recent_activity() {
        let earlier = Utc.with_ymd_and_hms(2026, 8, 27, 11, 0, 0).unwrap();
        let later = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        let summary = |id: &str, title: &str, running, clients, activity| SessionSummary {
            id: id.parse().unwrap(),
            title: moh::session::SessionTitle::parse(title).unwrap(),
            title_revision: 0,
            cwd: b"/work/moh".to_vec(),
            cwd_display: "/work/moh".into(),
            running_jobs: 0,
            running,
            busy: running,
            attached_clients: clients,
            last_activity: activity,
        };

        let lines = format_sessions(vec![
            summary("session-2", "older", false, 0, earlier),
            summary("session-1", "Untitled session", false, 1, earlier),
            summary("session-3", "newer", true, 2, later),
        ]);

        assert_eq!(
            lines,
            [
                "session-3\tnewer\trunning\tclients=2\t2026-08-27T12:00:00+00:00",
                "session-1\tUntitled session\tidle\tclients=1\t2026-08-27T11:00:00+00:00",
                "session-2\tolder\tidle\tclients=0\t2026-08-27T11:00:00+00:00",
            ]
        );
    }

    #[test]
    fn platform_main_parser_rejects_unsupported_forms_before_runtime_setup() {
        let error = super::platform_main(["moh", "--unknown"].map(OsString::from)).unwrap_err();
        assert!(matches!(error, super::MainError::Cli(_)));
    }

    #[tokio::test]
    async fn server_dispatch_loads_config_without_resolving_cwd() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths_in(&directory);
        let runner = ExistingBackendRunner::default();

        dispatch_with(
            CliMode::Server { detached: false },
            || Ok(paths),
            |_| Ok(MohConfig::default()),
            || panic!("server dispatch must not resolve a working directory"),
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(*runner.server_runs.borrow(), 1);
        assert!(runner.client_runs.borrow().is_empty());
    }

    #[tokio::test]
    async fn attach_and_list_dispatch_preserve_raw_cwd_without_loading_server_config() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths_in(&directory);
        let runner = ExistingBackendRunner::default();
        let raw_cwd = b"/work/non-utf8-\xff".to_vec();

        for mode in [CliMode::Default, CliMode::Sessions] {
            dispatch_with(
                mode,
                || Ok(paths.clone()),
                |path| Err(MohConfig::parse("unknown = true", path).unwrap_err().into()),
                || Ok(raw_cwd.clone()),
                &runner,
            )
            .await
            .unwrap();
        }

        assert_eq!(
            *runner.client_runs.borrow(),
            [
                (CliMode::Default, raw_cwd.clone()),
                (CliMode::Sessions, raw_cwd),
            ]
        );
        assert_eq!(*runner.server_runs.borrow(), 0);
    }

    #[tokio::test]
    async fn malformed_server_config_is_reported_before_server_run() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths_in(&directory);
        let runner = ExistingBackendRunner::default();

        let error = dispatch_with(
            CliMode::Server { detached: false },
            || Ok(paths),
            |path| Err(MohConfig::parse("unknown = true", path).unwrap_err().into()),
            || panic!("server dispatch must not resolve a working directory"),
            &runner,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, MainError::Config(_)));
        assert_eq!(*runner.server_runs.borrow(), 0);
        assert!(runner.client_runs.borrow().is_empty());
    }
}

#[cfg(test)]
mod platform_tests {
    use std::ffi::OsString;

    use super::{
        MainError, format_main_diagnostic, parse_platform_arguments, unsupported_platform,
    };

    #[test]
    fn accepted_platform_modes_are_parsed_before_the_unsupported_diagnostic() {
        for arguments in [
            vec!["moh"],
            vec!["moh", "--new"],
            vec!["moh", "--resume", "session-7"],
            vec!["moh", "sessions"],
            vec!["moh", "server"],
            vec!["moh", "server", "--internal-detached"],
        ] {
            let mode = parse_platform_arguments(arguments.into_iter().map(OsString::from)).unwrap();
            assert_eq!(
                format_main_diagnostic(&unsupported_platform(mode).unwrap_err()),
                "moh: local backend transport is not supported on this platform"
            );
        }
    }

    #[test]
    fn malformed_platform_arguments_remain_cli_errors() {
        let error = parse_platform_arguments(["moh", "--unknown"].map(OsString::from)).unwrap_err();
        assert!(matches!(error, MainError::Cli(_)));
    }
}
