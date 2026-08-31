//! Checked conversions between generated wire values and transport-neutral domain values.

use std::{fmt, str::FromStr};

use capnp::NotInSchema;
use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use thiserror::Error;

use crate::{
    harness::{RunFailureKind, RunStage},
    moh_capnp as wire,
    runtime::rig::ReasoningLevel,
    session::{
        ActiveRunSnapshot, AttachmentId, DraftDefaults, JobSnapshotDto, ModelCatalogState,
        ModelInfoDto, PlanItem, PlanStatus, RunFailureSnapshot, SessionCommandError, SessionEvent,
        SessionEventEnvelope, SessionId, SessionListScope, SessionSelector, SessionSettings,
        SessionSnapshot, SessionSummary, SessionTitle, TranscriptItem,
    },
    tools::{JobKind, JobState},
};

pub use crate::session::ErrorCode;

/// RPC method and additive lifecycle capabilities required by protocol version 2.0.
pub const REQUIRED_FEATURES: [&str; 17] = [
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
];

/// Server metadata returned by the version-negotiation method.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolInfo {
    /// Protocol major version.
    pub major: u16,
    /// Protocol minor version.
    pub minor: u16,
    /// Stable identifier for this backend process.
    pub instance_id: String,
    /// Sanitized non-fatal startup diagnostics.
    pub startup_warnings: Vec<String>,
    /// Method features implemented by this backend.
    pub features: Vec<String>,
}

impl ProtocolInfo {
    /// Constructs the exact metadata advertised by a protocol 2.0 backend.
    pub fn v2(instance_id: String, startup_warnings: Vec<String>) -> Self {
        Self {
            major: wire::PROTOCOL_MAJOR,
            minor: wire::PROTOCOL_MINOR,
            instance_id,
            startup_warnings,
            features: REQUIRED_FEATURES
                .iter()
                .map(|feature| (*feature).into())
                .collect(),
        }
    }

    /// Transitional constructor retained until the protocol-v2 transport cutover.
    #[doc(hidden)]
    pub fn v1(instance_id: String, startup_warnings: Vec<String>) -> Self {
        Self::v2(instance_id, startup_warnings)
    }
}

/// Transport-safe command failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandError {
    /// Stable machine-readable category.
    pub code: ErrorCode,
    /// Sanitized human-readable description.
    pub message: String,
    /// Stable identifiers matching an ambiguous title, empty for other failures.
    pub ids: Vec<SessionId>,
}

impl From<&SessionCommandError> for CommandError {
    fn from(error: &SessionCommandError) -> Self {
        if let SessionCommandError::Reported { code, message } = error {
            return Self {
                code: *code,
                message: message.clone(),
                ids: Vec::new(),
            };
        }
        let code = match error {
            SessionCommandError::Reported { .. } => unreachable!("handled above"),
            SessionCommandError::Busy => ErrorCode::Busy,
            SessionCommandError::NotRunning => ErrorCode::NotRunning,
            SessionCommandError::ModelNotFound { .. } => ErrorCode::ModelNotFound,
            SessionCommandError::UnsupportedReasoning { .. } => ErrorCode::UnsupportedReasoning,
            SessionCommandError::InvalidJobId { .. } | SessionCommandError::InvalidPrompt => {
                ErrorCode::InvalidArgument
            }
            SessionCommandError::JobNotFound { .. } => ErrorCode::JobNotFound,
            SessionCommandError::Persistence { .. } => ErrorCode::Persistence,
            SessionCommandError::Deleting => ErrorCode::SessionDeleting,
            SessionCommandError::Unavailable => ErrorCode::BackendUnavailable,
            SessionCommandError::RunIdExhausted
            | SessionCommandError::Job { .. }
            | SessionCommandError::Projection { .. } => ErrorCode::Internal,
        };
        Self {
            code,
            message: error.to_string(),
            ids: Vec::new(),
        }
    }
}

/// A successfully opened session capability and its atomic attachment snapshot.
pub struct OpenSuccess {
    /// Session command capability.
    pub session: wire::session::Client,
    /// State at the attachment sequence.
    pub snapshot: SessionSnapshot,
}

impl fmt::Debug for OpenSuccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenSuccess")
            .field("session", &"<capability>")
            .field("snapshot", &self.snapshot)
            .finish()
    }
}

/// Result of opening or creating a session.
pub type OpenResult = Result<OpenSuccess, CommandError>;
/// Successful startup selection before transport-specific client wrapping.
#[derive(Debug)]
pub enum StartupSuccess {
    /// No running project session exists, so the client remains local-only.
    Draft(DraftDefaults),
    /// The newest running project session was attached.
    Attached(Box<OpenSuccess>),
}
/// Result of selecting startup state.
pub type StartupResult = Result<StartupSuccess, CommandError>;
/// Result of obtaining fresh nonselecting draft defaults.
pub type DraftDefaultsResult = Result<DraftDefaults, CommandError>;
/// A newly materialized session capability and its assigned first run.
pub struct MaterializeSuccess {
    /// Session command capability.
    pub session: wire::session::Client,
    /// State at the attachment sequence.
    pub snapshot: SessionSnapshot,
    /// Harness run identifier assigned to the first prompt.
    pub run_id: u64,
}

impl fmt::Debug for MaterializeSuccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializeSuccess")
            .field("session", &"<capability>")
            .field("snapshot", &self.snapshot)
            .field("run_id", &self.run_id)
            .finish()
    }
}

/// Result of materializing a first prompt.
pub type MaterializeResult = Result<MaterializeSuccess, CommandError>;
/// Result of listing sessions.
pub type SessionListResult = Result<Vec<SessionSummary>, CommandError>;
/// Result of submitting a prompt.
pub type SubmitResult = Result<u64, CommandError>;
/// Result of a command with no success payload.
pub type CommandResult = Result<(), CommandError>;
/// Result of exact detach with the authoritative post-detach attachment count.
pub type DetachResult = Result<u32, CommandError>;
/// Result of listing session-local jobs.
pub type JobListResult = Result<Vec<JobSnapshotDto>, CommandError>;
/// Result of a job command.
pub type JobResult = Result<JobSnapshotDto, CommandError>;

