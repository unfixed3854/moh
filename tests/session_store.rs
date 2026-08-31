mod support;

use std::{
    fs,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
};

use chrono::{TimeZone, Utc};
use futures::future::join_all;
use moh::{
    harness::{Message, Role, RunFailureKind, RunStage},
    runtime::rig::ReasoningLevel,
    session::{
        DurableTurn, MaterializeSession, PlanItem, PlanStatus, RunFailureSnapshot, SessionId,
        SessionListScope, SessionName, SessionRepository, SessionSelector, SessionSettings,
        SessionStore, SessionStoreError, SessionTitle, StoreWarning, TitleSource, TranscriptItem,
        TurnStatus,
    },
};
use rusqlite::{Connection, params};
use serde_json::json;

use support::{FailingRepository, RepositoryWriteOperation};

fn test_settings() -> SessionSettings {
    SessionSettings {
        model: "gpt-5.6-terra".into(),
        reasoning: ReasoningLevel::Medium,
        context_tokens: 0,
    }
}

fn materialize_request(
    cwd: &[u8],
    title: &str,
    prompt: &str,
    run_id: u64,
    created_at: chrono::DateTime<Utc>,
) -> MaterializeSession {
    MaterializeSession {
        cwd: cwd.to_vec(),
        title: SessionTitle::parse(title).unwrap(),
        settings: test_settings(),
        prompt: prompt.into(),
        run_id,
        created_at,
    }
}

#[tokio::test]
async fn materialize_persists_prompt_and_running_turn_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();
    let created_at = Utc.with_ymd_and_hms(2026, 8, 28, 8, 30, 0).unwrap();

    let record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Inspect persistence",
            "Inspect persistence",
            41,
            created_at,
        ))
        .await
        .unwrap();
    drop(opened);

    let connection = Connection::open(&path).unwrap();
    let sqlite_id = i64::try_from(record.id.get()).unwrap();
    let stored_session = connection
        .query_row(
            "SELECT title, title_source, title_revision, cwd, created_at, last_activity \
             FROM sessions WHERE id = ?1",
            [sqlite_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(stored_session.0, "Inspect persistence");
    assert_eq!(stored_session.1, "fallback");
    assert_eq!(stored_session.2, 0);
    assert_eq!(stored_session.3, b"/work/project");
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&stored_session.4)
            .unwrap()
            .with_timezone(&Utc),
        created_at
    );
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&stored_session.5)
            .unwrap()
            .with_timezone(&Utc),
        created_at
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT kind, text FROM transcript_items WHERE session_id = ?1",
                [sqlite_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap(),
        ("user".into(), "Inspect persistence".into())
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [sqlite_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT ordinal, run_id, prompt_position, status FROM turns WHERE session_id = ?1",
                [sqlite_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap(),
        (0, 41, 0, "running".into())
    );
}

#[tokio::test]
async fn list_supports_project_and_global_scopes_in_stable_order() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let earlier = Utc.with_ymd_and_hms(2026, 8, 28, 8, 0, 0).unwrap();
    let later = Utc.with_ymd_and_hms(2026, 8, 28, 9, 0, 0).unwrap();
    let first = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "First",
            "first",
            1,
            earlier,
        ))
        .await
        .unwrap();
    let second = opened
        .store
        .materialize(materialize_request(
            b"/work/other",
            "Second",
            "second",
            2,
            later,
        ))
        .await
        .unwrap();
    let third = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Third",
            "third",
            3,
            later,
        ))
        .await
        .unwrap();

    let project = opened
        .store
        .list(SessionListScope::Project(b"/work/project".to_vec()))
        .await
        .unwrap();
    let all = opened.store.list(SessionListScope::All).await.unwrap();

    assert_eq!(
        project.iter().map(|summary| summary.id).collect::<Vec<_>>(),
        vec![third.id, first.id]
    );
    assert_eq!(
        all.iter().map(|summary| summary.id).collect::<Vec<_>>(),
        vec![third.id, second.id, first.id]
    );
}

