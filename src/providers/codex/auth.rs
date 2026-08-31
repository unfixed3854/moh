use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Mutex as StdMutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CREDENTIAL_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const CREDENTIAL_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const OAUTH_READ_TIMEOUT: Duration = Duration::from_secs(10);
const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct RefreshPolicy {
    lock_timeout: Duration,
    lock_retry_interval: Duration,
    connect_timeout: Duration,
    read_timeout: Duration,
    request_timeout: Duration,
}

impl RefreshPolicy {
    const PRODUCTION: Self = Self {
        lock_timeout: CREDENTIAL_LOCK_TIMEOUT,
        lock_retry_interval: CREDENTIAL_LOCK_RETRY_INTERVAL,
        connect_timeout: OAUTH_CONNECT_TIMEOUT,
        read_timeout: OAUTH_READ_TIMEOUT,
        request_timeout: OAUTH_REQUEST_TIMEOUT,
    };
}

struct PendingRefreshes {
    next_id: u64,
    tasks: Vec<(u64, tokio::task::JoinHandle<()>)>,
}

fn pending_refreshes() -> &'static StdMutex<PendingRefreshes> {
    static PENDING: OnceLock<StdMutex<PendingRefreshes>> = OnceLock::new();
    PENDING.get_or_init(|| {
        StdMutex::new(PendingRefreshes {
            next_id: 0,
            tasks: Vec::new(),
        })
    })
}

fn refresh_drain_lock() -> &'static tokio::sync::Mutex<()> {
    static DRAIN_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    DRAIN_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn prune_completed_refresh_tasks(pending: &mut PendingRefreshes) {
    pending.tasks.retain(|(_, task)| !task.is_finished());
}

fn register_refresh_task(task: tokio::task::JoinHandle<()>) -> u64 {
    let mut pending = pending_refreshes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    prune_completed_refresh_tasks(&mut pending);
    let id = pending.next_id;
    pending.next_id += 1;
    pending.tasks.push((id, task));
    id
}

fn complete_refresh_task(id: u64) {
    let task = {
        let mut pending = pending_refreshes()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .tasks
            .iter()
            .position(|(pending_id, _)| *pending_id == id)
            .map(|index| pending.tasks.swap_remove(index).1)
    };
    drop(task);
}

async fn drain_pending_refresh_tasks() {
    let _drain = refresh_drain_lock().lock().await;
    let tasks = {
        let mut pending = pending_refreshes()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut pending.tasks)
    };
    for (_, task) in tasks {
        let _ = task.await;
    }
}

#[allow(dead_code)]
#[derive(Clone)]
struct Secret(String);

#[allow(dead_code)]
impl Secret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// ChatGPT credentials loaded from Codex's file-backed credential store.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct CodexCredentials {
    access_token: Secret,
    refresh_token: Secret,
    account_id: String,
}

#[allow(dead_code)]
impl CodexCredentials {
    /// Returns the account identifier associated with these credentials.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub(crate) fn access_token(&self) -> &str {
        self.access_token.expose()
    }

    pub(crate) fn refresh_token(&self) -> &str {
        self.refresh_token.expose()
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct AuthDocument {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<TokenDocument>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
struct TokenDocument {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct RefreshErrorEnvelope {
    error: Option<RefreshErrorValue>,
    code: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RefreshErrorValue {
    Code(String),
    Object { code: Option<String> },
}

/// A classified permanent failure returned by the Codex token refresh endpoint.
#[derive(Debug, Clone, Copy)]
pub enum RefreshFailure {
    /// The refresh token has expired.
    Expired,
    /// The refresh token was already consumed.
    Reused,
    /// The refresh token was invalidated or revoked.
    Revoked,
    /// The authentication service rejected the refresh request.
    Rejected,
}

impl fmt::Display for RefreshFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Expired => "refresh token expired; run `codex login`",
            Self::Reused => "refresh token was already used; run `codex login`",
            Self::Revoked => "refresh token was revoked; run `codex login`",
            Self::Rejected => "authentication service rejected the refresh; run `codex login`",
        })
    }
}