/// A checked wire-to-domain conversion failure.
#[derive(Debug, Error)]
pub enum RpcConversionError {
    /// Cap'n Proto could not read a pointer or capability.
    #[error("could not read Cap'n Proto value: {0}")]
    Capnp(#[from] capnp::Error),
    /// A text field was not valid UTF-8.
    #[error("RPC field {field} is not valid UTF-8")]
    InvalidText {
        /// Logical schema field.
        field: &'static str,
    },
    /// A timestamp was not UTC RFC 3339 text.
    #[error("RPC field {field} is not a UTC RFC 3339 timestamp")]
    InvalidTimestamp {
        /// Logical schema field.
        field: &'static str,
    },
    /// Tool arguments were not valid JSON.
    #[error("RPC tool arguments are not valid JSON")]
    InvalidToolArguments,
    /// A stable session identifier was malformed.
    #[error("RPC session identifier is malformed")]
    InvalidSessionId,
    /// A CWD-scoped session title was malformed.
    #[error("RPC session title is malformed")]
    InvalidSessionTitle,
    /// Attachment zero is reserved and cannot identify an observer.
    #[error("RPC attachment identifier must be nonzero")]
    InvalidAttachmentId,
    /// A CWD-scoped session name was malformed.
    #[error("RPC session name is malformed")]
    InvalidSessionName,
    /// A plan item did not satisfy the session-domain validation rules.
    #[error("RPC plan item is invalid")]
    InvalidPlanItem,
    /// A future enum value is not understood by this build.
    #[error("unknown {name} enum value {value}")]
    UnknownEnum {
        /// Schema enum name.
        name: &'static str,
        /// Numeric wire value.
        value: u16,
    },
    /// A future union member is not understood by this build.
    #[error("unknown {name} union value {value}")]
    UnknownUnion {
        /// Schema union name.
        name: &'static str,
        /// Numeric wire discriminant.
        value: u16,
    },
    /// A list cannot be represented by Cap'n Proto's 32-bit list length.
    #[error("RPC {field} list is too long")]
    ListTooLong {
        /// Logical list field.
        field: &'static str,
    },
    /// A Text or Data field exceeds Cap'n Proto's representable byte length.
    #[error("RPC {field} field is too long")]
    FieldTooLong {
        /// Logical Text or Data field.
        field: &'static str,
    },
    /// An HTTP failure omitted or incorrectly supplied its guarded status.
    #[error("RPC HTTP status guard is inconsistent with the failure kind")]
    InvalidHttpStatusGuard,
}

const CAPNP_LIST_LIMIT: usize = 1 << 29;

/// Maximum raw working-directory bytes accepted at an application RPC boundary.
pub const MAX_RPC_CWD_BYTES: usize = 16 * 1024;
/// Maximum prompt bytes accepted at an application RPC boundary.
pub const MAX_RPC_PROMPT_BYTES: usize = 256 * 1024;
/// Maximum model, job, or stable-identity bytes accepted at an application RPC boundary.
pub const MAX_RPC_IDENTIFIER_BYTES: usize = 4 * 1024;
/// Maximum encoded session-title bytes accepted before scalar-level title validation.
pub const MAX_RPC_TITLE_BYTES: usize = 64 * 4;

/// Applies a practical application limit before an inbound value is copied into owned storage.
#[doc(hidden)]
pub fn validate_inbound_field_length(
    length: usize,
    limit: usize,
    field: &'static str,
) -> Result<(), RpcConversionError> {
    if length > limit {
        return Err(RpcConversionError::FieldTooLong { field });
    }
    Ok(())
}

/// Validates a Text byte length, including its required NUL terminator.
#[doc(hidden)]
pub fn validate_wire_text_length(
    length: usize,
    field: &'static str,
) -> Result<(), RpcConversionError> {
    let encoded_length = length
        .checked_add(1)
        .ok_or(RpcConversionError::FieldTooLong { field })?;
    if encoded_length >= CAPNP_LIST_LIMIT {
        return Err(RpcConversionError::FieldTooLong { field });
    }
    Ok(())
}

/// Validates a Data byte length against Cap'n Proto's list-element limit.
#[doc(hidden)]
pub fn validate_wire_data_length(
    length: usize,
    field: &'static str,
) -> Result<(), RpcConversionError> {
    if length >= CAPNP_LIST_LIMIT {
        return Err(RpcConversionError::FieldTooLong { field });
    }
    Ok(())
}

/// Validates a non-inline list element count against Cap'n Proto's strict limit.
#[doc(hidden)]
pub fn validate_wire_list_length(
    length: usize,
    field: &'static str,
) -> Result<u32, RpcConversionError> {
    if length >= CAPNP_LIST_LIMIT {
        return Err(RpcConversionError::ListTooLong { field });
    }
    u32::try_from(length).map_err(|_| RpcConversionError::ListTooLong { field })
}

/// Validates an inline-composite list's element and encoded word counts.
#[doc(hidden)]
pub fn validate_wire_inline_composite_list_length(
    length: usize,
    words_per_element: usize,
    field: &'static str,
) -> Result<u32, RpcConversionError> {
    let wire_length = validate_wire_list_length(length, field)?;
    let word_count = length
        .checked_mul(words_per_element)
        .ok_or(RpcConversionError::ListTooLong { field })?;
    if word_count >= CAPNP_LIST_LIMIT {
        return Err(RpcConversionError::ListTooLong { field });
    }
    Ok(wire_length)
}

fn checked_text<'a>(value: &'a str, field: &'static str) -> Result<&'a str, RpcConversionError> {
    validate_wire_text_length(value.len(), field)?;
    Ok(value)
}

fn checked_data<'a>(value: &'a [u8], field: &'static str) -> Result<&'a [u8], RpcConversionError> {
    validate_wire_data_length(value.len(), field)?;
    Ok(value)
}

fn wire_struct_list_len<T: capnp::traits::HasStructSize>(
    length: usize,
    field: &'static str,
) -> Result<u32, RpcConversionError> {
    let size = T::STRUCT_SIZE;
    let words_per_element = usize::from(size.data) + usize::from(size.pointers);
    validate_wire_inline_composite_list_length(length, words_per_element, field)
}

fn read_text(
    value: capnp::Result<capnp::text::Reader<'_>>,
    field: &'static str,
) -> Result<String, RpcConversionError> {
    value?
        .to_str()
        .map(str::to_owned)
        .map_err(|_| RpcConversionError::InvalidText { field })
}

/// Reads bounded inbound UTF-8 without allocating before the byte limit is checked.
#[doc(hidden)]
pub fn read_inbound_text(
    value: capnp::Result<capnp::text::Reader<'_>>,
    limit: usize,
    field: &'static str,
) -> Result<String, RpcConversionError> {
    let value = value?;
    validate_inbound_field_length(value.as_bytes().len(), limit, field)?;
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| RpcConversionError::InvalidText { field })
}

/// Reads bounded inbound binary data without allocating before the byte limit is checked.
#[doc(hidden)]
pub fn read_inbound_data(
    value: capnp::Result<capnp::data::Reader<'_>>,
    limit: usize,
    field: &'static str,
) -> Result<Vec<u8>, RpcConversionError> {
    let value = value?;
    validate_inbound_field_length(value.len(), limit, field)?;
    Ok(value.to_vec())
}

fn write_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn read_timestamp(
    value: capnp::Result<capnp::text::Reader<'_>>,
    field: &'static str,
) -> Result<DateTime<Utc>, RpcConversionError> {
    let value = read_text(value, field)?;
    let parsed = DateTime::<FixedOffset>::parse_from_rfc3339(&value)
        .map_err(|_| RpcConversionError::InvalidTimestamp { field })?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(RpcConversionError::InvalidTimestamp { field });
    }
    Ok(parsed.with_timezone(&Utc))
}

fn unknown_enum(name: &'static str, error: NotInSchema) -> RpcConversionError {
    RpcConversionError::UnknownEnum {
        name,
        value: error.0,
    }
}

fn unknown_union(name: &'static str, error: NotInSchema) -> RpcConversionError {
    RpcConversionError::UnknownUnion {
        name,
        value: error.0,
    }
}

/// Writes backend protocol metadata.
pub fn write_protocol_info(
    mut builder: wire::protocol_info::Builder<'_>,
    value: &ProtocolInfo,
) -> Result<(), RpcConversionError> {
    builder.set_major(value.major);
    builder.set_minor(value.minor);
    builder.set_instance_id(checked_text(value.instance_id.as_str(), "instanceId")?);
    let warnings_len = validate_wire_list_length(value.startup_warnings.len(), "startupWarnings")?;
    let mut warnings = builder.reborrow().init_startup_warnings(warnings_len);
    for (index, warning) in (0..warnings_len).zip(&value.startup_warnings) {
        warnings.set(index, checked_text(warning.as_str(), "startupWarnings")?);
    }
    let features_len = validate_wire_list_length(value.features.len(), "features")?;
    let mut features = builder.reborrow().init_features(features_len);
    for (index, feature) in (0..features_len).zip(&value.features) {
        features.set(index, checked_text(feature.as_str(), "features")?);
    }
    Ok(())
}

/// Reads backend protocol metadata.
pub fn read_protocol_info(
    reader: wire::protocol_info::Reader<'_>,
) -> Result<ProtocolInfo, RpcConversionError> {
    let warnings = reader.get_startup_warnings()?;
    let startup_warnings = (0..warnings.len())
        .map(|index| read_text(warnings.get(index), "startupWarnings"))
        .collect::<Result<_, _>>()?;
    let features = reader.get_features()?;
    let features = (0..features.len())
        .map(|index| read_text(features.get(index), "features"))
        .collect::<Result<_, _>>()?;
    Ok(ProtocolInfo {
        major: reader.get_major(),
        minor: reader.get_minor(),
        instance_id: read_text(reader.get_instance_id(), "instanceId")?,
        startup_warnings,
        features,
    })
}

/// Writes a stable ID or CWD-scoped title selector.
pub fn write_session_selector(
    mut builder: wire::session_selector::Builder<'_>,
    value: &SessionSelector,
) -> Result<(), RpcConversionError> {
    match value {
        SessionSelector::Id(id) => {
            let id = id.to_string();
            validate_inbound_field_length(id.len(), MAX_RPC_IDENTIFIER_BYTES, "id")?;
            builder.set_id(checked_text(&id, "id")?);
        }
        SessionSelector::Title(title) => {
            validate_inbound_field_length(title.as_str().len(), MAX_RPC_TITLE_BYTES, "title")?;
            builder.set_title(checked_text(title.as_str(), "title")?);
        }
    }
    Ok(())
}

