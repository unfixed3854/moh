//! Durable SQLite storage for text-file hashline anchors.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use crate::tools::blocking::{self, BlockingError};

const BUSY_RETRY_DELAY: Duration = Duration::from_millis(100);
const BUSY_RETRIES: usize = 3;
const SNAPSHOTS_SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS snapshots (
        path TEXT PRIMARY KEY,
        checksum TEXT NOT NULL,
        line_count INTEGER NOT NULL,
        hashes TEXT NOT NULL,
        lines TEXT NOT NULL DEFAULT '[]'
    )
"#;

/// A persisted anchor allocation for one normalized text-file snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnchorSnapshot {
    /// xxHash64 checksum of the normalized file contents.
    pub checksum: String,
    /// Number of normalized logical lines in the file.
    pub line_count: usize,
    /// One unique three-character anchor for every logical line.
    pub hashes: Vec<String>,
    /// Canonical normalized line identities for changed-snapshot anchor matching.
    ///
    /// Legacy rows written before this field existed load with an empty vector.
    pub lines: Vec<String>,
}

/// Errors raised while locating or operating Moh's durable anchor store.
#[derive(Debug, Error)]
pub enum AnchorStoreError {
    /// The platform did not provide a suitable per-user state directory.
    #[error("could not resolve Moh's platform state directory")]
    StateDirectoryUnavailable,
    /// The parent directory for the SQLite database could not be created.
    #[error("could not create the anchor-store directory {path}: {source}")]
    CreateDirectory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying file-system failure.
        source: std::io::Error,
    },
    /// A SQLite operation other than a bounded lock conflict failed.
    #[error("anchor-store {operation} failed: {source}")]
    Database {
        /// Operation that SQLite rejected.
        operation: &'static str,
        /// Underlying SQLite failure.
        source: rusqlite::Error,
    },
    /// SQLite remained busy or locked after the bounded retry policy.
    #[error("anchor store remained busy after {BUSY_RETRIES} retries")]
    DatabaseBusy,
    /// SQLite reported a corrupt database after the one allowed rebuild attempt.
    #[error("anchor store at {path} is corrupt after rebuilding")]
    CorruptDatabase {
        /// Database path that remained corrupt.
        path: PathBuf,
    },
    /// A corrupt database could not be moved aside before it was recreated.
    #[error("could not quarantine corrupt anchor store from {from} to {to}: {source}")]
    Quarantine {
        /// Original corrupt file path.
        from: PathBuf,
        /// Quarantine destination path.
        to: PathBuf,
        /// Underlying file-system failure.
        source: std::io::Error,
    },
    /// The system clock predates the Unix epoch, so a safe quarantine name could not be made.
    #[error("could not timestamp the corrupt anchor-store quarantine: {source}")]
    QuarantineTimestamp {
        /// Underlying system-clock failure.
        source: std::time::SystemTimeError,
    },
    /// A caller supplied a non-Unicode canonical path that SQLite cannot store losslessly.
    #[error("canonical path is not valid Unicode: {path}")]
    NonUnicodePath {
        /// Path rejected before persistence.
        path: PathBuf,
    },
    /// A snapshot did not satisfy the durable-anchor invariants.
    #[error("invalid anchor snapshot: {reason}")]
    InvalidSnapshot {
        /// Invariant that the snapshot violated.
        reason: &'static str,
    },
    /// A snapshot stored on disk had invalid JSON or anchor invariants.
    #[error("anchor store contains an invalid snapshot for {path}")]
    InvalidStoredSnapshot {
        /// Path whose persisted snapshot was invalid.
        path: PathBuf,
    },
    /// Hash anchors could not be encoded as JSON.
    #[error("could not encode anchor snapshot: {source}")]
    Encode {
        /// Underlying JSON encoding failure.
        source: serde_json::Error,
    },
    /// The store connection mutex was poisoned by a previous panic.
    #[error("anchor store connection is unavailable after a previous panic")]
    ConnectionPoisoned,
    /// Tokio could not complete a blocking anchor-store operation.
    #[error("anchor store blocking worker failed: {source}")]
    Worker {
        /// Underlying Tokio worker failure.
        #[source]
        source: tokio::task::JoinError,
    },
}

/// Durable SQLite-backed anchor snapshots.
pub struct AnchorStore {
    connection: Arc<Mutex<Connection>>,
}

/// Resolves Moh's durable platform state directory.
pub fn moh_state_dir() -> Result<PathBuf, AnchorStoreError> {
    let directories =
        ProjectDirs::from("", "", "moh").ok_or(AnchorStoreError::StateDirectoryUnavailable)?;
    Ok(directories
        .state_dir()
        .unwrap_or_else(|| directories.data_local_dir())
        .to_path_buf())
}

