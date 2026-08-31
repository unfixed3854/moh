//! Conversation-lifetime identities and checksums for files seen by the agent.

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use xxhash_rust::xxh64::xxh64;

use super::write::WriteToolError;

#[derive(Clone)]
pub(crate) struct FileObservation {
    pub(crate) canonical_path: PathBuf,
    pub(crate) checksum: u64,
}

/// In-memory file versions observed during the current conversation.
#[derive(Clone, Default)]
pub(crate) struct FileObservations {
    entries: Arc<Mutex<HashMap<PathBuf, FileObservation>>>,
}

impl FileObservations {
    pub(crate) fn get(&self, path: &Path) -> Result<Option<FileObservation>, WriteToolError> {
        let key = lexical_identity(path);
        self.entries
            .lock()
            .map(|entries| entries.get(&key).cloned())
            .map_err(|_| WriteToolError::Runtime)
    }

    pub(crate) fn record(
        &self,
        requested_path: &Path,
        canonical_path: PathBuf,
        bytes: &[u8],
    ) -> Result<(), WriteToolError> {
        let observation = FileObservation {
            canonical_path: canonical_path.clone(),
            checksum: xxh64(bytes, 0),
        };
        let mut entries = self.entries.lock().map_err(|_| WriteToolError::Runtime)?;
        entries.insert(lexical_identity(requested_path), observation.clone());
        entries.insert(lexical_identity(&canonical_path), observation);
        Ok(())
    }

    pub(crate) fn forget(&self, observation: &FileObservation) -> Result<(), WriteToolError> {
        self.entries
            .lock()
            .map_err(|_| WriteToolError::Runtime)?
            .retain(|_, entry| entry.canonical_path != observation.canonical_path);
        Ok(())
    }
}

fn lexical_identity(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}
