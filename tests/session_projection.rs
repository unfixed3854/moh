use std::sync::Arc;

use chrono::{TimeZone, Utc};
use moh::{
    harness::{Message, Role, RunFailure, RunFailureKind, RunStage},
    runtime::rig::ReasoningLevel,
    session::{
        ActiveRunSnapshot, JobSnapshotDto, ModelCatalogState, ModelInfoDto, PlanItem, PlanStatus,
        RunFailureSnapshot, SessionEvent, SessionProjection, SessionRecord, SessionSettings,
        SessionTitle, TranscriptItem,
    },
    tools::{JobDetails, JobKind, JobRegistry},
};
use serde_json::json;

#[derive(Debug)]
struct TestJobDetails;

impl JobDetails for TestJobDetails {
    fn render(&self) -> String {
        "reading src/main.rs".into()
    }
}

#[test]
fn plan_changes_replace_the_snapshot_plan_while_idle_or_busy() {
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);
    let plan = vec![PlanItem::parse("Verify", PlanStatus::InProgress).unwrap()];

    projection
        .apply(SessionEvent::PlanChanged(plan.clone()))
        .unwrap();
    assert_eq!(projection.snapshot(vec![]).plan, plan);

    projection
        .apply(SessionEvent::Started {
            run_id: 7,
            prompt: "inspect".into(),
        })
        .unwrap();
    let replacement = vec![PlanItem::parse("Ship", PlanStatus::Completed).unwrap()];
    projection
        .apply(SessionEvent::PlanChanged(replacement.clone()))
        .unwrap();
    assert_eq!(projection.snapshot(vec![]).plan, replacement);
}

fn record() -> SessionRecord {
    SessionRecord {
        plan: Vec::new(),
        id: "session-1".parse().unwrap(),
        title: moh::session::fallback_title("earlier prompt"),
        title_source: moh::session::TitleSource::Fallback,
        title_revision: 0,
        cwd: b"/work/moh".to_vec(),
        settings: SessionSettings {
            model: "gpt-5.6-terra".into(),
            reasoning: ReasoningLevel::Medium,
            context_tokens: 12,
        },
        transcript: vec![
            TranscriptItem::User("earlier prompt".into()),
            TranscriptItem::Assistant("earlier response".into()),
        ],
        turns: vec![],
        history: vec![
            moh::harness::Message::new(moh::harness::Role::User, "earlier prompt"),
            moh::harness::Message::new(moh::harness::Role::Assistant, "earlier response"),
        ],
        created_at: Utc.with_ymd_and_hms(2026, 8, 26, 9, 0, 0).unwrap(),
        last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 9, 1, 0).unwrap(),
    }
}

#[test]
fn restores_durable_visible_transcript() {
    let interrupted = RunFailureSnapshot {
        stage: RunStage::Finalization,
        kind: RunFailureKind::RuntimeInfrastructure,
        retryable: true,
        message: "run interrupted by backend restart".into(),
    };
    let failed = RunFailureSnapshot {
        stage: RunStage::ModelRequest,
        kind: RunFailureKind::HttpRejected { status: 429 },
        retryable: true,
        message: "rate limited".into(),
    };
    let expected = vec![
        TranscriptItem::User("successful prompt".into()),
        TranscriptItem::Assistant("successful response".into()),
        TranscriptItem::User("failed prompt".into()),
        TranscriptItem::ToolStarted {
            run_id: 7,
            call_id: "call-1".into(),
            name: "read".into(),
            arguments: json!({"path": "src/session/actor.rs"}),
        },
        TranscriptItem::Failed {
            run_id: 7,
            failure: failed,
        },
        TranscriptItem::User("cancelled prompt".into()),
        TranscriptItem::Cancelled { run_id: 8 },
        TranscriptItem::User("interrupted prompt".into()),
        TranscriptItem::Failed {
            run_id: 9,
            failure: interrupted,
        },
    ];
    let mut durable = record();
    durable.transcript = expected.clone();
    durable.history = vec![
        Message::new(Role::User, "successful prompt"),
        Message::new(Role::Assistant, "successful response"),
    ];

    let snapshot = SessionProjection::from_record(durable.clone(), ModelCatalogState::Loading)
        .snapshot(vec![]);

    assert_eq!(snapshot.transcript, expected);
    assert_eq!(snapshot.active_run, None);
    assert!(!snapshot.busy);

    durable.transcript.clear();
    assert!(
        SessionProjection::from_record(durable, ModelCatalogState::Loading)
            .snapshot(vec![])
            .transcript
            .is_empty(),
        "the durable transcript remains authoritative when it is empty"
    );
}