/// Reads and validates a stable ID or CWD-scoped title selector.
pub fn read_session_selector(
    reader: wire::session_selector::Reader<'_>,
) -> Result<SessionSelector, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("SessionSelector", error))?
    {
        wire::session_selector::Id(value) => {
            SessionId::from_str(&read_inbound_text(value, MAX_RPC_IDENTIFIER_BYTES, "id")?)
                .map(SessionSelector::Id)
                .map_err(|_| RpcConversionError::InvalidSessionId)
        }
        wire::session_selector::Title(value) => {
            SessionTitle::parse(read_inbound_text(value, MAX_RPC_TITLE_BYTES, "title")?)
                .map(SessionSelector::Title)
                .map_err(|_| RpcConversionError::InvalidSessionTitle)
        }
    }
}

/// Converts a checked attachment ID to its exact wire representation.
pub fn write_attachment_id(value: AttachmentId) -> Result<u64, RpcConversionError> {
    if value.0 == 0 {
        return Err(RpcConversionError::InvalidAttachmentId);
    }
    Ok(value.0)
}

/// Validates and converts an exact wire attachment ID.
pub fn read_attachment_id(value: u64) -> Result<AttachmentId, RpcConversionError> {
    if value == 0 {
        return Err(RpcConversionError::InvalidAttachmentId);
    }
    Ok(AttachmentId(value))
}

/// Splits a domain listing scope into the wire enum and independent raw CWD bytes.
pub fn write_session_list_scope(
    value: &SessionListScope,
) -> Result<(wire::SessionListScope, &[u8]), RpcConversionError> {
    match value {
        SessionListScope::Project(cwd) => Ok((wire::SessionListScope::Project, {
            validate_inbound_field_length(cwd.len(), MAX_RPC_CWD_BYTES, "cwd")?;
            checked_data(cwd.as_slice(), "cwd")?
        })),
        SessionListScope::All => Ok((wire::SessionListScope::All, &[])),
    }
}

/// Combines a wire listing scope with its separately encoded raw CWD bytes.
pub fn read_session_list_scope(
    scope: Result<wire::SessionListScope, NotInSchema>,
    cwd: capnp::Result<capnp::data::Reader<'_>>,
) -> Result<SessionListScope, RpcConversionError> {
    match scope.map_err(|error| unknown_enum("SessionListScope", error))? {
        wire::SessionListScope::Project => Ok(SessionListScope::Project(read_inbound_data(
            cwd,
            MAX_RPC_CWD_BYTES,
            "cwd",
        )?)),
        wire::SessionListScope::All => Ok(SessionListScope::All),
    }
}

fn wire_error_code(value: ErrorCode) -> wire::ErrorCode {
    match value {
        ErrorCode::Busy => wire::ErrorCode::Busy,
        ErrorCode::NotRunning => wire::ErrorCode::NotRunning,
        ErrorCode::SessionNotFound => wire::ErrorCode::SessionNotFound,
        ErrorCode::AmbiguousTitle => wire::ErrorCode::AmbiguousTitle,
        ErrorCode::SessionNameConflict => wire::ErrorCode::SessionNameConflict,
        ErrorCode::InvalidArgument => wire::ErrorCode::InvalidArgument,
        ErrorCode::ModelNotFound => wire::ErrorCode::ModelNotFound,
        ErrorCode::UnsupportedReasoning => wire::ErrorCode::UnsupportedReasoning,
        ErrorCode::JobNotFound => wire::ErrorCode::JobNotFound,
        ErrorCode::BackendStarting => wire::ErrorCode::BackendStarting,
        ErrorCode::BackendUnavailable => wire::ErrorCode::BackendUnavailable,
        ErrorCode::Persistence => wire::ErrorCode::Persistence,
        ErrorCode::SessionDeleting => wire::ErrorCode::SessionDeleting,
        ErrorCode::SessionDeleted => wire::ErrorCode::SessionDeleted,
        ErrorCode::Internal => wire::ErrorCode::Internal,
    }
}

fn domain_error_code(value: wire::ErrorCode) -> ErrorCode {
    match value {
        wire::ErrorCode::Busy => ErrorCode::Busy,
        wire::ErrorCode::NotRunning => ErrorCode::NotRunning,
        wire::ErrorCode::SessionNotFound => ErrorCode::SessionNotFound,
        wire::ErrorCode::AmbiguousTitle => ErrorCode::AmbiguousTitle,
        wire::ErrorCode::SessionNameConflict => ErrorCode::SessionNameConflict,
        wire::ErrorCode::InvalidArgument => ErrorCode::InvalidArgument,
        wire::ErrorCode::ModelNotFound => ErrorCode::ModelNotFound,
        wire::ErrorCode::UnsupportedReasoning => ErrorCode::UnsupportedReasoning,
        wire::ErrorCode::JobNotFound => ErrorCode::JobNotFound,
        wire::ErrorCode::BackendStarting => ErrorCode::BackendStarting,
        wire::ErrorCode::BackendUnavailable => ErrorCode::BackendUnavailable,
        wire::ErrorCode::Persistence => ErrorCode::Persistence,
        wire::ErrorCode::SessionDeleting => ErrorCode::SessionDeleting,
        wire::ErrorCode::SessionDeleted => ErrorCode::SessionDeleted,
        wire::ErrorCode::Internal => ErrorCode::Internal,
    }
}

/// Writes a transport-safe command error.
pub fn write_command_error(
    mut builder: wire::command_error::Builder<'_>,
    value: &CommandError,
) -> Result<(), RpcConversionError> {
    builder.set_code(wire_error_code(value.code));
    builder.set_message(checked_text(value.message.as_str(), "message")?);
    let ids_len = validate_wire_list_length(value.ids.len(), "ids")?;
    let mut ids = builder.reborrow().init_ids(ids_len);
    for (index, id) in (0..ids_len).zip(&value.ids) {
        let id = id.to_string();
        ids.set(index, checked_text(&id, "ids")?);
    }
    Ok(())
}

/// Reads a transport-safe command error.
pub fn read_command_error(
    reader: wire::command_error::Reader<'_>,
) -> Result<CommandError, RpcConversionError> {
    let code = reader
        .get_code()
        .map(domain_error_code)
        .map_err(|error| unknown_enum("ErrorCode", error))?;
    let ids = reader.get_ids()?;
    let ids = (0..ids.len())
        .map(|index| {
            SessionId::from_str(&read_text(ids.get(index), "ids")?)
                .map_err(|_| RpcConversionError::InvalidSessionId)
        })
        .collect::<Result<_, _>>()?;
    Ok(CommandError {
        code,
        message: read_text(reader.get_message(), "message")?,
        ids,
    })
}

/// Converts a domain reasoning level to its wire enum.
pub fn write_reasoning_level(value: ReasoningLevel) -> wire::ReasoningLevel {
    match value {
        ReasoningLevel::None => wire::ReasoningLevel::None,
        ReasoningLevel::Minimal => wire::ReasoningLevel::Minimal,
        ReasoningLevel::Low => wire::ReasoningLevel::Low,
        ReasoningLevel::Medium => wire::ReasoningLevel::Medium,
        ReasoningLevel::High => wire::ReasoningLevel::High,
        ReasoningLevel::Xhigh => wire::ReasoningLevel::Xhigh,
        ReasoningLevel::Max => wire::ReasoningLevel::Max,
    }
}

/// Converts a checked wire reasoning enum to its domain value.
pub fn read_reasoning_level(
    value: Result<wire::ReasoningLevel, NotInSchema>,
) -> Result<ReasoningLevel, RpcConversionError> {
    value
        .map(|value| match value {
            wire::ReasoningLevel::None => ReasoningLevel::None,
            wire::ReasoningLevel::Minimal => ReasoningLevel::Minimal,
            wire::ReasoningLevel::Low => ReasoningLevel::Low,
            wire::ReasoningLevel::Medium => ReasoningLevel::Medium,
            wire::ReasoningLevel::High => ReasoningLevel::High,
            wire::ReasoningLevel::Xhigh => ReasoningLevel::Xhigh,
            wire::ReasoningLevel::Max => ReasoningLevel::Max,
        })
        .map_err(|error| unknown_enum("ReasoningLevel", error))
}