#[tokio::test]
async fn duplicate_titles_require_id_after_ambiguous_lookup() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let cwd = b"/work/project".to_vec();
    let title = SessionTitle::parse("Duplicate").unwrap();
    let created_at = Utc.with_ymd_and_hms(2026, 8, 28, 9, 0, 0).unwrap();
    let first = opened
        .store
        .materialize(materialize_request(
            &cwd,
            title.as_str(),
            "first",
            1,
            created_at,
        ))
        .await
        .unwrap();
    let second = opened
        .store
        .materialize(materialize_request(
            &cwd,
            title.as_str(),
            "second",
            2,
            created_at,
        ))
        .await
        .unwrap();

    let error = opened
        .store
        .resolve(SessionSelector::Title(title.clone()), cwd.clone())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SessionStoreError::AmbiguousTitle {
            title: ambiguous,
            ids,
        } if ambiguous == title && ids == vec![first.id, second.id]
    ));
    assert_eq!(
        opened
            .store
            .resolve(SessionSelector::Id(second.id), cwd)
            .await
            .unwrap()
            .id,
        second.id
    );
}

#[tokio::test]
async fn manual_rename_increments_revision_and_allows_duplicates() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let cwd = b"/work/project".to_vec();
    let created_at = Utc.with_ymd_and_hms(2026, 8, 28, 9, 0, 0).unwrap();
    let first = opened
        .store
        .materialize(materialize_request(&cwd, "First", "first", 1, created_at))
        .await
        .unwrap();
    let duplicate_title = SessionTitle::parse("Shared").unwrap();
    opened
        .store
        .materialize(materialize_request(
            &cwd,
            duplicate_title.as_str(),
            "second",
            2,
            created_at,
        ))
        .await
        .unwrap();

    let renamed = opened
        .store
        .rename(first.id, duplicate_title.clone())
        .await
        .unwrap();

    assert_eq!(renamed.title, duplicate_title);
    assert_eq!(renamed.title_source, TitleSource::Manual);
    assert_eq!(renamed.title_revision, 1);
    assert_eq!(
        opened
            .store
            .list(SessionListScope::Project(cwd))
            .await
            .unwrap()
            .iter()
            .filter(|summary| summary.title == renamed.title)
            .count(),
        2
    );
}

#[tokio::test]
async fn generated_title_compare_and_set_cannot_overwrite_manual_rename() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let created_at = Utc.with_ymd_and_hms(2026, 8, 28, 9, 0, 0).unwrap();
    let record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Fallback",
            "prompt",
            1,
            created_at,
        ))
        .await
        .unwrap();
    let generated = opened
        .store
        .compare_and_set_generated_title(record.id, 0, SessionTitle::parse("Generated").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(generated.title_source, TitleSource::Generated);
    assert_eq!(generated.title_revision, 1);
    let manual_title = SessionTitle::parse("Manual").unwrap();
    let renamed = opened
        .store
        .rename(record.id, manual_title.clone())
        .await
        .unwrap();

    assert!(
        opened
            .store
            .compare_and_set_generated_title(
                record.id,
                generated.title_revision,
                SessionTitle::parse("Stale generated").unwrap(),
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        opened
            .store
            .compare_and_set_generated_title(
                record.id,
                renamed.title_revision,
                SessionTitle::parse("Current generated").unwrap(),
            )
            .await
            .unwrap()
            .is_none()
    );
    let restored = opened.store.load(record.id).await.unwrap();
    assert_eq!(restored.title, manual_title);
    assert_eq!(restored.title_source, TitleSource::Manual);
    assert_eq!(restored.title_revision, 2);
}

#[tokio::test]
async fn delete_cascades_every_session_child_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();
    let created_at = Utc.with_ymd_and_hms(2026, 8, 28, 9, 0, 0).unwrap();
    let mut record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Delete me",
            "question",
            1,
            created_at,
        ))
        .await
        .unwrap();
    record
        .transcript
        .push(TranscriptItem::Assistant("answer".into()));
    record.turns[0].status = TurnStatus::Completed;
    record.history = vec![
        Message::new(Role::User, "question"),
        Message::new(Role::Assistant, "answer"),
    ];
    opened.store.checkpoint(record.clone()).await.unwrap();

    opened.store.delete(record.id).await.unwrap();

    assert!(matches!(
        opened.store.load(record.id).await.unwrap_err(),
        SessionStoreError::NotFound { .. }
    ));
    let connection = Connection::open(&path).unwrap();
    let sqlite_id = i64::try_from(record.id.get()).unwrap();
    for table in ["sessions", "messages", "transcript_items", "turns"] {
        let count = connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE {} = ?1",
                    if table == "sessions" {
                        "id"
                    } else {
                        "session_id"
                    }
                ),
                [sqlite_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(count, 0, "{table} still contains deleted session rows");
    }
}