impl AnchorStore {
    /// Opens the SQLite store at `path`, creating its parent directory and schema when necessary.
    pub async fn open_at(path: &Path) -> Result<Self, AnchorStoreError> {
        let path = path.to_path_buf();
        blocking::run(move || open_sync(&path))
            .await
            .map_err(Self::from_blocking)
    }

    /// Loads the stored snapshot for a canonical file path, if one exists.
    pub async fn load(
        &self,
        canonical_path: &Path,
    ) -> Result<Option<AnchorSnapshot>, AnchorStoreError> {
        let connection = Arc::clone(&self.connection);
        let canonical_path = canonical_path.to_path_buf();
        blocking::run(move || load_sync(&connection, &canonical_path))
            .await
            .map_err(Self::from_blocking)
    }

    /// Atomically saves a snapshot for a canonical file path.
    pub async fn save(
        &self,
        canonical_path: &Path,
        snapshot: &AnchorSnapshot,
    ) -> Result<(), AnchorStoreError> {
        let connection = Arc::clone(&self.connection);
        let canonical_path = canonical_path.to_path_buf();
        let snapshot = snapshot.clone();
        blocking::run(move || save_sync(&connection, &canonical_path, &snapshot))
            .await
            .map_err(Self::from_blocking)
    }

    fn from_blocking(error: BlockingError<AnchorStoreError>) -> AnchorStoreError {
        match error {
            BlockingError::Operation(error) => error,
            BlockingError::Worker(source) => AnchorStoreError::Worker { source },
        }
    }
}

fn open_sync(path: &Path) -> Result<AnchorStore, AnchorStoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| AnchorStoreError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let connection = match initialize_database(path) {
        Ok(connection) => connection,
        Err(InitializationError::Corrupt) => {
            quarantine_corrupt_database(path)?;
            match initialize_database(path) {
                Ok(connection) => connection,
                Err(InitializationError::Corrupt) => {
                    return Err(AnchorStoreError::CorruptDatabase {
                        path: path.to_path_buf(),
                    });
                }
                Err(InitializationError::Database(source)) => {
                    return Err(map_database_error("rebuild", source));
                }
            }
        }
        Err(InitializationError::Database(source)) => {
            return Err(map_database_error("initialize", source));
        }
    };

    Ok(AnchorStore {
        connection: Arc::new(Mutex::new(connection)),
    })
}

fn load_sync(
    connection: &Mutex<Connection>,
    canonical_path: &Path,
) -> Result<Option<AnchorSnapshot>, AnchorStoreError> {
    let path = sqlite_path(canonical_path)?;
    let connection = connection
        .lock()
        .map_err(|_| AnchorStoreError::ConnectionPoisoned)?;
    let row = retry_busy("load", || {
        connection
            .query_row(
                "SELECT checksum, line_count, hashes, lines FROM snapshots WHERE path = ?1",
                [&path],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
    })?;

    let Some((checksum, line_count, hashes, lines)) = row else {
        return Ok(None);
    };
    let line_count =
        usize::try_from(line_count).map_err(|_| AnchorStoreError::InvalidStoredSnapshot {
            path: canonical_path.to_path_buf(),
        })?;
    let hashes = serde_json::from_str::<Vec<String>>(&hashes).map_err(|_| {
        AnchorStoreError::InvalidStoredSnapshot {
            path: canonical_path.to_path_buf(),
        }
    })?;
    let lines = serde_json::from_str::<Vec<String>>(&lines).map_err(|_| {
        AnchorStoreError::InvalidStoredSnapshot {
            path: canonical_path.to_path_buf(),
        }
    })?;
    let snapshot = AnchorSnapshot {
        checksum,
        line_count,
        hashes,
        lines,
    };
    validate_snapshot(&snapshot, true).map_err(|_| AnchorStoreError::InvalidStoredSnapshot {
        path: canonical_path.to_path_buf(),
    })?;
    Ok(Some(snapshot))
}

fn save_sync(
    connection: &Mutex<Connection>,
    canonical_path: &Path,
    snapshot: &AnchorSnapshot,
) -> Result<(), AnchorStoreError> {
    validate_snapshot(snapshot, false)?;
    let path = sqlite_path(canonical_path)?;
    let hashes = serde_json::to_string(&snapshot.hashes)
        .map_err(|source| AnchorStoreError::Encode { source })?;
    let lines = serde_json::to_string(&snapshot.lines)
        .map_err(|source| AnchorStoreError::Encode { source })?;
    let line_count =
        i64::try_from(snapshot.line_count).map_err(|_| AnchorStoreError::InvalidSnapshot {
            reason: "line count exceeds SQLite's integer range",
        })?;
    let mut connection = connection
        .lock()
        .map_err(|_| AnchorStoreError::ConnectionPoisoned)?;

    retry_busy("save", || {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
                "INSERT INTO snapshots (path, checksum, line_count, hashes, lines) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(path) DO UPDATE SET checksum = excluded.checksum, \
                 line_count = excluded.line_count, hashes = excluded.hashes, lines = excluded.lines",
                params![path, snapshot.checksum, line_count, hashes, lines],
            )?;
        transaction.commit()
    })
}

