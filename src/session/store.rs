//! Versioned SQLite persistence for committed session state.

use std::{
    ffi::OsString,
    fmt, fs,
    fs::File,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::BoxFuture;
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

#[cfg(not(unix))]
use std::fs::OpenOptions;

use super::{
    DurableTurn, MaterializeSession, PlanItem, PlanStatus, RunFailureSnapshot, SessionId,
    SessionListScope, SessionName, SessionRecord, SessionSelector, SessionSettings, SessionSummary,
    SessionTitle, TitleSource, TranscriptItem, TurnStatus, fallback_title,
};
use crate::{
    harness::{Message, Role, RunFailureKind, RunStage},
    runtime::rig::ReasoningLevel,
    tools::blocking::{self, BlockingError},
};

const SCHEMA_VERSION: u32 = 3;
const SESSION_SCHEMA: &str = r#"CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    title_source TEXT NOT NULL CHECK (title_source IN ('fallback', 'generated', 'manual')),
    title_revision INTEGER NOT NULL,
    cwd BLOB NOT NULL,
    model TEXT NOT NULL,
    reasoning TEXT NOT NULL,
    context_tokens INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_activity TEXT NOT NULL
);
CREATE INDEX sessions_by_cwd_title ON sessions(cwd, title);
CREATE TABLE messages (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    text TEXT NOT NULL,
    PRIMARY KEY (session_id, position)
);
CREATE TABLE transcript_items (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('user', 'assistant', 'tool_started', 'failed', 'cancelled')),
    text TEXT,
    run_id INTEGER,
    call_id TEXT,
    tool_name TEXT,
    arguments_json TEXT,
    failure_stage TEXT,
    failure_kind TEXT,
    failure_http_status INTEGER,
    failure_retryable INTEGER CHECK (failure_retryable IN (0, 1)),
    failure_message TEXT,
    PRIMARY KEY (session_id, position)
);
CREATE TABLE turns (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    run_id INTEGER NOT NULL,
    prompt_position INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed', 'cancelled', 'interrupted')),
    PRIMARY KEY (session_id, ordinal)
);
CREATE TABLE plan_items (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    step TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'in_progress', 'completed', 'blocked', 'cancelled')),
    PRIMARY KEY (session_id, position)
);
PRAGMA user_version = 3;
"#;

const ADD_PLAN_TABLE_TO_SESSION_V2: &str = r#"CREATE TABLE plan_items (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    step TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'in_progress', 'completed', 'blocked', 'cancelled')),
    PRIMARY KEY (session_id, position)
);
PRAGMA user_version = 3;
"#;

/// A non-fatal condition encountered while opening the durable session store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreWarning {
    /// SQLite corruption was moved aside and replaced with a fresh versioned store.
    CorruptDatabaseQuarantined {
        /// Path containing the quarantined database bytes.
        path: PathBuf,
    },
}

impl StoreWarning {
    /// Renders the non-sensitive warning text advertised during protocol negotiation.
    pub fn sanitized_message(&self) -> String {
        match self {
            Self::CorruptDatabaseQuarantined { path } => format!(
                "corrupt session store was quarantined at {}",
                path.display()
            ),
        }
    }
}

/// An initialized store and any non-fatal startup warnings.
#[derive(Debug)]
pub struct OpenedSessionStore {
    /// The ready durable repository.
    pub store: SessionStore,
    /// Startup warnings that should be surfaced to clients.
    pub warnings: Vec<StoreWarning>,
}

