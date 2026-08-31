use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use garde::Validate;
use moh::tools::{
    JobCancelArgs, JobDetails, JobId, JobKind, JobRegistry, JobRegistryError, JobService, JobState,
    JobStatusArgs, JobWaitArgs,
};
use schemars::schema_for;
use serde_json::json;

#[derive(Debug)]
struct TestDetails(&'static str);

impl JobDetails for TestDetails {
    fn render(&self) -> String {
        self.0.to_owned()
    }
}

#[derive(Debug)]
struct CleanupDetails(Arc<AtomicUsize>);

impl JobDetails for CleanupDetails {
    fn render(&self) -> String {
        "cleanup".into()
    }

    fn cleanup(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn running(registry: &JobRegistry) -> moh::tools::JobLease {
    registry
        .start(JobKind::Bash, "fixture", Arc::new(TestDetails("running")))
        .unwrap()
}

#[tokio::test]
async fn registry_change_subscription_reports_running_count_transitions() {
    let registry = JobRegistry::new();
    let mut changes = registry.subscribe_changes();
    let lease = running(&registry);

    changes.changed().await.unwrap();
    assert_eq!(registry.running_count().unwrap(), 1);

    drop(lease);
    changes.changed().await.unwrap();
    assert_eq!(registry.running_count().unwrap(), 0);
}

#[test]
fn job_ids_display_monotonically_and_reject_malformed_values() {
    let registry = JobRegistry::new();
    let first = running(&registry);
    let second = running(&registry);

    assert_eq!(first.id().to_string(), "job-0");
    assert_eq!(second.id().to_string(), "job-1");
    assert_eq!(JobId::from_str("job-1").unwrap(), second.id());
    for invalid in ["", "job-", "job--1", "job-01", "job-1x", "Job-1", "1"] {
        assert!(JobId::from_str(invalid).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn wait_wakes_when_a_running_job_finishes() {
    let registry = JobRegistry::new();
    let lease = running(&registry);
    let id = lease.id();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        lease
            .finish(JobState::Completed, Arc::new(TestDetails("done")))
            .unwrap();
    });

    let result = registry
        .wait(&[id], Some(Duration::from_secs(1)))
        .await
        .unwrap();

    assert!(!result.timed_out);
    assert_eq!(result.snapshots[0].state(), JobState::Completed);
    assert_eq!(result.snapshots[0].details().render(), "done");
}

#[tokio::test]
async fn cancel_is_idempotent_and_waits_for_the_terminal_snapshot() {
    let registry = JobRegistry::new();
    let mut lease = running(&registry);
    let id = lease.id();
    tokio::spawn(async move {
        lease.cancelled().await;
        lease
            .finish(JobState::Cancelled, Arc::new(TestDetails("stopped")))
            .unwrap();
    });

    let first = registry.cancel(id).await.unwrap();
    let second = registry.cancel(id).await.unwrap();

    assert_eq!(first.state(), JobState::Cancelled);
    assert_eq!(second.state(), JobState::Cancelled);
}

#[tokio::test]
async fn wait_returns_a_terminal_snapshot_immediately_and_times_out_when_still_running() {
    let registry = JobRegistry::new();
    let lease = running(&registry);
    let id = lease.id();
    lease
        .finish(JobState::Completed, Arc::new(TestDetails("done")))
        .unwrap();
    let completed = registry.wait(&[id], None).await.unwrap();
    assert!(!completed.timed_out);
    assert_eq!(completed.snapshots[0].state(), JobState::Completed);

    let lease = running(&registry);
    let timeout = registry
        .wait(&[lease.id()], Some(Duration::from_millis(1)))
        .await
        .unwrap();
    assert!(timeout.timed_out);
    assert!(timeout.snapshots.is_empty());
}

#[tokio::test]
async fn unknown_ids_are_not_found_for_status_wait_and_cancel() {
    let registry = JobRegistry::new();
    let id = JobId::from_str("job-9").unwrap();
    assert!(matches!(
        registry.status(Some(id)),
        Err(JobRegistryError::NotFound(_))
    ));
    assert!(matches!(
        registry.wait(&[id], None).await,
        Err(JobRegistryError::NotFound(_))
    ));
    assert!(matches!(
        registry.cancel(id).await,
        Err(JobRegistryError::NotFound(_))
    ));
}

#[tokio::test]
async fn concurrent_waiters_observe_the_same_completion() {
    let registry = JobRegistry::new();
    let lease = running(&registry);
    let id = lease.id();
    let first = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.wait(&[id], Some(Duration::from_secs(1))).await })
    };
    let second = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.wait(&[id], Some(Duration::from_secs(1))).await })
    };
    tokio::task::yield_now().await;
    lease
        .finish(JobState::Completed, Arc::new(TestDetails("done")))
        .unwrap();
    assert_eq!(
        first.await.unwrap().unwrap().snapshots[0].state(),
        JobState::Completed
    );
    assert_eq!(
        second.await.unwrap().unwrap().snapshots[0].state(),
        JobState::Completed
    );
}

