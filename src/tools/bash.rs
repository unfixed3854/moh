//! Foreground and background non-interactive Bash execution.

use std::{
    collections::VecDeque,
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use garde::Validate;
use rig::tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Child,
    sync::{Mutex as AsyncMutex, oneshot},
};

use super::{
    JobDetails, JobKind, JobLease, JobRegistry, JobRegistryError, JobSnapshot, JobState,
    JobUpdater, blocking,
};

const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2_000;

/// Arguments accepted by the model-visible Bash tool.
#[derive(Debug, Deserialize, JsonSchema, Validate)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// Command interpreted by Bash with `-lc`.
    #[garde(length(min = 1))]
    pub command: String,
    /// Return when the command is running instead of waiting for completion.
    #[serde(default)]
    #[garde(skip)]
    pub background: bool,
    /// Optional command timeout in milliseconds.
    #[garde(range(min = 1, max = 3_600_000))]
    pub timeout_ms: Option<u64>,
}

/// Creates cwd-bound Bash services sharing one job registry.
#[derive(Clone)]
pub struct BashServiceFactory {
    registry: JobRegistry,
    program: OsString,
}

/// Async non-interactive Bash executor bound to one working directory.
pub struct BashService {
    cwd: PathBuf,
    registry: JobRegistry,
    program: OsString,
}

/// Bash-specific details rendered in generic job snapshots.
#[derive(Clone, Debug)]
pub struct BashJobDetails {
    command: String,
    output: String,
    full_output: Option<Arc<OutputLog>>,
    truncated: bool,
    exit_code: Option<i32>,
    reason: Option<String>,
}

#[derive(Debug)]
struct OutputLog {
    path: PathBuf,
    temporary_path: Mutex<Option<tempfile::TempPath>>,
}

struct OutputStore {
    file: Box<dyn AsyncWrite + Send + Unpin>,
    command: String,
    lines: VecDeque<String>,
    bytes: usize,
    observed_bytes: usize,
    observed_lines: usize,
    truncated: bool,
    log: Arc<OutputLog>,
}

#[derive(Default)]
struct StreamCarry {
    bytes: Vec<u8>,
    text: String,
}

struct CancelOnDrop {
    registry: JobRegistry,
    id: super::JobId,
    armed: bool,
}

struct SupervisorCommand {
    cwd: PathBuf,
    program: OsString,
    text: String,
    timeout_ms: Option<u64>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.registry.request_cancel(self.id);
        }
    }
}

/// Stable model-visible failures returned by the Bash service.
#[derive(Debug, Error)]
pub enum BashToolError {
    #[error("[E_INVALID_ARGUMENT] {0}")]
    /// The request does not satisfy the Bash contract.
    InvalidArgument(&'static str),
    #[error("[E_BUSY] too many Bash jobs are running")]
    /// The process-wide active Bash job limit has been reached.
    Busy,
    #[error("[E_SPAWN] failed to start {id}")]
    /// Bash could not be started after its job was reserved.
    Spawn {
        /// Reserved job identifier associated with the failed spawn.
        id: super::JobId,
        /// Operating-system error returned while starting Bash.
        #[source]
        source: std::io::Error,
    },
    #[error("[E_RUNTIME] Bash job registry is unavailable")]
    /// The shared registry could not safely complete the operation.
    Runtime,
    #[error("[E_RUNTIME] Bash output log is unavailable")]
    /// The temporary output log could not be created or updated.
    Output,
}

impl BashServiceFactory {
    /// Creates a factory using the platform's `bash` executable.
    pub fn new(registry: JobRegistry) -> Self {
        Self::with_program(registry, OsString::from("bash"))
    }

    fn with_program(registry: JobRegistry, program: OsString) -> Self {
        Self { registry, program }
    }

    /// Binds Bash execution to the cwd for one agent run.
    pub fn for_cwd(&self, cwd: PathBuf) -> BashService {
        BashService {
            cwd,
            registry: self.registry.clone(),
            program: self.program.clone(),
        }
    }
}

impl BashService {
    /// Returns the model-facing description for Bash execution.
    pub fn description() -> &'static str {
        "Run a non-interactive Bash command in the current working directory. Commands can run in the foreground or continue as background jobs."
    }