/// Failures while opening or operating the durable session repository.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// A stable selector did not resolve to a stored session.
    #[error("session {selector} was not found")]
    NotFound {
        /// Stable identifier or CWD-scoped name that did not resolve.
        selector: String,
    },
    /// An exact CWD-scoped title resolved to more than one durable session.
    #[error("session title {title} is ambiguous; select a session ID")]
    AmbiguousTitle {
        /// Exact title that matched multiple sessions.
        title: SessionTitle,
        /// Stable matching identifiers in ascending order.
        ids: Vec<SessionId>,
    },
    /// A name is already in use within the requested working directory.
    #[error("session name {name} is already in use in this working directory")]
    NameConflict {
        /// Conflicting validated name.
        name: SessionName,
    },
    /// The database schema is newer than this Moh build understands.
    #[error("session-store schema version {found} is unsupported; maximum is {supported}")]
    UnsupportedSchemaVersion {
        /// Version read from SQLite.
        found: u32,
        /// Latest version understood by this build.
        supported: u32,
    },
    /// A stored value cannot be represented by the session domain.
    #[error("session store contains invalid {field}: {reason}")]
    InvalidStoredData {
        /// Logical field that failed validation or conversion.
        field: &'static str,
        /// Non-sensitive description of the violated invariant.
        reason: String,
    },
    /// A caller value cannot be represented safely by SQLite.
    #[error("session {field} exceeds SQLite's supported range")]
    ValueOutOfRange {
        /// Logical value that could not be converted.
        field: &'static str,
    },
    /// SQLite rejected an operation.
    #[error("session-store {operation} failed: {source}")]
    Database {
        /// Operation that SQLite rejected.
        operation: &'static str,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// The serialized connection lock was poisoned by a previous panic.
    #[error("session-store connection is unavailable after a previous panic")]
    ConnectionPoisoned,
    /// Tokio could not complete a blocking session-store operation.
    #[error("session-store blocking worker failed: {source}")]
    Worker {
        /// Underlying blocking-worker failure.
        #[source]
        source: tokio::task::JoinError,
    },
    /// A corrupt database could not be moved to its quarantine path.
    #[error("could not quarantine corrupt session store from {from} to {to}: {source}")]
    Quarantine {
        /// Original database path.
        from: PathBuf,
        /// Intended quarantine path.
        to: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// The clock could not supply a Unix timestamp for a quarantine name.
    #[error("could not timestamp corrupt session-store quarantine: {source}")]
    QuarantineTimestamp {
        /// Underlying system-clock failure.
        source: std::time::SystemTimeError,
    },
    /// SQLite failed while creating the one allowed replacement database.
    #[error("could not rebuild corrupt session store at {path}: {source}")]
    RebuildDatabase {
        /// Rebuilt database path.
        path: PathBuf,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// SQLite still classified the fresh replacement database as corrupt.
    #[error("freshly rebuilt session store at {path} is corrupt")]
    CorruptAfterRebuild {
        /// Rebuilt database path.
        path: PathBuf,
    },
    /// The sibling advisory lock could not be opened or acquired.
    #[error("could not acquire session-store initialization lock {path}: {source}")]
    InitializationLock {
        /// Sibling lock-file path.
        path: PathBuf,
        /// Underlying filesystem or locking error.
        source: std::io::Error,
    },
    /// The database file could not be created or restricted to owner-only access.
    #[error("could not prepare private session-store database {path}: {source}")]
    PrepareDatabase {
        /// Exact database path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
}

/// Object-safe durable boundary consumed by session actors and managers.
pub trait SessionRepository: Send + Sync {
    /// Returns non-fatal startup warnings retained by this repository instance.
    fn startup_warnings(&self) -> Vec<StoreWarning> {
        Vec::new()
    }

    /// Resolves a global ID or an exact title scoped to `cwd_for_title`.
    fn resolve(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;

    /// Loads a session by its globally stable ID.
    fn load(&self, id: SessionId) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;

    /// Atomically creates a session with its visible prompt and running first turn.
    fn materialize(
        &self,
        request: MaterializeSession,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;

    /// Lists persisted sessions in stable activity order.
    fn list(
        &self,
        scope: SessionListScope,
    ) -> BoxFuture<'static, Result<Vec<SessionSummary>, SessionStoreError>>;

    /// Applies a manual title and increments its monotonic revision.
    fn rename(
        &self,
        id: SessionId,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>>;

    /// Applies a generated title only while the expected non-manual revision remains current.
    fn compare_and_set_generated_title(
        &self,
        id: SessionId,
        expected_revision: u64,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<Option<SessionRecord>, SessionStoreError>>;

    /// Deletes a durable session and all of its child rows.
    fn delete(&self, id: SessionId) -> BoxFuture<'static, Result<(), SessionStoreError>>;

    /// Atomically persists metadata, model history, transcript, and durable turns.
    fn checkpoint(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>>;

    /// Persists title metadata, model settings, context usage, and last activity only.
    fn update_metadata(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>>;
}

/// SQLite-backed durable session repository.
#[derive(Clone)]
pub struct SessionStore {
    connection: Arc<Mutex<Connection>>,
    startup_warnings: Arc<[StoreWarning]>,
}

impl fmt::Debug for SessionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionStore")
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Opens `path`, validates or creates schema version 3, and quarantines actual corruption once.
    pub async fn open_at(path: &Path) -> Result<OpenedSessionStore, SessionStoreError> {
        let path = path.to_path_buf();
        blocking::run(move || open_sync(&path))
            .await
            .map_err(Self::from_blocking)
    }

    fn from_blocking(error: BlockingError<SessionStoreError>) -> SessionStoreError {
        match error {
            BlockingError::Operation(error) => error,
            BlockingError::Worker(source) => SessionStoreError::Worker { source },
        }
    }

    fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, SessionStoreError> + Send + 'static,
    ) -> BoxFuture<'static, Result<T, SessionStoreError>> {
        let connection = Arc::clone(&self.connection);
        Box::pin(async move {
            blocking::run(move || {
                let mut connection = connection
                    .lock()
                    .map_err(|_| SessionStoreError::ConnectionPoisoned)?;
                operation(&mut connection)
            })
            .await
            .map_err(Self::from_blocking)
        })
    }
}

impl SessionRepository for SessionStore {
    fn startup_warnings(&self) -> Vec<StoreWarning> {
        self.startup_warnings.as_ref().to_vec()
    }

    fn resolve(
        &self,
        selector: SessionSelector,
        cwd_for_title: Vec<u8>,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.run(move |connection| resolve_sync(connection, selector, cwd_for_title))
    }

    fn load(&self, id: SessionId) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.run(move |connection| load_sync(connection, id))
    }

    fn materialize(
        &self,
        request: MaterializeSession,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.run(move |connection| materialize_sync(connection, request))
    }

    fn list(
        &self,
        scope: SessionListScope,
    ) -> BoxFuture<'static, Result<Vec<SessionSummary>, SessionStoreError>> {
        self.run(move |connection| list_sync(connection, scope))
    }

    fn rename(
        &self,
        id: SessionId,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<SessionRecord, SessionStoreError>> {
        self.run(move |connection| rename_sync(connection, id, title))
    }

    fn compare_and_set_generated_title(
        &self,
        id: SessionId,
        expected_revision: u64,
        title: SessionTitle,
    ) -> BoxFuture<'static, Result<Option<SessionRecord>, SessionStoreError>> {
        self.run(move |connection| {
            compare_and_set_generated_title_sync(connection, id, expected_revision, title)
        })
    }

    fn delete(&self, id: SessionId) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        self.run(move |connection| delete_sync(connection, id))
    }

    fn checkpoint(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        self.run(move |connection| checkpoint_sync(connection, record))
    }

    fn update_metadata(
        &self,
        record: SessionRecord,
    ) -> BoxFuture<'static, Result<(), SessionStoreError>> {
        self.run(move |connection| update_metadata_sync(connection, record))
    }
}

fn open_sync(path: &Path) -> Result<OpenedSessionStore, SessionStoreError> {
    let _initialization_lock = acquire_initialization_lock(path)?;
    match initialize_database(path) {
        Ok(connection) => Ok(OpenedSessionStore {
            store: SessionStore {
                connection: Arc::new(Mutex::new(connection)),
                startup_warnings: Arc::from([]),
            },
            warnings: Vec::new(),
        }),
        Err(InitializationError::Corrupt) => {
            let quarantine = quarantine_corrupt_database(path)?;
            let connection = match initialize_database(path) {
                Ok(connection) => connection,
                Err(InitializationError::Corrupt) => {
                    return Err(SessionStoreError::CorruptAfterRebuild {
                        path: path.to_path_buf(),
                    });
                }
                Err(InitializationError::Database(source)) => {
                    return Err(SessionStoreError::RebuildDatabase {
                        path: path.to_path_buf(),
                        source,
                    });
                }
                Err(InitializationError::Prepare(source)) => {
                    return Err(SessionStoreError::PrepareDatabase {
                        path: path.to_path_buf(),
                        source,
                    });
                }
                Err(InitializationError::UnsupportedVersion { found }) => {
                    return Err(SessionStoreError::UnsupportedSchemaVersion {
                        found,
                        supported: SCHEMA_VERSION,
                    });
                }
            };
            let warning = StoreWarning::CorruptDatabaseQuarantined { path: quarantine };
            Ok(OpenedSessionStore {
                store: SessionStore {
                    connection: Arc::new(Mutex::new(connection)),
                    startup_warnings: Arc::from([warning.clone()]),
                },
                warnings: vec![warning],
            })
        }
        Err(InitializationError::Database(source)) => Err(SessionStoreError::Database {
            operation: "initialize",
            source,
        }),
        Err(InitializationError::Prepare(source)) => Err(SessionStoreError::PrepareDatabase {
            path: path.to_path_buf(),
            source,
        }),
        Err(InitializationError::UnsupportedVersion { found }) => {
            Err(SessionStoreError::UnsupportedSchemaVersion {
                found,
                supported: SCHEMA_VERSION,
            })
        }
    }
}

#[cfg(unix)]
fn open_private_file(path: &Path) -> Result<File, std::io::Error> {
    use nix::{
        fcntl::{OFlag, open},
        sys::stat::{Mode, fchmod},
    };

    let descriptor = open(
        path,
        OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(std::io::Error::from)?;
    fchmod(&descriptor, Mode::from_bits_truncate(0o600)).map_err(std::io::Error::from)?;
    Ok(descriptor.into())
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> Result<File, std::io::Error> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

fn acquire_initialization_lock(path: &Path) -> Result<File, SessionStoreError> {
    let lock_path = path_with_suffix(path, ".lock");
    let file =
        open_private_file(&lock_path).map_err(|source| SessionStoreError::InitializationLock {
            path: lock_path.clone(),
            source,
        })?;
    file.lock()
        .map_err(|source| SessionStoreError::InitializationLock {
            path: lock_path,
            source,
        })?;
    Ok(file)
}

enum InitializationError {
    Database(rusqlite::Error),
    Prepare(std::io::Error),
    Corrupt,
    UnsupportedVersion { found: u32 },
}

fn initialize_database(path: &Path) -> Result<Connection, InitializationError> {
    prepare_database_file(path).map_err(InitializationError::Prepare)?;
    let mut connection = Connection::open(path).map_err(classify_initialization_error)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(classify_initialization_error)?;
    let quick_check = connection
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map_err(classify_initialization_error)?;
    if quick_check != "ok" {
        return Err(InitializationError::Corrupt);
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(classify_initialization_error)?;
    let version = transaction
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(classify_initialization_error)?;
    let version = u32::try_from(version)
        .map_err(|_| InitializationError::UnsupportedVersion { found: u32::MAX })?;
    match version {
        0 => transaction
            .execute_batch(SESSION_SCHEMA)
            .map_err(classify_initialization_error)?,
        1 => migrate_legacy_to_v3(&transaction, false).map_err(classify_initialization_error)?,
        2 if table_has_column(&transaction, "sessions", "title")
            .map_err(classify_initialization_error)? =>
        {
            transaction
                .execute_batch(ADD_PLAN_TABLE_TO_SESSION_V2)
                .map_err(classify_initialization_error)?;
        }
        2 => migrate_legacy_to_v3(&transaction, true).map_err(classify_initialization_error)?,
        SCHEMA_VERSION => {}
        found => return Err(InitializationError::UnsupportedVersion { found }),
    }
    transaction
        .commit()
        .map_err(classify_initialization_error)?;
    Ok(connection)
}

struct LegacySessionRow {
    id: i64,
    name: Option<String>,
    cwd: Vec<u8>,
    model: String,
    reasoning: String,
    context_tokens: i64,
    created_at: String,
    last_activity: String,
}

fn table_has_column(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool, rusqlite::Error> {
    transaction
        .query_row(
            "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2 LIMIT 1",
            params![table, column],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
}

fn migrate_legacy_to_v3(
    transaction: &rusqlite::Transaction<'_>,
    preserve_plan: bool,
) -> Result<(), rusqlite::Error> {
    if preserve_plan {
        transaction.execute_batch("ALTER TABLE plan_items RENAME TO v2_plan_items;")?;
    }
    transaction.execute_batch(
        "ALTER TABLE messages RENAME TO v1_messages;\
         ALTER TABLE sessions RENAME TO v1_sessions;",
    )?;

    let legacy_sessions = {
        let mut statement = transaction.prepare(
            "SELECT id, name, cwd, model, reasoning, context_tokens, created_at, last_activity \
             FROM v1_sessions ORDER BY id",
        )?;
        statement
            .query_map([], |row| {
                Ok(LegacySessionRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    cwd: row.get(2)?,
                    model: row.get(3)?,
                    reasoning: row.get(4)?,
                    context_tokens: row.get(5)?,
                    created_at: row.get(6)?,
                    last_activity: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    transaction.execute_batch(SESSION_SCHEMA)?;

    for legacy in legacy_sessions {
        let messages = {
            let mut statement = transaction.prepare(
                "SELECT position, role, text FROM v1_messages \
                 WHERE session_id = ?1 ORDER BY position",
            )?;
            statement
                .query_map([legacy.id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        if messages.is_empty() {
            continue;
        }

        for (expected, (position, role, _)) in messages.iter().enumerate() {
            if usize::try_from(*position).ok() != Some(expected)
                || !matches!(role.as_str(), "user" | "assistant")
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
        }
        let (message_pairs, remainder) = messages.as_slice().as_chunks::<2>();
        if !remainder.is_empty()
            || message_pairs
                .iter()
                .any(|pair| pair[0].1 != "user" || pair[1].1 != "assistant")
        {
            return Err(rusqlite::Error::InvalidQuery);
        }

        let (title, title_source) = match legacy.name {
            Some(name) => (
                SessionTitle::parse(name).map_err(|_| rusqlite::Error::InvalidQuery)?,
                TitleSource::Manual,
            ),
            None => (fallback_title(&messages[0].2), TitleSource::Fallback),
        };
        transaction.execute(
            "INSERT INTO sessions \
             (id, title, title_source, title_revision, cwd, model, reasoning, context_tokens, created_at, last_activity) \
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                legacy.id,
                title.as_str(),
                title_source.as_stored(),
                legacy.cwd,
                legacy.model,
                legacy.reasoning,
                legacy.context_tokens,
                legacy.created_at,
                legacy.last_activity,
            ],
        )?;

        for (position, role, text) in &messages {
            transaction.execute(
                "INSERT INTO messages (session_id, position, role, text) VALUES (?1, ?2, ?3, ?4)",
                params![legacy.id, position, role, text],
            )?;
            transaction.execute(
                "INSERT INTO transcript_items (session_id, position, kind, text) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![legacy.id, position, role, text],
            )?;
        }
        for (ordinal, pair) in message_pairs.iter().enumerate() {
            let ordinal = i64::try_from(ordinal).map_err(|_| rusqlite::Error::InvalidQuery)?;
            transaction.execute(
                "INSERT INTO turns (session_id, ordinal, run_id, prompt_position, status) \
                 VALUES (?1, ?2, ?3, ?4, 'completed')",
                params![legacy.id, ordinal, ordinal + 1, pair[0].0],
            )?;
        }
    }

    if preserve_plan {
        transaction.execute_batch(
            "INSERT INTO plan_items (session_id, position, step, status) \
             SELECT session_id, position, step, status FROM v2_plan_items \
             WHERE session_id IN (SELECT id FROM sessions);\
             DROP TABLE v2_plan_items;",
        )?;
    }
    transaction.execute_batch(
        "DROP TABLE v1_messages;\
         DROP TABLE v1_sessions;\
         PRAGMA user_version = 3;",
    )
}

fn prepare_database_file(path: &Path) -> Result<(), std::io::Error> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(());
    }
    drop(open_private_file(path)?);
    Ok(())
}

fn classify_initialization_error(source: rusqlite::Error) -> InitializationError {
    if is_corruption(&source) {
        InitializationError::Corrupt
    } else {
        InitializationError::Database(source)
    }
}

fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn quarantine_corrupt_database(path: &Path) -> Result<PathBuf, SessionStoreError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| SessionStoreError::QuarantineTimestamp { source })?
        .as_millis();
    let quarantine = available_quarantine_path(path, timestamp);
    fs::rename(path, &quarantine).map_err(|source| SessionStoreError::Quarantine {
        from: path.to_path_buf(),
        to: quarantine.clone(),
        source,
    })?;
    Ok(quarantine)
}

fn available_quarantine_path(path: &Path, timestamp: u128) -> PathBuf {
    let mut suffix = timestamp;
    loop {
        let candidate = path_with_suffix(path, &format!(".corrupt-{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut suffixed: OsString = path.as_os_str().to_owned();
    suffixed.push(suffix);
    PathBuf::from(suffixed)
}

fn materialize_sync(
    connection: &mut Connection,
    request: MaterializeSession,
) -> Result<SessionRecord, SessionStoreError> {
    let MaterializeSession {
        cwd,
        title,
        settings,
        prompt,
        run_id,
        created_at,
    } = request;
    let context_tokens = sqlite_u64(settings.context_tokens, "context token count")?;
    let sqlite_run_id = sqlite_u64(run_id, "turn run id")?;
    let stored_created_at = format_timestamp(created_at);
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error("begin session-materialization transaction", source))?;
    transaction
        .execute(
            "INSERT INTO sessions \
             (title, title_source, title_revision, cwd, model, reasoning, context_tokens, created_at, last_activity) \
             VALUES (?1, 'fallback', 0, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                title.as_str(),
                &cwd,
                &settings.model,
                settings.reasoning.as_str(),
                context_tokens,
                &stored_created_at,
            ],
        )
        .map_err(|source| database_error("materialize session", source))?;
    let id = stored_session_id(transaction.last_insert_rowid())?;
    let sqlite_id = sqlite_session_id(id)?;
    transaction
        .execute(
            "INSERT INTO transcript_items (session_id, position, kind, text) \
             VALUES (?1, 0, 'user', ?2)",
            params![sqlite_id, &prompt],
        )
        .map_err(|source| database_error("materialize first prompt", source))?;
    transaction
        .execute(
            "INSERT INTO turns (session_id, ordinal, run_id, prompt_position, status) \
             VALUES (?1, 0, ?2, 0, 'running')",
            params![sqlite_id, sqlite_run_id],
        )
        .map_err(|source| database_error("materialize first turn", source))?;
    let record = load_record(&transaction, id)?;
    transaction
        .commit()
        .map_err(|source| database_error("commit session materialization", source))?;
    Ok(record)
}

fn resolve_sync(
    connection: &mut Connection,
    selector: SessionSelector,
    cwd_for_title: Vec<u8>,
) -> Result<SessionRecord, SessionStoreError> {
    match selector {
        SessionSelector::Id(id) => load_sync(connection, id),
        SessionSelector::Title(title) => {
            let ids = {
                let mut statement = connection
                    .prepare("SELECT id FROM sessions WHERE cwd = ?1 AND title = ?2 ORDER BY id")
                    .map_err(|source| database_error("prepare session-title resolution", source))?;
                statement
                    .query_map(params![cwd_for_title, title.as_str()], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(|source| database_error("resolve session title", source))?
                    .map(|row| {
                        row.map_err(|source| {
                            database_error("read session-title resolution", source)
                        })
                        .and_then(stored_session_id)
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            match ids.as_slice() {
                [] => Err(SessionStoreError::NotFound {
                    selector: title.to_string(),
                }),
                [id] => load_sync(connection, *id),
                _ => Err(SessionStoreError::AmbiguousTitle { title, ids }),
            }
        }
    }
}

fn load_sync(
    connection: &mut Connection,
    id: SessionId,
) -> Result<SessionRecord, SessionStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error("begin session-load transaction", source))?;
    let mut record = load_record(&transaction, id)?;
    let running_turns = record
        .turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Running)
        .map(|turn| turn.run_id)
        .collect::<Vec<_>>();
    if !running_turns.is_empty() {
        let sqlite_id = sqlite_session_id(id)?;
        transaction
            .execute(
                "UPDATE turns SET status = 'interrupted' \
                 WHERE session_id = ?1 AND status = 'running'",
                [sqlite_id],
            )
            .map_err(|source| database_error("interrupt running session turns", source))?;
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO transcript_items \
                     (session_id, position, kind, text, run_id, call_id, tool_name, arguments_json, \
                      failure_stage, failure_kind, failure_http_status, failure_retryable, failure_message) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )
                .map_err(|source| {
                    database_error("prepare interrupted-turn transcript append", source)
                })?;
            for (offset, run_id) in running_turns.into_iter().enumerate() {
                let position = record.transcript.len().checked_add(offset).ok_or(
                    SessionStoreError::ValueOutOfRange {
                        field: "transcript position",
                    },
                )?;
                let position =
                    i64::try_from(position).map_err(|_| SessionStoreError::ValueOutOfRange {
                        field: "transcript position",
                    })?;
                let item = encode_transcript_item(
                    position,
                    TranscriptItem::Failed {
                        run_id,
                        failure: RunFailureSnapshot {
                            stage: RunStage::Finalization,
                            kind: RunFailureKind::RuntimeInfrastructure,
                            retryable: true,
                            message: "run interrupted by backend restart".into(),
                        },
                    },
                )?;
                statement
                    .execute(params![
                        sqlite_id,
                        item.position,
                        item.kind,
                        item.text,
                        item.run_id,
                        item.call_id,
                        item.tool_name,
                        item.arguments_json,
                        item.failure_stage,
                        item.failure_kind,
                        item.failure_http_status,
                        item.failure_retryable,
                        item.failure_message,
                    ])
                    .map_err(|source| database_error("append interrupted-turn failure", source))?;
            }
        }
        record = load_record(&transaction, id)?;
    }
    transaction
        .commit()
        .map_err(|source| database_error("commit session load", source))?;
    Ok(record)
}

fn load_record(connection: &Connection, id: SessionId) -> Result<SessionRecord, SessionStoreError> {
    let sqlite_id = sqlite_session_id(id)?;
    let row = connection
        .query_row(
            "SELECT id, title, title_source, title_revision, cwd, model, reasoning, context_tokens, created_at, last_activity \
             FROM sessions WHERE id = ?1",
            [sqlite_id],
            |row| {
                Ok(StoredSessionRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    title_source: row.get(2)?,
                    title_revision: row.get(3)?,
                    cwd: row.get(4)?,
                    model: row.get(5)?,
                    reasoning: row.get(6)?,
                    context_tokens: row.get(7)?,
                    created_at: row.get(8)?,
                    last_activity: row.get(9)?,
                })
            },
        )
        .optional()
        .map_err(|source| database_error("load session", source))?
        .ok_or_else(|| SessionStoreError::NotFound {
            selector: id.to_string(),
        })?;
    let mut record = stored_record(row)?;
    let mut statement = connection
        .prepare(
            "SELECT position, role, text FROM messages WHERE session_id = ?1 ORDER BY position",
        )
        .map_err(|source| database_error("prepare session-history load", source))?;
    let rows = statement
        .query_map([sqlite_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|source| database_error("load session history", source))?;
    for (expected_position, row) in rows.enumerate() {
        let (position, role, text) =
            row.map_err(|source| database_error("read session history", source))?;
        let position = usize::try_from(position)
            .map_err(|_| invalid_stored("message position", "position is negative or too large"))?;
        if position != expected_position {
            return Err(invalid_stored(
                "message position",
                "positions are not contiguous from zero",
            ));
        }
        let role = match role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => return Err(invalid_stored("message role", "role is unsupported")),
        };
        record.history.push(Message::new(role, text));
    }

    let mut statement = connection
        .prepare(
            "SELECT position, kind, text, run_id, call_id, tool_name, arguments_json, \
                    failure_stage, failure_kind, failure_http_status, failure_retryable, failure_message \
             FROM transcript_items WHERE session_id = ?1 ORDER BY position",
        )
        .map_err(|source| database_error("prepare transcript load", source))?;
    let rows = statement
        .query_map([sqlite_id], |row| {
            Ok(StoredTranscriptRow {
                position: row.get(0)?,
                kind: row.get(1)?,
                text: row.get(2)?,
                run_id: row.get(3)?,
                call_id: row.get(4)?,
                tool_name: row.get(5)?,
                arguments_json: row.get(6)?,
                failure_stage: row.get(7)?,
                failure_kind: row.get(8)?,
                failure_http_status: row.get(9)?,
                failure_retryable: row.get(10)?,
                failure_message: row.get(11)?,
            })
        })
        .map_err(|source| database_error("load transcript", source))?;
    for (expected_position, row) in rows.enumerate() {
        let row = row.map_err(|source| database_error("read transcript", source))?;
        let position = usize::try_from(row.position).map_err(|_| {
            invalid_stored("transcript position", "position is negative or too large")
        })?;
        if position != expected_position {
            return Err(invalid_stored(
                "transcript position",
                "positions are not contiguous from zero",
            ));
        }
        record.transcript.push(stored_transcript_item(row)?);
    }

    let mut statement = connection
        .prepare(
            "SELECT ordinal, run_id, prompt_position, status \
             FROM turns WHERE session_id = ?1 ORDER BY ordinal",
        )
        .map_err(|source| database_error("prepare turns load", source))?;
    let rows = statement
        .query_map([sqlite_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|source| database_error("load turns", source))?;
    for (expected_ordinal, row) in rows.enumerate() {
        let (ordinal, run_id, prompt_position, status) =
            row.map_err(|source| database_error("read turns", source))?;
        let ordinal = stored_u64(ordinal, "turn ordinal")?;
        if ordinal != expected_ordinal as u64 {
            return Err(invalid_stored(
                "turn ordinal",
                "ordinals are not contiguous from zero",
            ));
        }
        record.turns.push(DurableTurn {
            ordinal,
            run_id: stored_u64(run_id, "turn run id")?,
            prompt_position: stored_u64(prompt_position, "turn prompt position")?,
            status: stored_turn_status(&status)?,
        });
    }

    let mut statement = connection
        .prepare(
            "SELECT position, step, status FROM plan_items WHERE session_id = ?1 ORDER BY position",
        )
        .map_err(|source| database_error("prepare session-plan load", source))?;
    let rows = statement
        .query_map([sqlite_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|source| database_error("load session plan", source))?;
    for (expected_position, row) in rows.enumerate() {
        let (position, step, status) =
            row.map_err(|source| database_error("read session plan", source))?;
        let position = usize::try_from(position)
            .map_err(|_| invalid_stored("plan position", "position is negative or too large"))?;
        if position != expected_position {
            return Err(invalid_stored(
                "plan position",
                "positions are not contiguous from zero",
            ));
        }
        let status = PlanStatus::from_str(&status)
            .map_err(|_| invalid_stored("plan status", "value is unsupported"))?;
        let item = PlanItem::parse(step, status)
            .map_err(|_| invalid_stored("plan item", "step violates validation rules"))?;
        record.plan.push(item);
    }
    validate_plan(&record.plan)?;
    Ok(record)
}

fn list_sync(
    connection: &Connection,
    scope: SessionListScope,
) -> Result<Vec<SessionSummary>, SessionStoreError> {
    let cwd = match scope {
        SessionListScope::Project(cwd) => Some(cwd),
        SessionListScope::All => None,
    };
    let mut statement = connection
        .prepare(
            "SELECT id, title, title_revision, cwd, last_activity FROM sessions \
             WHERE ?1 IS NULL OR cwd = ?1 \
             ORDER BY last_activity DESC, id DESC",
        )
        .map_err(|source| database_error("prepare session list", source))?;
    let rows = statement
        .query_map(params![cwd], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|source| database_error("list sessions", source))?;
    let mut summaries = Vec::new();
    for row in rows {
        let (id, title, title_revision, cwd, last_activity) =
            row.map_err(|source| database_error("read session list", source))?;
        summaries.push(SessionSummary {
            id: stored_session_id(id)?,
            title: stored_title(title)?,
            title_revision: stored_u64(title_revision, "title revision")?,
            cwd_display: String::from_utf8_lossy(&cwd).into_owned(),
            cwd,
            running_jobs: 0,
            running: false,
            busy: false,
            attached_clients: 0,
            last_activity: stored_timestamp(last_activity, "last activity")?,
        });
    }
    Ok(summaries)
}

fn rename_sync(
    connection: &mut Connection,
    id: SessionId,
    title: SessionTitle,
) -> Result<SessionRecord, SessionStoreError> {
    let sqlite_id = sqlite_session_id(id)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error("begin session-rename transaction", source))?;
    let record = load_record(&transaction, id)?;
    let title_revision =
        record
            .title_revision
            .checked_add(1)
            .ok_or(SessionStoreError::ValueOutOfRange {
                field: "title revision",
            })?;
    let title_revision = sqlite_u64(title_revision, "title revision")?;
    transaction
        .execute(
            "UPDATE sessions SET title = ?1, title_source = 'manual', title_revision = ?2 \
             WHERE id = ?3",
            params![title.as_str(), title_revision, sqlite_id],
        )
        .map_err(|source| database_error("rename session", source))?;
    let record = load_record(&transaction, id)?;
    transaction
        .commit()
        .map_err(|source| database_error("commit session rename", source))?;
    Ok(record)
}

fn compare_and_set_generated_title_sync(
    connection: &mut Connection,
    id: SessionId,
    expected_revision: u64,
    title: SessionTitle,
) -> Result<Option<SessionRecord>, SessionStoreError> {
    let sqlite_id = sqlite_session_id(id)?;
    let expected_revision = sqlite_u64(expected_revision, "title revision")?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error("begin generated-title transaction", source))?;
    let current = transaction
        .query_row(
            "SELECT title_source, title_revision FROM sessions WHERE id = ?1",
            [sqlite_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|source| database_error("load generated-title revision", source))?
        .ok_or_else(|| SessionStoreError::NotFound {
            selector: id.to_string(),
        })?;
    if current.0 == "manual" || current.1 != expected_revision {
        transaction
            .commit()
            .map_err(|source| database_error("commit generated-title rejection", source))?;
        return Ok(None);
    }
    let next_revision = stored_u64(current.1, "title revision")?
        .checked_add(1)
        .ok_or(SessionStoreError::ValueOutOfRange {
            field: "title revision",
        })?;
    let next_revision = sqlite_u64(next_revision, "title revision")?;
    transaction
        .execute(
            "UPDATE sessions SET title = ?1, title_source = 'generated', title_revision = ?2 \
             WHERE id = ?3",
            params![title.as_str(), next_revision, sqlite_id],
        )
        .map_err(|source| database_error("apply generated title", source))?;
    let record = load_record(&transaction, id)?;
    transaction
        .commit()
        .map_err(|source| database_error("commit generated title", source))?;
    Ok(Some(record))
}

fn delete_sync(connection: &mut Connection, id: SessionId) -> Result<(), SessionStoreError> {
    let sqlite_id = sqlite_session_id(id)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error("begin session-delete transaction", source))?;
    let deleted = transaction
        .execute("DELETE FROM sessions WHERE id = ?1", [sqlite_id])
        .map_err(|source| database_error("delete session", source))?;
    if deleted == 0 {
        return Err(SessionStoreError::NotFound {
            selector: id.to_string(),
        });
    }
    transaction
        .commit()
        .map_err(|source| database_error("commit session deletion", source))
}

fn checkpoint_sync(
    connection: &mut Connection,
    record: SessionRecord,
) -> Result<(), SessionStoreError> {
    validate_plan(&record.plan)?;
    let SessionRecord {
        id,
        title,
        title_source,
        title_revision,
        cwd: _,
        settings,
        transcript,
        turns,
        history,
        plan,
        created_at: _,
        last_activity,
    } = record;
    let sqlite_id = sqlite_session_id(id)?;
    let sqlite_title_revision = sqlite_u64(title_revision, "title revision")?;
    let context_tokens = sqlite_u64(settings.context_tokens, "context token count")?;
    let last_activity = format_timestamp(last_activity);
    let history = history
        .into_iter()
        .enumerate()
        .map(|(position, message)| {
            let position =
                i64::try_from(position).map_err(|_| SessionStoreError::ValueOutOfRange {
                    field: "message position",
                })?;
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            Ok((position, role, message.text))
        })
        .collect::<Result<Vec<_>, SessionStoreError>>()?;
    let transcript = transcript
        .into_iter()
        .enumerate()
        .map(|(position, item)| {
            let position =
                u64::try_from(position).map_err(|_| SessionStoreError::ValueOutOfRange {
                    field: "transcript position",
                })?;
            let position = sqlite_u64(position, "transcript position")?;
            encode_transcript_item(position, item)
        })
        .collect::<Result<Vec<_>, SessionStoreError>>()?;
    let turns = turns
        .into_iter()
        .map(|turn| {
            Ok((
                sqlite_u64(turn.ordinal, "turn ordinal")?,
                sqlite_u64(turn.run_id, "turn run id")?,
                sqlite_u64(turn.prompt_position, "turn prompt position")?,
                turn_status_as_stored(turn.status),
            ))
        })
        .collect::<Result<Vec<_>, SessionStoreError>>()?;
    let plan = plan
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let position =
                i64::try_from(position).map_err(|_| SessionStoreError::ValueOutOfRange {
                    field: "plan position",
                })?;
            Ok((position, item.step(), item.status().as_str()))
        })
        .collect::<Result<Vec<_>, SessionStoreError>>()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error("begin checkpoint transaction", source))?;
    let updated = transaction
        .execute(
            "UPDATE sessions SET title = ?1, title_source = ?2, title_revision = ?3, \
             model = ?4, reasoning = ?5, context_tokens = ?6, last_activity = ?7 WHERE id = ?8",
            params![
                title.as_str(),
                title_source.as_stored(),
                sqlite_title_revision,
                settings.model,
                settings.reasoning.as_str(),
                context_tokens,
                last_activity,
                sqlite_id,
            ],
        )
        .map_err(|source| database_error("checkpoint metadata", source))?;
    if updated == 0 {
        return Err(SessionStoreError::NotFound {
            selector: id.to_string(),
        });
    }
    transaction
        .execute("DELETE FROM messages WHERE session_id = ?1", [sqlite_id])
        .map_err(|source| database_error("replace session history", source))?;
    transaction
        .execute("DELETE FROM plan_items WHERE session_id = ?1", [sqlite_id])
        .map_err(|source| database_error("replace session plan", source))?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO messages (session_id, position, role, text) VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(|source| database_error("prepare session-history checkpoint", source))?;
        for (position, role, text) in history {
            statement
                .execute(params![sqlite_id, position, role, text])
                .map_err(|source| database_error("checkpoint session history", source))?;
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO plan_items (session_id, position, step, status) VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(|source| database_error("prepare session-plan checkpoint", source))?;
        for (position, step, status) in plan {
            statement
                .execute(params![sqlite_id, position, step, status])
                .map_err(|source| database_error("checkpoint session plan", source))?;
        }
    }
    transaction
        .execute(
            "DELETE FROM transcript_items WHERE session_id = ?1",
            [sqlite_id],
        )
        .map_err(|source| database_error("replace session transcript", source))?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO transcript_items \
                 (session_id, position, kind, text, run_id, call_id, tool_name, arguments_json, \
                  failure_stage, failure_kind, failure_http_status, failure_retryable, failure_message) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )
            .map_err(|source| database_error("prepare transcript checkpoint", source))?;
        for item in transcript {
            statement
                .execute(params![
                    sqlite_id,
                    item.position,
                    item.kind,
                    item.text,
                    item.run_id,
                    item.call_id,
                    item.tool_name,
                    item.arguments_json,
                    item.failure_stage,
                    item.failure_kind,
                    item.failure_http_status,
                    item.failure_retryable,
                    item.failure_message,
                ])
                .map_err(|source| database_error("checkpoint transcript", source))?;
        }
    }
    transaction
        .execute("DELETE FROM turns WHERE session_id = ?1", [sqlite_id])
        .map_err(|source| database_error("replace session turns", source))?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO turns (session_id, ordinal, run_id, prompt_position, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|source| database_error("prepare turns checkpoint", source))?;
        for (ordinal, run_id, prompt_position, status) in turns {
            statement
                .execute(params![sqlite_id, ordinal, run_id, prompt_position, status])
                .map_err(|source| database_error("checkpoint turns", source))?;
        }
    }
    transaction
        .commit()
        .map_err(|source| database_error("commit checkpoint", source))
}

fn update_metadata_sync(
    connection: &Connection,
    record: SessionRecord,
) -> Result<(), SessionStoreError> {
    let sqlite_id = sqlite_session_id(record.id)?;
    let title_revision = sqlite_u64(record.title_revision, "title revision")?;
    let context_tokens = sqlite_u64(record.settings.context_tokens, "context token count")?;
    let updated = connection
        .execute(
            "UPDATE sessions SET title = ?1, title_source = ?2, title_revision = ?3, \
             model = ?4, reasoning = ?5, context_tokens = ?6, last_activity = ?7 WHERE id = ?8",
            params![
                record.title.as_str(),
                record.title_source.as_stored(),
                title_revision,
                record.settings.model,
                record.settings.reasoning.as_str(),
                context_tokens,
                format_timestamp(record.last_activity),
                sqlite_id,
            ],
        )
        .map_err(|source| database_error("update session metadata", source))?;
    if updated == 0 {
        return Err(SessionStoreError::NotFound {
            selector: record.id.to_string(),
        });
    }
    Ok(())
}

struct EncodedTranscriptItem {
    position: i64,
    kind: &'static str,
    text: Option<String>,
    run_id: Option<i64>,
    call_id: Option<String>,
    tool_name: Option<String>,
    arguments_json: Option<String>,
    failure_stage: Option<&'static str>,
    failure_kind: Option<&'static str>,
    failure_http_status: Option<i64>,
    failure_retryable: Option<i64>,
    failure_message: Option<String>,
}

fn encode_transcript_item(
    position: i64,
    item: TranscriptItem,
) -> Result<EncodedTranscriptItem, SessionStoreError> {
    let mut encoded = EncodedTranscriptItem {
        position,
        kind: "user",
        text: None,
        run_id: None,
        call_id: None,
        tool_name: None,
        arguments_json: None,
        failure_stage: None,
        failure_kind: None,
        failure_http_status: None,
        failure_retryable: None,
        failure_message: None,
    };
    match item {
        TranscriptItem::User(text) => encoded.text = Some(text),
        TranscriptItem::Assistant(text) => {
            encoded.kind = "assistant";
            encoded.text = Some(text);
        }
        TranscriptItem::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => {
            encoded.kind = "tool_started";
            encoded.run_id = Some(sqlite_u64(run_id, "transcript run id")?);
            encoded.call_id = Some(call_id);
            encoded.tool_name = Some(name);
            encoded.arguments_json = Some(
                serde_json::to_string(&arguments)
                    .expect("serde_json::Value is always serializable"),
            );
        }
        TranscriptItem::Failed { run_id, failure } => {
            encoded.kind = "failed";
            encoded.run_id = Some(sqlite_u64(run_id, "transcript run id")?);
            encoded.failure_stage = Some(run_stage_as_stored(failure.stage));
            let (kind, status) = run_failure_kind_as_stored(&failure.kind);
            encoded.failure_kind = Some(kind);
            encoded.failure_http_status = status.map(i64::from);
            encoded.failure_retryable = Some(i64::from(failure.retryable));
            encoded.failure_message = Some(failure.message);
        }
        TranscriptItem::Cancelled { run_id } => {
            encoded.kind = "cancelled";
            encoded.run_id = Some(sqlite_u64(run_id, "transcript run id")?);
        }
    }
    Ok(encoded)
}

struct StoredTranscriptRow {
    position: i64,
    kind: String,
    text: Option<String>,
    run_id: Option<i64>,
    call_id: Option<String>,
    tool_name: Option<String>,
    arguments_json: Option<String>,
    failure_stage: Option<String>,
    failure_kind: Option<String>,
    failure_http_status: Option<i64>,
    failure_retryable: Option<i64>,
    failure_message: Option<String>,
}

fn stored_transcript_item(row: StoredTranscriptRow) -> Result<TranscriptItem, SessionStoreError> {
    match row.kind.as_str() {
        "user" => Ok(TranscriptItem::User(required_stored(
            row.text,
            "user transcript text",
        )?)),
        "assistant" => Ok(TranscriptItem::Assistant(required_stored(
            row.text,
            "assistant transcript text",
        )?)),
        "tool_started" => {
            let arguments_json = required_stored(row.arguments_json, "tool arguments")?;
            let arguments = serde_json::from_str(&arguments_json)
                .map_err(|_| invalid_stored("tool arguments", "value is not valid JSON"))?;
            Ok(TranscriptItem::ToolStarted {
                run_id: stored_u64(required_stored(row.run_id, "tool run id")?, "tool run id")?,
                call_id: required_stored(row.call_id, "tool call id")?,
                name: required_stored(row.tool_name, "tool name")?,
                arguments,
            })
        }
        "failed" => Ok(TranscriptItem::Failed {
            run_id: stored_u64(
                required_stored(row.run_id, "failed run id")?,
                "failed run id",
            )?,
            failure: super::RunFailureSnapshot {
                stage: stored_run_stage(&required_stored(row.failure_stage, "failure stage")?)?,
                kind: stored_run_failure_kind(
                    &required_stored(row.failure_kind, "failure kind")?,
                    row.failure_http_status,
                )?,
                retryable: stored_bool(
                    required_stored(row.failure_retryable, "failure retryable flag")?,
                    "failure retryable flag",
                )?,
                message: required_stored(row.failure_message, "failure message")?,
            },
        }),
        "cancelled" => Ok(TranscriptItem::Cancelled {
            run_id: stored_u64(
                required_stored(row.run_id, "cancelled run id")?,
                "cancelled run id",
            )?,
        }),
        _ => Err(invalid_stored("transcript kind", "value is unsupported")),
    }
}

fn required_stored<T>(value: Option<T>, field: &'static str) -> Result<T, SessionStoreError> {
    value.ok_or_else(|| invalid_stored(field, "required value is absent"))
}

fn run_stage_as_stored(stage: RunStage) -> &'static str {
    match stage {
        RunStage::Startup => "startup",
        RunStage::ModelRequest => "model_request",
        RunStage::ToolExecution => "tool_execution",
        RunStage::Finalization => "finalization",
    }
}

fn stored_run_stage(value: &str) -> Result<RunStage, SessionStoreError> {
    match value {
        "startup" => Ok(RunStage::Startup),
        "model_request" => Ok(RunStage::ModelRequest),
        "tool_execution" => Ok(RunStage::ToolExecution),
        "finalization" => Ok(RunStage::Finalization),
        _ => Err(invalid_stored("failure stage", "value is unsupported")),
    }
}

fn run_failure_kind_as_stored(kind: &RunFailureKind) -> (&'static str, Option<u16>) {
    match kind {
        RunFailureKind::Authentication => ("authentication", None),
        RunFailureKind::Transport => ("transport", None),
        RunFailureKind::HttpRejected { status } => ("http_rejected", Some(*status)),
        RunFailureKind::Protocol => ("protocol", None),
        RunFailureKind::EmptyResponse => ("empty_response", None),
        RunFailureKind::BudgetExhausted => ("budget_exhausted", None),
        RunFailureKind::RuntimeInfrastructure => ("runtime_infrastructure", None),
        RunFailureKind::ToolInfrastructure => ("tool_infrastructure", None),
    }
}

fn stored_run_failure_kind(
    value: &str,
    http_status: Option<i64>,
) -> Result<RunFailureKind, SessionStoreError> {
    match value {
        "authentication" => Ok(RunFailureKind::Authentication),
        "transport" => Ok(RunFailureKind::Transport),
        "http_rejected" => {
            let status = required_stored(http_status, "failure HTTP status")?;
            let status = u16::try_from(status)
                .map_err(|_| invalid_stored("failure HTTP status", "value is out of range"))?;
            Ok(RunFailureKind::HttpRejected { status })
        }
        "protocol" => Ok(RunFailureKind::Protocol),
        "empty_response" => Ok(RunFailureKind::EmptyResponse),
        "budget_exhausted" => Ok(RunFailureKind::BudgetExhausted),
        "runtime_infrastructure" => Ok(RunFailureKind::RuntimeInfrastructure),
        "tool_infrastructure" => Ok(RunFailureKind::ToolInfrastructure),
        _ => Err(invalid_stored("failure kind", "value is unsupported")),
    }
}

fn turn_status_as_stored(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Cancelled => "cancelled",
        TurnStatus::Interrupted => "interrupted",
    }
}