#[tokio::test]
async fn loading_converts_running_turn_to_one_interruption_idempotently() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let created_at = Utc.with_ymd_and_hms(2026, 8, 28, 9, 0, 0).unwrap();
    let record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Interrupted",
            "continue the work",
            73,
            created_at,
        ))
        .await
        .unwrap();

    let recovered = opened.store.load(record.id).await.unwrap();
    assert_eq!(recovered.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(recovered.transcript.len(), 2);
    assert!(matches!(
        &recovered.transcript[1],
        TranscriptItem::Failed {
            run_id: 73,
            failure: RunFailureSnapshot {
                stage: RunStage::Finalization,
                kind: RunFailureKind::RuntimeInfrastructure,
                retryable: true,
                message,
            },
        } if message == "run interrupted by backend restart"
    ));

    let loaded_again = opened.store.load(record.id).await.unwrap();
    assert_eq!(loaded_again, recovered);
}

#[test]
fn session_ids_and_names_have_unambiguous_namespaces() {
    let id: SessionId = "session-42".parse().unwrap();
    assert_eq!(id.to_string(), "session-42");
    assert!("session-01".parse::<SessionId>().is_err());
    assert!(SessionName::parse("review").is_ok());
    assert!(SessionName::parse("session-7").is_err());
    assert!(SessionName::parse("bad\nname").is_err());
}

#[test]
fn session_names_enforce_scalar_and_whitespace_boundaries() {
    assert!(SessionName::parse("").is_err());
    assert!(SessionName::parse(" review").is_err());
    assert!(SessionName::parse("review ").is_err());
    assert!(SessionName::parse("界".repeat(64)).is_ok());
    assert!(SessionName::parse("界".repeat(65)).is_err());
}

#[tokio::test]
async fn newly_created_database_and_initialization_lock_are_owner_only() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sessions.sqlite");

    let _opened = SessionStore::open_at(&database).await.unwrap();

    for path in [&database, &directory.path().join("sessions.sqlite.lock")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{} must not be readable by other users",
            path.display()
        );
    }
}

