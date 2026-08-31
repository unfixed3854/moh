//! Process-local, typed lifecycle tracking for background producers.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use garde::Validate;
use rig::tool::ToolOutput;
use schemars::JsonSchema;
use thiserror::Error;
use tokio::sync::watch;

const MAX_RUNNING_JOBS: usize = 16;
const MAX_TERMINAL_JOBS: usize = 64;
const DEFAULT_WAIT: Duration = Duration::from_secs(30);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
/// A process-local job identifier rendered as `job-N`.
pub struct JobId(u64);

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "job-{}", self.0)
    }
}

impl FromStr for JobId {
    type Err = JobRegistryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digits = value
            .strip_prefix("job-")
            .filter(|digits| !digits.is_empty())
            .ok_or(JobRegistryError::MalformedId)?;
        if digits.len() > 1 && digits.starts_with('0')
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(JobRegistryError::MalformedId);
        }
        digits
            .parse()
            .map(JobId)
            .map_err(|_| JobRegistryError::MalformedId)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The producer family that owns a job.
pub enum JobKind {
    /// A job produced by the Bash tool.
    Bash,
}

impl fmt::Display for JobKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bash => formatter.write_str("bash"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The current lifecycle state of a job.
pub enum JobState {
    /// The producer still owns a running job.
    Running,
    /// The producer completed the job successfully.
    Completed,
    /// The producer ended the job unsuccessfully.
    Failed,
    /// Cancellation or registry shutdown ended the job.
    Cancelled,
}

impl JobState {
    fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => formatter.write_str("running"),
            Self::Completed => formatter.write_str("completed"),
            Self::Failed => formatter.write_str("failed"),
            Self::Cancelled => formatter.write_str("cancelled"),
        }
    }
}

/// Producer-specific information attached to a job snapshot.
pub trait JobDetails: fmt::Debug + Send + Sync {
    /// Renders model-visible producer information for lifecycle tools.
    fn render(&self) -> String;

    /// Releases producer-owned temporary resources when details are discarded.
    fn cleanup(&self) {}
}

#[derive(Clone)]
/// An immutable point-in-time view of a job.
pub struct JobSnapshot {
    id: JobId,
    kind: JobKind,
    state: JobState,
    title: String,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    details: Arc<dyn JobDetails>,
}

impl JobSnapshot {
    /// Returns the snapshot's process-local job identifier.
    pub fn id(&self) -> JobId {
        self.id
    }
    /// Returns the family of producer that created the job.
    pub fn kind(&self) -> JobKind {
        self.kind
    }
    /// Returns the current lifecycle state.
    pub fn state(&self) -> JobState {
        self.state
    }
    /// Returns the producer-provided job title.
    pub fn title(&self) -> &str {
        &self.title
    }
    /// Returns the UTC time at which the job was started.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
    /// Returns the UTC terminal-transition time, if the job is terminal.
    pub fn completed_at(&self) -> Option<DateTime<Utc>> {
        self.completed_at
    }
    /// Returns the producer-specific details currently attached to the job.
    pub fn details(&self) -> &Arc<dyn JobDetails> {
        &self.details
    }
}

#[derive(Clone)]
/// Shared process-local lifecycle storage for all job producers.
pub struct JobRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    state: Mutex<RegistryState>,
    version: watch::Sender<u64>,
}

struct RegistryState {
    jobs: BTreeMap<JobId, JobEntry>,
    terminal_order: VecDeque<JobId>,
    next_id: u64,
    next_token: u64,
    shutting_down: bool,
}

struct JobEntry {
    snapshot: JobSnapshot,
    token: u64,
    cancellation: watch::Sender<bool>,
    cleaned: bool,
}

/// Exclusive producer ownership of one running job.
pub struct JobLease {
    registry: JobRegistry,
    id: JobId,
    cancellation: watch::Receiver<bool>,
    settled: bool,
}

#[derive(Clone)]
/// A cloneable, token-scoped handle for publishing running-job details.
pub struct JobUpdater {
    registry: JobRegistry,
    id: JobId,
    token: u64,
}