#[test]
fn title_change_updates_summary_and_deleted_only_advances_sequence() {
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);
    let title = SessionTitle::parse("Renamed session").unwrap();

    let renamed = projection
        .apply(SessionEvent::TitleChanged {
            title: title.clone(),
            title_revision: 4,
        })
        .unwrap();
    let before_deleted = projection.snapshot(vec![]);
    let deleted = projection
        .apply(SessionEvent::Deleted {
            session_id: before_deleted.summary.id,
        })
        .unwrap();
    let after_deleted = projection.snapshot(vec![]);

    assert_eq!(renamed.sequence, 1);
    assert_eq!(before_deleted.summary.title, title);
    assert_eq!(before_deleted.summary.title_revision, 4);
    assert_eq!(deleted.sequence, 2);
    assert!(matches!(
        deleted.event,
        SessionEvent::Deleted { session_id } if session_id == before_deleted.summary.id
    ));
    assert_eq!(after_deleted.sequence, 2);
    assert_eq!(after_deleted.summary, before_deleted.summary);
    assert_eq!(after_deleted.transcript, before_deleted.transcript);
    assert_eq!(after_deleted.active_run, before_deleted.active_run);
}

#[test]
fn projection_reduces_active_run_deltas_tools_and_context_usage() {
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);

    projection
        .apply(SessionEvent::Started {
            run_id: 7,
            prompt: "inspect".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::AssistantDelta {
            run_id: 7,
            text: "partial".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::ToolStarted {
            run_id: 7,
            call_id: "call-1".into(),
            name: "read".into(),
            arguments: json!({"path": "src/main.rs"}),
        })
        .unwrap();
    projection
        .apply(SessionEvent::ToolFinished {
            run_id: 7,
            call_id: "call-1".into(),
            name: "read".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::ContextUsage {
            run_id: 7,
            input_tokens: 321,
            last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 9, 2, 0).unwrap(),
        })
        .unwrap();

    let snapshot = projection.snapshot(vec![]);

    assert!(snapshot.busy);
    assert!(snapshot.summary.busy);
    assert_eq!(snapshot.sequence, 5);
    assert_eq!(snapshot.settings.context_tokens, 321);
    assert_eq!(
        snapshot.active_run,
        Some(ActiveRunSnapshot {
            run_id: 7,
            prompt: "inspect".into(),
            assistant_text: "partial".into(),
        })
    );
    assert!(matches!(
        snapshot.transcript.as_slice(),
        [
            TranscriptItem::User(previous_prompt),
            TranscriptItem::Assistant(previous_response),
            TranscriptItem::User(prompt),
            TranscriptItem::ToolStarted { run_id: 7, call_id, name, arguments },
        ] if previous_prompt == "earlier prompt"
            && previous_response == "earlier response"
            && prompt == "inspect"
            && call_id == "call-1"
            && name == "read"
            && arguments == &json!({"path": "src/main.rs"})
    ));
}

#[test]
fn completion_clears_active_projection_and_commits_final_assistant_once() {
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);
    projection
        .apply(SessionEvent::Started {
            run_id: 7,
            prompt: "inspect".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::AssistantDelta {
            run_id: 7,
            text: "partial".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::Completed {
            run_id: 7,
            response: "complete answer".into(),
            last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 9, 2, 0).unwrap(),
        })
        .unwrap();

    let completed = projection.snapshot(vec![]);
    assert!(!completed.busy);
    assert!(!completed.summary.busy);
    assert_eq!(completed.sequence, 3);
    assert_eq!(completed.active_run, None);
    assert_eq!(
        completed.transcript,
        vec![
            TranscriptItem::User("earlier prompt".into()),
            TranscriptItem::Assistant("earlier response".into()),
            TranscriptItem::User("inspect".into()),
            TranscriptItem::Assistant("complete answer".into()),
        ]
    );

    assert!(
        projection
            .apply(SessionEvent::Completed {
                run_id: 7,
                response: "complete answer".into(),
                last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 9, 2, 0).unwrap(),
            })
            .is_err()
    );
    assert_eq!(projection.snapshot(vec![]), completed);
}

#[test]
fn failed_and_cancelled_runs_clear_live_state_without_committing_assistant_text() {
    let failure = RunFailure::new(
        RunStage::ToolExecution,
        RunFailureKind::ToolInfrastructure,
        true,
        "tool service unavailable",
    )
    .with_source(std::io::Error::other("provider-secret"));
    let failure_snapshot = RunFailureSnapshot::from(&failure);
    assert_eq!(failure_snapshot.message, "tool service unavailable");

    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);
    projection
        .apply(SessionEvent::Started {
            run_id: 7,
            prompt: "inspect".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::AssistantDelta {
            run_id: 7,
            text: "partial".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::Failed {
            run_id: 7,
            failure: failure_snapshot.clone(),
        })
        .unwrap();

    let failed = projection.snapshot(vec![]);
    assert!(!failed.busy);
    assert_eq!(failed.active_run, None);
    assert!(matches!(
        failed.transcript.last(),
        Some(TranscriptItem::Failed { run_id: 7, failure }) if failure == &failure_snapshot
    ));
    assert!(
        !failed
            .transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::Assistant(text) if text == "partial"))
    );

    projection
        .apply(SessionEvent::Started {
            run_id: 8,
            prompt: "cancel this".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::Cancelled { run_id: 8 })
        .unwrap();

    let cancelled = projection.snapshot(vec![]);
    assert!(!cancelled.busy);
    assert_eq!(cancelled.active_run, None);
    assert!(matches!(
        cancelled.transcript.last(),
        Some(TranscriptItem::Cancelled { run_id: 8 })
    ));
}