/// Writes durable session settings.
pub fn write_session_settings(
    mut builder: wire::session_settings::Builder<'_>,
    value: &SessionSettings,
) -> Result<(), RpcConversionError> {
    validate_inbound_field_length(value.model.len(), MAX_RPC_IDENTIFIER_BYTES, "model")?;
    builder.set_model(checked_text(value.model.as_str(), "model")?);
    builder.set_reasoning(write_reasoning_level(value.reasoning));
    builder.set_context_tokens(value.context_tokens);
    Ok(())
}

/// Reads durable session settings.
pub fn read_session_settings(
    reader: wire::session_settings::Reader<'_>,
) -> Result<SessionSettings, RpcConversionError> {
    Ok(SessionSettings {
        model: read_inbound_text(reader.get_model(), MAX_RPC_IDENTIFIER_BYTES, "model")?,
        reasoning: read_reasoning_level(reader.get_reasoning())?,
        context_tokens: reader.get_context_tokens(),
    })
}

/// Writes factory-provided defaults for an unmaterialized local draft.
pub fn write_draft_defaults(
    mut builder: wire::draft_defaults::Builder<'_>,
    value: &DraftDefaults,
) -> Result<(), RpcConversionError> {
    builder.set_cwd(checked_data(value.cwd.as_slice(), "cwd")?);
    write_session_settings(builder.reborrow().init_settings(), &value.settings)?;
    write_model_catalog(builder.reborrow().init_catalog(), &value.catalog)
}

/// Reads factory-provided defaults without deriving display text from raw CWD bytes.
pub fn read_draft_defaults(
    reader: wire::draft_defaults::Reader<'_>,
) -> Result<DraftDefaults, RpcConversionError> {
    Ok(DraftDefaults {
        cwd: read_inbound_data(reader.get_cwd(), MAX_RPC_CWD_BYTES, "cwd")?,
        settings: read_session_settings(reader.get_settings()?)?,
        catalog: read_model_catalog(reader.get_catalog()?)?,
    })
}

/// Writes a nonselecting draft-defaults result union.
pub fn write_draft_defaults_result(
    mut builder: wire::draft_defaults_result::Builder<'_>,
    value: &DraftDefaultsResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(defaults) => write_draft_defaults(builder.reborrow().init_defaults(), defaults),
        Err(error) => write_command_error(builder.reborrow().init_error(), error),
    }
}