#[tokio::test]
async fn materialization_allows_two_sessions_in_one_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let cwd = directory.path().as_os_str().as_bytes().to_vec();
    let first = opened
        .store
        .materialize(materialize_request(
            &cwd,
            "First",
            "first prompt",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    let second = opened
        .store
        .materialize(materialize_request(
            &cwd,
            "Second",
            "second prompt",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(
        opened
            .store
            .list(SessionListScope::Project(cwd))
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn ids_resolve_globally_but_titles_resolve_only_within_their_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let first_cwd = b"/work/first".to_vec();
    let second_cwd = b"/work/second".to_vec();
    let title = SessionTitle::parse("review").unwrap();
    let first = opened
        .store
        .materialize(materialize_request(
            &first_cwd,
            title.as_str(),
            "first review",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    let second = opened
        .store
        .materialize(materialize_request(
            &second_cwd,
            title.as_str(),
            "second review",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();

    assert_eq!(
        opened
            .store
            .resolve(SessionSelector::Id(first.id), second_cwd.clone())
            .await
            .unwrap()
            .id,
        first.id
    );
    assert_eq!(
        opened
            .store
            .resolve(SessionSelector::Title(title), second_cwd)
            .await
            .unwrap()
            .id,
        second.id
    );
}

#[tokio::test]
async fn duplicate_titles_are_allowed_within_one_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let cwd = b"/work/project".to_vec();
    let first = opened
        .store
        .materialize(materialize_request(
            &cwd,
            "review",
            "first review",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();

    let second = opened
        .store
        .materialize(materialize_request(
            &cwd,
            "review",
            "second review",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(first.title, second.title);
    assert_eq!(
        opened
            .store
            .list(SessionListScope::Project(cwd))
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn checkpoint_restores_committed_history_and_metadata_exactly() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();
    let mut record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Initial checkpoint",
            "initial checkpoint",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    record.settings = SessionSettings {
        model: "openai/gpt-5.6-sol".into(),
        reasoning: ReasoningLevel::Xhigh,
        context_tokens: 98_765,
    };
    record.title = SessionTitle::parse("Durable checkpoint").unwrap();
    record.title_source = TitleSource::Generated;
    record.title_revision = 4;
    record.transcript = vec![
        TranscriptItem::User("inspect the store".into()),
        TranscriptItem::ToolStarted {
            run_id: 7,
            call_id: "call-1".into(),
            name: "read".into(),
            arguments: json!({"path": "src/session/store.rs"}),
        },
        TranscriptItem::Failed {
            run_id: 7,
            failure: RunFailureSnapshot {
                stage: RunStage::ModelRequest,
                kind: RunFailureKind::HttpRejected { status: 429 },
                retryable: true,
                message: "rate limited".into(),
            },
        },
        TranscriptItem::User("try again".into()),
        TranscriptItem::Cancelled { run_id: 8 },
        TranscriptItem::Assistant("the store is durable".into()),
    ];
    record.turns = vec![
        DurableTurn {
            ordinal: 0,
            run_id: 7,
            prompt_position: 0,
            status: TurnStatus::Failed,
        },
        DurableTurn {
            ordinal: 1,
            run_id: 8,
            prompt_position: 3,
            status: TurnStatus::Cancelled,
        },
        DurableTurn {
            ordinal: 2,
            run_id: 9,
            prompt_position: 3,
            status: TurnStatus::Interrupted,
        },
        DurableTurn {
            ordinal: 3,
            run_id: 10,
            prompt_position: 3,
            status: TurnStatus::Completed,
        },
        DurableTurn {
            ordinal: 4,
            run_id: 11,
            prompt_position: 3,
            status: TurnStatus::Interrupted,
        },
    ];
    record.history = vec![
        Message::new(Role::User, "inspect the store"),
        Message::new(Role::Assistant, "the store is durable"),
    ];
    record.plan = vec![
        PlanItem::parse("Inspect the store", PlanStatus::Completed).unwrap(),
        PlanItem::parse("Verify durability", PlanStatus::InProgress).unwrap(),
    ];
    record.last_activity = Utc.with_ymd_and_hms(2026, 8, 26, 12, 34, 56).unwrap();

    opened.store.checkpoint(record.clone()).await.unwrap();
    drop(opened);

    let reopened = SessionStore::open_at(&path).await.unwrap();
    assert_eq!(reopened.store.load(record.id).await.unwrap(), record);
}

#[tokio::test]
async fn checkpoint_replaces_and_clears_ordered_plan_items() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();
    let mut record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Plan persistence",
            "Plan persistence",
            1,
            Utc::now(),
        ))
        .await
        .unwrap();

    record.plan = vec![
        PlanItem::parse("Inspect", PlanStatus::Completed).unwrap(),
        PlanItem::parse("Verify", PlanStatus::InProgress).unwrap(),
    ];
    opened.store.checkpoint(record.clone()).await.unwrap();
    assert_eq!(
        opened.store.load(record.id).await.unwrap().plan,
        record.plan
    );

    record.plan.clear();
    opened.store.checkpoint(record.clone()).await.unwrap();
    assert!(opened.store.load(record.id).await.unwrap().plan.is_empty());

    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn checkpoint_rejects_an_invalid_complete_plan_without_replacing_rows() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let mut record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Plan validation",
            "inspect",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    let committed_plan = vec![PlanItem::parse("Retain this plan", PlanStatus::Pending).unwrap()];
    record.plan = committed_plan.clone();
    opened.store.checkpoint(record.clone()).await.unwrap();
    for plan in [
        vec![
            PlanItem::parse("Inspect", PlanStatus::InProgress).unwrap(),
            PlanItem::parse("Verify", PlanStatus::InProgress).unwrap(),
        ],
        vec![PlanItem::parse("Inspect", PlanStatus::Pending).unwrap(); 33],
        vec![serde_json::from_str(r#"{"step":" ","status":"pending"}"#).unwrap()],
    ] {
        record.plan = plan;

        let error = opened.store.checkpoint(record.clone()).await.unwrap_err();

        assert!(matches!(error, SessionStoreError::InvalidStoredData { .. }));
        assert_eq!(
            opened.store.load(record.id).await.unwrap().plan,
            committed_plan
        );
    }
}

#[tokio::test]
async fn load_rejects_invalid_persisted_plan_rows() {
    let cases = [
        (
            "non-contiguous position",
            "INSERT INTO plan_items VALUES (?1, 1, 'Inspect', 'pending')",
            false,
        ),
        (
            "unknown status",
            "INSERT INTO plan_items VALUES (?1, 0, 'Inspect', 'unknown')",
            true,
        ),
        (
            "invalid step",
            "INSERT INTO plan_items VALUES (?1, 0, ' Inspect ', 'pending')",
            false,
        ),
        (
            "two active rows",
            "INSERT INTO plan_items VALUES (?1, 0, 'Inspect', 'in_progress'); INSERT INTO plan_items VALUES (?1, 1, 'Verify', 'in_progress')",
            false,
        ),
    ];

    for (name, sql, disable_checks) in cases {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite");
        let opened = SessionStore::open_at(&path).await.unwrap();
        let record = opened
            .store
            .materialize(materialize_request(
                b"/work/project",
                "Invalid plan fixture",
                "inspect",
                0,
                Utc::now(),
            ))
            .await
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        if disable_checks {
            connection
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .unwrap();
        }
        let sqlite_id = i64::try_from(record.id.get()).unwrap();
        if sql.matches("?1").count() > 1 {
            let sql = sql.replace("?1", &sqlite_id.to_string());
            connection.execute_batch(&sql).unwrap();
        } else {
            connection.execute(sql, [sqlite_id]).unwrap();
        }

        let error = opened.store.load(record.id).await.unwrap_err();
        assert!(
            matches!(error, SessionStoreError::InvalidStoredData { .. }),
            "{name}: {error:?}"
        );
    }
}

#[tokio::test]
async fn invalid_v2_transcript_json_kinds_and_turn_statuses_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();

    let mut invalid_json = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "invalid-json",
            "invalid json",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    invalid_json.transcript = vec![TranscriptItem::ToolStarted {
        run_id: 1,
        call_id: "call-1".into(),
        name: "read".into(),
        arguments: json!({"path": "src/main.rs"}),
    }];
    opened.store.checkpoint(invalid_json.clone()).await.unwrap();

    let mut invalid_kind = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "invalid-kind",
            "invalid kind",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    invalid_kind.transcript = vec![TranscriptItem::User("prompt".into())];
    opened.store.checkpoint(invalid_kind.clone()).await.unwrap();

    let invalid_status = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "invalid-status",
            "invalid status",
            1,
            Utc::now(),
        ))
        .await
        .unwrap();
    opened
        .store
        .checkpoint(invalid_status.clone())
        .await
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    connection
        .execute(
            "UPDATE transcript_items SET arguments_json = '{' WHERE session_id = ?1",
            [i64::try_from(invalid_json.id.get()).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE transcript_items SET kind = 'future_kind' WHERE session_id = ?1",
            [i64::try_from(invalid_kind.id.get()).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE turns SET status = 'future_status' WHERE session_id = ?1",
            params![i64::try_from(invalid_status.id.get()).unwrap()],
        )
        .unwrap();
    drop(connection);

    for (id, expected_field) in [
        (invalid_json.id, "tool arguments"),
        (invalid_kind.id, "transcript kind"),
        (invalid_status.id, "turn status"),
    ] {
        let error = opened.store.load(id).await.unwrap_err();
        assert!(matches!(
            error,
            SessionStoreError::InvalidStoredData { field, .. } if field == expected_field
        ));
    }
}

#[tokio::test]
async fn update_metadata_does_not_rewrite_committed_message_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();
    let mut record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "metadata",
            "keep this",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    let committed_history = vec![
        Message::new(Role::User, "keep this"),
        Message::new(Role::Assistant, "kept"),
    ];
    record.history = committed_history.clone();
    record.turns[0].status = TurnStatus::Completed;
    opened.store.checkpoint(record.clone()).await.unwrap();

    record.settings = SessionSettings {
        model: "openai/gpt-5.6-sol".into(),
        reasoning: ReasoningLevel::High,
        context_tokens: 42,
    };
    record.last_activity = Utc.with_ymd_and_hms(2026, 8, 26, 13, 0, 0).unwrap();
    record.history = vec![Message::new(Role::User, "must not be written")];
    opened.store.update_metadata(record.clone()).await.unwrap();

    let restored = opened.store.load(record.id).await.unwrap();
    assert_eq!(restored.history, committed_history);
    assert_eq!(restored.settings, record.settings);
    assert_eq!(restored.last_activity, record.last_activity);
}

#[tokio::test]
async fn corrupt_database_is_quarantined_and_replaced_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    fs::write(&path, b"not sqlite").unwrap();

    let opened = SessionStore::open_at(&path).await.unwrap();

    let [StoreWarning::CorruptDatabaseQuarantined { path: quarantine }] =
        opened.warnings.as_slice()
    else {
        panic!("expected one corruption quarantine warning");
    };
    assert!(quarantine.exists());
    assert_eq!(fs::read(quarantine).unwrap(), b"not sqlite");
    assert!(
        opened
            .store
            .list(SessionListScope::Project(b"/work/project".to_vec()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unsupported_schema_versions_are_not_quarantined() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 4).unwrap();
    drop(connection);

    let error = SessionStore::open_at(&path).await.unwrap_err();

    assert!(matches!(
        error,
        SessionStoreError::UnsupportedSchemaVersion { found: 4, .. }
    ));
    assert!(path.exists());
    assert!(directory.path().read_dir().unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".corrupt-")
    }));
}

#[tokio::test]
async fn ordinary_database_open_errors_are_not_quarantined() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("database-is-a-directory");
    fs::create_dir(&path).unwrap();

    let error = SessionStore::open_at(&path).await.unwrap_err();

    assert!(matches!(error, SessionStoreError::Database { .. }));
    assert!(path.is_dir());
    assert!(directory.path().read_dir().unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".corrupt-")
    }));
}

#[tokio::test]
async fn concurrent_new_store_opens_share_one_atomic_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");

    let opened = join_all((0..16).map(|_| SessionStore::open_at(&path)))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    for store in opened {
        assert!(store.warnings.is_empty());
        assert!(
            store
                .store
                .list(SessionListScope::Project(b"/work/project".to_vec()))
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn migrates_v1_sessions_to_titles_and_drops_empty_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            r#"CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT,
                cwd BLOB NOT NULL,
                is_default INTEGER NOT NULL CHECK (is_default IN (0, 1)),
                model TEXT NOT NULL,
                reasoning TEXT NOT NULL,
                context_tokens INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                last_activity TEXT NOT NULL
            );
            CREATE UNIQUE INDEX one_default_per_cwd
                ON sessions(cwd) WHERE is_default = 1;
            CREATE UNIQUE INDEX one_name_per_cwd
                ON sessions(cwd, name) WHERE name IS NOT NULL;
            CREATE TABLE messages (
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
                text TEXT NOT NULL,
                PRIMARY KEY (session_id, position)
            );
            INSERT INTO sessions VALUES
                (1, 'review', X'2F776F726B2F70726F6A656374', 0, 'gpt-5.6-terra', 'medium', 11,
                 '2026-08-26T10:00:00Z', '2026-08-26T12:00:00Z'),
                (2, NULL, X'2F776F726B2F70726F6A656374', 0, 'gpt-5.6-terra', 'medium', 22,
                 '2026-08-26T09:00:00Z', '2026-08-26T11:00:00Z'),
                (3, NULL, X'2F776F726B2F70726F6A656374', 1, 'gpt-5.6-terra', 'medium', 0,
                 '2026-08-26T08:00:00Z', '2026-08-26T08:00:00Z');
            INSERT INTO messages VALUES
                (1, 0, 'user', 'Review the durable session changes'),
                (1, 1, 'assistant', 'The changes look consistent.'),
                (2, 0, 'user', 'Investigate the parser'),
                (2, 1, 'assistant', 'The parser needs one more guard.');
            PRAGMA user_version = 1;"#,
        )
        .unwrap();
    drop(connection);

    let opened = SessionStore::open_at(&path).await.unwrap();
    let sessions = opened
        .store
        .list(SessionListScope::Project(b"/work/project".to_vec()))
        .await
        .unwrap();

    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].title.as_str(), "review");
    assert_eq!(sessions[1].title.as_str(), "Investigate the parser");
    assert!(
        sessions
            .iter()
            .all(|summary| !summary.title.as_str().is_empty())
    );
    let connection = Connection::open(&path).unwrap();
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .unwrap();
    assert_eq!(version, 3);
}

