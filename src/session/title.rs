//! Session title validation, normalization, and generation boundaries.

use std::fmt;

use futures::future::BoxFuture;
use thiserror::Error;

use super::SessionId;
use crate::runtime::rig::ReasoningLevel;

/// The greatest permitted number of Unicode scalar values in a session title.
pub const MAX_SESSION_TITLE_SCALARS: usize = 64;

const FALLBACK_EMPTY_TITLE: &str = "Untitled session";

/// Validated display metadata for a session.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionTitle(String);

impl SessionTitle {
    /// Validates and owns a session title.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionTitleParseError> {
        let value = value.into();
        let count = value.chars().count();
        if count == 0
            || count > MAX_SESSION_TITLE_SCALARS
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(SessionTitleParseError);
        }
        Ok(Self(value))
    }

    /// Returns the validated title text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A session title that violates the length, whitespace, or control rules.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("session titles must contain 1-64 scalars without surrounding whitespace or controls")]
pub struct SessionTitleParseError;

/// The mechanism that last set a session title.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TitleSource {
    /// A deterministic shortening of the session's first message.
    Fallback,
    /// An asynchronous title generated from the first message.
    Generated,
    /// A title directly entered by the user.
    Manual,
}

impl TitleSource {
    #[allow(dead_code)] // Consumed by the staged durable session-store migration.
    pub(crate) const fn as_stored(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Generated => "generated",
            Self::Manual => "manual",
        }
    }

    #[allow(dead_code)] // Consumed by the staged durable session-store migration.
    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        match value {
            "fallback" => Some(Self::Fallback),
            "generated" => Some(Self::Generated),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// Builds the deterministic first-message title used until generation completes.
pub fn fallback_title(first_message: &str) -> SessionTitle {
    let without_controls = strip_terminal_controls_preserving_whitespace(first_message);
    let normalized = collapse_whitespace(&without_controls);
    let normalized = if normalized.is_empty() {
        FALLBACK_EMPTY_TITLE.to_owned()
    } else {
        normalized
    };

    SessionTitle::parse(truncate_title(&normalized))
        .expect("normalized fallback titles always satisfy session title validation")
}

/// Normalizes raw generated text into one valid session title.
pub fn sanitize_generated_title(generated: &str) -> Option<SessionTitle> {
    let line = generated.lines().find(|line| !line.trim().is_empty())?;
    let without_controls = strip_terminal_controls(line);
    let plain = trim_paired_surrounding(&without_controls);
    let normalized = collapse_whitespace(plain);

    SessionTitle::parse(truncate_title(&normalized)).ok()
}

/// Inputs required to generate a title without exposing conversation history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleRequest {
    /// The session whose title is being generated.
    pub session_id: SessionId,
    /// The selected provider model identifier.
    pub model: String,
    /// The reasoning level used for the title request.
    pub reasoning: ReasoningLevel,
    /// The first user message, and the only conversation text sent to the generator.
    pub first_message: String,
    /// The title revision that must still be current when applying the result.
    pub expected_revision: u64,
}

/// A sanitized category of title-generation failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TitleGenerationError {
    /// Provider authentication did not permit generation.
    #[error("title generation authentication failed")]
    Authentication,
    /// Provider transport failed before a title was generated.
    #[error("title generation transport failed")]
    Transport,
    /// Provider completion failed without exposing its response body.
    #[error("title generation completion failed")]
    Completion,
}

/// Asynchronously generates raw title text from one first-message request.
pub trait SessionTitleGenerator: Send + Sync {
    /// Generates raw title text; callers must sanitize successful output before use.
    fn generate(
        &self,
        request: TitleRequest,
    ) -> BoxFuture<'static, Result<String, TitleGenerationError>>;
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_title(value: &str) -> String {
    if value.chars().count() <= MAX_SESSION_TITLE_SCALARS {
        return value.to_owned();
    }

    let prefix = value
        .chars()
        .take(MAX_SESSION_TITLE_SCALARS - 1)
        .collect::<String>();
    let word_boundary = prefix.rfind(' ').filter(|index| *index > 0);
    let prefix = word_boundary.map_or(prefix.as_str(), |index| &prefix[..index]);
    format!("{prefix}…")
}

fn trim_paired_surrounding(value: &str) -> &str {
    let mut value = value.trim();

    loop {
        let Some(inner) = paired_inner(value) else {
            return value;
        };
        value = inner.trim();
    }
}

fn paired_inner(value: &str) -> Option<&str> {
    [("\"", "\""), ("'", "'"), ("`", "`"), ("*", "*"), ("_", "_")]
        .into_iter()
        .find_map(|(start, end)| value.strip_prefix(start)?.strip_suffix(end))
}

fn strip_terminal_controls(value: &str) -> String {
    strip_terminal_controls_with_whitespace(value, false)
}

fn strip_terminal_controls_preserving_whitespace(value: &str) -> String {
    strip_terminal_controls_with_whitespace(value, true)
}

fn strip_terminal_controls_with_whitespace(value: &str, preserve_whitespace: bool) -> String {
    let mut characters = value.chars().peekable();
    let mut output = String::new();

    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' | '\u{009b}' => consume_escape_sequence(&mut characters),
            character if preserve_whitespace && character.is_whitespace() => {
                output.push(character);
            }
            character if character.is_control() => {}
            character => output.push(character),
        }
    }

    output
}

fn consume_escape_sequence(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match characters.peek() {
        Some('[') => {
            characters.next();
            for character in characters.by_ref() {
                if ('@'..='~').contains(&character) {
                    break;
                }
            }
        }
        Some(']') => {
            characters.next();
            while let Some(character) = characters.next() {
                if character == '\u{7}' {
                    break;
                }
                if character == '\u{1b}' && characters.next_if_eq(&'\\').is_some() {
                    break;
                }
            }
        }
        Some(_) => {
            characters.next();
        }
        None => {}
    }
}
