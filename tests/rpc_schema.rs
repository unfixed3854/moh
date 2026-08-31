use std::{cell::RefCell, rc::Rc, str::FromStr};

use capnp::traits::{Imbue, ImbueMut};
use capnp::{
    capability::{FromClientHook, Promise, Request},
    introspect::{Introspect, TypeVariant},
    message::Builder,
    private::capability::{ClientHook, ParamsHook, ResultsHook},
};
use chrono::{TimeZone, Utc};
use moh::{
    harness::{RunFailureKind, RunStage},
    rpc::{
        convert::{
            CommandError, DetachResult, DraftDefaultsResult, ErrorCode, MAX_RPC_CWD_BYTES,
            MAX_RPC_IDENTIFIER_BYTES, MAX_RPC_PROMPT_BYTES, MAX_RPC_TITLE_BYTES, MaterializeResult,
            MaterializeSuccess, OpenResult, OpenSuccess, ProtocolInfo, REQUIRED_FEATURES,
            RpcConversionError, StartupResult, StartupSuccess, read_attachment_id,
            read_command_result, read_detach_result, read_draft_defaults,
            read_draft_defaults_result, read_event_envelope, read_job_list_result, read_job_result,
            read_materialize_result, read_open_result, read_protocol_info, read_run_failure,
            read_session_list_result, read_session_list_scope, read_session_selector,
            read_session_settings, read_session_snapshot, read_startup_result, read_submit_result,
            validate_inbound_field_length, validate_wire_data_length,
            validate_wire_inline_composite_list_length, validate_wire_list_length,
            validate_wire_text_length, write_attachment_id, write_command_result,
            write_detach_result, write_draft_defaults, write_draft_defaults_result,
            write_event_envelope, write_job_list_result, write_job_result,
            write_materialize_result, write_open_result, write_protocol_info, write_run_failure,
            write_session_list_result, write_session_list_scope, write_session_selector,
            write_session_settings, write_session_snapshot, write_startup_result,
            write_submit_result,
        },
        moh_capnp,
    },
    runtime::rig::ReasoningLevel,
    session::{
        ActiveRunSnapshot, AttachmentId, DraftDefaults, JobSnapshotDto, ModelCatalogState,
        ModelInfoDto, PlanItem, PlanStatus, RunFailureSnapshot, SessionCommandError, SessionEvent,
        SessionEventEnvelope, SessionId, SessionListScope, SessionSelector, SessionSettings,
        SessionSnapshot, SessionSummary, SessionTitle, TranscriptItem,
    },
    tools::{JobKind, JobState},
};
use serde_json::json;

const ALL_REASONING: [ReasoningLevel; 7] = [
    ReasoningLevel::None,
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::Xhigh,
    ReasoningLevel::Max,
];

const SCHEMA_FILE_ID: u64 = 0x9ea0_e1de_9de6_bd37;

type FieldContract<'a> = (&'a str, u16, Option<u16>);
type CallLog = Rc<RefCell<Vec<(u64, u16)>>>;