    /// Starts Bash and either returns its initial or terminal job snapshot.
    pub async fn bash(&self, args: BashArgs) -> Result<ToolOutput, BashToolError> {
        args.validate()
            .map_err(|_| BashToolError::InvalidArgument("invalid Bash arguments"))?;
        let (log, file) = create_output_log().await?;
        let initial_details = Arc::new(BashJobDetails::running(
            args.command.clone(),
            Arc::clone(&log),
        ));
        let lease = self
            .registry
            .start(
                JobKind::Bash,
                format!("bash: {}", args.command),
                initial_details,
            )
            .map_err(map_registry_error)?;
        let id = lease.id();
        let mut cancellation_guard = CancelOnDrop {
            registry: self.registry.clone(),
            id,
            armed: !args.background,
        };
        let initial = lease.snapshot().map_err(map_registry_error)?;
        let updater = lease.updater().map_err(map_registry_error)?;
        let store = Arc::new(AsyncMutex::new(OutputStore::new(
            file,
            log,
            args.command.clone(),
        )));
        let (started_tx, started_rx) = oneshot::channel();
        tokio::spawn(run_supervisor(
            lease,
            updater,
            SupervisorCommand {
                cwd: self.cwd.clone(),
                program: self.program.clone(),
                text: args.command,
                timeout_ms: args.timeout_ms,
            },
            store,
            started_tx,
        ));
        started_rx.await.map_err(|_| BashToolError::Runtime)??;

        if args.background {
            return Ok(render(initial));
        }
        let mut waited = self
            .registry
            .wait(&[id], None)
            .await
            .map_err(map_registry_error)?
            .snapshots;
        cancellation_guard.armed = false;
        Ok(render(waited.remove(0)))
    }
}

impl BashJobDetails {
    fn running(command: String, log: Arc<OutputLog>) -> Self {
        Self {
            command,
            output: String::new(),
            full_output: Some(log),
            truncated: false,
            exit_code: None,
            reason: None,
        }
    }
}

impl JobDetails for BashJobDetails {
    fn render(&self) -> String {
        let mut rendered = format!("command: {}\noutput:{}", self.command, self.output);
        if let Some(exit_code) = self.exit_code {
            rendered.push_str(&format!("\nexit code: {exit_code}"));
        }
        if let Some(reason) = &self.reason {
            rendered.push_str(&format!("\nreason: {reason}"));
        }
        if self.truncated
            && let Some(log) = &self.full_output
            && log.is_available()
        {
            rendered.push_str(&format!("\nFull output: {}", log.path.display()));
        }
        rendered
    }

    fn cleanup(&self) {
        if let Some(log) = &self.full_output {
            log.cleanup();
        }
    }
}

impl OutputLog {
    fn is_available(&self) -> bool {
        self.temporary_path
            .lock()
            .map(|path| path.is_some())
            .unwrap_or(false)
    }

    fn cleanup(&self) {
        if let Ok(mut path) = self.temporary_path.lock() {
            path.take();
        }
    }
}

impl OutputStore {
    fn new(
        file: impl AsyncWrite + Send + Unpin + 'static,
        log: Arc<OutputLog>,
        command: String,
    ) -> Self {
        Self {
            file: Box::new(file),
            command,
            lines: VecDeque::new(),
            bytes: 0,
            observed_bytes: 0,
            observed_lines: 0,
            truncated: false,
            log,
        }
    }

    async fn append_line(
        &mut self,
        source: &str,
        line: &str,
        terminated: bool,
    ) -> Result<BashJobDetails, std::io::Error> {
        let entry = format!("[{source}] {line}");
        self.file.write_all(entry.as_bytes()).await?;
        if terminated {
            self.file.write_all(b"\n").await?;
        }
        self.observed_bytes += entry.len() + 1;
        self.observed_lines += 1;
        self.bytes += entry.len() + 1;
        self.lines.push_back(entry);
        trim_tail(&mut self.lines, &mut self.bytes);
        self.truncated =
            self.observed_lines > MAX_OUTPUT_LINES || self.observed_bytes > MAX_OUTPUT_BYTES;
        Ok(self.details(None, None))
    }