/// Reads a nonselecting draft-defaults result union.
pub fn read_draft_defaults_result(
    reader: wire::draft_defaults_result::Reader<'_>,
) -> Result<DraftDefaultsResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("DraftDefaultsResult", error))?
    {
        wire::draft_defaults_result::Defaults(defaults) => Ok(Ok(read_draft_defaults(defaults?)?)),
        wire::draft_defaults_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes list-facing session state, preserving canonical CWD bytes.
pub fn write_session_summary(
    mut builder: wire::session_summary::Builder<'_>,
    value: &SessionSummary,
) -> Result<(), RpcConversionError> {
    let id = value.id.to_string();
    builder.set_id(checked_text(&id, "id")?);
    builder.set_title(checked_text(value.title.as_str(), "title")?);
    builder.set_cwd(checked_data(value.cwd.as_slice(), "cwd")?);
    builder.set_cwd_display(checked_text(value.cwd_display.as_str(), "cwdDisplay")?);
    builder.set_title_revision(value.title_revision);
    builder.set_busy(value.busy);
    builder.set_attached_clients(value.attached_clients);
    let last_activity = write_timestamp(value.last_activity);
    builder.set_last_activity(checked_text(&last_activity, "lastActivity")?);
    builder.set_running(value.running);
    builder.set_running_jobs(value.running_jobs);
    Ok(())
}

/// Reads and validates list-facing session state.
pub fn read_session_summary(
    reader: wire::session_summary::Reader<'_>,
) -> Result<SessionSummary, RpcConversionError> {
    let id = SessionId::from_str(&read_text(reader.get_id(), "id")?)
        .map_err(|_| RpcConversionError::InvalidSessionId)?;
    let title = SessionTitle::parse(read_text(reader.get_title(), "title")?)
        .map_err(|_| RpcConversionError::InvalidSessionTitle)?;
    Ok(SessionSummary {
        id,
        title,
        title_revision: reader.get_title_revision(),
        cwd: read_inbound_data(reader.get_cwd(), MAX_RPC_CWD_BYTES, "cwd")?,
        cwd_display: read_text(reader.get_cwd_display(), "cwdDisplay")?,
        running_jobs: reader.get_running_jobs(),
        running: reader.get_running(),
        busy: reader.get_busy(),
        attached_clients: reader.get_attached_clients(),
        last_activity: read_timestamp(reader.get_last_activity(), "lastActivity")?,
    })
}

fn wire_run_stage(value: RunStage) -> wire::RunStage {
    match value {
        RunStage::Startup => wire::RunStage::Startup,
        RunStage::ModelRequest => wire::RunStage::ModelRequest,
        RunStage::ToolExecution => wire::RunStage::ToolExecution,
        RunStage::Finalization => wire::RunStage::Finalization,
    }
}

fn domain_run_stage(
    value: Result<wire::RunStage, NotInSchema>,
) -> Result<RunStage, RpcConversionError> {
    value
        .map(|value| match value {
            wire::RunStage::Startup => RunStage::Startup,
            wire::RunStage::ModelRequest => RunStage::ModelRequest,
            wire::RunStage::ToolExecution => RunStage::ToolExecution,
            wire::RunStage::Finalization => RunStage::Finalization,
        })
        .map_err(|error| unknown_enum("RunStage", error))
}

/// Writes a transport-safe run failure and its guarded HTTP status.
pub fn write_run_failure(
    mut builder: wire::run_failure::Builder<'_>,
    value: &RunFailureSnapshot,
) -> Result<(), RpcConversionError> {
    builder.set_stage(wire_run_stage(value.stage));
    match value.kind {
        RunFailureKind::Authentication => builder.set_kind(wire::RunFailureKind::Authentication),
        RunFailureKind::Transport => builder.set_kind(wire::RunFailureKind::Transport),
        RunFailureKind::HttpRejected { status } => {
            builder.set_kind(wire::RunFailureKind::HttpRejected);
            builder.set_has_http_status(true);
            builder.set_http_status(status);
        }
        RunFailureKind::Protocol => builder.set_kind(wire::RunFailureKind::Protocol),
        RunFailureKind::EmptyResponse => builder.set_kind(wire::RunFailureKind::EmptyResponse),
        RunFailureKind::BudgetExhausted => builder.set_kind(wire::RunFailureKind::BudgetExhausted),
        RunFailureKind::RuntimeInfrastructure => {
            builder.set_kind(wire::RunFailureKind::RuntimeInfrastructure)
        }
        RunFailureKind::ToolInfrastructure => {
            builder.set_kind(wire::RunFailureKind::ToolInfrastructure)
        }
    }
    builder.set_retryable(value.retryable);
    builder.set_message(checked_text(value.message.as_str(), "message")?);
    Ok(())
}

/// Reads a transport-safe run failure and validates its HTTP-status guard.
pub fn read_run_failure(
    reader: wire::run_failure::Reader<'_>,
) -> Result<RunFailureSnapshot, RpcConversionError> {
    let wire_kind = reader
        .get_kind()
        .map_err(|error| unknown_enum("RunFailureKind", error))?;
    let kind = match (wire_kind, reader.get_has_http_status()) {
        (wire::RunFailureKind::HttpRejected, true) => RunFailureKind::HttpRejected {
            status: reader.get_http_status(),
        },
        (wire::RunFailureKind::HttpRejected, false)
        | (wire::RunFailureKind::Authentication, true)
        | (wire::RunFailureKind::Transport, true)
        | (wire::RunFailureKind::Protocol, true)
        | (wire::RunFailureKind::EmptyResponse, true)
        | (wire::RunFailureKind::BudgetExhausted, true)
        | (wire::RunFailureKind::RuntimeInfrastructure, true)
        | (wire::RunFailureKind::ToolInfrastructure, true) => {
            return Err(RpcConversionError::InvalidHttpStatusGuard);
        }
        (wire::RunFailureKind::Authentication, false) => RunFailureKind::Authentication,
        (wire::RunFailureKind::Transport, false) => RunFailureKind::Transport,
        (wire::RunFailureKind::Protocol, false) => RunFailureKind::Protocol,
        (wire::RunFailureKind::EmptyResponse, false) => RunFailureKind::EmptyResponse,
        (wire::RunFailureKind::BudgetExhausted, false) => RunFailureKind::BudgetExhausted,
        (wire::RunFailureKind::RuntimeInfrastructure, false) => {
            RunFailureKind::RuntimeInfrastructure
        }
        (wire::RunFailureKind::ToolInfrastructure, false) => RunFailureKind::ToolInfrastructure,
    };
    Ok(RunFailureSnapshot {
        stage: domain_run_stage(reader.get_stage())?,
        kind,
        retryable: reader.get_retryable(),
        message: read_text(reader.get_message(), "message")?,
    })
}

fn wire_job_kind(value: JobKind) -> wire::JobKind {
    match value {
        JobKind::Bash => wire::JobKind::Bash,
    }
}

fn domain_job_kind(
    value: Result<wire::JobKind, NotInSchema>,
) -> Result<JobKind, RpcConversionError> {
    value
        .map(|value| match value {
            wire::JobKind::Bash => JobKind::Bash,
        })
        .map_err(|error| unknown_enum("JobKind", error))
}

fn wire_job_state(value: JobState) -> wire::JobState {
    match value {
        JobState::Running => wire::JobState::Running,
        JobState::Completed => wire::JobState::Completed,
        JobState::Failed => wire::JobState::Failed,
        JobState::Cancelled => wire::JobState::Cancelled,
    }
}

fn domain_job_state(
    value: Result<wire::JobState, NotInSchema>,
) -> Result<JobState, RpcConversionError> {
    value
        .map(|value| match value {
            wire::JobState::Running => JobState::Running,
            wire::JobState::Completed => JobState::Completed,
            wire::JobState::Failed => JobState::Failed,
            wire::JobState::Cancelled => JobState::Cancelled,
        })
        .map_err(|error| unknown_enum("JobState", error))
}

/// Converts a domain plan status to its canonical wire enum value.
pub fn write_plan_status(value: PlanStatus) -> wire::PlanStatus {
    match value {
        PlanStatus::Pending => wire::PlanStatus::Pending,
        PlanStatus::InProgress => wire::PlanStatus::InProgress,
        PlanStatus::Completed => wire::PlanStatus::Completed,
        PlanStatus::Blocked => wire::PlanStatus::Blocked,
        PlanStatus::Cancelled => wire::PlanStatus::Cancelled,
    }
}

/// Converts a wire plan status to its canonical domain enum value.
pub fn read_plan_status(
    value: Result<wire::PlanStatus, NotInSchema>,
) -> Result<PlanStatus, RpcConversionError> {
    value
        .map(|value| match value {
            wire::PlanStatus::Pending => PlanStatus::Pending,
            wire::PlanStatus::InProgress => PlanStatus::InProgress,
            wire::PlanStatus::Completed => PlanStatus::Completed,
            wire::PlanStatus::Blocked => PlanStatus::Blocked,
            wire::PlanStatus::Cancelled => PlanStatus::Cancelled,
        })
        .map_err(|error| unknown_enum("PlanStatus", error))
}

fn write_plan_item(
    mut builder: wire::plan_item::Builder<'_>,
    value: &PlanItem,
) -> Result<(), RpcConversionError> {
    builder.set_step(checked_text(value.step(), "plan.step")?);
    builder.set_status(write_plan_status(value.status()));
    Ok(())
}

fn read_plan_item(reader: wire::plan_item::Reader<'_>) -> Result<PlanItem, RpcConversionError> {
    PlanItem::parse(
        read_text(reader.get_step(), "plan.step")?,
        read_plan_status(reader.get_status())?,
    )
    .map_err(|_| RpcConversionError::InvalidPlanItem)
}

/// Writes an ordered plan into an initialized wire list.
fn write_plan_items(
    mut builder: capnp::struct_list::Builder<'_, wire::plan_item::Owned>,
    plan: &[PlanItem],
) -> Result<(), RpcConversionError> {
    for (index, item) in (0..builder.len()).zip(plan) {
        write_plan_item(builder.reborrow().get(index), item)?;
    }
    Ok(())
}

/// Reads and validates an ordered plan from a wire list.
fn read_plan_items(
    reader: capnp::struct_list::Reader<'_, wire::plan_item::Owned>,
) -> Result<Vec<PlanItem>, RpcConversionError> {
    (0..reader.len())
        .map(|index| read_plan_item(reader.get(index)))
        .collect()
}

/// Writes a transport-safe job snapshot.
pub fn write_job_snapshot(
    mut builder: wire::job_snapshot::Builder<'_>,
    value: &JobSnapshotDto,
) -> Result<(), RpcConversionError> {
    builder.set_id(checked_text(value.id.as_str(), "id")?);
    builder.set_kind(wire_job_kind(value.kind));
    builder.set_state(wire_job_state(value.state));
    builder.set_title(checked_text(value.title.as_str(), "title")?);
    let started_at = write_timestamp(value.started_at);
    builder.set_started_at(checked_text(&started_at, "startedAt")?);
    let completed_at = value.completed_at.map(write_timestamp);
    builder.set_completed_at(checked_text(
        completed_at.as_deref().unwrap_or(""),
        "completedAt",
    )?);
    builder.set_details(checked_text(value.details.as_str(), "details")?);
    Ok(())
}

/// Reads a transport-safe job snapshot.
pub fn read_job_snapshot(
    reader: wire::job_snapshot::Reader<'_>,
) -> Result<JobSnapshotDto, RpcConversionError> {
    let completed_at = read_text(reader.get_completed_at(), "completedAt")?;
    let completed_at = if completed_at.is_empty() {
        None
    } else {
        Some(read_timestamp(
            Ok(completed_at.as_str().into()),
            "completedAt",
        )?)
    };
    Ok(JobSnapshotDto {
        id: read_text(reader.get_id(), "id")?,
        kind: domain_job_kind(reader.get_kind())?,
        state: domain_job_state(reader.get_state())?,
        title: read_text(reader.get_title(), "title")?,
        started_at: read_timestamp(reader.get_started_at(), "startedAt")?,
        completed_at,
        details: read_text(reader.get_details(), "details")?,
    })
}

/// Writes a model catalog entry.
pub fn write_model_info(
    mut builder: wire::model_info::Builder<'_>,
    value: &ModelInfoDto,
) -> Result<(), RpcConversionError> {
    builder.set_id(checked_text(value.id.as_str(), "id")?);
    builder.set_display_name(checked_text(value.display_name.as_str(), "displayName")?);
    builder.set_description(checked_text(value.description.as_str(), "description")?);
    let efforts_len = validate_wire_list_length(value.reasoning_efforts.len(), "reasoningEfforts")?;
    let mut efforts = builder.reborrow().init_reasoning_efforts(efforts_len);
    for (index, effort) in (0..efforts_len).zip(value.reasoning_efforts.iter().copied()) {
        efforts.set(index, write_reasoning_level(effort));
    }
    if let Some(reasoning) = value.default_reasoning {
        builder.set_has_default_reasoning(true);
        builder.set_default_reasoning(write_reasoning_level(reasoning));
    }
    Ok(())
}

/// Reads a model catalog entry.
pub fn read_model_info(
    reader: wire::model_info::Reader<'_>,
) -> Result<ModelInfoDto, RpcConversionError> {
    let efforts = reader.get_reasoning_efforts()?;
    let reasoning_efforts = (0..efforts.len())
        .map(|index| read_reasoning_level(efforts.get(index)))
        .collect::<Result<_, _>>()?;
    let default_reasoning = reader
        .get_has_default_reasoning()
        .then(|| read_reasoning_level(reader.get_default_reasoning()))
        .transpose()?;
    Ok(ModelInfoDto {
        id: read_text(reader.get_id(), "id")?,
        display_name: read_text(reader.get_display_name(), "displayName")?,
        description: read_text(reader.get_description(), "description")?,
        reasoning_efforts,
        default_reasoning,
    })
}

/// Writes asynchronous model catalog state.
pub fn write_model_catalog(
    mut builder: wire::model_catalog::Builder<'_>,
    value: &ModelCatalogState,
) -> Result<(), RpcConversionError> {
    match value {
        ModelCatalogState::Loading => builder.set_loading(()),
        ModelCatalogState::Ready(models) => {
            let models_len =
                wire_struct_list_len::<wire::model_info::Builder<'_>>(models.len(), "ready")?;
            let mut list = builder.reborrow().init_ready(models_len);
            for (index, model) in (0..models_len).zip(models) {
                write_model_info(list.reborrow().get(index), model)?;
            }
        }
        ModelCatalogState::Failed(message) => {
            builder.set_failed(checked_text(message.as_str(), "catalogFailed")?);
        }
    }
    Ok(())
}

/// Reads asynchronous model catalog state.
pub fn read_model_catalog(
    reader: wire::model_catalog::Reader<'_>,
) -> Result<ModelCatalogState, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("ModelCatalog", error))?
    {
        wire::model_catalog::Loading(()) => Ok(ModelCatalogState::Loading),
        wire::model_catalog::Ready(models) => {
            let models = models?;
            let values = (0..models.len())
                .map(|index| read_model_info(models.get(index)))
                .collect::<Result<_, _>>()?;
            Ok(ModelCatalogState::Ready(values))
        }
        wire::model_catalog::Failed(message) => Ok(ModelCatalogState::Failed(read_text(
            message,
            "catalogFailed",
        )?)),
    }
}