#[test]
fn capacity_is_released_after_a_job_finishes() {
    let registry = JobRegistry::new();
    let mut leases = (0..16).map(|_| running(&registry)).collect::<Vec<_>>();
    assert!(matches!(
        running_result(&registry),
        Err(JobRegistryError::Capacity)
    ));
    leases
        .pop()
        .unwrap()
        .finish(JobState::Completed, Arc::new(TestDetails("done")))
        .unwrap();
    assert!(running_result(&registry).is_ok());
}

#[test]
fn late_updater_cannot_replace_terminal_details() {
    let registry = JobRegistry::new();
    let lease = running(&registry);
    let id = lease.id();
    let updater = lease.updater().unwrap();
    lease
        .finish(JobState::Completed, Arc::new(TestDetails("done")))
        .unwrap();

    assert!(matches!(
        updater.update(Arc::new(TestDetails("late"))),
        Err(JobRegistryError::AlreadySettled(found)) if found == id
    ));
    assert_eq!(
        registry.status(Some(id)).unwrap()[0].details().render(),
        "done"
    );
}

fn running_result(registry: &JobRegistry) -> Result<moh::tools::JobLease, JobRegistryError> {
    registry.start(JobKind::Bash, "fixture", Arc::new(TestDetails("running")))
}

#[test]
fn terminal_retention_evicts_oldest_and_cleans_its_details_once() {
    let registry = JobRegistry::new();
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    for index in 0..65 {
        let lease = running(&registry);
        let details: Arc<dyn JobDetails> = if index == 0 {
            Arc::new(CleanupDetails(Arc::clone(&cleanup_count)))
        } else {
            Arc::new(TestDetails("done"))
        };
        lease.finish(JobState::Completed, details).unwrap();
    }
    assert!(matches!(
        registry.status(Some(JobId::from_str("job-0").unwrap())),
        Err(JobRegistryError::NotFound(_))
    ));
    for index in 1..65 {
        assert!(
            registry
                .status(Some(JobId::from_str(&format!("job-{index}")).unwrap()))
                .is_ok()
        );
    }
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_cancels_running_jobs_cleans_retained_details_and_blocks_starts() {
    let registry = JobRegistry::new();
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let mut lease = registry
        .start(
            JobKind::Bash,
            "fixture",
            Arc::new(CleanupDetails(Arc::clone(&cleanup_count))),
        )
        .unwrap();
    let id = lease.id();
    let producer_cleanup = Arc::clone(&cleanup_count);
    let producer = tokio::spawn(async move {
        lease.cancelled().await;
        lease
            .finish(
                JobState::Cancelled,
                Arc::new(CleanupDetails(producer_cleanup)),
            )
            .unwrap();
    });
    registry.shutdown().await.unwrap();
    producer.await.unwrap();
    assert!(matches!(
        running_result(&registry),
        Err(JobRegistryError::ShuttingDown)
    ));
    let snapshot = registry.status(Some(id)).unwrap().pop().unwrap();
    assert_eq!(snapshot.state(), JobState::Cancelled);
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    registry.shutdown().await.unwrap();
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_timeout_cleans_retained_and_late_details_exactly_once() {
    let registry = JobRegistry::new();
    let initial_cleanup = Arc::new(AtomicUsize::new(0));
    let late_update_cleanup = Arc::new(AtomicUsize::new(0));
    let late_finish_cleanup = Arc::new(AtomicUsize::new(0));
    let lease = registry
        .start(
            JobKind::Bash,
            "ignores cancellation",
            Arc::new(CleanupDetails(Arc::clone(&initial_cleanup))),
        )
        .unwrap();
    let updater = lease.updater().unwrap();

    assert_eq!(
        registry.shutdown().await,
        Err(JobRegistryError::ShutdownTimeout)
    );
    assert_eq!(initial_cleanup.load(Ordering::SeqCst), 1);

    updater
        .update(Arc::new(CleanupDetails(Arc::clone(&late_update_cleanup))))
        .unwrap();
    assert_eq!(late_update_cleanup.load(Ordering::SeqCst), 1);
    lease
        .finish(
            JobState::Failed,
            Arc::new(CleanupDetails(Arc::clone(&late_finish_cleanup))),
        )
        .unwrap();
    assert_eq!(late_finish_cleanup.load(Ordering::SeqCst), 1);
    assert_eq!(initial_cleanup.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_an_unsettled_lease_marks_the_job_failed() {
    let registry = JobRegistry::new();
    let lease = running(&registry);
    let id = lease.id();

    drop(lease);

    let snapshot = registry.status(Some(id)).unwrap().pop().unwrap();
    assert_eq!(snapshot.state(), JobState::Failed);
    assert!(snapshot.completed_at().is_some());
    assert!(snapshot.details().render().contains("producer disappeared"));
}

#[tokio::test]
async fn service_validates_strict_args_renders_snapshots_and_exposes_schema() {
    let registry = JobRegistry::new();
    let service = JobService::new(registry.clone());
    let lease = running(&registry);
    let id = lease.id();
    let status = service
        .status(JobStatusArgs { job_id: None })
        .await
        .unwrap();
    assert!(status.as_text().unwrap().contains("job-0"));
    lease
        .finish(JobState::Completed, Arc::new(TestDetails("done")))
        .unwrap();
    let waited = service
        .wait(JobWaitArgs {
            job_ids: vec![id.to_string()],
            timeout_ms: None,
        })
        .await
        .unwrap();
    assert!(waited.as_text().unwrap().contains("done"));
    let cancelled = service
        .cancel(JobCancelArgs {
            job_id: id.to_string(),
        })
        .await
        .unwrap();
    assert!(cancelled.as_text().unwrap().contains("completed"));
    let schema = serde_json::to_value(schema_for!(JobWaitArgs)).unwrap();
    assert_eq!(schema["required"], json!(["job_ids"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["job_ids"]["minItems"], 1);
    assert_eq!(schema["properties"]["timeout_ms"]["maximum"], 300_000);
    assert!(
        JobWaitArgs {
            job_ids: vec![],
            timeout_ms: None,
        }
        .validate()
        .is_err()
    );
    assert!(
        JobWaitArgs {
            job_ids: vec!["job-0".into()],
            timeout_ms: Some(0),
        }
        .validate()
        .is_ok()
    );
    assert!(
        JobWaitArgs {
            job_ids: vec!["job-0".into()],
            timeout_ms: Some(300_001),
        }
        .validate()
        .is_err()
    );
    assert!(serde_json::from_value::<JobStatusArgs>(json!({"unexpected": true})).is_err());
    assert!(
        serde_json::from_value::<JobWaitArgs>(json!({"job_ids": ["job-0"], "unexpected": true}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<JobCancelArgs>(json!({"job_id": "job-0", "unexpected": true}))
            .is_err()
    );
    let empty = service
        .wait(JobWaitArgs {
            job_ids: vec![],
            timeout_ms: None,
        })
        .await
        .unwrap_err();
    assert!(empty.to_string().starts_with("[E_INVALID_ARGUMENT]"));
    let excessive = service
        .wait(JobWaitArgs {
            job_ids: vec![id.to_string()],
            timeout_ms: Some(300_001),
        })
        .await
        .unwrap_err();
    assert!(excessive.to_string().starts_with("[E_INVALID_ARGUMENT]"));
}