    async fn finish(
        &mut self,
        exit_code: Option<i32>,
        reason: Option<String>,
    ) -> Result<BashJobDetails, std::io::Error> {
        self.file.flush().await?;
        Ok(self.details(exit_code, reason))
    }

    fn details(&self, exit_code: Option<i32>, reason: Option<String>) -> BashJobDetails {
        BashJobDetails {
            command: self.command.clone(),
            output: self.lines.iter().cloned().collect::<Vec<_>>().join("\n"),
            full_output: Some(Arc::clone(&self.log)),
            truncated: self.truncated,
            exit_code,
            reason,
        }
    }
}

fn trim_tail(lines: &mut VecDeque<String>, bytes: &mut usize) {
    while lines.len() > MAX_OUTPUT_LINES || *bytes > MAX_OUTPUT_BYTES {
        if lines.len() == 1 {
            let line = lines.front_mut().expect("tail contains one line");
            let prefix_end = line.find("] ").map_or(0, |index| index + 2);
            let retained_body = (MAX_OUTPUT_BYTES - 1).saturating_sub(prefix_end);
            let mut start = line.len().saturating_sub(retained_body).max(prefix_end);
            while start < line.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            *line = format!("{}{}", &line[..prefix_end], &line[start..]);
            *bytes = line.len() + 1;
            break;
        }
        if let Some(removed) = lines.pop_front() {
            *bytes -= removed.len() + 1;
        }
    }
}

async fn create_output_log() -> Result<(Arc<OutputLog>, tokio::fs::File), BashToolError> {
    let (file, temporary_path, path) = blocking::run(|| {
        let file = tempfile::NamedTempFile::new().map_err(|_| ())?;
        let path = file.path().to_path_buf();
        let (file, temporary_path) = file.into_parts();
        Ok::<_, ()>((file, temporary_path, path))
    })
    .await
    .map_err(|_| BashToolError::Output)?;
    Ok((
        Arc::new(OutputLog {
            path,
            temporary_path: Mutex::new(Some(temporary_path)),
        }),
        tokio::fs::File::from_std(file),
    ))
}

async fn run_supervisor(
    lease: JobLease,
    updater: JobUpdater,
    command_spec: SupervisorCommand,
    store: Arc<AsyncMutex<OutputStore>>,
    started: oneshot::Sender<Result<(), BashToolError>>,
) {
    let id = lease.id();
    let mut command = tokio::process::Command::new(&command_spec.program);
    command
        .arg("-lc")
        .arg(&command_spec.text)
        .current_dir(command_spec.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            let details =
                match terminal_details(&store, None, Some("failed to spawn Bash".into())).await {
                    Ok(details) => details,
                    Err(error) => store.lock().await.details(
                        None,
                        Some(format!(
                            "output capture failed: output log flush failed: {error}"
                        )),
                    ),
                };
            let _ = lease.finish(JobState::Failed, Arc::new(details));
            let _ = started.send(Err(BashToolError::Spawn { id, source }));
            return;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        let details = match terminal_details(
            &store,
            None,
            Some("Bash output pipes are unavailable".into()),
        )
        .await
        {
            Ok(details) => details,
            Err(error) => store.lock().await.details(
                None,
                Some(format!(
                    "output capture failed: output log flush failed: {error}"
                )),
            ),
        };
        let _ = lease.finish(JobState::Failed, Arc::new(details));
        let _ = started.send(Err(BashToolError::Runtime));
        return;
    };
    let _ = started.send(Ok(()));
    let (capture_tx, capture_rx) = tokio::sync::mpsc::unbounded_channel();
    let stdout_reader = spawn_reader(
        stdout,
        "stdout",
        Arc::clone(&store),
        updater.clone(),
        capture_tx.clone(),
    );
    let stderr_reader = spawn_reader(stderr, "stderr", Arc::clone(&store), updater, capture_tx);
    finish_child(
        lease,
        &mut child,
        store,
        stdout_reader,
        stderr_reader,
        capture_rx,
        command_spec.timeout_ms,
    )
    .await;
}