/// The parsed contents and location of Codex's file-backed auth document.
pub struct AuthFile {
    path: PathBuf,
    document: AuthDocument,
}

impl fmt::Debug for AuthFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthFile")
            .field("path", &self.path)
            .field("credentials", &"[REDACTED]")
            .finish()
    }
}

/// Errors raised while locating, reading, or validating Codex credentials.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Neither `CODEX_HOME` nor a home-directory path was available.
    #[error("could not resolve the home directory for Codex credentials")]
    HomeDirectoryUnavailable,
    /// The required file-backed credential document is absent.
    #[error(
        "file-backed Codex credentials are required at {path}; set cli_auth_credentials_store = \"file\" and run `codex login`"
    )]
    FileRequired {
        /// Path to the missing credential document.
        path: PathBuf,
    },
    /// The credential document could not be read.
    #[error("could not read Codex credentials at {path}: {source}")]
    Read {
        /// Path to the unreadable credential document.
        path: PathBuf,
        /// Underlying file-system error.
        source: std::io::Error,
    },
    /// The credential document did not contain valid JSON.
    #[error("Codex credentials at {path} are malformed")]
    Malformed {
        /// Path to the malformed credential document.
        path: PathBuf,
    },
    /// The credential document does not use ChatGPT authentication.
    #[error("unsupported Codex auth mode {mode:?}; sign in with ChatGPT using file-backed storage")]
    UnsupportedAuthMode {
        /// The unsupported mode, if supplied by the document.
        mode: Option<String>,
    },
    /// A required ChatGPT credential value is absent or blank.
    #[error("Codex ChatGPT credentials are missing {field}")]
    MissingCredentialField {
        /// Name of the missing required field.
        field: &'static str,
    },
    /// The token refresh endpoint rejected a refresh token permanently.
    #[error("Codex token refresh failed: {0}")]
    RefreshFailed(RefreshFailure),
    /// The credentials on disk were changed while a refresh request was in progress.
    #[error("Codex credentials changed while refresh was in progress; retry the request")]
    ConcurrentCredentialChange,
    /// Another process held the stable credential lock past the bounded wait.
    #[error("Codex credential store is busy; retry the request")]
    CredentialStoreBusy,
    /// Refreshed credentials could not be atomically persisted to disk.
    #[error("could not persist refreshed Codex credentials at {path}: {source}")]
    Persist {
        /// Credential file path that could not be updated.
        path: PathBuf,
        /// Underlying file-system error.
        source: std::io::Error,
    },
    /// The token refresh request or response could not be completed safely.
    #[error("Codex token refresh transport failed")]
    RefreshTransport,
}

/// Resolves Codex's configuration directory from an explicit path or home directory.
pub fn resolve_codex_home(
    codex_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf, AuthError> {
    if let Some(codex_home) = codex_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(codex_home));
    }
    home.filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.join(".codex"))
        .ok_or(AuthError::HomeDirectoryUnavailable)
}