#[tokio::test]
async fn migrates_session_management_v2_by_adding_an_empty_plan_table() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let opened = SessionStore::open_at(&path).await.unwrap();
    let record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "Session v2",
            "preserve this prompt",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    drop(opened);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("DROP TABLE plan_items; PRAGMA user_version = 2;")
        .unwrap();
    drop(connection);

    let opened = SessionStore::open_at(&path).await.unwrap();
    let restored = opened.store.load(record.id).await.unwrap();

    assert!(matches!(
        restored.transcript.as_slice(),
        [TranscriptItem::User(prompt), TranscriptItem::Failed { run_id: 0, .. }]
            if prompt == "preserve this prompt"
    ));
    assert!(restored.plan.is_empty());
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn migrates_plan_tool_v2_to_session_v3_without_losing_messages_or_plans() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            r#"CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT,
                cwd BLOB NOT NULL,
                is_default INTEGER NOT NULL CHECK (is_default IN (0, 1)),
                model TEXT NOT NULL,
                reasoning TEXT NOT NULL,
                context_tokens INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                last_activity TEXT NOT NULL
            );
            CREATE TABLE messages (
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
                text TEXT NOT NULL,
                PRIMARY KEY (session_id, position)
            );
            CREATE TABLE plan_items (
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                position INTEGER NOT NULL CHECK (position >= 0),
                step TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'in_progress', 'completed', 'blocked', 'cancelled')),
                PRIMARY KEY (session_id, position)
            );
            INSERT INTO sessions VALUES
                (1, 'review', X'2F776F726B2F70726F6A656374', 0, 'gpt-5.6-terra', 'medium', 11,
                 '2026-08-26T10:00:00Z', '2026-08-26T12:00:00Z');
            INSERT INTO messages VALUES
                (1, 0, 'user', 'Review the durable session changes'),
                (1, 1, 'assistant', 'The changes look consistent.');
            INSERT INTO plan_items VALUES
                (1, 0, 'Inspect', 'completed'),
                (1, 1, 'Verify', 'in_progress');
            PRAGMA user_version = 2;"#,
        )
        .unwrap();
    drop(connection);

    let opened = SessionStore::open_at(&path).await.unwrap();
    let restored = opened
        .store
        .load("session-1".parse().unwrap())
        .await
        .unwrap();

    assert_eq!(restored.title.as_str(), "review");
    assert_eq!(
        restored.history,
        vec![
            Message::new(Role::User, "Review the durable session changes"),
            Message::new(Role::Assistant, "The changes look consistent."),
        ]
    );
    assert_eq!(
        restored.plan,
        vec![
            PlanItem::parse("Inspect", PlanStatus::Completed).unwrap(),
            PlanItem::parse("Verify", PlanStatus::InProgress).unwrap(),
        ]
    );
}