fn spawn_reader<R>(
    reader: R,
    source: &'static str,
    store: Arc<AsyncMutex<OutputStore>>,
    updater: JobUpdater,
    failures: tokio::sync::mpsc::UnboundedSender<String>,
) -> tokio::task::JoinHandle<Result<(), String>>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let result = read_output(reader, source, store, updater).await;
        if let Err(reason) = &result {
            let _ = failures.send(reason.clone());
        }
        result
    })
}

async fn read_output<R>(
    mut reader: R,
    source: &'static str,
    store: Arc<AsyncMutex<OutputStore>>,
    updater: JobUpdater,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 4096];
    let mut carry = StreamCarry::default();
    loop {
        let bytes = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(bytes) => bytes,
            Err(error) => return Err(format!("{source} pipe read failed: {error}")),
        };
        carry.bytes.extend_from_slice(&buffer[..bytes]);
        decode_available(&mut carry);
        publish_lines(&mut carry, source, &store, &updater, false).await?;
    }
    if !carry.bytes.is_empty() {
        carry.text.push_str(&String::from_utf8_lossy(&carry.bytes));
        carry.bytes.clear();
    }
    publish_lines(&mut carry, source, &store, &updater, true).await
}

fn decode_available(carry: &mut StreamCarry) {
    loop {
        match std::str::from_utf8(&carry.bytes) {
            Ok(text) => {
                carry.text.push_str(text);
                carry.bytes.clear();
                return;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                // SAFETY: `valid_up_to` identifies a valid UTF-8 prefix.
                carry
                    .text
                    .push_str(unsafe { std::str::from_utf8_unchecked(&carry.bytes[..valid]) });
                match error.error_len() {
                    Some(length) => {
                        carry.text.push('�');
                        carry.bytes.drain(..valid + length);
                    }
                    None => {
                        carry.bytes.drain(..valid);
                        return;
                    }
                }
            }
        }
    }
}

async fn publish_lines(
    carry: &mut StreamCarry,
    source: &'static str,
    store: &AsyncMutex<OutputStore>,
    updater: &JobUpdater,
    eof: bool,
) -> Result<(), String> {
    while let Some(newline) = carry.text.find('\n') {
        let line = carry.text[..newline].trim_end_matches('\r').to_owned();
        carry.text.drain(..=newline);
        publish_line(source, &line, true, store, updater).await?;
    }
    if eof && !carry.text.is_empty() {
        let line = std::mem::take(&mut carry.text);
        publish_line(source, &line, false, store, updater).await?;
    }
    Ok(())
}

async fn publish_line(
    source: &'static str,
    line: &str,
    terminated: bool,
    store: &AsyncMutex<OutputStore>,
    updater: &JobUpdater,
) -> Result<(), String> {
    let details = store
        .lock()
        .await
        .append_line(source, line, terminated)
        .await
        .map_err(|error| format!("output log write failed: {error}"))?;
    let _ = updater.update(Arc::new(details));
    Ok(())
}

async fn finish_child(
    mut lease: JobLease,
    child: &mut Child,
    store: Arc<AsyncMutex<OutputStore>>,
    stdout: tokio::task::JoinHandle<Result<(), String>>,
    stderr: tokio::task::JoinHandle<Result<(), String>>,
    mut capture_failures: tokio::sync::mpsc::UnboundedReceiver<String>,
    timeout_ms: Option<u64>,
) {
    let timeout = async move {
        match timeout_ms {
            Some(milliseconds) => tokio::time::sleep(Duration::from_millis(milliseconds)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timeout);
    enum Outcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut(u64),
        CaptureFailed(String),
    }
    let outcome = tokio::select! {
        result = child.wait() => Outcome::Exited(result),
        () = lease.cancelled() => Outcome::Cancelled,
        () = &mut timeout => Outcome::TimedOut(timeout_ms.expect("timeout future completed")),
        Some(reason) = capture_failures.recv() => Outcome::CaptureFailed(reason),
    };
    if !matches!(outcome, Outcome::Exited(_)) {
        terminate_child(child).await;
    }
    let stdout_result = stdout.await;
    let stderr_result = stderr.await;
    let reader_failure =
        [stdout_result, stderr_result]
            .into_iter()
            .find_map(|result| match result {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason),
                Err(error) => Some(format!("output reader task failed: {error}")),
            });
    let (mut state, mut exit_code, mut reason) = match outcome {
        Outcome::Exited(Ok(status)) => (JobState::Completed, status.code(), None),
        Outcome::Exited(Err(_)) => (
            JobState::Failed,
            None,
            Some("failed to wait for Bash".into()),
        ),
        Outcome::Cancelled => (JobState::Cancelled, None, Some("cancelled".into())),
        Outcome::TimedOut(milliseconds) => (
            JobState::Failed,
            None,
            Some(format!("timeout after {milliseconds} ms")),
        ),
        Outcome::CaptureFailed(reason) => (
            JobState::Failed,
            None,
            Some(format!("output capture failed: {reason}")),
        ),
    };
    if let Some(failure) = reader_failure {
        state = JobState::Failed;
        exit_code = None;
        reason = Some(format!("output capture failed: {failure}"));
    }
    let details = match terminal_details(&store, exit_code, reason.clone()).await {
        Ok(details) => details,
        Err(error) => {
            state = JobState::Failed;
            store.lock().await.details(
                None,
                Some(format!(
                    "output capture failed: output log flush failed: {error}"
                )),
            )
        }
    };
    let _ = lease.finish(state, Arc::new(details));
}