/// The outcome of waiting for one or more jobs.
pub struct JobWaitResult {
    /// Terminal snapshots observed before the deadline.
    pub snapshots: Vec<JobSnapshot>,
    /// Whether the deadline elapsed before any requested job became terminal.
    pub timed_out: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
/// Failures returned by the generic registry lifecycle API.
pub enum JobRegistryError {
    #[error("malformed job ID")]
    /// A display identifier was not a canonical `job-N` value.
    MalformedId,
    #[error("job {0} was not found")]
    /// A requested job has never existed or has been evicted.
    NotFound(JobId),
    #[error("background job capacity is exhausted")]
    /// The registry has reached its running-job capacity.
    Capacity,
    #[error("job registry is shutting down")]
    /// The registry no longer accepts new producers.
    ShuttingDown,
    #[error("job ID space is exhausted")]
    /// The monotonic job identifier counter cannot be incremented.
    IdExhausted,
    #[error("job {0} is already settled")]
    /// A stale producer attempted to update a terminal job.
    AlreadySettled(JobId),
    #[error("job registry state is unavailable")]
    /// The in-memory registry lock was poisoned.
    Poisoned,
    #[error("job registry invariant failed")]
    /// An internal registry invariant could not be maintained.
    Invariant,
    #[error("job registry shutdown timed out")]
    /// One or more producers did not settle within the shutdown deadline.
    ShutdownTimeout,
}

#[derive(Debug)]
struct ProducerDisappeared;

impl JobDetails for ProducerDisappeared {
    fn render(&self) -> String {
        "producer disappeared before settling the job".into()
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    /// Creates an empty registry with fresh monotonic identifiers.
    pub fn new() -> Self {
        let (version, _) = watch::channel(0_u64);
        Self {
            inner: Arc::new(RegistryInner {
                state: Mutex::new(RegistryState {
                    jobs: BTreeMap::new(),
                    terminal_order: VecDeque::new(),
                    next_id: 0,
                    next_token: 0,
                    shutting_down: false,
                }),
                version,
            }),
        }
    }

    /// Subscribes to changes in the registry's observable job activity version.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    /// Returns the number of jobs currently in the running state.
    pub fn running_count(&self) -> Result<usize, JobRegistryError> {
        Ok(self
            .lock()?
            .jobs
            .values()
            .filter(|entry| entry.snapshot.state() == JobState::Running)
            .count())
    }

    /// Reserves capacity and creates a running job owned by a producer lease.
    pub fn start(
        &self,
        kind: JobKind,
        title: impl Into<String>,
        initial_details: Arc<dyn JobDetails>,
    ) -> Result<JobLease, JobRegistryError> {
        let (id, cancellation, cleanup) = {
            let mut state = self.lock()?;
            if state.shutting_down {
                return Err(JobRegistryError::ShuttingDown);
            }
            if state
                .jobs
                .values()
                .filter(|job| job.snapshot.state == JobState::Running)
                .count()
                >= MAX_RUNNING_JOBS
            {
                return Err(JobRegistryError::Capacity);
            }
            let id = JobId(state.next_id);
            state.next_id = state
                .next_id
                .checked_add(1)
                .ok_or(JobRegistryError::IdExhausted)?;
            let token = state.next_token;
            state.next_token = state
                .next_token
                .checked_add(1)
                .ok_or(JobRegistryError::IdExhausted)?;
            let (cancel, cancellation) = watch::channel(false);
            state.jobs.insert(
                id,
                JobEntry {
                    snapshot: JobSnapshot {
                        id,
                        kind,
                        state: JobState::Running,
                        title: title.into(),
                        started_at: Utc::now(),
                        completed_at: None,
                        details: initial_details,
                    },
                    token,
                    cancellation: cancel,
                    cleaned: false,
                },
            );
            let cleanup = Self::evict(&mut state);
            (id, cancellation, cleanup)
        };
        Self::cleanup(cleanup);
        self.notify()?;
        Ok(JobLease {
            registry: self.clone(),
            id,
            cancellation,
            settled: false,
        })
    }

    /// Returns one requested snapshot or all retained snapshots in creation order.
    pub fn status(&self, id: Option<JobId>) -> Result<Vec<JobSnapshot>, JobRegistryError> {
        let state = self.lock()?;
        match id {
            Some(id) => Ok(vec![
                state
                    .jobs
                    .get(&id)
                    .ok_or(JobRegistryError::NotFound(id))?
                    .snapshot
                    .clone(),
            ]),
            None => Ok(state
                .jobs
                .values()
                .map(|entry| entry.snapshot.clone())
                .collect()),
        }
    }

    /// Waits until any requested job becomes terminal or the optional deadline expires.
    pub async fn wait(
        &self,
        ids: &[JobId],
        timeout: Option<Duration>,
    ) -> Result<JobWaitResult, JobRegistryError> {
        let mut version = self.inner.version.subscribe();
        let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
        loop {
            let snapshots = self.terminal_requested(ids)?;
            if !snapshots.is_empty() {
                return Ok(JobWaitResult {
                    snapshots,
                    timed_out: false,
                });
            }
            let changed = version.changed();
            match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, changed).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => return Err(JobRegistryError::Invariant),
                    Err(_) => {
                        return Ok(JobWaitResult {
                            snapshots: Vec::new(),
                            timed_out: true,
                        });
                    }
                },
                None => changed.await.map_err(|_| JobRegistryError::Invariant)?,
            }
        }
    }

    /// Requests cancellation and waits for the job's terminal snapshot.
    pub async fn cancel(&self, id: JobId) -> Result<JobSnapshot, JobRegistryError> {
        if let Some(snapshot) = self.request_cancel(id)? {
            return Ok(snapshot);
        }
        let result = self.wait(&[id], None).await?;
        result
            .snapshots
            .into_iter()
            .next()
            .ok_or(JobRegistryError::Invariant)
    }

    /// Rejects starts, requests cancellation, waits for producers, and cleans details.
    pub async fn shutdown(&self) -> Result<(), JobRegistryError> {
        let running = {
            let mut state = self.lock()?;
            state.shutting_down = true;
            state
                .jobs
                .values()
                .filter(|entry| entry.snapshot.state == JobState::Running)
                .map(|entry| {
                    entry.cancellation.send_replace(true);
                    entry.snapshot.id
                })
                .collect::<Vec<_>>()
        };
        let result = async {
            self.notify()?;
            let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
            for id in running {
                match tokio::time::timeout_at(deadline, self.wait(&[id], None)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Err(JobRegistryError::ShutdownTimeout),
                }
            }
            Ok(())
        }
        .await;
        let cleanup_result = self.cleanup_retained();
        result.and(cleanup_result)
    }

    fn cleanup_retained(&self) -> Result<(), JobRegistryError> {
        let cleanup = {
            let mut state = self.lock()?;
            state
                .jobs
                .values_mut()
                .filter_map(|entry| {
                    if entry.cleaned {
                        None
                    } else {
                        entry.cleaned = true;
                        Some(Arc::clone(&entry.snapshot.details))
                    }
                })
                .collect()
        };
        Self::cleanup(cleanup);
        Ok(())
    }

    pub(crate) fn request_cancel(
        &self,
        id: JobId,
    ) -> Result<Option<JobSnapshot>, JobRegistryError> {
        let cancellation = {
            let state = self.lock()?;
            let job = state.jobs.get(&id).ok_or(JobRegistryError::NotFound(id))?;
            if job.snapshot.state.terminal() {
                return Ok(Some(job.snapshot.clone()));
            }
            job.cancellation.clone()
        };
        cancellation.send_replace(true);
        self.notify()?;
        Ok(None)
    }

    fn updater(&self, id: JobId) -> Result<JobUpdater, JobRegistryError> {
        let state = self.lock()?;
        let entry = state.jobs.get(&id).ok_or(JobRegistryError::NotFound(id))?;
        Ok(JobUpdater {
            registry: self.clone(),
            id,
            token: entry.token,
        })
    }

    fn snapshot(&self, id: JobId) -> Result<JobSnapshot, JobRegistryError> {
        self.status(Some(id))
            .map(|mut snapshots| snapshots.remove(0))
    }

    fn update(
        &self,
        id: JobId,
        token: u64,
        details: Arc<dyn JobDetails>,
    ) -> Result<(), JobRegistryError> {
        let cleanup = {
            let mut state = self.lock()?;
            let entry = state
                .jobs
                .get_mut(&id)
                .ok_or(JobRegistryError::NotFound(id))?;
            if entry.token != token || entry.snapshot.state.terminal() {
                return Err(JobRegistryError::AlreadySettled(id));
            }
            let cleanup = entry.cleaned.then(|| Arc::clone(&details));
            entry.snapshot.details = details;
            cleanup
        };
        if let Some(details) = cleanup {
            details.cleanup();
        }
        self.notify()
    }

    fn finish(
        &self,
        id: JobId,
        token: u64,
        state: JobState,
        details: Arc<dyn JobDetails>,
    ) -> Result<(), JobRegistryError> {
        if !state.terminal() {
            return Err(JobRegistryError::Invariant);
        }
        let (cleanup, replacement_cleanup) = {
            let mut registry = self.lock()?;
            let entry = registry
                .jobs
                .get_mut(&id)
                .ok_or(JobRegistryError::NotFound(id))?;
            if entry.token != token || entry.snapshot.state.terminal() {
                return Err(JobRegistryError::AlreadySettled(id));
            }
            entry.snapshot.state = state;
            entry.snapshot.completed_at = Some(Utc::now());
            let replacement_cleanup = entry.cleaned.then(|| Arc::clone(&details));
            entry.snapshot.details = details;
            registry.terminal_order.push_back(id);
            (Self::evict(&mut registry), replacement_cleanup)
        };
        if let Some(details) = replacement_cleanup {
            details.cleanup();
        }
        Self::cleanup(cleanup);
        self.notify()
    }

    fn terminal_requested(&self, ids: &[JobId]) -> Result<Vec<JobSnapshot>, JobRegistryError> {
        let state = self.lock()?;
        let mut terminal = Vec::new();
        for id in ids {
            let snapshot = &state
                .jobs
                .get(id)
                .ok_or(JobRegistryError::NotFound(*id))?
                .snapshot;
            if snapshot.state.terminal() {
                terminal.push(snapshot.clone());
            }
        }
        Ok(terminal)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, RegistryState>, JobRegistryError> {
        self.inner
            .state
            .lock()
            .map_err(|_| JobRegistryError::Poisoned)
    }

    fn evict(state: &mut RegistryState) -> Vec<Arc<dyn JobDetails>> {
        let mut cleanup = Vec::new();
        while state.terminal_order.len() > MAX_TERMINAL_JOBS {
            if let Some(id) = state.terminal_order.pop_front()
                && let Some(entry) = state.jobs.remove(&id)
                && !entry.cleaned
            {
                cleanup.push(entry.snapshot.details);
            }
        }
        cleanup
    }

    fn cleanup(details: Vec<Arc<dyn JobDetails>>) {
        for detail in details {
            detail.cleanup();
        }
    }

    fn notify(&self) -> Result<(), JobRegistryError> {
        let current = *self.inner.version.borrow();
        let next = current.checked_add(1).ok_or(JobRegistryError::Invariant)?;
        self.inner.version.send_replace(next);
        Ok(())
    }
}