fn write_tool_started(
    mut builder: wire::tool_started_record::Builder<'_>,
    run_id: u64,
    call_id: &str,
    name: &str,
    arguments: &serde_json::Value,
) -> Result<(), RpcConversionError> {
    let arguments =
        serde_json::to_string(arguments).map_err(|_| RpcConversionError::InvalidToolArguments)?;
    builder.set_run_id(run_id);
    builder.set_call_id(checked_text(call_id, "callId")?);
    builder.set_name(checked_text(name, "name")?);
    builder.set_arguments_json(checked_text(arguments.as_str(), "argumentsJson")?);
    Ok(())
}

fn read_tool_started(
    reader: wire::tool_started_record::Reader<'_>,
) -> Result<(u64, String, String, serde_json::Value), RpcConversionError> {
    let arguments = read_text(reader.get_arguments_json(), "argumentsJson")?;
    let arguments =
        serde_json::from_str(&arguments).map_err(|_| RpcConversionError::InvalidToolArguments)?;
    Ok((
        reader.get_run_id(),
        read_text(reader.get_call_id(), "callId")?,
        read_text(reader.get_name(), "name")?,
        arguments,
    ))
}

/// Writes one presentation-neutral transcript item.
pub fn write_transcript_item(
    mut builder: wire::transcript_item::Builder<'_>,
    value: &TranscriptItem,
) -> Result<(), RpcConversionError> {
    match value {
        TranscriptItem::User(text) => {
            builder.set_user(checked_text(text.as_str(), "user")?);
        }
        TranscriptItem::Assistant(text) => {
            builder.set_assistant(checked_text(text.as_str(), "assistant")?);
        }
        TranscriptItem::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => write_tool_started(
            builder.reborrow().init_tool_started(),
            *run_id,
            call_id,
            name,
            arguments,
        )?,
        TranscriptItem::Failed { run_id, failure } => {
            let mut failed = builder.reborrow().init_failed();
            failed.set_run_id(*run_id);
            write_run_failure(failed.init_failure(), failure)?;
        }
        TranscriptItem::Cancelled { run_id } => builder.set_cancelled_run_id(*run_id),
    }
    Ok(())
}

/// Reads one presentation-neutral transcript item.
pub fn read_transcript_item(
    reader: wire::transcript_item::Reader<'_>,
) -> Result<TranscriptItem, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("TranscriptItem", error))?
    {
        wire::transcript_item::User(text) => Ok(TranscriptItem::User(read_text(text, "user")?)),
        wire::transcript_item::Assistant(text) => {
            Ok(TranscriptItem::Assistant(read_text(text, "assistant")?))
        }
        wire::transcript_item::ToolStarted(tool) => {
            let (run_id, call_id, name, arguments) = read_tool_started(tool?)?;
            Ok(TranscriptItem::ToolStarted {
                run_id,
                call_id,
                name,
                arguments,
            })
        }
        wire::transcript_item::Failed(failed) => {
            let failed = failed?;
            Ok(TranscriptItem::Failed {
                run_id: failed.get_run_id(),
                failure: read_run_failure(failed.get_failure()?)?,
            })
        }
        wire::transcript_item::CancelledRunId(run_id) => Ok(TranscriptItem::Cancelled { run_id }),
    }
}

/// Writes the optional process-local active run state.
pub fn write_active_run(
    mut builder: wire::active_run::Builder<'_>,
    value: &ActiveRunSnapshot,
) -> Result<(), RpcConversionError> {
    builder.set_run_id(value.run_id);
    builder.set_prompt(checked_text(value.prompt.as_str(), "prompt")?);
    builder.set_assistant_text(checked_text(
        value.assistant_text.as_str(),
        "assistantText",
    )?);
    Ok(())
}

/// Reads process-local active run state.
pub fn read_active_run(
    reader: wire::active_run::Reader<'_>,
) -> Result<ActiveRunSnapshot, RpcConversionError> {
    Ok(ActiveRunSnapshot {
        run_id: reader.get_run_id(),
        prompt: read_text(reader.get_prompt(), "prompt")?,
        assistant_text: read_text(reader.get_assistant_text(), "assistantText")?,
    })
}

fn write_job_list(
    mut builder: capnp::struct_list::Builder<'_, wire::job_snapshot::Owned>,
    jobs: &[JobSnapshotDto],
) -> Result<(), RpcConversionError> {
    for (index, job) in (0..builder.len()).zip(jobs) {
        write_job_snapshot(builder.reborrow().get(index), job)?;
    }
    Ok(())
}

fn read_job_list(
    reader: capnp::struct_list::Reader<'_, wire::job_snapshot::Owned>,
) -> Result<Vec<JobSnapshotDto>, RpcConversionError> {
    (0..reader.len())
        .map(|index| read_job_snapshot(reader.get(index)))
        .collect()
}

/// Writes an authoritative session attachment snapshot.
pub fn write_session_snapshot(
    mut builder: wire::session_snapshot::Builder<'_>,
    value: &SessionSnapshot,
) -> Result<(), RpcConversionError> {
    write_session_summary(builder.reborrow().init_summary(), &value.summary)?;
    let transcript_len = wire_struct_list_len::<wire::transcript_item::Builder<'_>>(
        value.transcript.len(),
        "transcript",
    )?;
    let mut transcript = builder.reborrow().init_transcript(transcript_len);
    for (index, item) in (0..transcript_len).zip(&value.transcript) {
        write_transcript_item(transcript.reborrow().get(index), item)?;
    }
    if let Some(active_run) = &value.active_run {
        write_active_run(builder.reborrow().init_active_run(), active_run)?;
    }
    write_session_settings(builder.reborrow().init_settings(), &value.settings)?;
    write_model_catalog(builder.reborrow().init_catalog(), &value.catalog)?;
    let plan_len = wire_struct_list_len::<wire::plan_item::Builder<'_>>(value.plan.len(), "plan")?;
    let plan = builder.reborrow().init_plan(plan_len);
    write_plan_items(plan, &value.plan)?;
    let jobs_len =
        wire_struct_list_len::<wire::job_snapshot::Builder<'_>>(value.jobs.len(), "jobs")?;
    let jobs = builder.reborrow().init_jobs(jobs_len);
    write_job_list(jobs, &value.jobs)?;
    builder.set_persistence_warning(checked_text(
        value.persistence_warning.as_deref().unwrap_or(""),
        "persistenceWarning",
    )?);
    builder.set_sequence(value.sequence);
    builder.set_busy(value.busy);
    Ok(())
}