#[cfg(unix)]
async fn terminate_child(child: &mut Child) {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    if let Some(id) = child.id() {
        let group = Pid::from_raw(id as i32);
        let _ = killpg(group, Signal::SIGTERM);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut leader_reaped = false;
        loop {
            if !leader_reaped {
                leader_reaped = child.try_wait().ok().flatten().is_some();
            }
            let group_gone = killpg(group, None) == Err(Errno::ESRCH);
            if leader_reaped && group_gone {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if killpg(group, None) != Err(Errno::ESRCH) {
            let _ = killpg(group, Signal::SIGKILL);
        }
        if !leader_reaped {
            let _ = child.wait().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn terminal_details(
    store: &AsyncMutex<OutputStore>,
    exit_code: Option<i32>,
    reason: Option<String>,
) -> Result<BashJobDetails, std::io::Error> {
    store.lock().await.finish(exit_code, reason).await
}

fn map_registry_error(error: JobRegistryError) -> BashToolError {
    match error {
        JobRegistryError::Capacity => BashToolError::Busy,
        _ => BashToolError::Runtime,
    }
}

fn render(snapshot: JobSnapshot) -> ToolOutput {
    ToolOutput::text(format!(
        "id: {}\nkind: {}\nstate: {}\nstarted_at: {}\ncompleted_at: {}\ntitle: {}\ndetails: {}",
        snapshot.id(),
        snapshot.kind(),
        snapshot.state(),
        snapshot.started_at().to_rfc3339(),
        snapshot
            .completed_at()
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into()),
        snapshot.title(),
        snapshot.details().render(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    // These tests each spawn a Bash process and drive its lifecycle through a
    // separate current-thread Tokio runtime. Keep them isolated from each
    // other so the test harness cannot delay either runtime past its deadline.
    static SUPERVISOR_TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

    struct FailingWriter {
        write: bool,
        flush: bool,
    }

    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("read fixture")))
        }
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.write {
                Poll::Ready(Err(std::io::Error::other("write fixture")))
            } else {
                Poll::Ready(Ok(buffer.len()))
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.flush {
                Poll::Ready(Err(std::io::Error::other("flush fixture")))
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn fixture_log() -> Arc<OutputLog> {
        Arc::new(OutputLog {
            path: PathBuf::from("fixture"),
            temporary_path: Mutex::new(None),
        })
    }

    #[tokio::test]
    async fn output_store_reports_write_and_flush_failures() {
        let mut write_store = OutputStore::new(
            FailingWriter {
                write: true,
                flush: false,
            },
            fixture_log(),
            "fixture".into(),
        );
        assert!(
            write_store
                .append_line("stdout", "line", true)
                .await
                .is_err()
        );
        let mut flush_store = OutputStore::new(
            FailingWriter {
                write: false,
                flush: true,
            },
            fixture_log(),
            "fixture".into(),
        );
        assert!(flush_store.finish(Some(0), None).await.is_err());
    }

    #[tokio::test]
    async fn reader_reports_pipe_read_failure() {
        let registry = JobRegistry::new();
        let lease = registry
            .start(
                JobKind::Bash,
                "fixture",
                Arc::new(BashJobDetails::running("fixture".into(), fixture_log())),
            )
            .unwrap();
        let updater = lease.updater().unwrap();
        let store = Arc::new(AsyncMutex::new(OutputStore::new(
            tokio::io::sink(),
            fixture_log(),
            "fixture".into(),
        )));
        let error = read_output(FailingReader, "stdout", store, updater)
            .await
            .unwrap_err();
        assert!(error.contains("stdout pipe read failed: read fixture"));
    }

    #[tokio::test]
    async fn supervisor_settles_capture_write_failure_as_failed() {
        let _supervisor_lock = SUPERVISOR_TEST_LOCK.lock().await;
        let registry = JobRegistry::new();
        let lease = registry
            .start(
                JobKind::Bash,
                "fixture",
                Arc::new(BashJobDetails::running("printf x".into(), fixture_log())),
            )
            .unwrap();
        let id = lease.id();
        let updater = lease.updater().unwrap();
        let store = Arc::new(AsyncMutex::new(OutputStore::new(
            FailingWriter {
                write: true,
                flush: false,
            },
            fixture_log(),
            "printf x".into(),
        )));
        let (started_tx, started_rx) = oneshot::channel();
        tokio::spawn(run_supervisor(
            lease,
            updater,
            SupervisorCommand {
                cwd: std::env::temp_dir(),
                program: "bash".into(),
                text: "printf 'x\\n'; sleep 30".into(),
                timeout_ms: None,
            },
            store,
            started_tx,
        ));
        started_rx.await.unwrap().unwrap();
        let terminal = registry
            .wait(&[id], Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(!terminal.timed_out);
        assert_eq!(terminal.snapshots[0].state(), JobState::Failed);
        assert!(
            terminal.snapshots[0]
                .details()
                .render()
                .contains("output capture failed: output log write failed")
        );
    }

    #[tokio::test]
    async fn supervisor_settles_final_flush_failure_as_failed() {
        let _supervisor_lock = SUPERVISOR_TEST_LOCK.lock().await;
        let registry = JobRegistry::new();
        let lease = registry
            .start(
                JobKind::Bash,
                "fixture",
                Arc::new(BashJobDetails::running("true".into(), fixture_log())),
            )
            .unwrap();
        let id = lease.id();
        let updater = lease.updater().unwrap();
        let store = Arc::new(AsyncMutex::new(OutputStore::new(
            FailingWriter {
                write: false,
                flush: true,
            },
            fixture_log(),
            "true".into(),
        )));
        let (started_tx, started_rx) = oneshot::channel();
        tokio::spawn(run_supervisor(
            lease,
            updater,
            SupervisorCommand {
                cwd: std::env::temp_dir(),
                program: "bash".into(),
                text: "true".into(),
                timeout_ms: None,
            },
            store,
            started_tx,
        ));
        started_rx.await.unwrap().unwrap();
        let terminal = registry
            .wait(&[id], Some(Duration::from_secs(3)))
            .await
            .unwrap();
        assert_eq!(terminal.snapshots[0].state(), JobState::Failed);
        assert!(
            terminal.snapshots[0]
                .details()
                .render()
                .contains("output capture failed: output log flush failed")
        );
    }

    #[tokio::test]
    async fn spawn_failure_settles_the_reserved_job() {
        let registry = JobRegistry::new();
        let service = BashServiceFactory::with_program(
            registry.clone(),
            OsString::from("definitely-not-bash"),
        )
        .for_cwd(std::env::temp_dir());

        let error = service
            .bash(BashArgs {
                command: "true".into(),
                background: false,
                timeout_ms: None,
            })
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("[E_SPAWN] failed to start job-0")
        );
        assert_eq!(
            registry.status(Some("job-0".parse().unwrap())).unwrap()[0].state(),
            JobState::Failed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_logs_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;

        let (log, _file) = create_output_log().await.unwrap();
        let mode = std::fs::metadata(&log.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
}
