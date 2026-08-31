//! Whole-file writer used by Moh's agent runtime.

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
    read::ReadServiceFactory,
};

/// Arguments accepted by the whole-file `write` tool.
#[derive(Debug, Deserialize, JsonSchema, Validate)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// Cwd-relative or absolute path to write.
    #[garde(length(min = 1))]
    pub path: String,
    /// Complete contents that the target file should contain.
    #[garde(skip)]
    pub content: String,
}

/// Failure returned to Rig as the writer's model-visible tool error.
#[derive(Debug, Error)]
pub enum WriteToolError {
    /// The call supplied an unusable target path.
    #[error("[E_INVALID_ARGUMENT] path must not be empty")]
    InvalidArgument,
    /// An existing target has not been observed in this conversation.
    #[error("[E_NOT_READ] existing file must be read before it can be overwritten")]
    NotRead,
    /// The target no longer matches the version observed by the agent.
    #[error("[E_STALE_READ] file changed after it was read; read it again before overwriting")]
    StaleRead,
    /// The requested path could not be written.
    #[error("[E_ACCESS] file could not be written")]
    Access,
    /// In-memory conversation state was unavailable.
    #[error("[E_RUNTIME] write tool state is unavailable")]
    Runtime,
    /// Tokio could not complete the blocking worker that performed the write.
    #[error("[E_RUNTIME] write tool worker failed")]
    Worker(#[source] tokio::task::JoinError),
}

/// Async, cwd-bound whole-file writer.
pub struct WriteService {
    cwd: PathBuf,
    observations: FileObservations,
}

/// Creates writers that share conversation-lifetime observations with readers.
#[derive(Clone)]
pub struct WriteServiceFactory {
    observations: FileObservations,
}

impl WriteServiceFactory {
    /// Shares the supplied reader factory's in-memory observation state.
    pub fn sharing_reads(reads: &ReadServiceFactory) -> Self {
        Self {
            observations: reads.observations(),
        }
    }

    /// Binds a writer to the cwd supplied for one agent run.
    pub fn for_cwd(&self, cwd: PathBuf) -> WriteService {
        WriteService {
            cwd,
            observations: self.observations.clone(),
        }
    }
}

impl WriteService {
    /// Returns the model-facing write-tool description.
    pub fn description() -> &'static str {
        "Create a new text file or completely rewrite an existing file. Existing files must have been read first, and the write fails if the file changed after that read. Content replaces the entire file."
    }

    /// Creates or replaces a whole file.
    pub async fn write(&self, args: WriteArgs) -> Result<ToolOutput, WriteToolError> {
        args.validate()
            .map_err(|_| WriteToolError::InvalidArgument)?;
        let cwd = self.cwd.clone();
        let observations = self.observations.clone();
        blocking::run(move || write_sync(cwd, observations, args))
            .await
            .map_err(Self::from_blocking)
    }

    fn from_blocking(error: BlockingError<WriteToolError>) -> WriteToolError {
        match error {
            BlockingError::Operation(error) => error,
            BlockingError::Worker(source) => WriteToolError::Worker(source),
        }
    }
}

fn write_sync(
    cwd: PathBuf,
    observations: FileObservations,
    args: WriteArgs,
) -> Result<ToolOutput, WriteToolError> {
    let requested = PathBuf::from(&args.path);
    let path = if requested.is_absolute() {
        requested
    } else {
        cwd.join(requested)
    };
    let (write_path, observed_checksum, permissions) = if path.exists() {
        let canonical = fs::canonicalize(&path).map_err(|_| WriteToolError::Access)?;
        let observation = observations
            .get(&path)?
            .or(observations.get(&canonical)?)
            .ok_or(WriteToolError::NotRead)?;
        let current = fs::read(&canonical).map_err(|_| WriteToolError::Access)?;
        if observation.canonical_path != canonical || observation.checksum != xxh64(&current, 0) {
            observations.forget(&observation)?;
            return Err(WriteToolError::StaleRead);
        }
        let permissions = fs::metadata(&canonical)
            .map_err(|_| WriteToolError::Access)?
            .permissions();
        (canonical, Some(observation), Some(permissions))
    } else if let Some(observation) = observations.get(&path)? {
        observations.forget(&observation)?;
        return Err(WriteToolError::StaleRead);
    } else {
        (path.clone(), None, None)
    };
    let parent = write_path.parent().ok_or(WriteToolError::Access)?;
    fs::create_dir_all(parent).map_err(|_| WriteToolError::Access)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|_| WriteToolError::Access)?;
    staged
        .write_all(args.content.as_bytes())
        .map_err(|_| WriteToolError::Access)?;
    if let Some(permissions) = permissions {
        staged
            .as_file()
            .set_permissions(permissions)
            .map_err(|_| WriteToolError::Access)?;
    }
    staged
        .as_file()
        .sync_all()
        .map_err(|_| WriteToolError::Access)?;
    if let Some(observation) = observed_checksum {
        let current = fs::read(&write_path).map_err(|_| WriteToolError::StaleRead)?;
        if observation.checksum != xxh64(&current, 0) {
            observations.forget(&observation)?;
            return Err(WriteToolError::StaleRead);
        }
        staged
            .persist(&write_path)
            .map_err(|_| WriteToolError::Access)?;
    } else {
        staged.persist_noclobber(&write_path).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                WriteToolError::NotRead
            } else {
                WriteToolError::Access
            }
        })?;
    }
    let canonical = fs::canonicalize(&write_path).map_err(|_| WriteToolError::Access)?;
    observations.record(&path, canonical, args.content.as_bytes())?;
    Ok(ToolOutput::text(format!(
        "Successfully wrote {} bytes to {}",
        args.content.len(),
        args.path
    )))
}
