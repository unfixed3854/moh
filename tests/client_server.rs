#![cfg(unix)]

mod support;

use std::{
    cell::RefCell,
    collections::HashMap,
    fs::{self, OpenOptions},
    future::{self, Future},
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::future::join_all;
use moh::{
    backend::{BackendOptions, ShutdownReason, run_backend},
    harness::{EngineEvent, Role, RunFailure, RunFailureKind, RunStage},
    local::{BackendCommand, LocalPaths, PathRoots, ServerConfig, connect_or_spawn},
    rpc::client::{RpcBackendClient, RpcSessionClient, RpcStartup, SessionUpdate},
    runtime::rig::ReasoningLevel,
    session::{
        ModelCatalogState, ModelInfoDto, RunFailureSnapshot, SessionEvent, SessionEventEnvelope,
        SessionListScope, SessionRepository, SessionSelector, SessionStore, TranscriptItem,
    },
    tools::{JobDetails, JobKind, JobLease, JobState},
};
use nix::{
    sys::{
        signal::{Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{Pid, Uid},
};
use serde::{Deserialize, Serialize};
use support::{ControlledEngineControl, ControlledEngineFactory};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    task::LocalSet,
};

const CHILD_ROOT_ENV: &str = "MOH_CLIENT_SERVER_CHILD_ROOT";
const CHILD_CONTROL_ENV: &str = "MOH_CLIENT_SERVER_CONTROL_SOCKET";
const CHILD_IDLE_MS_ENV: &str = "MOH_CLIENT_SERVER_IDLE_MS";
const WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum ChildCommand {
    Ping,
    WaitRequests {
        engine: usize,
        count: usize,
    },
    WaitConsumed {
        engine: usize,
        count: u64,
    },
    EmitDelta {
        engine: usize,
        text: String,
    },
    EmitContext {
        engine: usize,
        input_tokens: u64,
    },
    EmitToolStarted {
        engine: usize,
        call_id: String,
        name: String,
    },
    EmitToolFinished {
        engine: usize,
        call_id: String,
        name: String,
    },
    EmitCompleted {
        engine: usize,
        response: String,
    },
    EmitFailed {
        engine: usize,
        message: String,
    },
    StartJob {
        engine: usize,
        title: String,
        details: String,
    },
    FinishJob {
        engine: usize,
        job_id: String,
        details: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ChildMessage {
    role: String,
    text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ChildRunRequest {
    prompt: String,
    history: Vec<ChildMessage>,
    cwd: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
enum ChildReply {
    Pong,
    Ack,
    Requests { requests: Vec<ChildRunRequest> },
    Consumed { count: u64 },
    JobStarted { job_id: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ChildResponse {
    Ok { value: ChildReply },
    Error { message: String },
}

#[derive(Debug)]
struct FixtureJobDetails(String);

impl JobDetails for FixtureJobDetails {
    fn render(&self) -> String {
        self.0.clone()
    }
}

#[test]
#[ignore]
fn child_backend_entry() {
    let Some(root) = std::env::var_os(CHILD_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    let control_path = required_child_path(CHILD_CONTROL_ENV);
    let idle_timeout = Duration::from_millis(
        std::env::var(CHILD_IDLE_MS_ENV)
            .expect("child idle timeout must be configured")
            .parse()
            .expect("child idle timeout must be milliseconds"),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    runtime.block_on(local.run_until(async move {
        let reason = run_child_backend(root, control_path, idle_timeout)
            .await
            .expect("child backend must run and shut down cleanly");
        assert!(matches!(
            reason,
            ShutdownReason::Idle | ShutdownReason::Signal
        ));
    }));
}

fn required_child_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("child environment must include {name}"))
}

async fn run_child_backend(
    root: PathBuf,
    control_path: PathBuf,
    idle_timeout: Duration,
) -> Result<ShutdownReason, Box<dyn std::error::Error>> {
    let paths = paths_from_root(&root);
    paths.prepare_state_dir()?;
    let opened = SessionStore::open_at(&paths.state_dir().join("sessions.sqlite")).await?;
    let repository: Arc<dyn SessionRepository> = Arc::new(opened.store);
    let factory = ControlledEngineFactory::new().with_catalog(fixture_catalog());
    let _ = fs::remove_file(&control_path);
    let control = UnixListener::bind(&control_path)?;
    let control_task = tokio::task::spawn_local(serve_child_control(control, factory.clone()));
    let result = run_backend(BackendOptions {
        paths,
        config: ServerConfig { idle_timeout },
        runtime_factory: factory,
        repository,
    })
    .await;
    control_task.abort();
    let _ = control_task.await;
    let _ = fs::remove_file(control_path);
    Ok(result?)
}

fn fixture_catalog() -> ModelCatalogState {
    ModelCatalogState::Ready(vec![
        ModelInfoDto {
            id: "gpt-5.6-terra".into(),
            display_name: "Fixture Terra".into(),
            description: "default fixture model".into(),
            reasoning_efforts: vec![ReasoningLevel::Low, ReasoningLevel::Medium],
            default_reasoning: Some(ReasoningLevel::Medium),
        },
        ModelInfoDto {
            id: "gpt-persist".into(),
            display_name: "Fixture Persist".into(),
            description: "persistence fixture model".into(),
            reasoning_efforts: vec![ReasoningLevel::Medium, ReasoningLevel::Xhigh],
            default_reasoning: Some(ReasoningLevel::Xhigh),
        },
    ])
}

async fn serve_child_control(listener: UnixListener, factory: ControlledEngineFactory) {
    let leases = Rc::new(RefCell::new(HashMap::<(usize, String), JobLease>::new()));
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        if let Err(error) = handle_child_control(stream, &factory, &leases).await {
            eprintln!("child control connection failed: {error}");
        }
    }
}

async fn handle_child_control(
    stream: UnixStream,
    factory: &ControlledEngineFactory,
    leases: &Rc<RefCell<HashMap<(usize, String), JobLease>>>,
) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response = match serde_json::from_str::<ChildCommand>(&line) {
        Ok(command) => match execute_child_command(command, factory, leases).await {
            Ok(value) => ChildResponse::Ok { value },
            Err(message) => ChildResponse::Error { message },
        },
        Err(error) => ChildResponse::Error {
            message: format!("invalid control request: {error}"),
        },
    };
    let mut encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.shutdown().await
}

async fn execute_child_command(
    command: ChildCommand,
    factory: &ControlledEngineFactory,
    leases: &Rc<RefCell<HashMap<(usize, String), JobLease>>>,
) -> Result<ChildReply, String> {
    match command {
        ChildCommand::Ping => Ok(ChildReply::Pong),
        ChildCommand::WaitRequests { engine, count } => {
            let control = factory.wait_for_control(engine).await;
            let requests = control
                .wait_for_request_count(count)
                .await
                .into_iter()
                .map(|request| ChildRunRequest {
                    prompt: request.prompt,
                    history: request
                        .history
                        .into_iter()
                        .map(|message| ChildMessage {
                            role: match message.role {
                                Role::User => "user",
                                Role::Assistant => "assistant",
                            }
                            .into(),
                            text: message.text,
                        })
                        .collect(),
                    cwd: request.context.cwd.display().to_string(),
                })
                .collect();
            Ok(ChildReply::Requests { requests })
        }
        ChildCommand::WaitConsumed { engine, count } => {
            let consumed = factory
                .wait_for_control(engine)
                .await
                .wait_for_consumed_count(count)
                .await;
            Ok(ChildReply::Consumed { count: consumed })
        }
        ChildCommand::EmitDelta { engine, text } => {
            child_control(factory, engine)
                .await
                .emit(Ok(EngineEvent::AssistantDelta(text)));
            Ok(ChildReply::Ack)
        }
        ChildCommand::EmitContext {
            engine,
            input_tokens,
        } => {
            child_control(factory, engine)
                .await
                .emit(Ok(EngineEvent::ContextUsage { input_tokens }));
            Ok(ChildReply::Ack)
        }
        ChildCommand::EmitToolStarted {
            engine,
            call_id,
            name,
        } => {
            child_control(factory, engine)
                .await
                .emit(Ok(EngineEvent::ToolStarted {
                    call_id,
                    name,
                    arguments: serde_json::json!({"fixture": true}),
                }));
            Ok(ChildReply::Ack)
        }
        ChildCommand::EmitToolFinished {
            engine,
            call_id,
            name,
        } => {
            child_control(factory, engine)
                .await
                .emit(Ok(EngineEvent::ToolFinished { call_id, name }));
            Ok(ChildReply::Ack)
        }
        ChildCommand::EmitCompleted { engine, response } => {
            child_control(factory, engine)
                .await
                .emit(Ok(EngineEvent::Completed(response)));
            Ok(ChildReply::Ack)
        }
        ChildCommand::EmitFailed { engine, message } => {
            child_control(factory, engine)
                .await
                .emit(Err(RunFailure::new(
                    RunStage::ModelRequest,
                    RunFailureKind::Transport,
                    false,
                    message,
                )));
            Ok(ChildReply::Ack)
        }
        ChildCommand::StartJob {
            engine,
            title,
            details,
        } => {
            let registry = factory.wait_for_registry(engine).await;
            let lease = registry
                .start(JobKind::Bash, title, Arc::new(FixtureJobDetails(details)))
                .map_err(|error| error.to_string())?;
            let job_id = lease.id().to_string();
            leases.borrow_mut().insert((engine, job_id.clone()), lease);
            Ok(ChildReply::JobStarted { job_id })
        }
        ChildCommand::FinishJob {
            engine,
            job_id,
            details,
        } => {
            let lease = leases
                .borrow_mut()
                .remove(&(engine, job_id.clone()))
                .ok_or_else(|| format!("controlled job {engine}/{job_id} is not running"))?;
            lease
                .finish(JobState::Completed, Arc::new(FixtureJobDetails(details)))
                .map_err(|error| error.to_string())?;
            Ok(ChildReply::Ack)
        }
    }
}

async fn child_control(
    factory: &ControlledEngineFactory,
    engine: usize,
) -> ControlledEngineControl {
    factory.wait_for_control(engine).await
}

struct ChildFixture {
    _directory: TempDir,
    paths: LocalPaths,
    control_path: PathBuf,
    log_path: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
    spawned_pids: Arc<Mutex<Vec<i32>>>,
    spawn_count: Arc<AtomicUsize>,
    command: BackendCommand,
}

impl ChildFixture {
    fn new(idle_timeout: Duration) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let paths = paths_from_root(&root);
        let control_path = root.join("control.sock");
        let log_path = root.join("child.log");
        let child = Arc::new(Mutex::new(None::<Child>));
        let spawned_pids = Arc::new(Mutex::new(Vec::new()));
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let captured_child = Arc::clone(&child);
        let captured_pids = Arc::clone(&spawned_pids);
        let captured_count = Arc::clone(&spawn_count);
        let captured_root = root.clone();
        let captured_control = control_path.clone();
        let captured_log = log_path.clone();
        let command = BackendCommand::injected(Arc::new(move |_| {
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&captured_log)?;
            let stderr = log.try_clone()?;
            let mut process = Command::new(std::env::current_exe()?);
            process
                .args(["--exact", "child_backend_entry", "--ignored", "--nocapture"])
                .env_clear()
                .env(CHILD_ROOT_ENV, &captured_root)
                .env(CHILD_CONTROL_ENV, &captured_control)
                .env(CHILD_IDLE_MS_ENV, idle_timeout.as_millis().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(stderr));
            let mut slot = captured_child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(previous) = slot.as_mut() {
                if previous.try_wait()?.is_none() {
                    return Err(io::Error::other(
                        "launcher attempted to replace a running child",
                    ));
                }
                slot.take();
            }
            let spawned = spawn_registered_child(&mut process, &captured_pids, &captured_count)?;
            *slot = Some(spawned);
            Ok(())
        }));
        Self {
            _directory: directory,
            paths,
            control_path,
            log_path,
            child,
            spawned_pids,
            spawn_count,
            command,
        }
    }

    async fn connect(&self) -> RpcBackendClient {
        connect_or_spawn(self.paths.clone(), self.command.clone())
            .await
            .unwrap_or_else(|error| panic!("connect_or_spawn failed: {error}\n{}", self.log()))
    }

    async fn command(&self, command: ChildCommand) -> ChildReply {
        let result = tokio::time::timeout(WAIT_TIMEOUT, async {
            let stream = loop {
                match UnixStream::connect(&self.control_path).await {
                    Ok(stream) => break stream,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => return Err(error),
                }
            };
            let (reader, mut writer) = stream.into_split();
            let mut encoded = serde_json::to_vec(&command).map_err(io::Error::other)?;
            encoded.push(b'\n');
            writer.write_all(&encoded).await?;
            writer.shutdown().await?;
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            serde_json::from_str::<ChildResponse>(&line).map_err(io::Error::other)
        })
        .await
        .unwrap_or_else(|_| panic!("child control timed out for {command:?}\n{}", self.log()))
        .unwrap_or_else(|error| {
            panic!(
                "child control I/O failed for {command:?}: {error}\n{}",
                self.log()
            )
        });
        match result {
            ChildResponse::Ok { value } => value,
            ChildResponse::Error { message } => {
                panic!("child rejected {command:?}: {message}\n{}", self.log())
            }
        }
    }

    async fn wait_requests(&self, engine: usize, count: usize) -> Vec<ChildRunRequest> {
        match self
            .command(ChildCommand::WaitRequests { engine, count })
            .await
        {
            ChildReply::Requests { requests } => requests,
            other => panic!("expected request reply, got {other:?}"),
        }
    }

    async fn wait_consumed(&self, engine: usize, count: u64) {
        match self
            .command(ChildCommand::WaitConsumed { engine, count })
            .await
        {
            ChildReply::Consumed { count: actual } if actual >= count => {}
            other => panic!("expected consumed count {count}, got {other:?}"),
        }
    }

    async fn start_job(&self, engine: usize, title: &str, details: &str) -> String {
        match self
            .command(ChildCommand::StartJob {
                engine,
                title: title.into(),
                details: details.into(),
            })
            .await
        {
            ChildReply::JobStarted { job_id } => job_id,
            other => panic!("expected job start reply, got {other:?}"),
        }
    }

    async fn finish_job(&self, engine: usize, job_id: &str, details: &str) {
        assert!(matches!(
            self.command(ChildCommand::FinishJob {
                engine,
                job_id: job_id.into(),
                details: details.into(),
            })
            .await,
            ChildReply::Ack
        ));
    }

    async fn stop(&self) {
        let pid = self
            .child_pid()
            .expect("a child must be running before stop");
        match kill(Pid::from_raw(pid), Signal::SIGTERM) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(error) => panic!("could not signal child {pid}: {error}\n{}", self.log()),
        }
        wait_for_path_absent(&self.paths.socket_path().to_path_buf(), WAIT_TIMEOUT)
            .await
            .unwrap_or_else(|message| panic!("{message}\n{}", self.log()));
        self.wait_for_exit(WAIT_TIMEOUT)
            .await
            .unwrap_or_else(|message| panic!("{message}\n{}", self.log()));
        assert!(
            !self.control_path.exists(),
            "control socket must be removed after stop\n{}",
            self.log()
        );
    }

    fn child_pid(&self) -> Option<i32> {
        self.child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|child| i32::try_from(child.id()).unwrap())
    }

    async fn wait_for_exit(&self, timeout: Duration) -> Result<(), String> {
        tokio::time::timeout(timeout, async {
            loop {
                let status = {
                    let mut child = self
                        .child
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    child
                        .as_mut()
                        .ok_or_else(|| "child handle is absent".to_string())?
                        .try_wait()
                        .map_err(|error| format!("could not inspect child exit: {error}"))?
                };
                if let Some(status) = status {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("child exited unsuccessfully: {status}"));
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| format!("child did not exit within {timeout:?}"))?
    }

    fn spawn_count(&self) -> usize {
        self.spawn_count.load(Ordering::Acquire)
    }

    fn registered_pids(&self) -> Vec<i32> {
        self.spawned_pids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_else(|error| {
            format!(
                "<child log {} unavailable: {error}>",
                self.log_path.display()
            )
        })
    }
}

impl Drop for ChildFixture {
    fn drop(&mut self) {
        let retained = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(child) = retained {
            cleanup_retained_child(child);
        }
        let registered = std::mem::take(
            &mut *self
                .spawned_pids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for pid in registered {
            cleanup_registered_pid(pid);
        }
    }
}

fn spawn_registered_child(
    process: &mut Command,
    spawned_pids: &Arc<Mutex<Vec<i32>>>,
    spawn_count: &AtomicUsize,
) -> io::Result<Child> {
    let mut registered = spawned_pids
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registered.try_reserve(1).map_err(io::Error::other)?;
    let mut child = process.spawn()?;
    spawn_count.fetch_add(1, Ordering::AcqRel);
    let pid = match i32::try_from(child.id()) {
        Ok(pid) => pid,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(error));
        }
    };
    registered.push(pid);
    Ok(child)
}

fn cleanup_retained_child(mut child: Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::yield_now();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredChildDone {
    Absent,
    Reaped(WaitStatus),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredChildPoll {
    Done(RegisteredChildDone),
    Alive,
    Retry,
    Unexpected(nix::errno::Errno),
}

fn classify_registered_child_poll(
    result: Result<WaitStatus, nix::errno::Errno>,
) -> RegisteredChildPoll {
    match result {
        Err(nix::errno::Errno::ECHILD) => RegisteredChildPoll::Done(RegisteredChildDone::Absent),
        Ok(status @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => {
            RegisteredChildPoll::Done(RegisteredChildDone::Reaped(status))
        }
        Ok(_) => RegisteredChildPoll::Alive,
        Err(nix::errno::Errno::EINTR) => RegisteredChildPoll::Retry,
        Err(error) => RegisteredChildPoll::Unexpected(error),
    }
}

fn poll_registered_child(pid: Pid, flags: Option<WaitPidFlag>) -> RegisteredChildPoll {
    loop {
        match classify_registered_child_poll(waitpid(pid, flags)) {
            RegisteredChildPoll::Retry => {}
            result => return result,
        }
    }
}

fn registered_child_is_done(poll: RegisteredChildPoll, pid: Pid, phase: &str) -> bool {
    match poll {
        RegisteredChildPoll::Done(_) => true,
        RegisteredChildPoll::Alive => false,
        RegisteredChildPoll::Retry => unreachable!("poll helper retries EINTR internally"),
        RegisteredChildPoll::Unexpected(error) => {
            eprintln!(
                "fixture waitpid({pid}) failed during {phase}: {error}; treating registered child as live"
            );
            false
        }
    }
}

fn cleanup_registered_pid(raw_pid: i32) {
    let pid = Pid::from_raw(raw_pid);
    if registered_child_is_done(
        poll_registered_child(pid, Some(WaitPidFlag::WNOHANG)),
        pid,
        "initial cleanup inspection",
    ) {
        return;
    }

    let _ = kill(pid, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match poll_registered_child(pid, Some(WaitPidFlag::WNOHANG)) {
            RegisteredChildPoll::Done(_) => return,
            RegisteredChildPoll::Alive => std::thread::yield_now(),
            RegisteredChildPoll::Retry => unreachable!("poll helper retries EINTR internally"),
            RegisteredChildPoll::Unexpected(error) => {
                eprintln!(
                    "fixture waitpid({pid}) failed while awaiting SIGTERM: {error}; escalating to SIGKILL"
                );
                break;
            }
        }
    }
    if registered_child_is_done(
        poll_registered_child(pid, Some(WaitPidFlag::WNOHANG)),
        pid,
        "pre-SIGKILL recheck",
    ) {
        return;
    }

    let _ = kill(pid, Signal::SIGKILL);
    loop {
        match poll_registered_child(pid, None) {
            RegisteredChildPoll::Done(_) => return,
            RegisteredChildPoll::Alive => {
                eprintln!(
                    "fixture waitpid({pid}) returned a nonterminal state after SIGKILL; waiting again"
                );
            }
            RegisteredChildPoll::Retry => unreachable!("poll helper retries EINTR internally"),
            RegisteredChildPoll::Unexpected(error) => {
                eprintln!(
                    "fixture waitpid({pid}) failed after SIGKILL: {error}; signaling and waiting again"
                );
                let _ = kill(pid, Signal::SIGKILL);
                std::thread::yield_now();
            }
        }
    }
}

#[test]
fn registered_child_poll_treats_only_terminal_or_absent_children_as_done() {
    let pid = Pid::from_raw(41);
    let exited = WaitStatus::Exited(pid, 0);
    assert_eq!(
        classify_registered_child_poll(Ok(exited)),
        RegisteredChildPoll::Done(RegisteredChildDone::Reaped(exited))
    );
    let signaled = WaitStatus::Signaled(pid, Signal::SIGTERM, false);
    assert_eq!(
        classify_registered_child_poll(Ok(signaled)),
        RegisteredChildPoll::Done(RegisteredChildDone::Reaped(signaled))
    );
    assert_eq!(
        classify_registered_child_poll(Err(nix::errno::Errno::ECHILD)),
        RegisteredChildPoll::Done(RegisteredChildDone::Absent)
    );

    for status in [
        WaitStatus::StillAlive,
        WaitStatus::Stopped(pid, Signal::SIGSTOP),
        WaitStatus::Continued(pid),
    ] {
        assert_eq!(
            classify_registered_child_poll(Ok(status)),
            RegisteredChildPoll::Alive
        );
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    for status in [
        WaitStatus::PtraceEvent(pid, Signal::SIGTRAP, 1),
        WaitStatus::PtraceSyscall(pid),
    ] {
        assert_eq!(
            classify_registered_child_poll(Ok(status)),
            RegisteredChildPoll::Alive
        );
    }

    assert_eq!(
        classify_registered_child_poll(Err(nix::errno::Errno::EINTR)),
        RegisteredChildPoll::Retry
    );
    assert_eq!(
        classify_registered_child_poll(Err(nix::errno::Errno::EINVAL)),
        RegisteredChildPoll::Unexpected(nix::errno::Errno::EINVAL)
    );
}

fn assert_child_was_reaped(child: &Arc<Mutex<Option<Child>>>, pid: i32, scenario: &str) {
    let leaked = child
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(mut leaked) = leaked {
        let _ = leaked.kill();
        let _ = leaked.wait();
        panic!("{scenario} left child {pid} retained after fixture drop");
    }
    assert_pid_was_reaped(pid, scenario);
}

fn assert_pid_was_reaped(pid: i32, scenario: &str) {
    match poll_registered_child(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)) {
        RegisteredChildPoll::Done(RegisteredChildDone::Absent) => {}
        RegisteredChildPoll::Done(RegisteredChildDone::Reaped(status)) => {
            panic!("{scenario} reaped child {pid} only during the post-drop assertion: {status:?}")
        }
        RegisteredChildPoll::Alive => {
            cleanup_registered_pid(pid);
            panic!("{scenario} left child {pid} alive after fixture drop");
        }
        RegisteredChildPoll::Retry => unreachable!("poll helper retries EINTR internally"),
        RegisteredChildPoll::Unexpected(error) => {
            eprintln!(
                "{scenario} could not inspect child {pid} after fixture drop: {error}; cleaning it before reporting failure"
            );
            cleanup_registered_pid(pid);
            panic!("{scenario} could not inspect child {pid} after fixture drop: {error}");
        }
    }
}

fn paths_from_root(root: &Path) -> LocalPaths {
    LocalPaths::from_roots(PathRoots {
        runtime_dir: Some(root.join("runtime")),
        temp_dir: root.join("tmp"),
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        effective_uid: Uid::effective().as_raw(),
    })
}

async fn wait_for_path_absent(path: &PathBuf, timeout: Duration) -> Result<(), String> {
    tokio::time::timeout(timeout, async {
        loop {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Ok(_) => tokio::task::yield_now().await,
                Err(error) => panic!("could not inspect {}: {error}", path.display()),
            }
        }
    })
    .await
    .map_err(|_| format!("{} remained present for {timeout:?}", path.display()))
}

async fn assert_path_remains(path: &PathBuf, duration: Duration, diagnostics: &str) {
    let result = wait_for_path_absent(path, duration).await;
    assert!(
        result.is_err(),
        "{} disappeared while lifecycle work was active\n{diagnostics}",
        path.display()
    );
}

async fn next_matching(
    session: &mut RpcSessionClient,
    predicate: impl Fn(&SessionEvent) -> bool,
) -> SessionEventEnvelope {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            match session.next_update().await.unwrap() {
                SessionUpdate::Event(event) if predicate(&event.event) => return event,
                SessionUpdate::Event(_) | SessionUpdate::SnapshotReplaced(_) => {}
                SessionUpdate::Warning(warning) => {
                    panic!("unexpected session recovery warning: {warning}")
                }
                SessionUpdate::Deleted { .. } => {
                    panic!("session was deleted while waiting for an ordinary event")
                }
            }
        }
    })
    .await
    .expect("session event predicate timed out")
}

async fn collect_through_completion(session: &mut RpcSessionClient) -> Vec<SessionEventEnvelope> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        let mut events = Vec::new();
        loop {
            match session.next_update().await.unwrap() {
                SessionUpdate::Event(event) => {
                    let completed = matches!(event.event, SessionEvent::Completed { .. });
                    events.push(event);
                    if completed {
                        return events;
                    }
                }
                SessionUpdate::SnapshotReplaced(_) => {
                    panic!("contiguous observer stream unexpectedly required replacement")
                }
                SessionUpdate::Warning(warning) => {
                    panic!("unexpected session recovery warning: {warning}")
                }
                SessionUpdate::Deleted { .. } => {
                    panic!("session was deleted while waiting for completion")
                }
            }
        }
    })
    .await
    .expect("completion event timed out")
}

