//! Hash-anchored line-range editor used by Moh's agent runtime.

use std::{fs, io::Write, path::PathBuf};

use garde::Validate;
use rig::tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use thiserror::Error;
use xxhash_rust::xxh64::xxh64;

use super::{
    blocking::{self, BlockingError},
    observations::FileObservations,
    read::{ReadArgs, ReadService, ReadServiceFactory, logical_lines, normalize_text},
};

/// Arguments accepted by the hash-anchored `edit` tool.
#[derive(Debug, Deserialize, JsonSchema, Validate)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Cwd-relative or absolute path to edit.
    #[garde(length(min = 1))]
    pub path: String,
    /// First three-character line anchor to remove, inclusive.
    #[garde(pattern(r"^[A-Za-z0-9]{3}$"))]
    pub remove_from: String,
    /// Last three-character line anchor to remove, inclusive.
    #[garde(pattern(r"^[A-Za-z0-9]{3}$"))]
    pub remove_to: String,
    /// Replacement content with exactly one logical line per element.
    #[garde(inner(pattern(r"^[^\r\n]*$")))]
    pub replacement_lines: Vec<String>,
}

/// Failure returned to Rig as the editor's model-visible tool error.
#[derive(Debug, Error)]
pub enum EditToolError {
    /// The request shape or values cannot describe a safe edit.
    #[error("[E_INVALID_ARGUMENT] {0}")]
    InvalidArgument(&'static str),
    /// The target was not read during the current process.
    #[error("[E_NOT_READ] existing file must be read before it can be edited")]
    NotRead,
    /// The target changed after it was read.
    #[error("[E_STALE_READ] file changed after it was read; read it again before editing")]
    StaleRead,
    /// One or both requested anchors do not identify the observed file.
    #[error("[E_STALE_ANCHOR] edit anchors are not present in the observed file; read it again")]
    StaleAnchor,
    /// The requested path could not be edited.
    #[error("[E_ACCESS] file could not be edited")]
    Access,
    /// In-memory or durable edit state was unavailable.
    #[error("[E_RUNTIME] edit tool state is unavailable")]
    Runtime,
    /// Tokio could not complete a blocking worker used by the edit.
    #[error("[E_RUNTIME] edit tool worker failed")]
    Worker(#[source] tokio::task::JoinError),
}

/// Async, cwd-bound hash-anchored editor.
pub struct EditService {
    cwd: PathBuf,
    reader: ReadService,
    observations: FileObservations,
}

/// Creates editors that share observations and anchors with readers.
#[derive(Clone)]
pub struct EditServiceFactory {
    reads: ReadServiceFactory,
    observations: FileObservations,
}

impl EditServiceFactory {
    /// Shares the supplied reader factory's observation and anchor state.
    pub fn sharing_reads(reads: &ReadServiceFactory) -> Self {
        Self {
            reads: reads.clone(),
            observations: reads.observations(),
        }
    }

    /// Binds an editor to the cwd supplied for one agent run.
    pub fn for_cwd(&self, cwd: PathBuf) -> EditService {
        EditService {
            reader: self.reads.for_cwd(cwd.clone()),
            cwd,
            observations: self.observations.clone(),
        }
    }
}

impl EditService {
    /// Returns the model-facing edit-tool description.
    pub fn description() -> &'static str {
        "Replace one inclusive range of HASH│content lines in an existing text file. Use remove_from and remove_to anchors returned by read, and provide exactly one logical line per replacement_lines entry. The file must have been read first."
    }

    /// Edits one inclusive hash-anchored range and returns refreshed anchors.
    pub async fn edit(&self, args: EditArgs) -> Result<ToolOutput, EditToolError> {
        args.validate()
            .map_err(|_| EditToolError::InvalidArgument("invalid edit arguments"))?;
        let cwd = self.cwd.clone();
        let observations = self.observations.clone();
        let path_arg = args.path.clone();
        let payload = blocking::run(move || collect_edit_payload(cwd, observations, path_arg))
            .await
            .map_err(Self::from_blocking)?;
        let snapshot = self
            .reader
            .stored_snapshot(payload.canonical.clone())
            .await
            .map_err(|_| EditToolError::Runtime)?
            .ok_or(EditToolError::NotRead)?;
        let from = snapshot
            .hashes
            .iter()
            .position(|hash| hash == &args.remove_from)
            .ok_or(EditToolError::StaleAnchor)?;
        let to = snapshot
            .hashes
            .iter()
            .position(|hash| hash == &args.remove_to)
            .ok_or(EditToolError::StaleAnchor)?;
        if from > to {
            return Err(EditToolError::InvalidArgument(
                "remove_from must precede or equal remove_to",
            ));
        }
        let (normalized, _) = normalize_text(&payload.bytes);
        let mut lines = logical_lines(&normalized);
        lines.splice(from..=to, args.replacement_lines);
        let replacement = encode_lines(&payload.bytes, &lines);
        let observations = self.observations.clone();
        blocking::run(move || {
            replace_file(&payload.canonical, &payload.bytes, &replacement)?;
            observations
                .record(&payload.path, payload.canonical, &replacement)
                .map_err(|_| EditToolError::Runtime)
        })
        .await
        .map_err(Self::from_blocking)?;
        self.reader
            .read(ReadArgs::path(args.path))
            .await
            .map_err(|_| EditToolError::Runtime)
    }

    fn from_blocking(error: BlockingError<EditToolError>) -> EditToolError {
        match error {
            BlockingError::Operation(error) => error,
            BlockingError::Worker(source) => EditToolError::Worker(source),
        }
    }
}

struct EditPayload {
    path: PathBuf,
    canonical: PathBuf,
    bytes: Vec<u8>,
}

fn collect_edit_payload(
    cwd: PathBuf,
    observations: FileObservations,
    path_arg: String,
) -> Result<EditPayload, EditToolError> {
    let requested = PathBuf::from(path_arg);
    let path = if requested.is_absolute() {
        requested
    } else {
        cwd.join(requested)
    };
    let requested_observation = observations
        .get(&path)
        .map_err(|_| EditToolError::Runtime)?;
    let canonical = match fs::canonicalize(&path) {
        Ok(canonical) => canonical,
        Err(_) => {
            if let Some(observation) = requested_observation {
                observations
                    .forget(&observation)
                    .map_err(|_| EditToolError::Runtime)?;
                return Err(EditToolError::StaleRead);
            }
            return Err(EditToolError::Access);
        }
    };
    let observation = requested_observation
        .or(observations
            .get(&canonical)
            .map_err(|_| EditToolError::Runtime)?)
        .ok_or(EditToolError::NotRead)?;
    let bytes = match fs::read(&canonical) {
        Ok(bytes) => bytes,
        Err(_) => {
            observations
                .forget(&observation)
                .map_err(|_| EditToolError::Runtime)?;
            return Err(EditToolError::StaleRead);
        }
    };
    if observation.canonical_path != canonical || observation.checksum != xxh64(&bytes, 0) {
        observations
            .forget(&observation)
            .map_err(|_| EditToolError::Runtime)?;
        return Err(EditToolError::StaleRead);
    }
    Ok(EditPayload {
        path,
        canonical,
        bytes,
    })
}

fn encode_lines(original: &[u8], lines: &[String]) -> Vec<u8> {
    let separator = if original.windows(2).any(|pair| pair == b"\r\n") {
        "\r\n"
    } else if original.contains(&b'\r') {
        "\r"
    } else {
        "\n"
    };
    let had_final_newline = original.ends_with(b"\n") || original.ends_with(b"\r");
    let mut text = lines.join(separator);
    if had_final_newline && !lines.is_empty() {
        text.push_str(separator);
    }
    let mut encoded = Vec::with_capacity(text.len() + 3);
    if original.starts_with(&[0xef, 0xbb, 0xbf]) {
        encoded.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    encoded.extend_from_slice(text.as_bytes());
    encoded
}

fn replace_file(
    path: &std::path::Path,
    expected: &[u8],
    replacement: &[u8],
) -> Result<(), EditToolError> {
    let parent = path.parent().ok_or(EditToolError::Access)?;
    let permissions = fs::metadata(path)
        .map_err(|_| EditToolError::Access)?
        .permissions();
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|_| EditToolError::Access)?;
    staged
        .write_all(replacement)
        .map_err(|_| EditToolError::Access)?;
    staged
        .as_file()
        .set_permissions(permissions)
        .map_err(|_| EditToolError::Access)?;
    staged
        .as_file()
        .sync_all()
        .map_err(|_| EditToolError::Access)?;
    let current = fs::read(path).map_err(|_| EditToolError::StaleRead)?;
    if current != expected {
        return Err(EditToolError::StaleRead);
    }
    staged.persist(path).map_err(|_| EditToolError::Access)?;
    Ok(())
}
