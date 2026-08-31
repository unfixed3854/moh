//! Rig adapters used by Moh's runtime.

pub mod bash_tool;
mod codex;
/// Non-blocking Rig adapter for hash-anchored text edits.
pub mod edit_tool;
pub mod job_tool;
/// Rig adapter for authoritative execution-plan replacements.
pub mod plan_tool;
mod title;

/// Rig-backed Codex run engine and its explicit agent configuration.
pub use codex::{
    ActiveModel, ActiveReasoning, AgentConfig, CodexRunEngine, CodexSessionEngineFactory,
    DEFAULT_MODEL, ReasoningLevel,
};
/// Production Codex adapter for independent session-title requests.
pub use title::CodexTitleGenerator;
/// Non-blocking Rig adapter for the text reader.
pub mod read_tool;
/// Non-blocking Rig adapter for the whole-file writer.
pub mod write_tool;