#[tokio::test]
async fn migration_sanitizes_non_whitespace_controls_in_fallback_titles() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            r#"CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT,
                cwd BLOB NOT NULL,
                is_default INTEGER NOT NULL CHECK (is_default IN (0, 1)),
                model TEXT NOT NULL,
                reasoning TEXT NOT NULL,
                context_tokens INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                last_activity TEXT NOT NULL
            );
            CREATE TABLE messages (
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
                text TEXT NOT NULL,
                PRIMARY KEY (session_id, position)
            );
            INSERT INTO sessions VALUES
                (1, NULL, X'2F776F726B2F70726F6A656374', 1, 'gpt-5.6-terra', 'medium', 0,
                 '2026-08-26T10:00:00Z', '2026-08-26T12:00:00Z');
            INSERT INTO messages VALUES
                (1, 0, 'user', 'Investigate ' || char(0) || ' the parser'),
                (1, 1, 'assistant', 'The parser needs one more guard.');
            PRAGMA user_version = 1;"#,
        )
        .unwrap();
    drop(connection);

    let opened = SessionStore::open_at(&path).await.unwrap();
    let sessions = opened
        .store
        .list(SessionListScope::Project(b"/work/project".to_vec()))
        .await
        .unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title.as_str(), "Investigate the parser");
}