fn stored_turn_status(value: &str) -> Result<TurnStatus, SessionStoreError> {
    match value {
        "running" => Ok(TurnStatus::Running),
        "completed" => Ok(TurnStatus::Completed),
        "failed" => Ok(TurnStatus::Failed),
        "cancelled" => Ok(TurnStatus::Cancelled),
        "interrupted" => Ok(TurnStatus::Interrupted),
        _ => Err(invalid_stored("turn status", "value is unsupported")),
    }
}

struct StoredSessionRow {
    id: i64,
    title: String,
    title_source: String,
    title_revision: i64,
    cwd: Vec<u8>,
    model: String,
    reasoning: String,
    context_tokens: i64,
    created_at: String,
    last_activity: String,
}

fn stored_record(row: StoredSessionRow) -> Result<SessionRecord, SessionStoreError> {
    let reasoning = ReasoningLevel::parse(&row.reasoning)
        .ok_or_else(|| invalid_stored("reasoning level", "value is unsupported"))?;
    let context_tokens = u64::try_from(row.context_tokens)
        .map_err(|_| invalid_stored("context token count", "value is negative"))?;
    Ok(SessionRecord {
        id: stored_session_id(row.id)?,
        title: stored_title(row.title)?,
        title_source: TitleSource::from_stored(&row.title_source)
            .ok_or_else(|| invalid_stored("title source", "value is unsupported"))?,
        title_revision: stored_u64(row.title_revision, "title revision")?,
        cwd: row.cwd,
        settings: SessionSettings {
            model: row.model,
            reasoning,
            context_tokens,
        },
        transcript: Vec::new(),
        turns: Vec::new(),
        history: Vec::new(),
        plan: Vec::new(),
        created_at: stored_timestamp(row.created_at, "creation time")?,
        last_activity: stored_timestamp(row.last_activity, "last activity")?,
    })
}