impl JobLease {
    /// Returns the running job identifier owned by this lease.
    pub fn id(&self) -> JobId {
        self.id
    }
    /// Captures the registry's current snapshot for this job.
    pub fn snapshot(&self) -> Result<JobSnapshot, JobRegistryError> {
        self.registry.snapshot(self.id)
    }
    /// Creates a token-scoped updater for publishing partial producer details.
    pub fn updater(&self) -> Result<JobUpdater, JobRegistryError> {
        self.registry.updater(self.id)
    }
    /// Settles the job exactly once with a terminal state and final details.
    pub fn finish(
        mut self,
        state: JobState,
        details: Arc<dyn JobDetails>,
    ) -> Result<(), JobRegistryError> {
        self.registry
            .finish(self.id, self.updater()?.token, state, details)?;
        self.settled = true;
        Ok(())
    }
    /// Waits until cancellation is requested for this running job.
    pub async fn cancelled(&mut self) {
        while !*self.cancellation.borrow() {
            if self.cancellation.changed().await.is_err() {
                break;
            }
        }
    }
}

impl Drop for JobLease {
    fn drop(&mut self) {
        if !self.settled
            && let Ok(updater) = self.registry.updater(self.id)
        {
            let _ = self.registry.finish(
                self.id,
                updater.token,
                JobState::Failed,
                Arc::new(ProducerDisappeared),
            );
        }
    }
}