/// Reads an authoritative session attachment snapshot.
pub fn read_session_snapshot(
    reader: wire::session_snapshot::Reader<'_>,
) -> Result<SessionSnapshot, RpcConversionError> {
    let transcript = reader.get_transcript()?;
    let transcript = (0..transcript.len())
        .map(|index| read_transcript_item(transcript.get(index)))
        .collect::<Result<_, _>>()?;
    let active_run = reader
        .has_active_run()
        .then(|| reader.get_active_run().map_err(RpcConversionError::from))
        .transpose()?
        .map(read_active_run)
        .transpose()?;
    let persistence_warning = read_text(reader.get_persistence_warning(), "persistenceWarning")?;
    Ok(SessionSnapshot {
        summary: read_session_summary(reader.get_summary()?)?,
        transcript,
        active_run,
        settings: read_session_settings(reader.get_settings()?)?,
        catalog: read_model_catalog(reader.get_catalog()?)?,
        plan: read_plan_items(reader.get_plan()?)?,
        jobs: read_job_list(reader.get_jobs()?)?,
        persistence_warning: (!persistence_warning.is_empty()).then_some(persistence_warning),
        sequence: reader.get_sequence(),
        busy: reader.get_busy(),
    })
}

/// Writes one sequenced session event.
pub fn write_event_envelope(
    mut builder: wire::event_envelope::Builder<'_>,
    value: &SessionEventEnvelope,
) -> Result<(), RpcConversionError> {
    builder.set_sequence(value.sequence);
    match &value.event {
        SessionEvent::TitleChanged {
            title,
            title_revision,
        } => {
            let mut event = builder.reborrow().init_title_changed();
            event.set_title(checked_text(title.as_str(), "title")?);
            event.set_title_revision(*title_revision);
        }
        SessionEvent::Started { run_id, prompt } => {
            let mut event = builder.reborrow().init_started();
            event.set_run_id(*run_id);
            event.set_prompt(checked_text(prompt.as_str(), "prompt")?);
        }
        SessionEvent::AssistantDelta { run_id, text } => {
            let mut event = builder.reborrow().init_assistant_delta();
            event.set_run_id(*run_id);
            event.set_text(checked_text(text.as_str(), "text")?);
        }
        SessionEvent::ContextUsage {
            run_id,
            input_tokens,
            last_activity,
        } => {
            let mut event = builder.reborrow().init_context_usage();
            event.set_run_id(*run_id);
            event.set_input_tokens(*input_tokens);
            let last_activity = write_timestamp(*last_activity);
            event.set_last_activity(checked_text(&last_activity, "lastActivity")?);
        }
        SessionEvent::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => write_tool_started(
            builder.reborrow().init_tool_started(),
            *run_id,
            call_id,
            name,
            arguments,
        )?,
        SessionEvent::ToolFinished {
            run_id,
            call_id,
            name,
        } => {
            let mut event = builder.reborrow().init_tool_finished();
            event.set_run_id(*run_id);
            event.set_call_id(checked_text(call_id.as_str(), "callId")?);
            event.set_name(checked_text(name.as_str(), "name")?);
        }
        SessionEvent::Completed {
            run_id,
            response,
            last_activity,
        } => {
            let mut event = builder.reborrow().init_completed();
            event.set_run_id(*run_id);
            event.set_response(checked_text(response.as_str(), "response")?);
            let last_activity = write_timestamp(*last_activity);
            event.set_last_activity(checked_text(&last_activity, "lastActivity")?);
        }
        SessionEvent::Failed { run_id, failure } => {
            let mut event = builder.reborrow().init_failed();
            event.set_run_id(*run_id);
            write_run_failure(event.init_failure(), failure)?;
        }
        SessionEvent::Cancelled { run_id } => builder.set_cancelled_run_id(*run_id),
        SessionEvent::SettingsChanged {
            settings,
            last_activity,
        } => {
            let mut event = builder.reborrow().init_settings_changed();
            write_session_settings(event.reborrow().init_settings(), settings)?;
            let last_activity = write_timestamp(*last_activity);
            event.set_last_activity(checked_text(&last_activity, "lastActivity")?);
        }
        SessionEvent::PlanChanged(plan) => {
            let plan_len =
                wire_struct_list_len::<wire::plan_item::Builder<'_>>(plan.len(), "planChanged")?;
            let list = builder.reborrow().init_plan_changed(plan_len);
            write_plan_items(list, plan)?;
        }
        SessionEvent::JobsChanged(jobs) => {
            let jobs_len =
                wire_struct_list_len::<wire::job_snapshot::Builder<'_>>(jobs.len(), "jobsChanged")?;
            let list = builder.reborrow().init_jobs_changed(jobs_len);
            write_job_list(list, jobs)?;
        }
        SessionEvent::CatalogChanged(catalog) => {
            write_model_catalog(builder.reborrow().init_catalog_changed(), catalog)?;
        }
        SessionEvent::PersistenceWarning(warning) => {
            builder.set_persistence_warning(checked_text(
                warning.as_deref().unwrap_or(""),
                "persistenceWarning",
            )?);
        }
        SessionEvent::Deleted { session_id } => {
            let session_id = session_id.to_string();
            builder
                .reborrow()
                .init_deleted()
                .set_session_id(checked_text(&session_id, "sessionId")?);
        }
    }
    Ok(())
}

/// Reads one sequenced session event.
pub fn read_event_envelope(
    reader: wire::event_envelope::Reader<'_>,
) -> Result<SessionEventEnvelope, RpcConversionError> {
    let event = match reader
        .which()
        .map_err(|error| unknown_union("EventEnvelope", error))?
    {
        wire::event_envelope::TitleChanged(event) => {
            let event = event?;
            SessionEvent::TitleChanged {
                title: SessionTitle::parse(read_text(event.get_title(), "title")?)
                    .map_err(|_| RpcConversionError::InvalidSessionTitle)?,
                title_revision: event.get_title_revision(),
            }
        }
        wire::event_envelope::Started(event) => {
            let event = event?;
            SessionEvent::Started {
                run_id: event.get_run_id(),
                prompt: read_text(event.get_prompt(), "prompt")?,
            }
        }
        wire::event_envelope::AssistantDelta(event) => {
            let event = event?;
            SessionEvent::AssistantDelta {
                run_id: event.get_run_id(),
                text: read_text(event.get_text(), "text")?,
            }
        }
        wire::event_envelope::ContextUsage(event) => {
            let event = event?;
            SessionEvent::ContextUsage {
                run_id: event.get_run_id(),
                input_tokens: event.get_input_tokens(),
                last_activity: read_timestamp(event.get_last_activity(), "lastActivity")?,
            }
        }
        wire::event_envelope::ToolStarted(event) => {
            let (run_id, call_id, name, arguments) = read_tool_started(event?)?;
            SessionEvent::ToolStarted {
                run_id,
                call_id,
                name,
                arguments,
            }
        }
        wire::event_envelope::ToolFinished(event) => {
            let event = event?;
            SessionEvent::ToolFinished {
                run_id: event.get_run_id(),
                call_id: read_text(event.get_call_id(), "callId")?,
                name: read_text(event.get_name(), "name")?,
            }
        }
        wire::event_envelope::Completed(event) => {
            let event = event?;
            SessionEvent::Completed {
                run_id: event.get_run_id(),
                response: read_text(event.get_response(), "response")?,
                last_activity: read_timestamp(event.get_last_activity(), "lastActivity")?,
            }
        }
        wire::event_envelope::Failed(event) => {
            let event = event?;
            SessionEvent::Failed {
                run_id: event.get_run_id(),
                failure: read_run_failure(event.get_failure()?)?,
            }
        }
        wire::event_envelope::CancelledRunId(run_id) => SessionEvent::Cancelled { run_id },
        wire::event_envelope::SettingsChanged(event) => {
            let event = event?;
            SessionEvent::SettingsChanged {
                settings: read_session_settings(event.get_settings()?)?,
                last_activity: read_timestamp(event.get_last_activity(), "lastActivity")?,
            }
        }
        wire::event_envelope::PlanChanged(plan) => {
            SessionEvent::PlanChanged(read_plan_items(plan?)?)
        }
        wire::event_envelope::JobsChanged(jobs) => SessionEvent::JobsChanged(read_job_list(jobs?)?),
        wire::event_envelope::CatalogChanged(catalog) => {
            SessionEvent::CatalogChanged(read_model_catalog(catalog?)?)
        }
        wire::event_envelope::PersistenceWarning(warning) => {
            let warning = read_text(warning, "persistenceWarning")?;
            SessionEvent::PersistenceWarning((!warning.is_empty()).then_some(warning))
        }
        wire::event_envelope::Deleted(event) => {
            let session_id = read_text(event?.get_session_id(), "sessionId")?;
            SessionEvent::Deleted {
                session_id: SessionId::from_str(&session_id)
                    .map_err(|_| RpcConversionError::InvalidSessionId)?,
            }
        }
    };
    Ok(SessionEventEnvelope {
        sequence: reader.get_sequence(),
        event,
    })
}