#[test]
fn settings_catalog_jobs_and_persistence_warning_are_snapshotted_or_enveloped() {
    let registry = JobRegistry::new();
    let _lease = registry
        .start(JobKind::Bash, "read source", Arc::new(TestJobDetails))
        .unwrap();
    let event_jobs = registry
        .status(None)
        .unwrap()
        .iter()
        .map(JobSnapshotDto::from)
        .collect::<Vec<_>>();
    let mut snapshot_jobs = event_jobs.clone();
    snapshot_jobs[0].title = "current registry title".into();
    let settings = SessionSettings {
        model: "gpt-5.6-sol".into(),
        reasoning: ReasoningLevel::High,
        context_tokens: 987,
    };
    let catalog = ModelCatalogState::Ready(vec![ModelInfoDto {
        id: "gpt-5.6-sol".into(),
        display_name: "GPT-5.6 Sol".into(),
        description: "high-capability model".into(),
        reasoning_efforts: vec![ReasoningLevel::Medium, ReasoningLevel::High],
        default_reasoning: Some(ReasoningLevel::High),
    }]);
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);

    projection
        .apply(SessionEvent::SettingsChanged {
            settings: settings.clone(),
            last_activity: Utc.with_ymd_and_hms(2026, 8, 26, 9, 2, 0).unwrap(),
        })
        .unwrap();
    let jobs_event = projection
        .apply(SessionEvent::JobsChanged(event_jobs.clone()))
        .unwrap();
    projection
        .apply(SessionEvent::CatalogChanged(catalog.clone()))
        .unwrap();
    projection
        .apply(SessionEvent::PersistenceWarning(Some(
            "checkpoint failed".into(),
        )))
        .unwrap();

    let warned = projection.snapshot(snapshot_jobs.clone());
    assert_eq!(warned.settings, settings);
    assert_eq!(warned.catalog, catalog);
    assert_eq!(warned.jobs, snapshot_jobs);
    assert_eq!(
        warned.persistence_warning.as_deref(),
        Some("checkpoint failed")
    );
    assert_eq!(jobs_event.sequence, 2);
    assert!(matches!(jobs_event.event, SessionEvent::JobsChanged(jobs) if jobs == event_jobs));

    projection
        .apply(SessionEvent::PersistenceWarning(None))
        .unwrap();
    let cleared = projection.snapshot(vec![]);
    assert_eq!(cleared.persistence_warning, None);
    assert!(cleared.jobs.is_empty());
}

#[test]
fn durable_activity_events_update_the_authoritative_summary_timestamp() {
    let settings_activity = Utc.with_ymd_and_hms(2026, 8, 26, 10, 0, 0).unwrap();
    let context_activity = Utc.with_ymd_and_hms(2026, 8, 26, 10, 1, 0).unwrap();
    let completion_activity = Utc.with_ymd_and_hms(2026, 8, 26, 10, 2, 0).unwrap();
    let settings = SessionSettings {
        model: "gpt-5.6-sol".into(),
        reasoning: ReasoningLevel::High,
        context_tokens: 12,
    };
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);

    projection
        .apply(SessionEvent::SettingsChanged {
            settings,
            last_activity: settings_activity,
        })
        .unwrap();
    assert_eq!(
        projection.snapshot(vec![]).summary.last_activity,
        settings_activity
    );
    projection
        .apply(SessionEvent::Started {
            run_id: 7,
            prompt: "inspect".into(),
        })
        .unwrap();
    projection
        .apply(SessionEvent::ContextUsage {
            run_id: 7,
            input_tokens: 321,
            last_activity: context_activity,
        })
        .unwrap();
    assert_eq!(
        projection.snapshot(vec![]).summary.last_activity,
        context_activity
    );
    projection
        .apply(SessionEvent::Completed {
            run_id: 7,
            response: "complete answer".into(),
            last_activity: completion_activity,
        })
        .unwrap();

    assert_eq!(
        projection.snapshot(vec![]).summary.last_activity,
        completion_activity
    );
}

#[test]
fn mismatched_run_id_is_rejected_without_advancing_or_mutating_projection() {
    let mut projection = SessionProjection::from_record(record(), ModelCatalogState::Loading);
    projection
        .apply(SessionEvent::Started {
            run_id: 7,
            prompt: "inspect".into(),
        })
        .unwrap();
    let before = projection.snapshot(vec![]);

    assert!(
        projection
            .apply(SessionEvent::AssistantDelta {
                run_id: 8,
                text: "wrong run".into(),
            })
            .is_err()
    );

    assert_eq!(projection.snapshot(vec![]), before);
}
