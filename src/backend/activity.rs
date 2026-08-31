//! Keyed backend activity accounting and idle deadline invalidation.

use std::{
    collections::{HashMap, HashSet},
    future,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use tokio::sync::watch;

use crate::session::{ConnectionId, SessionId};

/// Point-in-time backend-global activity counts and invalidation generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivitySnapshot {
    /// Number of distinct live client connections.
    pub connections: usize,
    /// Number of distinct sessions with active runs.
    pub active_runs: usize,
    /// Total running jobs across all live sessions.
    pub running_jobs: usize,
    /// Number of in-flight session-title generation tasks.
    pub title_tasks: u32,
    /// Monotonic generation advanced after every real activity change.
    pub generation: u64,
}

impl ActivitySnapshot {
    fn is_idle(self) -> bool {
        self.connections == 0
            && self.active_runs == 0
            && self.running_jobs == 0
            && self.title_tasks == 0
    }
}

/// An idle timeout that was rechecked against the latest activity generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleDeadline {
    /// Activity state that remained idle for the complete configured timeout.
    pub snapshot: ActivitySnapshot,
}

/// Cloneable backend-global keyed activity source.
#[derive(Clone)]
pub struct ActivityTracker {
    inner: Arc<ActivityInner>,
}

struct ActivityInner {
    state: Mutex<ActivityState>,
    snapshots: watch::Sender<ActivitySnapshot>,
}

struct ActivityState {
    connections: HashSet<ConnectionId>,
    active_runs: HashSet<SessionId>,
    running_jobs: HashMap<SessionId, usize>,
    title_tasks: u32,
    generation: u64,
}

/// RAII lease that keeps backend idle shutdown vetoed for one title task.
#[must_use = "dropping the title-task guard releases its idle-shutdown veto"]
pub struct TitleTaskGuard {
    tracker: ActivityTracker,
}

impl Drop for TitleTaskGuard {
    fn drop(&mut self) {
        self.tracker.finish_title_task();
    }
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityTracker {
    /// Creates an idle tracker at generation zero.
    pub fn new() -> Self {
        let initial = ActivitySnapshot {
            connections: 0,
            active_runs: 0,
            running_jobs: 0,
            title_tasks: 0,
            generation: 0,
        };
        let (snapshots, _) = watch::channel(initial);
        Self {
            inner: Arc::new(ActivityInner {
                state: Mutex::new(ActivityState {
                    connections: HashSet::new(),
                    active_runs: HashSet::new(),
                    running_jobs: HashMap::new(),
                    title_tasks: 0,
                    generation: 0,
                }),
                snapshots,
            }),
        }
    }

    /// Subscribes to current and future global activity snapshots.
    pub fn subscribe(&self) -> watch::Receiver<ActivitySnapshot> {
        self.inner.snapshots.subscribe()
    }

    /// Records whether one stable client connection is live.
    pub fn set_connection(&self, id: ConnectionId, connected: bool) {
        let mut state = self.lock();
        if state.connections.contains(&id) == connected {
            return;
        }
        Self::advance_generation(&mut state);
        if connected {
            state.connections.insert(id);
        } else {
            state.connections.remove(&id);
        }
        self.publish(&state);
    }

    /// Records whether one stable session currently owns an active run.
    pub fn set_run(&self, id: SessionId, running: bool) {
        let mut state = self.lock();
        if state.active_runs.contains(&id) == running {
            return;
        }
        Self::advance_generation(&mut state);
        if running {
            state.active_runs.insert(id);
        } else {
            state.active_runs.remove(&id);
        }
        self.publish(&state);
    }

    /// Records one session registry's authoritative running-job count.
    pub fn set_running_jobs(&self, id: SessionId, count: usize) {
        let mut state = self.lock();
        if state.running_jobs.get(&id).copied().unwrap_or(0) == count {
            return;
        }
        Self::advance_generation(&mut state);
        if count == 0 {
            state.running_jobs.remove(&id);
        } else {
            state.running_jobs.insert(id, count);
        }
        self.publish(&state);
    }

    /// Starts one title-generation task and vetoes idle shutdown until its guard drops.
    pub fn begin_title_task(&self) -> TitleTaskGuard {
        let mut state = self.lock();
        state.title_tasks = state
            .title_tasks
            .checked_add(1)
            .expect("backend title-task count exhausted");
        Self::advance_generation(&mut state);
        self.publish(&state);
        TitleTaskGuard {
            tracker: self.clone(),
        }
    }

    fn finish_title_task(&self) {
        let mut state = self.lock();
        state.title_tasks = state
            .title_tasks
            .checked_sub(1)
            .expect("backend title-task guard released without a matching task");
        Self::advance_generation(&mut state);
        self.publish(&state);
    }

    fn lock(&self) -> MutexGuard<'_, ActivityState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn advance_generation(state: &mut ActivityState) {
        state.generation = state
            .generation
            .checked_add(1)
            .expect("backend activity generation exhausted");
    }

    fn publish(&self, state: &ActivityState) {
        let running_jobs = state
            .running_jobs
            .values()
            .try_fold(0_usize, |total, count| total.checked_add(*count))
            .expect("backend running-job count exhausted");
        self.inner.snapshots.send_replace(ActivitySnapshot {
            connections: state.connections.len(),
            active_runs: state.active_runs.len(),
            running_jobs,
            title_tasks: state.title_tasks,
            generation: state.generation,
        });
    }
}

/// Waits until one unchanged eligible generation has remained idle for `timeout`.
pub async fn wait_for_idle(
    mut snapshots: watch::Receiver<ActivitySnapshot>,
    timeout: Duration,
) -> IdleDeadline {
    loop {
        let snapshot = *snapshots.borrow_and_update();
        if !snapshot.is_idle() {
            if snapshots.changed().await.is_err() {
                future::pending::<()>().await;
            }
            continue;
        }

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        let sender_closed = tokio::select! {
            biased;
            () = &mut sleep => false,
            changed = snapshots.changed() => {
                if changed.is_ok() {
                    continue;
                }
                true
            }
        };
        if sender_closed {
            sleep.await;
        }

        let current = *snapshots.borrow_and_update();
        if current.is_idle() && current.generation == snapshot.generation {
            return IdleDeadline { snapshot: current };
        }
    }
}