/// Writes a successful open payload.
pub fn write_open_success(
    mut builder: wire::open_success::Builder<'_>,
    value: &OpenSuccess,
) -> Result<(), RpcConversionError> {
    builder.set_session(value.session.clone());
    write_session_snapshot(builder.init_snapshot(), &value.snapshot)
}

/// Reads a successful open payload.
pub fn read_open_success(
    reader: wire::open_success::Reader<'_>,
) -> Result<OpenSuccess, RpcConversionError> {
    Ok(OpenSuccess {
        session: reader.get_session()?,
        snapshot: read_session_snapshot(reader.get_snapshot()?)?,
    })
}

/// Writes an open result union.
pub fn write_open_result(
    mut builder: wire::open_result::Builder<'_>,
    value: &OpenResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(success) => write_open_success(builder.init_success(), success),
        Err(error) => write_command_error(builder.reborrow().init_error(), error),
    }
}

/// Reads an open result union.
pub fn read_open_result(
    reader: wire::open_result::Reader<'_>,
) -> Result<OpenResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("OpenResult", error))?
    {
        wire::open_result::Success(success) => Ok(Ok(read_open_success(success?)?)),
        wire::open_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a startup selection result union.
pub fn write_startup_result(
    mut builder: wire::startup_result::Builder<'_>,
    value: &StartupResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(StartupSuccess::Draft(draft)) => {
            write_draft_defaults(builder.reborrow().init_draft(), draft)
        }
        Ok(StartupSuccess::Attached(attached)) => {
            write_open_success(builder.reborrow().init_attached(), attached)
        }
        Err(error) => write_command_error(builder.reborrow().init_error(), error),
    }
}

/// Reads a startup selection result union.
pub fn read_startup_result(
    reader: wire::startup_result::Reader<'_>,
) -> Result<StartupResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("StartupResult", error))?
    {
        wire::startup_result::Draft(draft) => {
            Ok(Ok(StartupSuccess::Draft(read_draft_defaults(draft?)?)))
        }
        wire::startup_result::Attached(attached) => Ok(Ok(StartupSuccess::Attached(Box::new(
            read_open_success(attached?)?,
        )))),
        wire::startup_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a successful materialization payload.
pub fn write_materialize_success(
    mut builder: wire::materialize_success::Builder<'_>,
    value: &MaterializeSuccess,
) -> Result<(), RpcConversionError> {
    builder.set_session(value.session.clone());
    write_session_snapshot(builder.reborrow().init_snapshot(), &value.snapshot)?;
    builder.set_run_id(value.run_id);
    Ok(())
}

/// Reads a successful materialization payload.
pub fn read_materialize_success(
    reader: wire::materialize_success::Reader<'_>,
) -> Result<MaterializeSuccess, RpcConversionError> {
    Ok(MaterializeSuccess {
        session: reader.get_session()?,
        snapshot: read_session_snapshot(reader.get_snapshot()?)?,
        run_id: reader.get_run_id(),
    })
}

/// Writes a materialization result union.
pub fn write_materialize_result(
    mut builder: wire::materialize_result::Builder<'_>,
    value: &MaterializeResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(success) => write_materialize_success(builder.reborrow().init_success(), success),
        Err(error) => write_command_error(builder.reborrow().init_error(), error),
    }
}

/// Reads a materialization result union.
pub fn read_materialize_result(
    reader: wire::materialize_result::Reader<'_>,
) -> Result<MaterializeResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("MaterializeResult", error))?
    {
        wire::materialize_result::Success(success) => Ok(Ok(read_materialize_success(success?)?)),
        wire::materialize_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a session-list result union.
pub fn write_session_list_result(
    mut builder: wire::session_list_result::Builder<'_>,
    value: &SessionListResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(sessions) => {
            let sessions_len = wire_struct_list_len::<wire::session_summary::Builder<'_>>(
                sessions.len(),
                "sessions",
            )?;
            let mut list = builder.reborrow().init_sessions(sessions_len);
            for (index, session) in (0..sessions_len).zip(sessions) {
                write_session_summary(list.reborrow().get(index), session)?;
            }
        }
        Err(error) => write_command_error(builder.reborrow().init_error(), error)?,
    }
    Ok(())
}

/// Reads a session-list result union.
pub fn read_session_list_result(
    reader: wire::session_list_result::Reader<'_>,
) -> Result<SessionListResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("SessionListResult", error))?
    {
        wire::session_list_result::Sessions(sessions) => {
            let sessions = sessions?;
            Ok(Ok((0..sessions.len())
                .map(|index| read_session_summary(sessions.get(index)))
                .collect::<Result<_, _>>()?))
        }
        wire::session_list_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a submit result union.
pub fn write_submit_result(
    mut builder: wire::submit_result::Builder<'_>,
    value: &SubmitResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(run_id) => builder.set_run_id(*run_id),
        Err(error) => write_command_error(builder.init_error(), error)?,
    }
    Ok(())
}

/// Reads a submit result union.
pub fn read_submit_result(
    reader: wire::submit_result::Reader<'_>,
) -> Result<SubmitResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("SubmitResult", error))?
    {
        wire::submit_result::RunId(run_id) => Ok(Ok(run_id)),
        wire::submit_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a no-payload command result union.
pub fn write_command_result(
    mut builder: wire::command_result::Builder<'_>,
    value: &CommandResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(()) => builder.set_ok(()),
        Err(error) => write_command_error(builder.init_error(), error)?,
    }
    Ok(())
}

/// Reads a no-payload command result union.
pub fn read_command_result(
    reader: wire::command_result::Reader<'_>,
) -> Result<CommandResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("CommandResult", error))?
    {
        wire::command_result::Ok(()) => Ok(Ok(())),
        wire::command_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes an exact-detach result and its authoritative post-detach attachment count.
pub fn write_detach_result(
    mut builder: wire::command_result::Builder<'_>,
    value: &DetachResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(attached_clients) => {
            builder.set_ok(());
            builder.set_attached_clients(*attached_clients);
        }
        Err(error) => write_command_error(builder.init_error(), error)?,
    }
    Ok(())
}

/// Reads an exact-detach result and its authoritative post-detach attachment count.
pub fn read_detach_result(
    reader: wire::command_result::Reader<'_>,
) -> Result<DetachResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("CommandResult", error))?
    {
        wire::command_result::Ok(()) => Ok(Ok(reader.get_attached_clients())),
        wire::command_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a job-list result union.
pub fn write_job_list_result(
    mut builder: wire::job_list_result::Builder<'_>,
    value: &JobListResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(jobs) => {
            let jobs_len =
                wire_struct_list_len::<wire::job_snapshot::Builder<'_>>(jobs.len(), "jobs")?;
            let list = builder.reborrow().init_jobs(jobs_len);
            write_job_list(list, jobs)?;
        }
        Err(error) => write_command_error(builder.reborrow().init_error(), error)?,
    }
    Ok(())
}

/// Reads a job-list result union.
pub fn read_job_list_result(
    reader: wire::job_list_result::Reader<'_>,
) -> Result<JobListResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("JobListResult", error))?
    {
        wire::job_list_result::Jobs(jobs) => Ok(Ok(read_job_list(jobs?)?)),
        wire::job_list_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}

/// Writes a single-job result union.
pub fn write_job_result(
    mut builder: wire::job_result::Builder<'_>,
    value: &JobResult,
) -> Result<(), RpcConversionError> {
    match value {
        Ok(job) => write_job_snapshot(builder.reborrow().init_job(), job)?,
        Err(error) => write_command_error(builder.reborrow().init_error(), error)?,
    }
    Ok(())
}

/// Reads a single-job result union.
pub fn read_job_result(
    reader: wire::job_result::Reader<'_>,
) -> Result<JobResult, RpcConversionError> {
    match reader
        .which()
        .map_err(|error| unknown_union("JobResult", error))?
    {
        wire::job_result::Job(job) => Ok(Ok(read_job_snapshot(job?)?)),
        wire::job_result::Error(error) => Ok(Err(read_command_error(error?)?)),
    }
}