impl AuthFile {
    /// Reads and validates a Codex file-backed auth document from `path`.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let path = path.into();
        let load_path = path.clone();
        tokio::task::spawn_blocking(move || load_sync(load_path))
            .await
            .map_err(|error| AuthError::Read {
                path,
                source: std::io::Error::other(error),
            })?
    }

    /// Loads the `auth.json` document from the environment-resolved Codex home.
    pub async fn load_from_env() -> Result<Self, AuthError> {
        let codex_home = std::env::var_os("CODEX_HOME");
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
            .map(PathBuf::from);
        Self::load(resolve_codex_home(codex_home, home)?.join("auth.json")).await
    }

    /// Refreshes ChatGPT credentials with at most five seconds of lock contention and a
    /// thirty-second OAuth request, preserving concurrent rotations and persisting atomically.
    pub async fn refresh(&mut self, endpoint: &str) -> Result<CodexCredentials, AuthError> {
        self.refresh_with_policy(endpoint, RefreshPolicy::PRODUCTION)
            .await
    }

    async fn refresh_with_policy(
        &mut self,
        endpoint: &str,
        policy: RefreshPolicy,
    ) -> Result<CodexCredentials, AuthError> {
        let path = self.path.clone();
        let endpoint = endpoint.to_owned();
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let refresh_id = register_refresh_task(tokio::spawn(async move {
            let result = refresh_and_persist(path, endpoint, policy).await;
            let _ = result_sender.send(result);
        }));
        let document = result_receiver
            .await
            .map_err(|_| AuthError::RefreshTransport)?;
        complete_refresh_task(refresh_id);
        self.document = document?;
        self.credentials()
    }

    /// Waits for cancelled refreshes before an application runtime is shut down.
    pub async fn drain_pending_refreshes() {
        drain_pending_refresh_tasks().await;
    }

    /// Returns validated ChatGPT credentials with secret debug output redacted.
    pub fn credentials(&self) -> Result<CodexCredentials, AuthError> {
        if self.document.auth_mode.as_deref() != Some("chatgpt") {
            return Err(AuthError::UnsupportedAuthMode {
                mode: self.document.auth_mode.clone(),
            });
        }
        let tokens = self
            .document
            .tokens
            .as_ref()
            .ok_or(AuthError::MissingCredentialField { field: "tokens" })?;
        let required = |value: &Option<String>, field| {
            value
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or(AuthError::MissingCredentialField { field })
        };
        Ok(CodexCredentials {
            access_token: Secret(required(&tokens.access_token, "access_token")?),
            refresh_token: Secret(required(&tokens.refresh_token, "refresh_token")?),
            account_id: required(&tokens.account_id, "account_id")?,
        })
    }
}

fn load_sync(path: PathBuf) -> Result<AuthFile, AuthError> {
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AuthError::FileRequired { path });
        }
        Err(source) => return Err(AuthError::Read { path, source }),
    };
    let document = serde_json::from_slice::<AuthDocument>(&bytes)
        .map_err(|_| AuthError::Malformed { path: path.clone() })?;
    let auth = AuthFile { path, document };
    auth.credentials()?;
    Ok(auth)
}

async fn refresh_and_persist(
    path: PathBuf,
    endpoint: String,
    policy: RefreshPolicy,
) -> Result<AuthDocument, AuthError> {
    let _credential_lock = acquire_credential_lock_async(path.clone(), policy).await?;
    let before = AuthFile::load(&path).await?;
    let before_credentials = before.credentials()?;
    let client = reqwest::Client::builder()
        .connect_timeout(policy.connect_timeout)
        .read_timeout(policy.read_timeout)
        .timeout(policy.request_timeout)
        .build()
        .map_err(|_| AuthError::RefreshTransport)?;
    let response = client
        .post(&endpoint)
        .json(&json!({
            "client_id": OAUTH_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": before_credentials.refresh_token(),
        }))
        .send()
        .await
        .map_err(|_| AuthError::RefreshTransport)?;

    let status = response.status();
    if !status.is_success() {
        let value = response.json::<RefreshErrorEnvelope>().await.ok();
        let code = value.as_ref().and_then(refresh_error_code);
        let failure = match code.as_deref() {
            Some("refresh_token_expired") => Some(RefreshFailure::Expired),
            Some("refresh_token_reused") => Some(RefreshFailure::Reused),
            Some("refresh_token_invalidated") => Some(RefreshFailure::Revoked),
            _ if status.is_client_error() => Some(RefreshFailure::Rejected),
            _ => None,
        };
        return Err(failure
            .map(AuthError::RefreshFailed)
            .unwrap_or(AuthError::RefreshTransport));
    }

    let rotated = response
        .json::<RefreshResponse>()
        .await
        .map_err(|_| AuthError::RefreshTransport)?;
    let access_token = rotated
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or(AuthError::MissingCredentialField {
            field: "refreshed access_token",
        })?;
    let refresh_token = rotated
        .refresh_token
        .map(|token| {
            if token.trim().is_empty() {
                Err(AuthError::MissingCredentialField {
                    field: "refreshed refresh_token",
                })
            } else {
                Ok(token)
            }
        })
        .transpose()?;
    let current = AuthFile::load(&path).await?;
    let current_credentials = current.credentials()?;
    if current_credentials.account_id() != before_credentials.account_id()
        || current_credentials.refresh_token() != before_credentials.refresh_token()
    {
        return Err(AuthError::ConcurrentCredentialChange);
    }

    let mut document = current.document;
    let tokens = document
        .tokens
        .as_mut()
        .expect("validated credentials have tokens");
    tokens.access_token = Some(access_token);
    if let Some(refresh_token) = refresh_token {
        tokens.refresh_token = Some(refresh_token);
    }
    if let Some(id_token) = rotated.id_token {
        tokens.id_token = Some(id_token);
    }
    document
        .extra
        .insert("last_refresh".into(), json!(Utc::now()));
    persist_atomically_async(path, document).await
}