enum InitializationError {
    Database(rusqlite::Error),
    Corrupt,
}

fn initialize_database(path: &Path) -> Result<Connection, InitializationError> {
    let connection =
        retry_sqlite(|| Connection::open(path)).map_err(classify_initialization_error)?;
    retry_sqlite(|| connection.execute_batch("PRAGMA journal_mode = WAL;"))
        .map_err(classify_initialization_error)?;
    let quick_check = retry_sqlite(|| {
        connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
    })
    .map_err(classify_initialization_error)?;
    if quick_check != "ok" {
        return Err(InitializationError::Corrupt);
    }
    retry_sqlite(|| connection.execute_batch(SNAPSHOTS_SCHEMA))
        .map_err(classify_initialization_error)?;
    retry_sqlite(|| ensure_snapshot_lines_column(&connection))
        .map_err(classify_initialization_error)?;
    Ok(connection)
}

fn ensure_snapshot_lines_column(connection: &Connection) -> rusqlite::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(snapshots)")?;
    let has_lines = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "lines");
    if has_lines {
        Ok(())
    } else {
        connection
            .execute_batch("ALTER TABLE snapshots ADD COLUMN lines TEXT NOT NULL DEFAULT '[]'")
    }
}

fn classify_initialization_error(source: rusqlite::Error) -> InitializationError {
    if is_corruption(&source) {
        InitializationError::Corrupt
    } else {
        InitializationError::Database(source)
    }
}

fn quarantine_corrupt_database(path: &Path) -> Result<(), AnchorStoreError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| AnchorStoreError::QuarantineTimestamp { source })?
        .as_millis();
    let quarantine = PathBuf::from(format!("{}.corrupt-{timestamp}", path.display()));
    move_if_present(path, &quarantine)?;

    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        let quarantined_sidecar = PathBuf::from(format!("{}{}", quarantine.display(), suffix));
        move_if_present(&sidecar, &quarantined_sidecar)?;
    }
    Ok(())
}

fn move_if_present(from: &Path, to: &Path) -> Result<(), AnchorStoreError> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AnchorStoreError::Quarantine {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            source,
        }),
    }
}

fn sqlite_path(path: &Path) -> Result<String, AnchorStoreError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| AnchorStoreError::NonUnicodePath {
            path: path.to_path_buf(),
        })
}

fn validate_snapshot(
    snapshot: &AnchorSnapshot,
    allow_legacy_lines: bool,
) -> Result<(), AnchorStoreError> {
    if snapshot.line_count != snapshot.hashes.len() {
        return Err(AnchorStoreError::InvalidSnapshot {
            reason: "line count does not match the number of anchors",
        });
    }
    if snapshot.lines.len() != snapshot.line_count
        && !(allow_legacy_lines && snapshot.line_count > 0 && snapshot.lines.is_empty())
    {
        return Err(AnchorStoreError::InvalidSnapshot {
            reason: "line identity count does not match the number of anchors",
        });
    }

    let mut anchors = HashSet::with_capacity(snapshot.hashes.len());
    for anchor in &snapshot.hashes {
        if anchor.len() != 3
            || !anchor
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Err(AnchorStoreError::InvalidSnapshot {
                reason: "anchors must be three ASCII alphanumeric characters",
            });
        }
        if !anchors.insert(anchor) {
            return Err(AnchorStoreError::InvalidSnapshot {
                reason: "anchors must be unique within a snapshot",
            });
        }
    }
    Ok(())
}

fn retry_busy<T>(
    operation: &'static str,
    action: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T, AnchorStoreError> {
    retry_sqlite(action).map_err(|source| map_database_error(operation, source))
}

fn retry_sqlite<T>(mut action: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    for attempt in 0..=BUSY_RETRIES {
        match action() {
            Ok(value) => return Ok(value),
            Err(error) if is_busy(&error) && attempt < BUSY_RETRIES => {
                thread::sleep(BUSY_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the retry loop always returns")
}

fn map_database_error(operation: &'static str, source: rusqlite::Error) -> AnchorStoreError {
    if is_busy(&source) {
        AnchorStoreError::DatabaseBusy
    } else {
        AnchorStoreError::Database { operation, source }
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}