#[tokio::test]
async fn concurrent_corrupt_store_opens_preserve_one_quarantine_and_share_rebuild() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite");
    fs::write(&path, b"not sqlite").unwrap();

    let opened = join_all((0..16).map(|_| SessionStore::open_at(&path)))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let quarantine_paths = opened
        .iter()
        .flat_map(|opened| &opened.warnings)
        .map(|warning| match warning {
            StoreWarning::CorruptDatabaseQuarantined { path } => path,
        })
        .collect::<Vec<_>>();

    assert_eq!(quarantine_paths.len(), 1);
    assert_eq!(fs::read(quarantine_paths[0]).unwrap(), b"not sqlite");
    assert_eq!(
        directory
            .path()
            .read_dir()
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sessions.sqlite.corrupt-")
            })
            .count(),
        1
    );
    for store in opened {
        assert!(
            store
                .store
                .list(SessionListScope::Project(b"/work/project".to_vec()))
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn failing_repository_fails_and_distinguishes_both_write_operations() {
    let directory = tempfile::tempdir().unwrap();
    let opened = SessionStore::open_at(&directory.path().join("sessions.sqlite"))
        .await
        .unwrap();
    let record = opened
        .store
        .materialize(materialize_request(
            b"/work/project",
            "failing repository",
            "failing repository",
            0,
            Utc::now(),
        ))
        .await
        .unwrap();
    let repository = FailingRepository::new(record.clone());
    repository.fail_checkpoints(true);

    assert!(repository.checkpoint(record.clone()).await.is_err());
    assert!(repository.update_metadata(record).await.is_err());
    assert_eq!(
        repository.write_operations(),
        vec![
            RepositoryWriteOperation::Checkpoint,
            RepositoryWriteOperation::UpdateMetadata,
        ]
    );
}