fn refresh_error_code(value: &RefreshErrorEnvelope) -> Option<String> {
    match value.error.as_ref() {
        Some(RefreshErrorValue::Code(code)) => Some(code.clone()),
        Some(RefreshErrorValue::Object { code }) => code.clone(),
        None => value.code.clone(),
    }
}

#[cfg(test)]
fn with_credential_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T, AuthError>,
) -> Result<T, AuthError> {
    with_credential_lock_timeout(
        path,
        CREDENTIAL_LOCK_TIMEOUT,
        CREDENTIAL_LOCK_RETRY_INTERVAL,
        operation,
    )
}

#[cfg(test)]
fn with_credential_lock_timeout<T>(
    path: &Path,
    timeout: Duration,
    retry_interval: Duration,
    operation: impl FnOnce() -> Result<T, AuthError>,
) -> Result<T, AuthError> {
    let _lock_file = acquire_credential_lock(path, timeout, retry_interval)?;
    operation()
}

fn acquire_credential_lock(
    path: &Path,
    timeout: Duration,
    retry_interval: Duration,
) -> Result<fs::File, AuthError> {
    let mut lock_name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| AuthError::Persist {
            path: path.to_owned(),
            source: std::io::Error::other("auth path has no file name"),
        })?;
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|source| AuthError::Persist {
            path: path.to_owned(),
            source,
        })?;
    let started = Instant::now();
    loop {
        match lock_file.try_lock() {
            Ok(()) => return Ok(lock_file),
            Err(fs::TryLockError::WouldBlock) => {
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(AuthError::CredentialStoreBusy);
                }
                thread::sleep(retry_interval.min(timeout - elapsed));
            }
            Err(fs::TryLockError::Error(source)) => {
                return Err(AuthError::Persist {
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }
}

async fn acquire_credential_lock_async(
    path: PathBuf,
    policy: RefreshPolicy,
) -> Result<fs::File, AuthError> {
    tokio::task::spawn_blocking(move || {
        acquire_credential_lock(&path, policy.lock_timeout, policy.lock_retry_interval)
    })
    .await
    .map_err(|_| AuthError::RefreshTransport)?
}

fn persist_atomically(path: &Path, document: &AuthDocument) -> Result<(), AuthError> {
    let parent = path.parent().ok_or_else(|| AuthError::Persist {
        path: path.to_owned(),
        source: std::io::Error::other("auth path has no parent"),
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| AuthError::Persist {
        path: path.to_owned(),
        source,
    })?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|source| AuthError::Persist {
            path: path.to_owned(),
            source,
        })?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), document).map_err(|source| {
        AuthError::Persist {
            path: path.to_owned(),
            source: std::io::Error::other(source),
        }
    })?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| AuthError::Persist {
            path: path.to_owned(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| AuthError::Persist {
            path: path.to_owned(),
            source: error.error,
        })?;
    Ok(())
}

async fn persist_atomically_async(
    path: PathBuf,
    document: AuthDocument,
) -> Result<AuthDocument, AuthError> {
    tokio::task::spawn_blocking(move || {
        persist_atomically(&path, &document)?;
        Ok(document)
    })
    .await
    .map_err(|_| AuthError::RefreshTransport)?
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::mpsc, thread, time::Duration};

    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::{
        AuthError, AuthFile, RefreshPolicy, persist_atomically, with_credential_lock,
        with_credential_lock_timeout,
    };

    fn short_test_policy() -> RefreshPolicy {
        RefreshPolicy {
            lock_timeout: Duration::from_millis(30),
            lock_retry_interval: Duration::from_millis(2),
            connect_timeout: Duration::from_millis(30),
            read_timeout: Duration::from_millis(30),
            request_timeout: Duration::from_millis(60),
        }
    }

    #[tokio::test]
    async fn stalled_refresh_response_returns_within_the_configured_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(json!({
                        "access_token": "rotated-access",
                        "refresh_token": "rotated-refresh"
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempdir().unwrap();
        let path = directory.path().join("auth.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "original-access",
                    "refresh_token": "original-refresh",
                    "account_id": "account-123"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut auth = AuthFile::load(&path).await.unwrap();

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            auth.refresh_with_policy(&server.uri(), short_test_policy()),
        )
        .await;

        assert!(matches!(outcome, Ok(Err(AuthError::RefreshTransport))));
        assert_eq!(
            fs::read(&path).unwrap(),
            serde_json::to_vec(&json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "original-access",
                    "refresh_token": "original-refresh",
                    "account_id": "account-123"
                }
            }))
            .unwrap()
        );
    }

    #[test]
    fn unreleased_companion_lock_returns_a_typed_busy_error_at_the_deadline() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("auth.json");
        fs::write(&path, b"{}").unwrap();
        let lock_path = directory.path().join("auth.json.lock");
        let (locked_tx, locked_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)
                .unwrap();
            lock.lock().unwrap();
            locked_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        locked_rx.recv().unwrap();

        let result = with_credential_lock_timeout(
            &path,
            Duration::from_millis(30),
            Duration::from_millis(2),
            || Ok(()),
        );

        assert!(matches!(result, Err(AuthError::CredentialStoreBusy)));
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn refresh_reserves_the_companion_lock_before_dispatching_oauth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let directory = tempdir().unwrap();
        let path = directory.path().join("auth.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "original-access",
                    "refresh_token": "original-refresh",
                    "account_id": "account-123"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.path().join("auth.json.lock"))
            .unwrap();
        lock.lock().unwrap();
        let mut auth = AuthFile::load(path).await.unwrap();

        let result = auth
            .refresh_with_policy(&server.uri(), short_test_policy())
            .await;

        assert!(matches!(result, Err(AuthError::CredentialStoreBusy)));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn credential_lock_serializes_a_rotation_started_after_revalidation() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("auth.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "original-access",
                    "refresh_token": "original-refresh",
                    "account_id": "account-123"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let (revalidated, revalidated_received) = mpsc::channel();
        let (allow_first_persist, allow_first_persist_received) = mpsc::channel();
        let first_path = path.clone();
        let first = thread::spawn(move || {
            with_credential_lock(&first_path, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let current = runtime.block_on(AuthFile::load(&first_path))?;
                current.credentials()?;
                revalidated.send(()).unwrap();
                allow_first_persist_received.recv().unwrap();
                let mut document = current.document;
                document.tokens.as_mut().unwrap().refresh_token = Some("first-refresh".into());
                persist_atomically(&first_path, &document)
            })
        });

        revalidated_received.recv().unwrap();
        let (second_attempted, second_attempted_received) = mpsc::channel();
        let (second_persisted, second_persisted_received) = mpsc::channel();
        let second_path = path.clone();
        let second = thread::spawn(move || {
            second_attempted.send(()).unwrap();
            with_credential_lock(&second_path, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let current = runtime.block_on(AuthFile::load(&second_path))?;
                let mut document = current.document;
                document.tokens.as_mut().unwrap().refresh_token = Some("newer-refresh".into());
                persist_atomically(&second_path, &document)?;
                second_persisted.send(()).unwrap();
                Ok(())
            })
        });

        second_attempted_received.recv().unwrap();
        assert!(
            second_persisted_received
                .recv_timeout(Duration::from_millis(250))
                .is_err()
        );
        allow_first_persist.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        second_persisted_received.recv().unwrap();

        let stored = fs::read_to_string(path).unwrap();
        assert!(stored.contains("newer-refresh"));
        assert!(!stored.contains("first-refresh"));
    }
}