impl JobUpdater {
    /// Publishes new details while the associated producer still owns a running job.
    pub fn update(&self, details: Arc<dyn JobDetails>) -> Result<(), JobRegistryError> {
        self.registry.update(self.id, self.token, details)
    }
}

#[derive(Debug, serde::Deserialize, JsonSchema, Validate)]
#[serde(deny_unknown_fields)]
/// Arguments accepted by the model-visible job status service.
pub struct JobStatusArgs {
    /// Optional canonical job identifier; omitting it lists all retained jobs.
    #[garde(skip)]
    pub job_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, JsonSchema, Validate)]
#[serde(deny_unknown_fields)]
/// Arguments accepted by the model-visible job wait service.
pub struct JobWaitArgs {
    /// One or more canonical job identifiers to wait for.
    #[garde(length(min = 1))]
    pub job_ids: Vec<String>,
    /// Optional bounded wait deadline in milliseconds.
    #[garde(range(min = 0, max = 300_000))]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, JsonSchema, Validate)]
#[serde(deny_unknown_fields)]
/// Arguments accepted by the model-visible job cancellation service.
pub struct JobCancelArgs {
    /// The canonical identifier of the job to cancel.
    #[garde(skip)]
    pub job_id: String,
}

#[derive(Clone)]
/// Model-facing lifecycle operations backed by a shared registry.
pub struct JobService {
    registry: JobRegistry,
}

#[derive(Debug, Error)]
/// Stable model-visible failures returned by lifecycle services.
pub enum JobToolError {
    #[error("[E_INVALID_ARGUMENT] {0}")]
    /// Request arguments did not satisfy the lifecycle contract.
    InvalidArgument(&'static str),
    #[error("[E_NOT_FOUND] job not found")]
    /// The requested retained job does not exist.
    NotFound,
    #[error("[E_BUSY] background job capacity is exhausted")]
    /// No running-job capacity remains.
    Busy,
    #[error("[E_RUNTIME] job registry is unavailable")]
    /// The registry could not safely complete the operation.
    Runtime,
}

impl JobService {
    /// Binds lifecycle rendering and validation to a shared registry.
    pub fn new(registry: JobRegistry) -> Self {
        Self { registry }
    }
    /// Renders a compact list or full snapshot for the requested job status.
    pub async fn status(&self, args: JobStatusArgs) -> Result<ToolOutput, JobToolError> {
        args.validate()
            .map_err(|_| JobToolError::InvalidArgument("invalid job status arguments"))?;
        let id = args.job_id.as_deref().map(Self::parse_id).transpose()?;
        let snapshots = self.registry.status(id).map_err(Self::map_error)?;
        Ok(ToolOutput::text(if id.is_some() {
            render_full(&snapshots[0])
        } else {
            snapshots
                .iter()
                .map(render_compact)
                .collect::<Vec<_>>()
                .join("\n")
        }))
    }
    /// Waits for a requested terminal job and renders its full snapshot.
    pub async fn wait(&self, args: JobWaitArgs) -> Result<ToolOutput, JobToolError> {
        args.validate()
            .map_err(|_| JobToolError::InvalidArgument("invalid job wait arguments"))?;
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_WAIT.as_millis() as u64);
        let ids = args
            .job_ids
            .iter()
            .map(|id| Self::parse_id(id))
            .collect::<Result<Vec<_>, _>>()?;
        let result = self
            .registry
            .wait(&ids, Some(Duration::from_millis(timeout_ms)))
            .await
            .map_err(Self::map_error)?;
        if result.timed_out {
            Ok(ToolOutput::text("timed out waiting for jobs"))
        } else {
            Ok(ToolOutput::text(
                result
                    .snapshots
                    .iter()
                    .map(render_full)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            ))
        }
    }
    /// Requests cancellation and renders the job's final snapshot.
    pub async fn cancel(&self, args: JobCancelArgs) -> Result<ToolOutput, JobToolError> {
        args.validate()
            .map_err(|_| JobToolError::InvalidArgument("invalid job cancel arguments"))?;
        let snapshot = self
            .registry
            .cancel(Self::parse_id(&args.job_id)?)
            .await
            .map_err(Self::map_error)?;
        Ok(ToolOutput::text(render_full(&snapshot)))
    }
    fn parse_id(value: &str) -> Result<JobId, JobToolError> {
        value
            .parse()
            .map_err(|_| JobToolError::InvalidArgument("job_id must be a valid job ID"))
    }
    fn map_error(error: JobRegistryError) -> JobToolError {
        match error {
            JobRegistryError::MalformedId => {
                JobToolError::InvalidArgument("job_id must be a valid job ID")
            }
            JobRegistryError::NotFound(_) => JobToolError::NotFound,
            JobRegistryError::Capacity => JobToolError::Busy,
            JobRegistryError::ShuttingDown
            | JobRegistryError::IdExhausted
            | JobRegistryError::AlreadySettled(_)
            | JobRegistryError::Poisoned
            | JobRegistryError::Invariant
            | JobRegistryError::ShutdownTimeout => JobToolError::Runtime,
        }
    }
}

fn render_compact(snapshot: &JobSnapshot) -> String {
    format!(
        "{} {} {} {}",
        snapshot.id, snapshot.kind, snapshot.state, snapshot.title
    )
}
fn render_full(snapshot: &JobSnapshot) -> String {
    format!(
        "id: {}\nkind: {}\nstate: {}\nstarted_at: {}\ncompleted_at: {}\ntitle: {}\ndetails: {}",
        snapshot.id,
        snapshot.kind,
        snapshot.state,
        snapshot.started_at.to_rfc3339(),
        snapshot
            .completed_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "-".into()),
        snapshot.title,
        snapshot.details.render()
    )
}