async fn wait_for_jobs(
    session: &RpcSessionClient,
    predicate: impl Fn(&[moh::session::JobSnapshotDto]) -> bool,
) -> Vec<moh::session::JobSnapshotDto> {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            let jobs = session.list_jobs().await.unwrap();
            if predicate(&jobs) {
                return jobs;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("job predicate timed out")
}

async fn disconnect(client: RpcBackendClient) {
    tokio::time::timeout(WAIT_TIMEOUT, client.disconnect())
        .await
        .expect("RPC disconnect timed out")
        .expect("RPC disconnect failed");
}

async fn run_scenario_with_deadline<T>(
    fixture: &ChildFixture,
    scenario: &str,
    timeout: Duration,
    operation: impl Future<Output = T>,
) -> Result<T, String> {
    tokio::time::timeout(timeout, operation).await.map_err(|_| {
        format!(
            "client-server scenario {scenario:?} timed out after {timeout:?}\n{}",
            fixture.log()
        )
    })
}

async fn run_scenario<T>(
    fixture: &ChildFixture,
    scenario: &str,
    operation: impl Future<Output = T>,
) -> T {
    run_scenario_with_deadline(fixture, scenario, SCENARIO_TIMEOUT, operation)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

#[tokio::test(flavor = "current_thread")]
async fn detached_run_commits_and_reattaches_to_the_same_backend() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            run_scenario(&fixture, "detach and reattach", async {
                let client = fixture.connect().await;
                let instance_id = client.info().instance_id.clone();
                let cwd = b"/work/detach".to_vec();
                let RpcStartup::Draft(draft) = client.startup(cwd.clone()).await.unwrap() else {
                    panic!("empty project must start with draft defaults");
                };
                let session = client
                    .materialize(cwd, "detached prompt".into(), draft.settings)
                    .await
                    .unwrap()
                    .session;
                let session_id = session.snapshot().summary.id;
                let requests = fixture.wait_requests(0, 1).await;
                assert_eq!(requests[0].prompt, "detached prompt");

                drop(session);
                disconnect(client).await;
                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitDelta {
                            engine: 0,
                            text: "detached ".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitCompleted {
                            engine: 0,
                            response: "detached answer".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                fixture.wait_consumed(0, 2).await;

                let reconnected = fixture.connect().await;
                assert_eq!(reconnected.info().instance_id, instance_id);
                let reattached = reconnected
                    .open_session(
                        SessionSelector::Id(session_id),
                        b"/ignored-for-id-selection".to_vec(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    reattached.snapshot().transcript,
                    [
                        TranscriptItem::User("detached prompt".into()),
                        TranscriptItem::Assistant("detached answer".into()),
                    ]
                );
                assert!(!reattached.snapshot().busy);
                assert!(reattached.snapshot().active_run.is_none());

                drop(reattached);
                disconnect(reconnected).await;
                fixture.stop().await;
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_clients_preserve_background_run_across_detach_switch_and_reattach() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            run_scenario(&fixture, "two-client detach switch and reattach", async {
                let first_backend = fixture.connect().await;
                let second_backend = fixture.connect().await;
                let first_cwd = b"/work/detach-switch/source".to_vec();
                let target_cwd = b"/work/detach-switch/target".to_vec();

                let RpcStartup::Draft(first_draft) =
                    first_backend.startup(first_cwd.clone()).await.unwrap()
                else {
                    panic!("source project must start as a draft");
                };
                let source = first_backend
                    .materialize(
                        first_cwd.clone(),
                        "keep running after switch".into(),
                        first_draft.settings,
                    )
                    .await
                    .unwrap()
                    .session;
                let source_id = source.snapshot().summary.id;

                let RpcStartup::Draft(target_draft) =
                    second_backend.startup(target_cwd.clone()).await.unwrap()
                else {
                    panic!("target project must start as a draft");
                };
                let target = second_backend
                    .materialize(
                        target_cwd.clone(),
                        "switch target".into(),
                        target_draft.settings,
                    )
                    .await
                    .unwrap()
                    .session;
                let target_id = target.snapshot().summary.id;
                fixture.wait_requests(0, 1).await;
                fixture.wait_requests(1, 1).await;

                let switched = first_backend
                    .open_session(SessionSelector::Id(target_id), first_cwd.clone())
                    .await
                    .unwrap();
                source.detach().await.unwrap();

                let live = first_backend
                    .list_sessions(SessionListScope::All)
                    .await
                    .unwrap();
                assert_eq!(
                    live.iter()
                        .find(|summary| summary.id == source_id)
                        .unwrap()
                        .attached_clients,
                    0
                );
                assert_eq!(
                    live.iter()
                        .find(|summary| summary.id == target_id)
                        .unwrap()
                        .attached_clients,
                    2
                );

                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitDelta {
                            engine: 0,
                            text: "background ".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitCompleted {
                            engine: 0,
                            response: "background answer".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                fixture.wait_consumed(0, 2).await;

                let reattached = first_backend
                    .open_session(SessionSelector::Id(source_id), target_cwd.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    reattached.snapshot().transcript,
                    [
                        TranscriptItem::User("keep running after switch".into()),
                        TranscriptItem::Assistant("background answer".into()),
                    ]
                );
                assert!(!reattached.snapshot().busy);

                switched.detach().await.unwrap();
                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitCompleted {
                            engine: 1,
                            response: "target answer".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                fixture.wait_consumed(1, 1).await;

                reattached.detach().await.unwrap();
                target.detach().await.unwrap();
                disconnect(first_backend).await;
                disconnect(second_backend).await;
                fixture.stop().await;
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_clients_receive_typed_remote_delete_then_startup_fallback() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            run_scenario(&fixture, "typed remote deletion and fallback", async {
                let deleting_backend = fixture.connect().await;
                let affected_backend = fixture.connect().await;
                let cwd = b"/work/remote-delete".to_vec();
                let RpcStartup::Draft(draft) = deleting_backend.startup(cwd.clone()).await.unwrap()
                else {
                    panic!("remote-delete project must start as a draft");
                };
                let fallback = deleting_backend
                    .materialize(
                        cwd.clone(),
                        "fallback stays running".into(),
                        draft.settings.clone(),
                    )
                    .await
                    .unwrap()
                    .session;
                let fallback_id = fallback.snapshot().summary.id;
                let mut victim = affected_backend
                    .materialize(
                        cwd.clone(),
                        "delete this current chat".into(),
                        draft.settings,
                    )
                    .await
                    .unwrap()
                    .session;
                let victim_id = victim.snapshot().summary.id;
                fixture.wait_requests(0, 1).await;
                fixture.wait_requests(1, 1).await;

                deleting_backend.delete_session(victim_id).await.unwrap();
                let deleted = tokio::time::timeout(WAIT_TIMEOUT, async {
                    loop {
                        match victim.next_update().await.unwrap() {
                            SessionUpdate::Deleted { session_id, cwd } => break (session_id, cwd),
                            SessionUpdate::Event(_) | SessionUpdate::SnapshotReplaced(_) => {}
                            SessionUpdate::Warning(warning) => {
                                panic!("unexpected session recovery warning: {warning}")
                            }
                        }
                    }
                })
                .await
                .expect("typed remote deletion timed out");
                assert_eq!(deleted, (victim_id, cwd.clone()));

                let RpcStartup::Attached(fallback_after_delete) =
                    affected_backend.startup(cwd.clone()).await.unwrap()
                else {
                    panic!("startup must attach the remaining running session");
                };
                assert_eq!(fallback_after_delete.snapshot().summary.id, fallback_id);

                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitCompleted {
                            engine: 0,
                            response: "fallback answer".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                fixture.wait_consumed(0, 1).await;

                fallback_after_delete.detach().await.unwrap();
                fallback.detach().await.unwrap();
                drop(victim);
                disconnect(deleting_backend).await;
                disconnect(affected_backend).await;
                fixture.stop().await;
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn same_cwd_sessions_isolate_runs_jobs_and_share_one_ordered_observer_stream() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            run_scenario(&fixture, "same-CWD isolation and observers", async {
                let first_backend = fixture.connect().await;
                let cwd = b"/work/shared".to_vec();
                let RpcStartup::Draft(draft) = first_backend.startup(cwd.clone()).await.unwrap()
                else {
                    panic!("empty shared project must start with draft defaults");
                };
                let first_materialized = first_backend
                    .materialize(cwd.clone(), "first prompt".into(), draft.settings.clone())
                    .await
                    .unwrap();
                let second_materialized = first_backend
                    .materialize(cwd.clone(), "second prompt".into(), draft.settings)
                    .await
                    .unwrap();
                assert_eq!(first_materialized.run_id, 0);
                assert_eq!(second_materialized.run_id, 0);
                let mut first = first_materialized.session;
                let mut second = second_materialized.session;
                let second_backend = fixture.connect().await;
                let mut first_observer = second_backend
                    .open_session(
                        SessionSelector::Id(first.snapshot().summary.id),
                        cwd.clone(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    first.snapshot().sequence,
                    first_observer.snapshot().sequence
                );

                let first_requests = fixture.wait_requests(0, 1).await;
                let second_requests = fixture.wait_requests(1, 1).await;
                assert_eq!(first_requests[0].prompt, "first prompt");
                assert_eq!(second_requests[0].prompt, "second prompt");
                assert!(first_requests[0].history.is_empty());
                assert!(second_requests[0].history.is_empty());
                let live = first_backend
                    .list_sessions(SessionListScope::Project(cwd.clone()))
                    .await
                    .unwrap();
                assert_eq!(live.len(), 2);
                assert!(live.iter().all(|summary| summary.busy));
                assert_ne!(live[0].id, live[1].id);

                let first_job = fixture
                    .start_job(0, "first-only job", "first running")
                    .await;
                let second_job = fixture
                    .start_job(1, "second-only job", "second running")
                    .await;
                let first_jobs = wait_for_jobs(&first, |jobs| jobs.len() == 1).await;
                let second_jobs = wait_for_jobs(&second, |jobs| jobs.len() == 1).await;
                assert_eq!(first_jobs[0].title, "first-only job");
                assert_eq!(second_jobs[0].title, "second-only job");
                assert_eq!(first_jobs[0].id, "job-0");
                assert_eq!(second_jobs[0].id, "job-0");

                for command in [
                    ChildCommand::EmitDelta {
                        engine: 0,
                        text: "first delta".into(),
                    },
                    ChildCommand::EmitCompleted {
                        engine: 0,
                        response: "first answer".into(),
                    },
                    ChildCommand::EmitDelta {
                        engine: 1,
                        text: "second delta".into(),
                    },
                    ChildCommand::EmitCompleted {
                        engine: 1,
                        response: "second answer".into(),
                    },
                ] {
                    assert!(matches!(fixture.command(command).await, ChildReply::Ack));
                }
                let (first_events, first_observer_events, second_events) = futures::join!(
                    collect_through_completion(&mut first),
                    collect_through_completion(&mut first_observer),
                    collect_through_completion(&mut second),
                );
                assert_eq!(first_events, first_observer_events);
                assert!(
                    first_events
                        .windows(2)
                        .all(|window| window[1].sequence == window[0].sequence + 1)
                );
                assert!(first_events.iter().any(|event| matches!(
                    &event.event,
                    SessionEvent::AssistantDelta { text, .. } if text == "first delta"
                )));
                assert!(second_events.iter().any(|event| matches!(
                    &event.event,
                    SessionEvent::AssistantDelta { text, .. } if text == "second delta"
                )));

                fixture.finish_job(0, &first_job, "first done").await;
                fixture.finish_job(1, &second_job, "second done").await;
                let first_jobs = wait_for_jobs(&first, |jobs| {
                    jobs.first()
                        .is_some_and(|job| job.state == JobState::Completed)
                })
                .await;
                let second_jobs = wait_for_jobs(&second, |jobs| {
                    jobs.first()
                        .is_some_and(|job| job.state == JobState::Completed)
                })
                .await;
                assert_eq!(first_jobs[0].details, "first done");
                assert_eq!(second_jobs[0].details, "second done");

                let first_snapshot = second_backend
                    .open_session(
                        SessionSelector::Id(first.snapshot().summary.id),
                        cwd.clone(),
                    )
                    .await
                    .unwrap();
                let second_snapshot = second_backend
                    .open_session(SessionSelector::Id(second.snapshot().summary.id), cwd)
                    .await
                    .unwrap();
                assert_eq!(
                    first_snapshot.snapshot().transcript,
                    [
                        TranscriptItem::User("first prompt".into()),
                        TranscriptItem::Assistant("first answer".into()),
                    ]
                );
                assert_eq!(
                    second_snapshot.snapshot().transcript,
                    [
                        TranscriptItem::User("second prompt".into()),
                        TranscriptItem::Assistant("second answer".into()),
                    ]
                );

                drop(first_snapshot);
                drop(second_snapshot);
                drop(first_observer);
                drop(first);
                drop(second);
                disconnect(second_backend).await;
                disconnect(first_backend).await;
                fixture.stop().await;
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn restart_restores_durable_visible_transcript_and_only_committed_model_history() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            run_scenario(&fixture, "restart durable visible transcript", async {
                let first_backend = fixture.connect().await;
                let first_instance = first_backend.info().instance_id.clone();
                let cwd = b"/work/persistence".to_vec();
                let RpcStartup::Draft(mut draft) =
                    first_backend.startup(cwd.clone()).await.unwrap()
                else {
                    panic!("empty persistence project must start with draft defaults");
                };
                draft.settings.model = "gpt-persist".into();
                draft.settings.reasoning = ReasoningLevel::Xhigh;
                let mut session = first_backend
                    .materialize(cwd, "committed prompt".into(), draft.settings)
                    .await
                    .unwrap()
                    .session;
                let session_id = session.snapshot().summary.id;
                fixture.wait_requests(0, 1).await;
                for command in [
                    ChildCommand::EmitContext {
                        engine: 0,
                        input_tokens: 77,
                    },
                    ChildCommand::EmitDelta {
                        engine: 0,
                        text: "partial committed text".into(),
                    },
                    ChildCommand::EmitToolStarted {
                        engine: 0,
                        call_id: "call-committed".into(),
                        name: "read".into(),
                    },
                    ChildCommand::EmitToolFinished {
                        engine: 0,
                        call_id: "call-committed".into(),
                        name: "read".into(),
                    },
                    ChildCommand::EmitCompleted {
                        engine: 0,
                        response: "committed answer".into(),
                    },
                ] {
                    assert!(matches!(fixture.command(command).await, ChildReply::Ack));
                }
                let _ = next_matching(&mut session, |event| {
                    matches!(event, SessionEvent::Completed { .. })
                })
                .await;
                let committed = first_backend
                    .open_session(
                        SessionSelector::Id(session_id),
                        b"/work/persistence".to_vec(),
                    )
                    .await
                    .unwrap();
                let committed_activity = committed.snapshot().summary.last_activity;
                drop(committed);

                session.submit("failed prompt".into()).await.unwrap();
                fixture.wait_requests(0, 2).await;
                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitFailed {
                            engine: 0,
                            message: "controlled failure".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                let _ = next_matching(&mut session, |event| {
                    matches!(event, SessionEvent::Failed { .. })
                })
                .await;

                session.submit("cancelled prompt".into()).await.unwrap();
                fixture.wait_requests(0, 3).await;
                session.cancel().await.unwrap();
                let _ = next_matching(&mut session, |event| {
                    matches!(event, SessionEvent::Cancelled { .. })
                })
                .await;

                let job = fixture
                    .start_job(0, "ephemeral job", "ephemeral running")
                    .await;
                wait_for_jobs(&session, |jobs| !jobs.is_empty()).await;
                fixture.finish_job(0, &job, "ephemeral done").await;
                wait_for_jobs(&session, |jobs| {
                    jobs.first()
                        .is_some_and(|job| job.state == JobState::Completed)
                })
                .await;

                session.submit("active prompt".into()).await.unwrap();
                fixture.wait_requests(0, 4).await;
                for command in [
                    ChildCommand::EmitDelta {
                        engine: 0,
                        text: "active partial".into(),
                    },
                    ChildCommand::EmitToolStarted {
                        engine: 0,
                        call_id: "call-active".into(),
                        name: "bash".into(),
                    },
                    ChildCommand::EmitToolFinished {
                        engine: 0,
                        call_id: "call-active".into(),
                        name: "bash".into(),
                    },
                ] {
                    assert!(matches!(fixture.command(command).await, ChildReply::Ack));
                }
                fixture.wait_consumed(0, 9).await;
                drop(session);
                disconnect(first_backend).await;
                fixture.stop().await;

                let restarted_backend = fixture.connect().await;
                assert_ne!(restarted_backend.info().instance_id, first_instance);
                let restored = restarted_backend
                    .open_session(
                        SessionSelector::Id(session_id),
                        b"/work/persistence".to_vec(),
                    )
                    .await
                    .unwrap();
                let snapshot = restored.snapshot();
                assert_eq!(
                    snapshot.transcript,
                    [
                        TranscriptItem::User("committed prompt".into()),
                        TranscriptItem::ToolStarted {
                            run_id: 0,
                            call_id: "call-committed".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"fixture": true}),
                        },
                        TranscriptItem::Assistant("committed answer".into()),
                        TranscriptItem::User("failed prompt".into()),
                        TranscriptItem::Failed {
                            run_id: 1,
                            failure: RunFailureSnapshot {
                                stage: RunStage::ModelRequest,
                                kind: RunFailureKind::Transport,
                                retryable: false,
                                message: "controlled failure".into(),
                            },
                        },
                        TranscriptItem::User("cancelled prompt".into()),
                        TranscriptItem::Cancelled { run_id: 2 },
                        TranscriptItem::User("active prompt".into()),
                        TranscriptItem::ToolStarted {
                            run_id: 3,
                            call_id: "call-active".into(),
                            name: "bash".into(),
                            arguments: serde_json::json!({"fixture": true}),
                        },
                        TranscriptItem::Failed {
                            run_id: 3,
                            failure: RunFailureSnapshot {
                                stage: RunStage::Finalization,
                                kind: RunFailureKind::RuntimeInfrastructure,
                                retryable: true,
                                message: "run interrupted by backend restart".into(),
                            },
                        },
                    ]
                );
                assert_eq!(snapshot.settings.model, "gpt-persist");
                assert_eq!(snapshot.settings.reasoning, ReasoningLevel::Xhigh);
                assert_eq!(snapshot.settings.context_tokens, 77);
                assert_eq!(snapshot.summary.last_activity, committed_activity);
                assert_eq!(snapshot.sequence, 0);
                assert_eq!(snapshot.summary.attached_clients, 1);
                assert!(!snapshot.busy);
                assert!(snapshot.active_run.is_none());
                assert!(snapshot.jobs.is_empty());
                assert!(snapshot.persistence_warning.is_none());

                restored.submit("after restart".into()).await.unwrap();
                assert_eq!(
                    fixture.wait_requests(0, 1).await,
                    [ChildRunRequest {
                        prompt: "after restart".into(),
                        history: vec![
                            ChildMessage {
                                role: "user".into(),
                                text: "committed prompt".into(),
                            },
                            ChildMessage {
                                role: "assistant".into(),
                                text: "committed answer".into(),
                            },
                        ],
                        cwd: "/work/persistence".into(),
                    }]
                );
                restored.cancel().await.unwrap();

                drop(restored);
                disconnect(restarted_backend).await;
                fixture.stop().await;
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_connect_or_spawn_calls_launch_exactly_one_backend() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            run_scenario(&fixture, "concurrent connect-or-spawn", async {
                let clients = join_all((0..8).map(|_| fixture.connect())).await;
                let instance = clients[0].info().instance_id.clone();
                assert!(
                    clients
                        .iter()
                        .all(|client| client.info().instance_id == instance)
                );
                assert_eq!(fixture.spawn_count(), 1);
                for client in clients {
                    disconnect(client).await;
                }
                fixture.stop().await;
            })
            .await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_injected_launch_does_not_spawn_an_untracked_child() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            let (duplicate_failed, spawn_count, retained_pid, retained_after, registered_pids) =
                run_scenario(&fixture, "duplicate injected launch", async {
                    fixture.command.spawn(&fixture.paths).unwrap();
                    assert!(matches!(
                        fixture.command(ChildCommand::Ping).await,
                        ChildReply::Pong
                    ));
                    let retained_pid = fixture.child_pid().unwrap();
                    let duplicate_failed = fixture.command.spawn(&fixture.paths).is_err();
                    (
                        duplicate_failed,
                        fixture.spawn_count(),
                        retained_pid,
                        fixture.child_pid(),
                        fixture.registered_pids(),
                    )
                })
                .await;

            drop(fixture);
            for pid in &registered_pids {
                assert_pid_was_reaped(*pid, "duplicate-launch fixture");
            }
            assert!(duplicate_failed);
            assert_eq!(
                spawn_count, 1,
                "duplicate launch registered OS child PIDs {registered_pids:?}"
            );
            assert_eq!(retained_after, Some(retained_pid));
            assert_eq!(registered_pids, [retained_pid]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_deadline_returns_and_fixture_drop_reaps_the_child() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            let child_slot = Arc::clone(&fixture.child);
            let child_pid = run_scenario(&fixture, "deadline cleanup regression", async {
                fixture.command.spawn(&fixture.paths).unwrap();
                assert!(matches!(
                    fixture.command(ChildCommand::Ping).await,
                    ChildReply::Pong
                ));
                let child_pid = fixture.child_pid().unwrap();
                let client = fixture.connect().await;
                let error = run_scenario_with_deadline(
                    &fixture,
                    "never-resolving fixture probe",
                    Duration::from_millis(25),
                    async move {
                        let _client = client;
                        future::pending::<()>().await;
                    },
                )
                .await
                .unwrap_err();
                assert!(error.contains("never-resolving fixture probe"));
                assert!(error.contains("timed out"));
                assert!(error.ends_with(&fixture.log()));
                child_pid
            })
            .await;

            drop(fixture);
            assert_child_was_reaped(&child_slot, child_pid, "timed-out fixture");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn poisoned_child_slot_is_still_cleaned_and_reaped() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            let child_slot = Arc::clone(&fixture.child);
            let child_pid = run_scenario(&fixture, "poisoned child-slot cleanup", async {
                fixture.command.spawn(&fixture.paths).unwrap();
                assert!(matches!(
                    fixture.command(ChildCommand::Ping).await,
                    ChildReply::Pong
                ));
                let child_pid = fixture.child_pid().unwrap();
                let poison_target = Arc::clone(&child_slot);
                let poisoned = catch_unwind(AssertUnwindSafe(move || {
                    let _guard = poison_target.lock().unwrap();
                    panic!("poison retained child slot");
                }));
                assert!(poisoned.is_err());
                child_pid
            })
            .await;

            drop(fixture);
            assert_child_was_reaped(&child_slot, child_pid, "poisoned-slot fixture");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn lost_spawn_handle_is_reaped_when_the_fixture_drops() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            let child_pid = run_scenario(&fixture, "lost child-handle cleanup", async {
                fixture.command.spawn(&fixture.paths).unwrap();
                assert!(matches!(
                    fixture.command(ChildCommand::Ping).await,
                    ChildReply::Pong
                ));
                let child_pid = fixture.child_pid().unwrap();
                drop(
                    fixture
                        .child
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .unwrap(),
                );
                child_pid
            })
            .await;

            drop(fixture);
            assert_pid_was_reaped(child_pid, "lost-handle fixture");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn poisoned_spawn_registry_still_reaps_a_lost_child() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_secs(5));
            let child_pid = run_scenario(&fixture, "poisoned PID-registry cleanup", async {
                fixture.command.spawn(&fixture.paths).unwrap();
                assert!(matches!(
                    fixture.command(ChildCommand::Ping).await,
                    ChildReply::Pong
                ));
                let child_pid = fixture.child_pid().unwrap();
                drop(
                    fixture
                        .child
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .unwrap(),
                );
                let poison_target = Arc::clone(&fixture.spawned_pids);
                let poisoned = catch_unwind(AssertUnwindSafe(move || {
                    let _guard = poison_target.lock().unwrap();
                    panic!("poison spawned PID registry");
                }));
                assert!(poisoned.is_err());
                child_pid
            })
            .await;

            let dropped = catch_unwind(AssertUnwindSafe(|| drop(fixture)));
            assert_pid_was_reaped(child_pid, "poisoned-registry fixture");
            assert!(
                dropped.is_ok(),
                "fixture drop must recover a poisoned registry"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn detached_run_and_job_veto_idle_until_both_settle() {
    LocalSet::new()
        .run_until(async {
            let fixture = ChildFixture::new(Duration::from_millis(100));
            run_scenario(&fixture, "detached run and job idle veto", async {
                let client = fixture.connect().await;
                let cwd = b"/work/idle".to_vec();
                let RpcStartup::Draft(draft) = client.startup(cwd.clone()).await.unwrap() else {
                    panic!("empty idle project must start with draft defaults");
                };
                let session = client
                    .materialize(cwd, "keep backend alive".into(), draft.settings)
                    .await
                    .unwrap()
                    .session;
                let session_id = session.snapshot().summary.id;
                fixture.wait_requests(0, 1).await;
                drop(session);
                disconnect(client).await;

                assert_path_remains(
                    &fixture.paths.socket_path().to_path_buf(),
                    Duration::from_millis(250),
                    &fixture.log(),
                )
                .await;

                let job_client = fixture.connect().await;
                let mut job_session = job_client
                    .open_session(SessionSelector::Id(session_id), b"/work/idle".to_vec())
                    .await
                    .unwrap();
                let job = fixture.start_job(0, "idle-veto job", "still running").await;
                let _ = next_matching(&mut job_session, |event| {
                    matches!(
                        event,
                        SessionEvent::JobsChanged(jobs)
                            if jobs.iter().any(|job| job.state == JobState::Running)
                    )
                })
                .await;
                drop(job_session);
                disconnect(job_client).await;

                assert!(matches!(
                    fixture
                        .command(ChildCommand::EmitCompleted {
                            engine: 0,
                            response: "run settled".into(),
                        })
                        .await,
                    ChildReply::Ack
                ));
                fixture.wait_consumed(0, 1).await;
                assert_path_remains(
                    &fixture.paths.socket_path().to_path_buf(),
                    Duration::from_millis(250),
                    &fixture.log(),
                )
                .await;

                fixture.finish_job(0, &job, "job settled").await;
                wait_for_path_absent(&fixture.paths.socket_path().to_path_buf(), WAIT_TIMEOUT)
                    .await
                    .unwrap_or_else(|message| panic!("{message}\n{}", fixture.log()));
                fixture
                    .wait_for_exit(WAIT_TIMEOUT)
                    .await
                    .unwrap_or_else(|message| panic!("{message}\n{}", fixture.log()));
                assert_eq!(fixture.spawn_count(), 1);
                assert!(!fixture.control_path.exists());
            })
            .await;
        })
        .await;
}