fn assert_struct_contract<T: Introspect>(name: &str, expected: &[FieldContract<'_>]) {
    let TypeVariant::Struct(raw) = T::introspect().which() else {
        panic!("{name} is not generated as a struct");
    };
    let schema = capnp::schema::StructSchema::from(raw);
    let proto = schema.get_proto();
    assert_eq!(proto.get_scope_id(), SCHEMA_FILE_ID, "{name} file ID");
    assert_eq!(
        proto.get_display_name().unwrap().to_str().unwrap(),
        format!("moh.capnp:{name}"),
        "{name} declaration name"
    );

    let actual = schema
        .get_fields()
        .unwrap()
        .iter()
        .map(|field| {
            let proto = field.get_proto();
            let name = proto.get_name().unwrap().to_str().unwrap().to_owned();
            let ordinal = match proto.get_ordinal().which().unwrap() {
                capnp::schema_capnp::field::ordinal::Explicit(value) => value,
                capnp::schema_capnp::field::ordinal::Implicit(()) => {
                    panic!("{name} must have an explicit ordinal")
                }
            };
            let discriminant = (proto.get_discriminant_value()
                != capnp::schema_capnp::field::NO_DISCRIMINANT)
                .then(|| proto.get_discriminant_value());
            (name, ordinal, discriminant)
        })
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .map(|(name, ordinal, discriminant)| ((*name).to_owned(), *ordinal, *discriminant))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{name} field contract");
}

fn assert_enum_contract<T: Introspect>(name: &str, expected: &[&str]) {
    let TypeVariant::Enum(raw) = T::introspect().which() else {
        panic!("{name} is not generated as an enum");
    };
    let schema = capnp::schema::EnumSchema::from(raw);
    let proto = schema.get_proto();
    assert_eq!(proto.get_scope_id(), SCHEMA_FILE_ID, "{name} file ID");
    assert_eq!(
        proto.get_display_name().unwrap().to_str().unwrap(),
        format!("moh.capnp:{name}"),
        "{name} declaration name"
    );
    let actual = schema
        .get_enumerants()
        .unwrap()
        .iter()
        .map(|enumerant| {
            (
                enumerant
                    .get_proto()
                    .get_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                enumerant.get_ordinal(),
            )
        })
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .enumerate()
        .map(|(ordinal, name)| ((*name).to_owned(), u16::try_from(ordinal).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{name} enumerant contract");
}

fn assert_parameter_struct_contract<T: Introspect>(display_name: &str, expected_fields: &[&str]) {
    let TypeVariant::Struct(raw) = T::introspect().which() else {
        panic!("{display_name} is not generated as a struct");
    };
    let schema = capnp::schema::StructSchema::from(raw);
    let proto = schema.get_proto();
    assert_eq!(
        proto.get_display_name().unwrap().to_str().unwrap(),
        format!("moh.capnp:{display_name}"),
        "{display_name} declaration name"
    );
    let actual = schema
        .get_fields()
        .unwrap()
        .iter()
        .map(|field| {
            let proto = field.get_proto();
            let ordinal = match proto.get_ordinal().which().unwrap() {
                capnp::schema_capnp::field::ordinal::Explicit(value) => value,
                capnp::schema_capnp::field::ordinal::Implicit(()) => {
                    panic!("method fields must have generated explicit ordinals")
                }
            };
            (
                proto.get_name().unwrap().to_str().unwrap().to_owned(),
                ordinal,
            )
        })
        .collect::<Vec<_>>();
    let expected = expected_fields
        .iter()
        .enumerate()
        .map(|(index, name)| ((*name).to_owned(), u16::try_from(index).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{display_name} field contract");
}

struct RecordingClientHook {
    inner: Box<dyn ClientHook>,
    calls: CallLog,
}

impl ClientHook for RecordingClientHook {
    fn add_ref(&self) -> Box<dyn ClientHook> {
        Box::new(Self {
            inner: self.inner.add_ref(),
            calls: self.calls.clone(),
        })
    }

    fn new_call(
        &self,
        interface_id: u64,
        method_id: u16,
        size_hint: Option<capnp::MessageSize>,
    ) -> Request<capnp::any_pointer::Owned, capnp::any_pointer::Owned> {
        self.calls.borrow_mut().push((interface_id, method_id));
        self.inner.new_call(interface_id, method_id, size_hint)
    }

    fn call(
        &self,
        interface_id: u64,
        method_id: u16,
        params: Box<dyn ParamsHook>,
        results: Box<dyn ResultsHook>,
    ) -> Promise<(), capnp::Error> {
        self.calls.borrow_mut().push((interface_id, method_id));
        self.inner.call(interface_id, method_id, params, results)
    }

    fn get_brand(&self) -> usize {
        self.inner.get_brand()
    }

    fn get_ptr(&self) -> usize {
        self.inner.get_ptr()
    }

    fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
        self.inner.get_resolved()
    }

    fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, capnp::Error>> {
        self.inner.when_more_resolved()
    }

    fn when_resolved(&self) -> Promise<(), capnp::Error> {
        self.inner.when_resolved()
    }
}

fn recording_client<C: FromClientHook>(inner: C) -> (C, CallLog) {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let client = C::new(Box::new(RecordingClientHook {
        inner: inner.into_client_hook(),
        calls: calls.clone(),
    }));
    (client, calls)
}

fn at(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 27, hour, minute, second)
        .single()
        .unwrap()
}

fn summary(name: Option<&str>) -> SessionSummary {
    SessionSummary {
        id: SessionId::from_str("session-42").unwrap(),
        title: name.map_or_else(
            || moh::session::fallback_title(""),
            |value| SessionTitle::parse(value).unwrap(),
        ),
        title_revision: 17,
        cwd: vec![b'/', b't', b'm', b'p', b'/', 0xff, 0x80],
        cwd_display: "/tmp/��".into(),
        running_jobs: 2,
        running: true,
        busy: true,
        attached_clients: 3,
        last_activity: at(9, 10, 11),
    }
}

fn http_failure() -> RunFailureSnapshot {
    RunFailureSnapshot {
        stage: RunStage::ModelRequest,
        kind: RunFailureKind::HttpRejected { status: 429 },
        retryable: true,
        message: "request rejected".into(),
    }
}

fn job(completed: bool) -> JobSnapshotDto {
    JobSnapshotDto {
        id: "job-7".into(),
        kind: JobKind::Bash,
        state: if completed {
            JobState::Completed
        } else {
            JobState::Running
        },
        title: "compile".into(),
        started_at: at(9, 0, 0),
        completed_at: completed.then(|| at(9, 1, 2)),
        details: "exit 0".into(),
    }
}

fn complete_plan() -> Vec<PlanItem> {
    vec![
        PlanItem::parse("Inspect", PlanStatus::Pending).unwrap(),
        PlanItem::parse("Implement", PlanStatus::InProgress).unwrap(),
        PlanItem::parse("Verify", PlanStatus::Completed).unwrap(),
        PlanItem::parse("Unblock", PlanStatus::Blocked).unwrap(),
        PlanItem::parse("Defer", PlanStatus::Cancelled).unwrap(),
    ]
}

fn full_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        summary: summary(Some("protocol")),
        transcript: vec![
            TranscriptItem::User("hello".into()),
            TranscriptItem::Assistant("hi".into()),
            TranscriptItem::ToolStarted {
                run_id: 8,
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: json!({"path": "README.md", "line": 3}),
            },
            TranscriptItem::Failed {
                run_id: 9,
                failure: http_failure(),
            },
            TranscriptItem::Cancelled { run_id: 10 },
        ],
        active_run: Some(ActiveRunSnapshot {
            run_id: 11,
            prompt: "continue".into(),
            assistant_text: "working".into(),
        }),
        settings: SessionSettings {
            model: "gpt-5.6".into(),
            reasoning: ReasoningLevel::Max,
            context_tokens: 12_345,
        },
        catalog: ModelCatalogState::Ready(vec![ModelInfoDto {
            id: "gpt-5.6".into(),
            display_name: "GPT-5.6".into(),
            description: "frontier".into(),
            reasoning_efforts: ALL_REASONING.to_vec(),
            default_reasoning: Some(ReasoningLevel::Xhigh),
        }]),
        plan: complete_plan(),
        jobs: vec![job(true)],
        persistence_warning: Some("checkpoint pending".into()),
        sequence: 77,
        busy: true,
    }
}

fn draft_defaults() -> DraftDefaults {
    DraftDefaults {
        cwd: vec![b'/', b'w', b'o', b'r', b'k', b'/', 0xff],
        settings: SessionSettings {
            model: "draft-model".into(),
            reasoning: ReasoningLevel::Low,
            context_tokens: 0,
        },
        catalog: ModelCatalogState::Ready(vec![ModelInfoDto {
            id: "draft-model".into(),
            display_name: "Draft Model".into(),
            description: "available before materialization".into(),
            reasoning_efforts: vec![ReasoningLevel::Low],
            default_reasoning: Some(ReasoningLevel::Low),
        }]),
    }
}

#[test]
fn protocol_info_round_trip_uses_v2_version_and_preserves_current_features() {
    let expected = ProtocolInfo::v2(
        "instance-123".into(),
        vec!["store was rebuilt".into(), "old socket removed".into()],
    );
    assert_eq!(expected.major, 2);
    assert_eq!(expected.minor, 0);
    assert_eq!(expected.features, REQUIRED_FEATURES);
    assert_eq!(
        REQUIRED_FEATURES,
        [
            "backend.startup",
            "backend.materialize",
            "backend.openSession",
            "backend.listSessions",
            "backend.listSessions.all",
            "backend.renameSession",
            "backend.deleteSession",
            "backend.draftDefaults",
            "session.submit",
            "session.cancel",
            "session.selectModel",
            "session.selectReasoning",
            "session.listJobs",
            "session.cancelJob",
            "session.detach",
            "session.detach.attachedClients",
            "observer.publish",
        ]
    );
    assert_eq!(moh_capnp::PROTOCOL_MAJOR, 2);
    assert_eq!(moh_capnp::PROTOCOL_MINOR, 0);

    let mut message = Builder::new_default();
    write_protocol_info(message.init_root(), &expected).unwrap();
    let actual = read_protocol_info(message.get_root_as_reader().unwrap()).unwrap();
    assert_eq!(actual, expected);
}

struct DummyBackend;

impl moh_capnp::backend::Server for DummyBackend {}

struct DummyObserver;

impl moh_capnp::observer::Server for DummyObserver {}

#[test]
fn generated_schema_metadata_and_requests_pin_the_complete_v2_abi() {
    assert_struct_contract::<moh_capnp::protocol_info::Owned>(
        "ProtocolInfo",
        &[
            ("major", 0, None),
            ("minor", 1, None),
            ("instanceId", 2, None),
            ("startupWarnings", 3, None),
            ("features", 4, None),
        ],
    );
    assert_struct_contract::<moh_capnp::session_selector::Owned>(
        "SessionSelector",
        &[("id", 0, Some(0)), ("title", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::command_error::Owned>(
        "CommandError",
        &[("code", 0, None), ("message", 1, None), ("ids", 2, None)],
    );
    assert_struct_contract::<moh_capnp::draft_defaults::Owned>(
        "DraftDefaults",
        &[
            ("cwd", 0, None),
            ("settings", 1, None),
            ("catalog", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::draft_defaults_result::Owned>(
        "DraftDefaultsResult",
        &[("defaults", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::startup_result::Owned>(
        "StartupResult",
        &[
            ("draft", 0, Some(0)),
            ("attached", 1, Some(1)),
            ("error", 2, Some(2)),
        ],
    );
    assert_struct_contract::<moh_capnp::materialize_result::Owned>(
        "MaterializeResult",
        &[("success", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::materialize_success::Owned>(
        "MaterializeSuccess",
        &[
            ("session", 0, None),
            ("snapshot", 1, None),
            ("runId", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::open_result::Owned>(
        "OpenResult",
        &[("success", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::open_success::Owned>(
        "OpenSuccess",
        &[("session", 0, None), ("snapshot", 1, None)],
    );
    assert_struct_contract::<moh_capnp::session_list_result::Owned>(
        "SessionListResult",
        &[("sessions", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::submit_result::Owned>(
        "SubmitResult",
        &[("runId", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::command_result::Owned>(
        "CommandResult",
        &[
            ("ok", 0, Some(0)),
            ("error", 1, Some(1)),
            ("attachedClients", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::job_list_result::Owned>(
        "JobListResult",
        &[("jobs", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::job_result::Owned>(
        "JobResult",
        &[("job", 0, Some(0)), ("error", 1, Some(1))],
    );
    assert_struct_contract::<moh_capnp::session_settings::Owned>(
        "SessionSettings",
        &[
            ("model", 0, None),
            ("reasoning", 1, None),
            ("contextTokens", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::session_summary::Owned>(
        "SessionSummary",
        &[
            ("id", 0, None),
            ("title", 1, None),
            ("cwd", 2, None),
            ("cwdDisplay", 3, None),
            ("titleRevision", 4, None),
            ("busy", 5, None),
            ("attachedClients", 6, None),
            ("lastActivity", 7, None),
            ("running", 8, None),
            ("runningJobs", 9, None),
        ],
    );
    assert_struct_contract::<moh_capnp::active_run::Owned>(
        "ActiveRun",
        &[
            ("runId", 0, None),
            ("prompt", 1, None),
            ("assistantText", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::tool_started_record::Owned>(
        "ToolStartedRecord",
        &[
            ("runId", 0, None),
            ("callId", 1, None),
            ("name", 2, None),
            ("argumentsJson", 3, None),
        ],
    );
    assert_struct_contract::<moh_capnp::failed_record::Owned>(
        "FailedRecord",
        &[("runId", 0, None), ("failure", 1, None)],
    );
    assert_struct_contract::<moh_capnp::transcript_item::Owned>(
        "TranscriptItem",
        &[
            ("user", 0, Some(0)),
            ("assistant", 1, Some(1)),
            ("toolStarted", 2, Some(2)),
            ("failed", 3, Some(3)),
            ("cancelledRunId", 4, Some(4)),
        ],
    );
    assert_struct_contract::<moh_capnp::model_info::Owned>(
        "ModelInfo",
        &[
            ("id", 0, None),
            ("displayName", 1, None),
            ("description", 2, None),
            ("reasoningEfforts", 3, None),
            ("hasDefaultReasoning", 4, None),
            ("defaultReasoning", 5, None),
        ],
    );
    assert_struct_contract::<moh_capnp::model_catalog::Owned>(
        "ModelCatalog",
        &[
            ("loading", 0, Some(0)),
            ("ready", 1, Some(1)),
            ("failed", 2, Some(2)),
        ],
    );
    assert_struct_contract::<moh_capnp::job_snapshot::Owned>(
        "JobSnapshot",
        &[
            ("id", 0, None),
            ("kind", 1, None),
            ("state", 2, None),
            ("title", 3, None),
            ("startedAt", 4, None),
            ("completedAt", 5, None),
            ("details", 6, None),
        ],
    );
    assert_struct_contract::<moh_capnp::run_failure::Owned>(
        "RunFailure",
        &[
            ("stage", 0, None),
            ("kind", 1, None),
            ("hasHttpStatus", 2, None),
            ("httpStatus", 3, None),
            ("retryable", 4, None),
            ("message", 5, None),
        ],
    );
    assert_struct_contract::<moh_capnp::session_snapshot::Owned>(
        "SessionSnapshot",
        &[
            ("summary", 0, None),
            ("transcript", 1, None),
            ("activeRun", 2, None),
            ("settings", 3, None),
            ("catalog", 4, None),
            ("jobs", 5, None),
            ("persistenceWarning", 6, None),
            ("sequence", 7, None),
            ("busy", 8, None),
            ("plan", 9, None),
        ],
    );
    assert_enum_contract::<moh_capnp::PlanStatus>(
        "PlanStatus",
        &["pending", "inProgress", "completed", "blocked", "cancelled"],
    );
    assert_struct_contract::<moh_capnp::plan_item::Owned>(
        "PlanItem",
        &[("step", 0, None), ("status", 1, None)],
    );
    assert_struct_contract::<moh_capnp::run_started::Owned>(
        "RunStarted",
        &[("runId", 0, None), ("prompt", 1, None)],
    );
    assert_struct_contract::<moh_capnp::assistant_delta::Owned>(
        "AssistantDelta",
        &[("runId", 0, None), ("text", 1, None)],
    );
    assert_struct_contract::<moh_capnp::context_usage::Owned>(
        "ContextUsage",
        &[
            ("runId", 0, None),
            ("inputTokens", 1, None),
            ("lastActivity", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::tool_finished::Owned>(
        "ToolFinished",
        &[("runId", 0, None), ("callId", 1, None), ("name", 2, None)],
    );
    assert_struct_contract::<moh_capnp::run_completed::Owned>(
        "RunCompleted",
        &[
            ("runId", 0, None),
            ("response", 1, None),
            ("lastActivity", 2, None),
        ],
    );
    assert_struct_contract::<moh_capnp::run_failed::Owned>(
        "RunFailed",
        &[("runId", 0, None), ("failure", 1, None)],
    );
    assert_struct_contract::<moh_capnp::settings_changed::Owned>(
        "SettingsChanged",
        &[("settings", 0, None), ("lastActivity", 1, None)],
    );
    assert_struct_contract::<moh_capnp::title_changed::Owned>(
        "TitleChanged",
        &[("title", 0, None), ("titleRevision", 1, None)],
    );
    assert_struct_contract::<moh_capnp::session_deleted::Owned>(
        "SessionDeleted",
        &[("sessionId", 0, None)],
    );
    assert_struct_contract::<moh_capnp::event_envelope::Owned>(
        "EventEnvelope",
        &[
            ("sequence", 0, None),
            ("started", 1, Some(0)),
            ("assistantDelta", 2, Some(1)),
            ("contextUsage", 3, Some(2)),
            ("toolStarted", 4, Some(3)),
            ("toolFinished", 5, Some(4)),
            ("completed", 6, Some(5)),
            ("failed", 7, Some(6)),
            ("cancelledRunId", 8, Some(7)),
            ("settingsChanged", 9, Some(8)),
            ("jobsChanged", 10, Some(9)),
            ("catalogChanged", 11, Some(10)),
            ("persistenceWarning", 12, Some(11)),
            ("titleChanged", 13, Some(12)),
            ("deleted", 14, Some(13)),
            ("planChanged", 15, Some(14)),
        ],
    );

    assert_enum_contract::<moh_capnp::ErrorCode>(
        "ErrorCode",
        &[
            "busy",
            "notRunning",
            "sessionNotFound",
            "sessionNameConflict",
            "invalidArgument",
            "modelNotFound",
            "unsupportedReasoning",
            "jobNotFound",
            "backendStarting",
            "backendUnavailable",
            "persistence",
            "internal",
            "ambiguousTitle",
            "sessionDeleting",
            "sessionDeleted",
        ],
    );
    assert_enum_contract::<moh_capnp::SessionListScope>("SessionListScope", &["project", "all"]);
    assert_enum_contract::<moh_capnp::ReasoningLevel>(
        "ReasoningLevel",
        &["none", "minimal", "low", "medium", "high", "xhigh", "max"],
    );
    assert_enum_contract::<moh_capnp::JobKind>("JobKind", &["bash"]);
    assert_enum_contract::<moh_capnp::JobState>(
        "JobState",
        &["running", "completed", "failed", "cancelled"],
    );
    assert_enum_contract::<moh_capnp::RunStage>(
        "RunStage",
        &["startup", "modelRequest", "toolExecution", "finalization"],
    );
    assert_enum_contract::<moh_capnp::RunFailureKind>(
        "RunFailureKind",
        &[
            "authentication",
            "transport",
            "httpRejected",
            "protocol",
            "emptyResponse",
            "budgetExhausted",
            "runtimeInfrastructure",
            "toolInfrastructure",
        ],
    );

    const BACKEND_ID: u64 = 0x82d4_4a14_ae61_4125;
    assert_parameter_struct_contract::<moh_capnp::backend::get_info_params::Owned>(
        "Backend.getInfo$Params",
        &[],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::get_info_results::Owned>(
        "Backend.getInfo$Results",
        &["info"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::startup_params::Owned>(
        "Backend.startup$Params",
        &["cwd", "attachmentId", "observer"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::startup_results::Owned>(
        "Backend.startup$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::materialize_params::Owned>(
        "Backend.materialize$Params",
        &["cwd", "prompt", "settings", "attachmentId", "observer"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::materialize_results::Owned>(
        "Backend.materialize$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::open_session_params::Owned>(
        "Backend.openSession$Params",
        &["selector", "cwdForTitle", "attachmentId", "observer"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::open_session_results::Owned>(
        "Backend.openSession$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::list_sessions_params::Owned>(
        "Backend.listSessions$Params",
        &["scope", "cwd"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::list_sessions_results::Owned>(
        "Backend.listSessions$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::rename_session_params::Owned>(
        "Backend.renameSession$Params",
        &["id", "title"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::rename_session_results::Owned>(
        "Backend.renameSession$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::delete_session_params::Owned>(
        "Backend.deleteSession$Params",
        &["id"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::delete_session_results::Owned>(
        "Backend.deleteSession$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::draft_defaults_params::Owned>(
        "Backend.draftDefaults$Params",
        &["cwd"],
    );
    assert_parameter_struct_contract::<moh_capnp::backend::draft_defaults_results::Owned>(
        "Backend.draftDefaults$Results",
        &["result"],
    );

    const SESSION_ID: u64 = 0xd0eb_aa8e_b0be_8606;
    assert_parameter_struct_contract::<moh_capnp::session::submit_params::Owned>(
        "Session.submit$Params",
        &["prompt"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::submit_results::Owned>(
        "Session.submit$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::cancel_params::Owned>(
        "Session.cancel$Params",
        &[],
    );
    assert_parameter_struct_contract::<moh_capnp::session::cancel_results::Owned>(
        "Session.cancel$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::select_model_params::Owned>(
        "Session.selectModel$Params",
        &["modelId"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::select_model_results::Owned>(
        "Session.selectModel$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::select_reasoning_params::Owned>(
        "Session.selectReasoning$Params",
        &["level"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::select_reasoning_results::Owned>(
        "Session.selectReasoning$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::list_jobs_params::Owned>(
        "Session.listJobs$Params",
        &[],
    );
    assert_parameter_struct_contract::<moh_capnp::session::list_jobs_results::Owned>(
        "Session.listJobs$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::cancel_job_params::Owned>(
        "Session.cancelJob$Params",
        &["jobId"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::cancel_job_results::Owned>(
        "Session.cancelJob$Results",
        &["result"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::detach_params::Owned>(
        "Session.detach$Params",
        &["attachmentId"],
    );
    assert_parameter_struct_contract::<moh_capnp::session::detach_results::Owned>(
        "Session.detach$Results",
        &["result"],
    );

    const OBSERVER_ID: u64 = 0xd533_0c06_8004_4e4f;
    assert_parameter_struct_contract::<moh_capnp::observer::publish_params::Owned>(
        "Observer.publish$Params",
        &["event"],
    );
    assert_parameter_struct_contract::<moh_capnp::observer::publish_results::Owned>(
        "Observer.publish$Results",
        &[],
    );

    let inner: moh_capnp::backend::Client = capnp_rpc::new_client(DummyBackend);
    let (backend, calls) = recording_client(inner);
    drop(backend.get_info_request());
    drop(backend.startup_request());
    drop(backend.materialize_request());
    drop(backend.open_session_request());
    drop(backend.list_sessions_request());
    drop(backend.rename_session_request());
    drop(backend.delete_session_request());
    drop(backend.draft_defaults_request());
    assert_eq!(
        *calls.borrow(),
        [
            (BACKEND_ID, 0),
            (BACKEND_ID, 1),
            (BACKEND_ID, 2),
            (BACKEND_ID, 3),
            (BACKEND_ID, 4),
            (BACKEND_ID, 5),
            (BACKEND_ID, 6),
            (BACKEND_ID, 7),
        ]
    );

    let inner: moh_capnp::session::Client = capnp_rpc::new_client(DummySession);
    let (session, calls) = recording_client(inner);
    drop(session.submit_request());
    drop(session.cancel_request());
    drop(session.select_model_request());
    drop(session.select_reasoning_request());
    drop(session.list_jobs_request());
    drop(session.cancel_job_request());
    drop(session.detach_request());
    assert_eq!(
        *calls.borrow(),
        [
            (SESSION_ID, 0),
            (SESSION_ID, 1),
            (SESSION_ID, 2),
            (SESSION_ID, 3),
            (SESSION_ID, 4),
            (SESSION_ID, 5),
            (SESSION_ID, 6),
        ]
    );

    let inner: moh_capnp::observer::Client = capnp_rpc::new_client(DummyObserver);
    let (observer, calls) = recording_client(inner);
    drop(observer.publish_request());
    assert_eq!(*calls.borrow(), [(OBSERVER_ID, 0)]);
}

#[test]
fn wire_length_validators_reject_capnp_boundaries_without_allocating_payloads() {
    let limit = 1_usize << 29;

    assert!(validate_wire_text_length(limit - 2, "text").is_ok());
    assert!(matches!(
        validate_wire_text_length(limit - 1, "text"),
        Err(RpcConversionError::FieldTooLong { field: "text" })
    ));
    assert!(matches!(
        validate_wire_text_length(usize::MAX, "text"),
        Err(RpcConversionError::FieldTooLong { field: "text" })
    ));

    assert!(validate_wire_data_length(limit - 1, "data").is_ok());
    assert!(matches!(
        validate_wire_data_length(limit, "data"),
        Err(RpcConversionError::FieldTooLong { field: "data" })
    ));

    assert_eq!(
        validate_wire_list_length(limit - 1, "pointers").unwrap(),
        u32::try_from(limit - 1).unwrap()
    );
    assert!(matches!(
        validate_wire_list_length(limit, "pointers"),
        Err(RpcConversionError::ListTooLong { field: "pointers" })
    ));

    let max_three_word_elements = (limit - 1) / 3;
    assert_eq!(
        validate_wire_inline_composite_list_length(max_three_word_elements, 3, "structs").unwrap(),
        u32::try_from(max_three_word_elements).unwrap()
    );
    assert!(matches!(
        validate_wire_inline_composite_list_length(max_three_word_elements + 1, 3, "structs"),
        Err(RpcConversionError::ListTooLong { field: "structs" })
    ));
    assert!(matches!(
        validate_wire_inline_composite_list_length(usize::MAX, 2, "structs"),
        Err(RpcConversionError::ListTooLong { field: "structs" })
    ));
}

#[test]
fn application_inbound_limits_accept_boundaries_and_reject_one_byte_over() {
    for (limit, field) in [
        (MAX_RPC_CWD_BYTES, "cwd"),
        (MAX_RPC_PROMPT_BYTES, "prompt"),
        (MAX_RPC_TITLE_BYTES, "title"),
        (MAX_RPC_IDENTIFIER_BYTES, "modelId"),
        (MAX_RPC_IDENTIFIER_BYTES, "jobId"),
    ] {
        assert!(validate_inbound_field_length(limit, limit, field).is_ok());
        assert!(matches!(
            validate_inbound_field_length(limit + 1, limit, field),
            Err(RpcConversionError::FieldTooLong { field: actual }) if actual == field
        ));
    }
}

#[test]
fn selectors_settings_and_every_reasoning_level_round_trip() {
    for selector in [
        SessionSelector::Id(SessionId::from_str("session-9").unwrap()),
        SessionSelector::Title(SessionTitle::parse("named").unwrap()),
    ] {
        let mut message = Builder::new_default();
        write_session_selector(message.init_root(), &selector).unwrap();
        assert_eq!(
            read_session_selector(message.get_root_as_reader().unwrap()).unwrap(),
            selector
        );
    }

    for reasoning in ALL_REASONING {
        let expected = SessionSettings {
            model: "model".into(),
            reasoning,
            context_tokens: u64::MAX,
        };
        let mut message = Builder::new_default();
        write_session_settings(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_session_settings(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }
}

#[test]
fn draft_defaults_and_scoped_listing_preserve_raw_cwd_bytes() {
    let expected = draft_defaults();
    let mut message = Builder::new_default();
    write_draft_defaults(message.init_root(), &expected).unwrap();
    assert_eq!(
        read_draft_defaults(message.get_root_as_reader().unwrap()).unwrap(),
        expected
    );

    for expected in [
        SessionListScope::Project(vec![b'/', b'w', b'o', b'r', b'k', b'/', 0xff, 0x80]),
        SessionListScope::All,
    ] {
        let (scope, cwd) = write_session_list_scope(&expected).unwrap();
        let actual = read_session_list_scope(Ok(scope), Ok(cwd)).unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn attachment_ids_round_trip_exactly_and_reject_zero() {
    let expected = AttachmentId(u64::MAX);
    let encoded = write_attachment_id(expected).unwrap();
    assert_eq!(encoded, u64::MAX);
    assert_eq!(read_attachment_id(encoded).unwrap(), expected);

    assert!(matches!(
        write_attachment_id(AttachmentId(0)),
        Err(RpcConversionError::InvalidAttachmentId)
    ));
    assert!(matches!(
        read_attachment_id(0),
        Err(RpcConversionError::InvalidAttachmentId)
    ));
}

#[test]
fn full_snapshot_round_trip_preserves_domain_values_and_non_utf8_cwd() {
    let expected = full_snapshot();
    let mut message = Builder::new_default();
    write_session_snapshot(message.init_root(), &expected).unwrap();
    let actual = read_session_snapshot(message.get_root_as_reader().unwrap()).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.summary.cwd,
        [b'/', b't', b'm', b'p', b'/', 0xff, 0x80]
    );
}

#[test]
fn null_optional_wire_fields_round_trip_and_titles_remain_nonempty() {
    let expected = SessionSnapshot {
        summary: summary(None),
        transcript: vec![],
        active_run: None,
        settings: SessionSettings {
            model: "model".into(),
            reasoning: ReasoningLevel::None,
            context_tokens: 0,
        },
        catalog: ModelCatalogState::Ready(vec![ModelInfoDto {
            id: "model".into(),
            display_name: "Model".into(),
            description: String::new(),
            reasoning_efforts: vec![ReasoningLevel::None],
            default_reasoning: None,
        }]),
        plan: Vec::new(),
        jobs: vec![job(false)],
        persistence_warning: None,
        sequence: 0,
        busy: false,
    };

    let mut message = Builder::new_default();
    write_session_snapshot(message.init_root(), &expected).unwrap();
    let wire: moh_capnp::session_snapshot::Reader<'_> = message.get_root_as_reader().unwrap();
    assert_eq!(
        wire.get_summary().unwrap().get_title().unwrap(),
        "Untitled session"
    );
    assert!(!wire.has_active_run());
    assert_eq!(wire.get_persistence_warning().unwrap(), "");
    let wire_model = match wire.get_catalog().unwrap().which().unwrap() {
        moh_capnp::model_catalog::Ready(models) => models.unwrap().get(0),
        _ => panic!("expected ready catalog"),
    };
    assert!(!wire_model.get_has_default_reasoning());
    assert_eq!(
        wire.get_jobs().unwrap().get(0).get_completed_at().unwrap(),
        ""
    );

    assert_eq!(read_session_snapshot(wire).unwrap(), expected);

    let failure = http_failure();
    let mut message = Builder::new_default();
    write_run_failure(message.init_root(), &failure).unwrap();
    let wire: moh_capnp::run_failure::Reader<'_> = message.get_root_as_reader().unwrap();
    assert!(wire.get_has_http_status());
    assert_eq!(wire.get_http_status(), 429);
    assert_eq!(read_run_failure(wire).unwrap(), failure);
}

#[test]
fn every_event_variant_round_trips_with_activity_timestamps() {
    let events = vec![
        SessionEvent::TitleChanged {
            title: SessionTitle::parse("Renamed session").unwrap(),
            title_revision: 18,
        },
        SessionEvent::Started {
            run_id: 1,
            prompt: "start".into(),
        },
        SessionEvent::AssistantDelta {
            run_id: 1,
            text: "delta".into(),
        },
        SessionEvent::ContextUsage {
            run_id: 1,
            input_tokens: 456,
            last_activity: at(10, 0, 1),
        },
        SessionEvent::ToolStarted {
            run_id: 1,
            call_id: "call".into(),
            name: "bash".into(),
            arguments: json!({"command": "pwd"}),
        },
        SessionEvent::ToolFinished {
            run_id: 1,
            call_id: "call".into(),
            name: "bash".into(),
        },
        SessionEvent::Completed {
            run_id: 1,
            response: "done".into(),
            last_activity: at(10, 0, 2),
        },
        SessionEvent::Failed {
            run_id: 1,
            failure: http_failure(),
        },
        SessionEvent::Cancelled { run_id: 1 },
        SessionEvent::SettingsChanged {
            settings: SessionSettings {
                model: "new-model".into(),
                reasoning: ReasoningLevel::High,
                context_tokens: 99,
            },
            last_activity: at(10, 0, 3),
        },
        SessionEvent::JobsChanged(vec![job(true)]),
        SessionEvent::CatalogChanged(ModelCatalogState::Failed("offline".into())),
        SessionEvent::PersistenceWarning(None),
        SessionEvent::PlanChanged(complete_plan()),
        SessionEvent::Deleted {
            session_id: SessionId::from_str("session-42").unwrap(),
        },
    ];

    for (index, event) in events.into_iter().enumerate() {
        let expected = SessionEventEnvelope {
            sequence: u64::try_from(index + 1).unwrap(),
            event,
        };
        let mut message = Builder::new_default();
        write_event_envelope(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_event_envelope(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }
}

#[test]
fn invalid_plan_text_is_rejected_during_snapshot_decode() {
    let mut message = Builder::new_default();
    write_session_snapshot(message.init_root(), &full_snapshot()).unwrap();
    let mut snapshot: moh_capnp::session_snapshot::Builder<'_> = message.get_root().unwrap();
    snapshot
        .reborrow()
        .get_plan()
        .unwrap()
        .get(0)
        .set_step(" Inspect");

    let error = read_session_snapshot(message.get_root_as_reader().unwrap()).unwrap_err();

    assert!(matches!(error, RpcConversionError::InvalidPlanItem));
}

struct DummySession;

impl moh_capnp::session::Server for DummySession {}

#[test]
fn every_result_union_round_trips_success_and_error_branches() {
    let error = CommandError {
        code: ErrorCode::BackendUnavailable,
        message: "backend unavailable".into(),
        ids: Vec::new(),
    };

    let client: moh_capnp::session::Client = capnp_rpc::new_client(DummySession);
    let open = OpenResult::Ok(OpenSuccess {
        session: client,
        snapshot: full_snapshot(),
    });
    let mut message = Builder::new_default();
    let mut cap_table = Vec::new();
    {
        let mut root: moh_capnp::open_result::Builder<'_> = message.init_root();
        root.imbue_mut(&mut cap_table);
        write_open_result(root, &open).unwrap();
    }
    let mut root: moh_capnp::open_result::Reader<'_> = message.get_root_as_reader().unwrap();
    root.imbue(&cap_table);
    let decoded = read_open_result(root).unwrap();
    let decoded = decoded.unwrap();
    assert_eq!(decoded.snapshot, full_snapshot());

    let startup_draft = StartupResult::Ok(StartupSuccess::Draft(draft_defaults()));
    let mut message = Builder::new_default();
    write_startup_result(message.init_root(), &startup_draft).unwrap();
    let StartupSuccess::Draft(actual) = read_startup_result(message.get_root_as_reader().unwrap())
        .unwrap()
        .unwrap()
    else {
        panic!("expected draft startup result");
    };
    assert_eq!(actual, draft_defaults());

    for expected in [
        DraftDefaultsResult::Ok(draft_defaults()),
        DraftDefaultsResult::Err(error.clone()),
    ] {
        let mut message = Builder::new_default();
        write_draft_defaults_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_draft_defaults_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }

    let client: moh_capnp::session::Client = capnp_rpc::new_client(DummySession);
    let startup_attached = StartupResult::Ok(StartupSuccess::Attached(Box::new(OpenSuccess {
        session: client,
        snapshot: full_snapshot(),
    })));
    let mut message = Builder::new_default();
    let mut cap_table = Vec::new();
    {
        let mut root: moh_capnp::startup_result::Builder<'_> = message.init_root();
        root.imbue_mut(&mut cap_table);
        write_startup_result(root, &startup_attached).unwrap();
    }
    let mut root: moh_capnp::startup_result::Reader<'_> = message.get_root_as_reader().unwrap();
    root.imbue(&cap_table);
    let StartupSuccess::Attached(decoded) = read_startup_result(root).unwrap().unwrap() else {
        panic!("expected attached startup result");
    };
    assert_eq!(decoded.snapshot, full_snapshot());

    let mut message = Builder::new_default();
    write_startup_result(message.init_root(), &StartupResult::Err(error.clone())).unwrap();
    assert_eq!(
        read_startup_result(message.get_root_as_reader().unwrap())
            .unwrap()
            .unwrap_err(),
        error
    );

    let client: moh_capnp::session::Client = capnp_rpc::new_client(DummySession);
    let materialized = MaterializeResult::Ok(MaterializeSuccess {
        session: client,
        snapshot: full_snapshot(),
        run_id: u64::MAX,
    });
    let mut message = Builder::new_default();
    let mut cap_table = Vec::new();
    {
        let mut root: moh_capnp::materialize_result::Builder<'_> = message.init_root();
        root.imbue_mut(&mut cap_table);
        write_materialize_result(root, &materialized).unwrap();
    }
    let mut root: moh_capnp::materialize_result::Reader<'_> = message.get_root_as_reader().unwrap();
    root.imbue(&cap_table);
    let decoded = read_materialize_result(root).unwrap().unwrap();
    assert_eq!(decoded.snapshot, full_snapshot());
    assert_eq!(decoded.run_id, u64::MAX);

    let mut message = Builder::new_default();
    write_materialize_result(message.init_root(), &MaterializeResult::Err(error.clone())).unwrap();
    assert_eq!(
        read_materialize_result(message.get_root_as_reader().unwrap())
            .unwrap()
            .unwrap_err(),
        error
    );

    let mut message = Builder::new_default();
    write_open_result(message.init_root(), &OpenResult::Err(error.clone())).unwrap();
    assert_eq!(
        read_open_result(message.get_root_as_reader().unwrap())
            .unwrap()
            .unwrap_err(),
        error
    );

    let session_lists = [Ok(vec![summary(Some("listed"))]), Err(error.clone())];
    for expected in session_lists {
        let mut message = Builder::new_default();
        write_session_list_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_session_list_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }

    let submits = [Ok(u64::MAX), Err(error.clone())];
    for expected in submits {
        let mut message = Builder::new_default();
        write_submit_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_submit_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }

    let commands = [Ok(()), Err(error.clone())];
    for expected in commands {
        let mut message = Builder::new_default();
        write_command_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_command_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }

    let detaches = [DetachResult::Ok(7), DetachResult::Err(error.clone())];
    for expected in detaches {
        let mut message = Builder::new_default();
        write_detach_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_detach_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }

    let job_lists = [Ok(vec![job(true)]), Err(error.clone())];
    for expected in job_lists {
        let mut message = Builder::new_default();
        write_job_list_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_job_list_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }

    let jobs = [Ok(job(true)), Err(error)];
    for expected in jobs {
        let mut message = Builder::new_default();
        write_job_result(message.init_root(), &expected).unwrap();
        assert_eq!(
            read_job_result(message.get_root_as_reader().unwrap()).unwrap(),
            expected
        );
    }
}

#[test]
fn ambiguous_title_errors_round_trip_exact_matching_ids() {
    let expected = CommandError {
        code: ErrorCode::AmbiguousTitle,
        message: "title matches more than one session".into(),
        ids: vec![
            SessionId::from_str("session-2").unwrap(),
            SessionId::from_str("session-9").unwrap(),
        ],
    };
    let mut message = Builder::new_default();
    write_command_result(message.init_root(), &Err(expected.clone())).unwrap();
    assert_eq!(
        read_command_result(message.get_root_as_reader().unwrap())
            .unwrap()
            .unwrap_err(),
        expected
    );

    for code in [ErrorCode::SessionDeleting, ErrorCode::SessionDeleted] {
        let expected = CommandError {
            code,
            message: "typed lifecycle state".into(),
            ids: Vec::new(),
        };
        let mut message = Builder::new_default();
        write_command_result(message.init_root(), &Err(expected.clone())).unwrap();
        assert_eq!(
            read_command_result(message.get_root_as_reader().unwrap())
                .unwrap()
                .unwrap_err(),
            expected
        );
    }
}

#[test]
fn actor_command_errors_map_to_stable_wire_categories() {
    let cases = [
        (
            SessionCommandError::Reported {
                code: ErrorCode::Busy,
                message: "reported busy".into(),
            },
            ErrorCode::Busy,
        ),
        (SessionCommandError::Busy, ErrorCode::Busy),
        (SessionCommandError::NotRunning, ErrorCode::NotRunning),
        (
            SessionCommandError::ModelNotFound { model: "x".into() },
            ErrorCode::ModelNotFound,
        ),
        (
            SessionCommandError::UnsupportedReasoning {
                model: "x".into(),
                reasoning: "max",
            },
            ErrorCode::UnsupportedReasoning,
        ),
        (
            SessionCommandError::InvalidJobId { id: "bad".into() },
            ErrorCode::InvalidArgument,
        ),
        (
            SessionCommandError::JobNotFound {
                id: "job-99".into(),
            },
            ErrorCode::JobNotFound,
        ),
        (
            SessionCommandError::Persistence {
                message: "disk full".into(),
            },
            ErrorCode::Persistence,
        ),
        (SessionCommandError::Deleting, ErrorCode::SessionDeleting),
        (
            SessionCommandError::Unavailable,
            ErrorCode::BackendUnavailable,
        ),
        (SessionCommandError::RunIdExhausted, ErrorCode::Internal),
        (
            SessionCommandError::Job {
                message: "registry failed".into(),
            },
            ErrorCode::Internal,
        ),
        (
            SessionCommandError::Projection {
                message: "projection rejected event".into(),
            },
            ErrorCode::Internal,
        ),
    ];

    for (source, expected_code) in cases {
        let error = CommandError::from(&source);
        assert_eq!(error.code, expected_code);
        assert_eq!(error.message, source.to_string());
    }
}

#[test]
fn malformed_json_and_timestamps_return_typed_errors() {
    let mut message = Builder::new_default();
    let mut item: moh_capnp::transcript_item::Builder<'_> = message.init_root();
    let mut tool = item.reborrow().init_tool_started();
    tool.set_run_id(1);
    tool.set_call_id("call");
    tool.set_name("read");
    tool.set_arguments_json("{");
    let error =
        moh::rpc::convert::read_transcript_item(message.get_root_as_reader().unwrap()).unwrap_err();
    assert!(matches!(error, RpcConversionError::InvalidToolArguments));

    let mut message = Builder::new_default();
    let mut wire: moh_capnp::session_summary::Builder<'_> = message.init_root();
    wire.set_id("session-1");
    wire.set_title("valid");
    wire.set_cwd(b"/tmp");
    wire.set_cwd_display("/tmp");
    wire.set_last_activity("not-a-timestamp");
    let error =
        moh::rpc::convert::read_session_summary(message.get_root_as_reader().unwrap()).unwrap_err();
    assert!(matches!(
        error,
        RpcConversionError::InvalidTimestamp {
            field: "lastActivity"
        }
    ));

    let mut message = Builder::new_default();
    let mut wire: moh_capnp::session_summary::Builder<'_> = message.init_root();
    wire.set_id("session-1");
    wire.set_title("valid");
    wire.set_cwd(b"/tmp");
    wire.set_cwd_display("/tmp");
    wire.set_last_activity("2026-08-27T10:00:00+02:00");
    let error =
        moh::rpc::convert::read_session_summary(message.get_root_as_reader().unwrap()).unwrap_err();
    assert!(matches!(error, RpcConversionError::InvalidTimestamp { .. }));

    let mut message = Builder::new_default();
    let mut wire: moh_capnp::run_failure::Builder<'_> = message.init_root();
    wire.set_stage(moh_capnp::RunStage::ModelRequest);
    wire.set_kind(moh_capnp::RunFailureKind::HttpRejected);
    wire.set_has_http_status(false);
    wire.set_message("missing status");
    let error = read_run_failure(message.get_root_as_reader().unwrap()).unwrap_err();
    assert!(matches!(error, RpcConversionError::InvalidHttpStatusGuard));

    for malformed in ["", " surrounding ", &"x".repeat(65)] {
        let mut message = Builder::new_default();
        let mut wire: moh_capnp::session_summary::Builder<'_> = message.init_root();
        wire.set_id("session-1");
        wire.set_title(malformed);
        wire.set_cwd(b"/tmp");
        wire.set_cwd_display("/tmp");
        wire.set_last_activity("2026-08-27T10:00:00Z");
        let error = moh::rpc::convert::read_session_summary(message.get_root_as_reader().unwrap())
            .unwrap_err();
        assert!(matches!(error, RpcConversionError::InvalidSessionTitle));
    }
}

#[test]
fn future_enum_and_union_values_return_typed_errors_without_panicking() {
    let mut message = Builder::new_default();
    let mut settings: moh_capnp::session_settings::Builder<'_> = message.init_root();
    settings.set_model("model");
    settings.set_reasoning(moh_capnp::ReasoningLevel::Medium);
    let mut bytes = capnp::serialize::write_message_to_words(&message);
    bytes[16..18].copy_from_slice(&99_u16.to_le_bytes());
    let mut bytes = bytes.as_slice();
    let reader = capnp::serialize::read_message_from_flat_slice(
        &mut bytes,
        capnp::message::ReaderOptions::new(),
    )
    .unwrap();
    let error = read_session_settings(reader.get_root().unwrap()).unwrap_err();
    assert!(matches!(
        error,
        RpcConversionError::UnknownEnum {
            name: "ReasoningLevel",
            value: 99
        }
    ));

    let mut message = Builder::new_default();
    let mut event: moh_capnp::event_envelope::Builder<'_> = message.init_root();
    event.set_sequence(1);
    event.init_started().set_run_id(1);
    let mut bytes = capnp::serialize::write_message_to_words(&message);
    bytes[24..26].copy_from_slice(&99_u16.to_le_bytes());
    let mut bytes = bytes.as_slice();
    let reader = capnp::serialize::read_message_from_flat_slice(
        &mut bytes,
        capnp::message::ReaderOptions::new(),
    )
    .unwrap();
    let error = read_event_envelope(reader.get_root().unwrap()).unwrap_err();
    assert!(matches!(
        error,
        RpcConversionError::UnknownUnion {
            name: "EventEnvelope",
            value: 99
        }
    ));
}