fn validate_plan(plan: &[PlanItem]) -> Result<(), SessionStoreError> {
    if plan.iter().any(|item| item.validate().is_err()) {
        return Err(invalid_stored("plan", "contains an invalid item"));
    }
    if plan.len() > 32 {
        return Err(invalid_stored("plan", "contains more than 32 items"));
    }
    if plan
        .iter()
        .filter(|item| item.status() == PlanStatus::InProgress)
        .nth(1)
        .is_some()
    {
        return Err(invalid_stored(
            "plan",
            "contains more than one in_progress item",
        ));
    }
    Ok(())
}

fn stored_session_id(value: i64) -> Result<SessionId, SessionStoreError> {
    let value = u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_stored("session id", "identifier is not positive"))?;
    Ok(SessionId::from_stored(value))
}

fn sqlite_session_id(id: SessionId) -> Result<i64, SessionStoreError> {
    i64::try_from(id.get()).map_err(|_| SessionStoreError::ValueOutOfRange {
        field: "session id",
    })
}

fn sqlite_u64(value: u64, field: &'static str) -> Result<i64, SessionStoreError> {
    i64::try_from(value).map_err(|_| SessionStoreError::ValueOutOfRange { field })
}

fn stored_u64(value: i64, field: &'static str) -> Result<u64, SessionStoreError> {
    u64::try_from(value).map_err(|_| invalid_stored(field, "value is negative"))
}

fn stored_title(value: String) -> Result<SessionTitle, SessionStoreError> {
    SessionTitle::parse(value)
        .map_err(|_| invalid_stored("session title", "title violates validation rules"))
}

fn stored_bool(value: i64, field: &'static str) -> Result<bool, SessionStoreError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid_stored(field, "value is not zero or one")),
    }
}

fn format_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn stored_timestamp(
    value: String,
    field: &'static str,
) -> Result<DateTime<Utc>, SessionStoreError> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| invalid_stored(field, "value is not an RFC 3339 timestamp"))
}

fn invalid_stored(field: &'static str, reason: &'static str) -> SessionStoreError {
    SessionStoreError::InvalidStoredData {
        field,
        reason: reason.into(),
    }
}

fn database_error(operation: &'static str, source: rusqlite::Error) -> SessionStoreError {
    SessionStoreError::Database { operation, source }
}
