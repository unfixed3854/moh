# Codex Authentication and Conversation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the deterministic demo echo with a minimal in-memory conversation that sends real `gpt-5.6-luna` requests at medium reasoning through Rig using an existing file-backed Codex CLI ChatGPT login.

**Architecture:** `codex_auth` owns defensive `auth.json` parsing, refresh, redaction, and atomic persistence; `codex_provider` adapts those credentials to Rig's OpenAI Responses client and retries one authentication rejection; `conversation` owns transactional in-memory history. The existing demo becomes an async but deliberately plain UI around one in-flight conversation request.

**Tech Stack:** Rust 2024, Tokio 1.53, Rig (`rig-core`) 0.41, Reqwest 0.13 with rustls, Serde/serde_json, Chrono, Tempfile, Thiserror, Wiremock.

## Global Constraints

- Support file-backed Codex CLI credentials at `$CODEX_HOME/auth.json`, defaulting to `~/.codex/auth.json`; OS keyring and encrypted-secrets storage remain unsupported.
- Accept ChatGPT-backed Codex credentials only; do not silently fall back to API-key billing or add a login flow.
- Send through Rig's OpenAI Responses integration to `gpt-5.6-luna` with reasoning effort exactly `medium`.
- Keep exactly one request in flight and retain conversation history in memory only. Codex requires SSE transport internally; buffer it fully before showing a completed answer so presentation remains non-streaming.
- Commit a user/assistant exchange to model history only after a successful response.
- Never expose access tokens, refresh tokens, ID tokens, authorization headers, or raw credential JSON through `Debug`, `Display`, logs, status text, snapshots, or command arguments.
- Retry exactly once and only after an HTTP 401 authentication rejection.
- Reserve the companion credential lock before OAuth refresh, wait at most 5 seconds for it, and bound the OAuth request to 30 seconds overall (5-second connect and 10-second read bounds). Explicit runtime teardown must wait for an already-dispatched refresh to finish persistence.
- Keep resize, help, and exit responsive while a request is pending; ignore text entry and submission until that request resolves.
- Do not add model selection, persistence, streaming, tools, concurrent requests, or substantial TUI redesign.

---

## File Structure

- `Cargo.toml`: add the exact async, Rig, HTTP, serialization, time, atomic-file, and test dependencies.
- `Cargo.lock`: lock the new dependency graph.
- `src/lib.rs`: export the new authentication, provider, and conversation modules.
- `src/codex_auth.rs`: credential paths, defensive document parsing, secret wrappers, refresh protocol, concurrency guard, and atomic persistence.
- `src/codex_provider.rs`: Rig Responses client construction, request translation, response extraction, 401 refresh/retry, and provider errors.
- `src/conversation.rs`: single-pending-turn state machine and committed in-memory history.
- `src/main.rs`: start the Tokio current-thread runtime and report startup/runtime errors.
- `src/demo.rs`: connect input/rendering to `Conversation<CodexProvider>` without freezing terminal events.
- `tests/codex_auth.rs`: black-box credential loading, redaction, refresh, concurrency, and permission tests.
- `tests/codex_provider.rs`: mock-server assertions for endpoint, headers, model, reasoning, history, and retry policy.
- `tests/conversation.rs`: mock-backend assertions for transactional history and busy-state behavior.
- `tests/codex_live.rs`: ignored, explicitly enabled real-login smoke test.
- `README.md`: user prerequisites, supported credential storage, run instructions, and experimental compatibility warning.

---

### Task 1: Load and validate file-backed Codex credentials

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/lib.rs`
- Create: `src/codex_auth.rs`
- Create: `tests/codex_auth.rs`

**Interfaces:**
- Consumes: `CODEX_HOME` and a resolved home directory only at the production boundary.
- Produces: `pub struct AuthFile`, `pub struct CodexCredentials`, `pub enum AuthError`, `pub fn resolve_codex_home(codex_home: Option<OsString>, home: Option<PathBuf>) -> Result<PathBuf, AuthError>`, `AuthFile::load(path: impl Into<PathBuf>) -> Result<Self, AuthError>`, `AuthFile::load_from_env() -> Result<Self, AuthError>`, and `AuthFile::credentials(&self) -> Result<CodexCredentials, AuthError>`.

- [ ] **Step 1: Add pinned feature dependencies for the vertical slice**

Update `Cargo.toml` to contain:

```toml
[dependencies]
chrono = { version = "0.4.45", default-features = false, features = ["clock", "serde"] }
crossterm = "0.29.0"
reqwest = { version = "0.13.4", default-features = false, features = ["json", "rustls"] }
rig = { package = "rig-core", version = "0.41.0" }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
tempfile = "3.27.0"
thiserror = "2.0.20"
tokio = { version = "1.53.1", features = ["macros", "rt", "sync", "time"] }
unicode-segmentation = "1.13.3"
unicode-width = "0.2.2"
vte = "0.15.0"

[dev-dependencies]
vt100 = "0.16.2"
wiremock = "0.6.5"
```

Run `cargo check --all-targets` once to resolve `Cargo.lock`.

- [ ] **Step 2: Write failing public credential-loading tests**

Create `tests/codex_auth.rs` with synthetic tokens only:

```rust
use std::{ffi::OsString, fs, path::PathBuf};

use moh::codex_auth::{AuthError, AuthFile, resolve_codex_home};
use serde_json::json;
use tempfile::tempdir;

fn write_auth(directory: &std::path::Path, value: serde_json::Value) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let path = directory.join("auth.json");
    fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    path
}

fn valid_auth() -> serde_json::Value {
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "synthetic-id-secret",
            "access_token": "synthetic-access-secret",
            "refresh_token": "synthetic-refresh-secret",
            "account_id": "account-123",
            "future_token_field": "preserve-me"
        },
        "last_refresh": "2026-08-13T10:00:00Z",
        "future_top_level_field": { "enabled": true }
    })
}

#[test]
fn resolves_explicit_codex_home_before_default_home() {
    assert_eq!(
        resolve_codex_home(
            Some(OsString::from("/tmp/custom-codex")),
            Some(PathBuf::from("/tmp/home")),
        )
        .unwrap(),
        PathBuf::from("/tmp/custom-codex")
    );
    assert_eq!(
        resolve_codex_home(None, Some(PathBuf::from("/tmp/home"))).unwrap(),
        PathBuf::from("/tmp/home/.codex")
    );
    assert!(matches!(
        resolve_codex_home(None, None),
        Err(AuthError::HomeDirectoryUnavailable)
    ));
}

#[test]
fn loads_chatgpt_credentials_without_exposing_secrets() {
    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let auth = AuthFile::load(path).unwrap();
    let credentials = auth.credentials().unwrap();

    assert_eq!(credentials.account_id(), "account-123");
    let debug = format!("{credentials:?}");
    assert!(!debug.contains("synthetic-access-secret"));
    assert!(!debug.contains("synthetic-refresh-secret"));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn rejects_missing_file_malformed_json_and_non_chatgpt_auth() {
    let directory = tempdir().unwrap();
    assert!(matches!(
        AuthFile::load(directory.path().join("auth.json")),
        Err(AuthError::FileRequired { .. })
    ));

    let malformed = directory.path().join("malformed.json");
    fs::write(&malformed, b"{not json").unwrap();
    assert!(matches!(AuthFile::load(malformed), Err(AuthError::Malformed { .. })));

    let api_key = write_auth(
        &directory.path().join("api"),
        json!({ "auth_mode": "api", "OPENAI_API_KEY": "synthetic-api-secret" }),
    );
    let error = AuthFile::load(api_key).unwrap_err();
    assert!(matches!(error, AuthError::UnsupportedAuthMode { .. }));
    assert!(!error.to_string().contains("synthetic-api-secret"));
}

#[test]
fn reports_each_missing_chatgpt_field_without_secret_values() {
    for field in ["access_token", "refresh_token", "account_id"] {
        let directory = tempdir().unwrap();
        let mut value = valid_auth();
        value["tokens"].as_object_mut().unwrap().remove(field);
        let path = write_auth(directory.path(), value);
        let error = AuthFile::load(path).unwrap_err();
        assert!(matches!(error, AuthError::MissingCredentialField { .. }));
        assert!(error.to_string().contains(field));
        assert!(!error.to_string().contains("synthetic-"));
    }
}
```

- [ ] **Step 3: Run the credential tests and verify the new module is absent**

Run: `cargo test --test codex_auth`

Expected: compilation fails because `moh::codex_auth` does not exist.

- [ ] **Step 4: Implement defensive parsing, pure path resolution, and redacted secret types**

Add `pub mod codex_auth;` to `src/lib.rs`. Implement `src/codex_auth.rs` with these concrete shapes:

```rust
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone)]
struct Secret(String);

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

#[derive(Clone, Debug)]
pub struct CodexCredentials {
    access_token: Secret,
    refresh_token: Secret,
    account_id: String,
}

impl CodexCredentials {
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

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("could not resolve the home directory for Codex credentials")]
    HomeDirectoryUnavailable,
    #[error("file-backed Codex credentials are required at {path}; set cli_auth_credentials_store = \"file\" and run `codex login`")]
    FileRequired { path: PathBuf },
    #[error("could not read Codex credentials at {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("Codex credentials at {path} are malformed")]
    Malformed { path: PathBuf },
    #[error("unsupported Codex auth mode {mode:?}; sign in with ChatGPT using file-backed storage")]
    UnsupportedAuthMode { mode: Option<String> },
    #[error("Codex ChatGPT credentials are missing {field}")]
    MissingCredentialField { field: &'static str },
}

pub fn resolve_codex_home(
    codex_home: Option<OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf, AuthError> {
    if let Some(codex_home) = codex_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(codex_home));
    }
    home.map(|path| path.join(".codex"))
        .ok_or(AuthError::HomeDirectoryUnavailable)
}

impl AuthFile {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let path = path.into();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(AuthError::FileRequired { path });
            }
            Err(source) => return Err(AuthError::Read { path, source }),
        };
        let document = serde_json::from_slice::<AuthDocument>(&bytes)
            .map_err(|_| AuthError::Malformed { path: path.clone() })?;
        let auth = Self { path, document };
        auth.credentials()?;
        Ok(auth)
    }

    pub fn load_from_env() -> Result<Self, AuthError> {
        let codex_home = std::env::var_os("CODEX_HOME");
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        Self::load(resolve_codex_home(codex_home, home)?.join("auth.json"))
    }

    pub fn credentials(&self) -> Result<CodexCredentials, AuthError> {
        if self.document.auth_mode.as_deref() != Some("chatgpt") {
            return Err(AuthError::UnsupportedAuthMode {
                mode: self.document.auth_mode.clone(),
            });
        }
        let tokens = self.document.tokens.as_ref().ok_or(
            AuthError::MissingCredentialField { field: "tokens" },
        )?;
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
```

Add rustdoc for every public item so `#![warn(missing_docs)]` remains clean. Do not derive `Debug` for `AuthDocument`, `TokenDocument`, or `Secret`.

- [ ] **Step 5: Run focused and full existing tests**

Run: `cargo test --test codex_auth && cargo test --all-targets`

Expected: all tests pass, including the pre-existing renderer and component suites.

- [ ] **Step 6: Commit credential loading**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/codex_auth.rs tests/codex_auth.rs
git commit -m "feat: load file-backed Codex credentials"
```

---

### Task 2: Refresh and atomically persist rotated Codex tokens

**Files:**
- Modify: `src/codex_auth.rs`
- Modify: `tests/codex_auth.rs`

**Interfaces:**
- Consumes: `AuthFile` and `reqwest::Client` from Task 1.
- Produces: `pub async fn AuthFile::refresh(&mut self, client: &reqwest::Client, endpoint: &str) -> Result<CodexCredentials, AuthError>` plus classified refresh and concurrent-change error variants.

- [ ] **Step 1: Add failing refresh protocol and persistence tests**

Append Tokio/Wiremock tests to `tests/codex_auth.rs`. Use the exact OAuth JSON contract and assert secrets never enter error text:

```rust
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::{body_json, method, path}};

#[tokio::test]
async fn refresh_rotates_tokens_preserves_unknown_fields_and_sets_private_permissions() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": "synthetic-refresh-secret"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "rotated-access-secret",
            "refresh_token": "rotated-refresh-secret",
            "id_token": "rotated-id-secret"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let mut auth = AuthFile::load(&path).unwrap();
    let credentials = auth
        .refresh(&reqwest::Client::new(), &format!("{}/oauth/token", server.uri()))
        .await
        .unwrap();

    assert_eq!(credentials.account_id(), "account-123");
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["tokens"]["access_token"], "rotated-access-secret");
    assert_eq!(stored["tokens"]["refresh_token"], "rotated-refresh-secret");
    assert_eq!(stored["tokens"]["future_token_field"], "preserve-me");
    assert_eq!(stored["future_top_level_field"]["enabled"], true);
    #[cfg(unix)]
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn refresh_classifies_permanent_failures_without_leaking_the_body() {
    for (code, expected) in [
        ("refresh_token_expired", "expired"),
        ("refresh_token_reused", "already used"),
        ("refresh_token_invalidated", "revoked"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": { "code": code, "message": "synthetic-refresh-secret" }
            })))
            .mount(&server)
            .await;
        let directory = tempdir().unwrap();
        let path = write_auth(directory.path(), valid_auth());
        let mut auth = AuthFile::load(path).unwrap();
        let error = auth
            .refresh(&reqwest::Client::new(), &server.uri())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected));
        assert!(!error.to_string().contains("synthetic-refresh-secret"));
    }
}

#[tokio::test]
async fn refresh_refuses_to_overwrite_credentials_rotated_concurrently() {
    let server = MockServer::start().await;
    let path_for_responder = std::sync::Arc::new(std::sync::Mutex::new(None::<PathBuf>));
    let responder_path = std::sync::Arc::clone(&path_for_responder);
    Mock::given(method("POST"))
        .respond_with(move |_request: &wiremock::Request| {
            let path = responder_path.lock().unwrap().clone().unwrap();
            let mut changed = valid_auth();
            changed["tokens"]["refresh_token"] = json!("newer-refresh-secret");
            fs::write(path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "rotated-access-secret",
                "refresh_token": "rotated-refresh-secret"
            }))
        })
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    *path_for_responder.lock().unwrap() = Some(path.clone());
    let mut auth = AuthFile::load(&path).unwrap();
    assert!(matches!(
        auth.refresh(&reqwest::Client::new(), &server.uri()).await,
        Err(AuthError::ConcurrentCredentialChange)
    ));
    let stored = fs::read_to_string(path).unwrap();
    assert!(stored.contains("newer-refresh-secret"));
    assert!(!stored.contains("rotated-refresh-secret"));
}
```

Guard the Unix permission import and assertion with `#[cfg(unix)]` so the suite compiles cross-platform.

- [ ] **Step 2: Run refresh tests and verify `refresh` is missing**

Run: `cargo test --test codex_auth refresh`

Expected: compilation fails because `AuthFile::refresh` and the new error variants do not exist.

- [ ] **Step 3: Implement the exact refresh request, safe classification, compare-before-write, and atomic replacement**

Extend `src/codex_auth.rs` with:

```rust
use chrono::Utc;
use serde_json::json;
use tempfile::NamedTempFile;

const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

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

#[derive(Debug, Clone, Copy)]
pub enum RefreshFailure {
    Expired,
    Reused,
    Revoked,
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

impl AuthFile {
    pub async fn refresh(
        &mut self,
        client: &reqwest::Client,
        endpoint: &str,
    ) -> Result<CodexCredentials, AuthError> {
        let before = Self::load(&self.path)?;
        let before_credentials = before.credentials()?;
        let response = client
            .post(endpoint)
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
        let current = Self::load(&self.path)?;
        let current_credentials = current.credentials()?;
        if current_credentials.account_id() != before_credentials.account_id()
            || current_credentials.refresh_token() != before_credentials.refresh_token()
        {
            return Err(AuthError::ConcurrentCredentialChange);
        }

        let mut document = current.document;
        let tokens = document.tokens.as_mut().expect("validated credentials have tokens");
        tokens.access_token = Some(rotated.access_token.ok_or(
            AuthError::MissingCredentialField { field: "refreshed access_token" },
        )?);
        if let Some(refresh_token) = rotated.refresh_token {
            tokens.refresh_token = Some(refresh_token);
        }
        if let Some(id_token) = rotated.id_token {
            tokens.id_token = Some(id_token);
        }
        document.extra.insert("last_refresh".into(), json!(Utc::now()));
        persist_atomically(&self.path, &document)?;
        self.document = document;
        self.credentials()
    }
}

fn refresh_error_code(value: &RefreshErrorEnvelope) -> Option<String> {
    match value.error.as_ref() {
        Some(RefreshErrorValue::Code(code)) => Some(code.clone()),
        Some(RefreshErrorValue::Object { code }) => code.clone(),
        None => value.code.clone(),
    }
}

fn persist_atomically(path: &Path, document: &AuthDocument) -> Result<(), AuthError> {
    let parent = path.parent().ok_or_else(|| AuthError::Persist {
        path: path.to_owned(),
        source: std::io::Error::other("auth path has no parent"),
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| AuthError::Persist {
        path: path.to_owned(), source,
    })?;
    #[cfg(unix)]
    temporary.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|source| AuthError::Persist { path: path.to_owned(), source })?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), document)
        .map_err(|source| AuthError::Persist {
            path: path.to_owned(), source: std::io::Error::other(source),
        })?;
    temporary.as_file_mut().sync_all().map_err(|source| AuthError::Persist {
        path: path.to_owned(), source,
    })?;
    temporary.persist(path).map_err(|error| AuthError::Persist {
        path: path.to_owned(), source: error.error,
    })?;
    Ok(())
}
```

Insert these exact variants into `AuthError` before adding the `AuthFile::refresh` implementation above:

```rust
#[error("Codex token refresh failed: {0}")]
RefreshFailed(RefreshFailure),
#[error("Codex credentials changed while refresh was in progress; retry the request")]
ConcurrentCredentialChange,
#[error("could not persist refreshed Codex credentials at {path}: {source}")]
Persist { path: PathBuf, source: std::io::Error },
#[error("Codex token refresh transport failed")]
RefreshTransport,
```

Import `std::os::unix::fs::PermissionsExt` behind `#[cfg(unix)]`. Keep provider response bodies out of every formatted error.
Add rustdoc to `RefreshFailure`, every new `AuthError` variant, and `AuthFile::refresh`.

- [ ] **Step 4: Run refresh, redaction, and full tests**

Run: `cargo test --test codex_auth && cargo test --all-targets`

Expected: all tests pass; each mock expects exactly one refresh request.

- [ ] **Step 5: Commit safe refresh**

```bash
git add src/codex_auth.rs tests/codex_auth.rs
git commit -m "feat: refresh Codex credentials safely"
```

---

### Task 3: Send Codex Responses requests through Rig with one auth retry

**Files:**
- Modify: `src/lib.rs`
- Create: `src/codex_provider.rs`
- Create: `tests/codex_provider.rs`

**Interfaces:**
- Consumes: `AuthFile`, `CodexCredentials`, and `AuthFile::refresh` from Tasks 1-2; Rig `Message` values.
- Produces: `pub trait ChatBackend: Clone + Send + Sync + 'static`, `pub type ChatFuture = Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>`, `pub struct CodexProvider`, `pub struct ProviderConfig`, `pub enum ProviderError`, `CodexProvider::from_env()`, `CodexProvider::new(auth, config)`, and `ChatBackend::complete(messages)`.

- [ ] **Step 1: Write failing mock-server tests for request shape, response text, and retry bounds**

Create `tests/codex_provider.rs` with helpers that build a temporary `AuthFile`, then add:

```rust
use moh::{
    codex_auth::AuthFile,
    codex_provider::{ChatBackend, CodexProvider, ProviderConfig, ProviderError},
};
use rig::message::Message;
use serde_json::json;
use tempfile::{TempDir, tempdir};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::{header, method, path}};

fn synthetic_auth_file() -> (TempDir, AuthFile) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "synthetic-id-secret",
            "access_token": "synthetic-access-secret",
            "refresh_token": "synthetic-refresh-secret",
            "account_id": "account-123"
        }
    })).unwrap()).unwrap();
    let auth = AuthFile::load(path).unwrap();
    (directory, auth)
}

fn success_response(text: &str) -> serde_json::Value {
    json!({
        "id": "resp_test",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": null,
        "max_output_tokens": null,
        "model": "gpt-5.6-luna",
        "usage": {
            "input_tokens": 1,
            "output_tokens": 2,
            "total_tokens": 3
        },
        "output": [{
            "type": "message",
            "id": "msg_test",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "text": text
            }]
        }],
        "tools": []
    })
}

#[tokio::test]
async fn sends_history_model_and_medium_reasoning_to_codex_responses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer synthetic-access-secret"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_response("second answer")))
        .expect(1)
        .mount(&server)
        .await;
    let (_directory, auth) = synthetic_auth_file();
    let provider = CodexProvider::new(
        auth,
        ProviderConfig {
            api_base: server.uri(),
            refresh_url: format!("{}/oauth/token", server.uri()),
        },
    );

    let answer = provider.complete(vec![
        Message::user("first question"),
        Message::assistant("first answer"),
        Message::user("second question"),
    ]).await.unwrap();
    assert_eq!(answer, "second answer");

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "gpt-5.6-luna");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["input"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn refreshes_and_retries_once_after_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error":"expired"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "rotated-access-secret",
            "refresh_token": "rotated-refresh-secret"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer rotated-access-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_response("recovered")))
        .expect(1)
        .mount(&server)
        .await;

    let (_directory, auth) = synthetic_auth_file();
    let provider = CodexProvider::new(auth, ProviderConfig {
        api_base: server.uri(),
        refresh_url: format!("{}/oauth/token", server.uri()),
    });
    assert_eq!(provider.complete(vec![Message::user("hello")]).await.unwrap(), "recovered");
}

#[tokio::test]
async fn does_not_refresh_non_auth_failures_or_retry_a_second_unauthorized() {
    for status in [500, 401] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        if status == 401 {
            Mock::given(method("POST"))
                .and(path("/oauth/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "rotated-access-secret",
                    "refresh_token": "rotated-refresh-secret"
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let (_directory, auth) = synthetic_auth_file();
        let provider = CodexProvider::new(auth, ProviderConfig {
            api_base: server.uri(),
            refresh_url: format!("{}/oauth/token", server.uri()),
        });
        assert!(matches!(
            provider.complete(vec![Message::user("hello")]).await,
            Err(ProviderError::Request { .. })
        ));
        let requests = server.received_requests().await.unwrap();
        let refreshes = requests.iter().filter(|request| request.url.path() == "/oauth/token").count();
        assert_eq!(refreshes, usize::from(status == 401));
    }
}
```

Use the `success_response(text)` helper shown above. It is complete test data for Rig 0.41's response deserializer, not production logic.

- [ ] **Step 2: Run provider tests and verify the module is absent**

Run: `cargo test --test codex_provider`

Expected: compilation fails because `moh::codex_provider` does not exist.

- [ ] **Step 3: Implement the boxed-future backend boundary and Rig request adapter**

Add `pub mod codex_provider;` to `src/lib.rs`. Implement `src/codex_provider.rs` around these exact interfaces:

```rust
use std::{future::Future, pin::Pin, sync::Arc};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rig::{
    client::CompletionClient,
    completion::{AssistantContent, CompletionError, CompletionModel},
    message::Message,
    providers::openai::{self, responses_api::{Reasoning, ReasoningEffort}},
};
use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::codex_auth::{AuthError, AuthFile};

pub const MODEL: &str = "gpt-5.6-luna";
pub const DEFAULT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";

pub type ChatFuture = Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send>>;

pub trait ChatBackend: Clone + Send + Sync + 'static {
    fn complete(&self, messages: Vec<Message>) -> ChatFuture;
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub api_base: String,
    pub refresh_url: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.into(),
            refresh_url: DEFAULT_REFRESH_URL.into(),
        }
    }
}

#[derive(Clone)]
pub struct CodexProvider {
    inner: Arc<Inner>,
}

struct Inner {
    auth: Mutex<AuthFile>,
    http: reqwest::Client,
    config: ProviderConfig,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("could not construct the Codex model client")]
    Client,
    #[error("Codex request failed with HTTP status {status:?}")]
    Request { status: Option<u16> },
    #[error("Codex returned no assistant text")]
    EmptyResponse,
}

#[derive(Debug, Error)]
enum AttemptError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Completion(#[from] CompletionError),
    #[error("could not construct the Codex model client")]
    Client,
    #[error("Codex returned no assistant text")]
    Empty,
}

impl CodexProvider {
    pub fn from_env() -> Result<Self, ProviderError> {
        Ok(Self::new(AuthFile::load_from_env()?, ProviderConfig::default()))
    }

    pub fn new(auth: AuthFile, config: ProviderConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                auth: Mutex::new(auth),
                http: reqwest::Client::new(),
                config,
            }),
        }
    }

    async fn attempt(&self, mut messages: Vec<Message>) -> Result<String, AttemptError> {
        let credentials = self.inner.auth.lock().await.credentials()?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_str(credentials.account_id()).map_err(|_| AttemptError::Client)?,
        );
        let client = openai::Client::builder()
            .api_key(credentials.access_token().to_owned())
            .base_url(&self.inner.config.api_base)
            .http_headers(headers)
            .build()
            .map_err(|_| AttemptError::Client)?;
        let model = client.completion_model(MODEL);
        let prompt = messages.pop().ok_or(AttemptError::Empty)?;
        let response = model
            .completion_request(prompt)
            .messages(messages)
            .additional_params(json!({
                "reasoning": Reasoning::new().with_effort(ReasoningEffort::Medium)
            }))
            .send()
            .await?;
        let text = response.choice.iter().filter_map(|item| match item {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("");
        if text.trim().is_empty() {
            return Err(AttemptError::Empty);
        }
        Ok(text)
    }
}

impl ChatBackend for CodexProvider {
    fn complete(&self, messages: Vec<Message>) -> ChatFuture {
        let provider = self.clone();
        Box::pin(async move {
            match provider.attempt(messages.clone()).await {
                Ok(answer) => Ok(answer),
                Err(AttemptError::Completion(error))
                    if error.provider_response_status().map(|s| s.as_u16()) == Some(401) =>
                {
                    provider.inner.auth.lock().await.refresh(
                        &provider.inner.http,
                        &provider.inner.config.refresh_url,
                    ).await?;
                    provider.attempt(messages).await.map_err(map_attempt_error)
                }
                Err(error) => Err(map_attempt_error(error)),
            }
        })
    }
}

fn map_attempt_error(error: AttemptError) -> ProviderError {
    match error {
        AttemptError::Auth(error) => ProviderError::Auth(error),
        AttemptError::Client => ProviderError::Client,
        AttemptError::Empty => ProviderError::EmptyResponse,
        AttemptError::Completion(error) => ProviderError::Request {
            status: error.provider_response_status().map(|status| status.as_u16()),
        },
    }
}
```

Task 1's `CodexCredentials::access_token` must be `pub(crate)`. Keep `AttemptError` private so only the stable, redacted `ProviderError` boundary is exposed to conversation and UI code.
Add rustdoc to all public constants, types, fields, trait methods, constructors, and error variants in this module.

- [ ] **Step 4: Run request-shape tests and inspect captured JSON on failure**

Run: `cargo test --test codex_provider -- --nocapture`

Expected: all three provider tests pass; the first body contains three ordered messages, `gpt-5.6-luna`, and `reasoning.effort = "medium"`.

- [ ] **Step 5: Run Clippy now to catch public-boundary and boxed-future issues**

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: no warnings, especially no secret-bearing derived `Debug` and no needless collection or visibility warnings.

- [ ] **Step 6: Commit the Rig provider**

```bash
git add src/lib.rs src/codex_provider.rs tests/codex_provider.rs
git commit -m "feat: send Codex requests through Rig"
```

---

### Task 4: Add transactional in-memory conversation state

**Files:**
- Modify: `src/lib.rs`
- Create: `src/conversation.rs`
- Create: `tests/conversation.rs`

**Interfaces:**
- Consumes: `ChatBackend`, `ChatFuture`, `ProviderError`, and Rig `Message` from Task 3.
- Produces: `pub struct Conversation<B: ChatBackend>`, `pub struct Turn`, `pub enum ConversationError`, `Conversation::new`, `Conversation::is_busy`, `Conversation::turns`, `Conversation::start_turn`, and `Conversation::resolve_turn`.

- [ ] **Step 1: Write failing tests for ordered history, busy rejection, commit, and rollback**

Create `tests/conversation.rs`:

```rust
use std::sync::{Arc, Mutex};

use moh::{
    codex_provider::{ChatBackend, ChatFuture, ProviderError},
    conversation::{Conversation, ConversationError},
};
use rig::message::Message;

#[derive(Clone)]
struct RecordingBackend {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    answers: Arc<Mutex<Vec<Result<String, ProviderError>>>>,
}

impl ChatBackend for RecordingBackend {
    fn complete(&self, messages: Vec<Message>) -> ChatFuture {
        self.requests.lock().unwrap().push(messages);
        let answer = self.answers.lock().unwrap().remove(0);
        Box::pin(async move { answer })
    }
}

#[tokio::test]
async fn successful_turns_commit_and_feed_ordered_history_to_the_next_request() {
    let backend = RecordingBackend {
        requests: Arc::default(),
        answers: Arc::new(Mutex::new(vec![Ok("one".into()), Ok("two".into())])),
    };
    let requests = Arc::clone(&backend.requests);
    let mut conversation = Conversation::new(backend);

    let first = conversation.start_turn("first".into()).unwrap().await;
    assert_eq!(conversation.resolve_turn(first).unwrap(), "one");
    let second = conversation.start_turn("second".into()).unwrap().await;
    assert_eq!(conversation.resolve_turn(second).unwrap(), "two");

    let calls = requests.lock().unwrap();
    assert_eq!(calls[0], vec![Message::user("first")]);
    assert_eq!(calls[1], vec![
        Message::user("first"),
        Message::assistant("one"),
        Message::user("second"),
    ]);
    assert_eq!(conversation.turns().len(), 2);
}

#[tokio::test]
async fn failed_turn_is_visible_to_the_caller_but_not_committed_to_history() {
    let backend = RecordingBackend {
        requests: Arc::default(),
        answers: Arc::new(Mutex::new(vec![
            Err(ProviderError::Request { status: Some(500) }),
            Ok("recovered".into()),
        ])),
    };
    let requests = Arc::clone(&backend.requests);
    let mut conversation = Conversation::new(backend);
    let failed = conversation.start_turn("failed".into()).unwrap().await;
    assert!(conversation.resolve_turn(failed).is_err());
    assert!(!conversation.is_busy());
    assert!(conversation.turns().is_empty());

    let retry = conversation.start_turn("retry".into()).unwrap().await;
    conversation.resolve_turn(retry).unwrap();
    assert_eq!(requests.lock().unwrap()[1], vec![Message::user("retry")]);
}

#[test]
fn rejects_a_second_turn_while_one_is_pending() {
    let backend = RecordingBackend {
        requests: Arc::default(),
        answers: Arc::new(Mutex::new(vec![Ok("answer".into())])),
    };
    let mut conversation = Conversation::new(backend);
    let _pending = conversation.start_turn("first".into()).unwrap();
    assert!(matches!(
        conversation.start_turn("second".into()),
        Err(ConversationError::Busy)
    ));
}
```

- [ ] **Step 2: Run the conversation tests and verify the module is absent**

Run: `cargo test --test conversation`

Expected: compilation fails because `moh::conversation` does not exist.

- [ ] **Step 3: Implement the single-pending-turn transaction boundary**

Add `pub mod conversation;` to `src/lib.rs`. Implement `src/conversation.rs`:

```rust
use rig::message::Message;
use thiserror::Error;

use crate::codex_provider::{ChatBackend, ChatFuture, ProviderError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Turn {
    pub user: String,
    pub assistant: String,
}

pub type PendingResult = Result<String, ProviderError>;

pub struct Conversation<B: ChatBackend> {
    backend: B,
    turns: Vec<Turn>,
    pending_user: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConversationError {
    #[error("a conversation request is already running")]
    Busy,
    #[error("no conversation request is pending")]
    NotBusy,
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

impl<B: ChatBackend> Conversation<B> {
    pub fn new(backend: B) -> Self {
        Self { backend, turns: Vec::new(), pending_user: None }
    }

    pub fn is_busy(&self) -> bool {
        self.pending_user.is_some()
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn start_turn(&mut self, user: String) -> Result<ChatFuture, ConversationError> {
        if self.is_busy() {
            return Err(ConversationError::Busy);
        }
        let mut messages = Vec::with_capacity(self.turns.len() * 2 + 1);
        for turn in &self.turns {
            messages.push(Message::user(turn.user.clone()));
            messages.push(Message::assistant(turn.assistant.clone()));
        }
        messages.push(Message::user(user.clone()));
        self.pending_user = Some(user);
        Ok(self.backend.complete(messages))
    }

    pub fn resolve_turn(
        &mut self,
        result: PendingResult,
    ) -> Result<String, ConversationError> {
        let user = self.pending_user.take().ok_or(ConversationError::NotBusy)?;
        match result {
            Ok(assistant) => {
                self.turns.push(Turn { user, assistant: assistant.clone() });
                Ok(assistant)
            }
            Err(error) => Err(ConversationError::Provider(error)),
        }
    }
}
```

Add rustdoc to every public item. Keep `Turn` text-only; do not retain reasoning blocks or transport metadata.

- [ ] **Step 4: Run focused and full tests**

Run: `cargo test --test conversation && cargo test --all-targets`

Expected: all tests pass, and the failed prompt never appears in the second captured request.

- [ ] **Step 5: Commit conversation state**

```bash
git add src/lib.rs src/conversation.rs tests/conversation.rs
git commit -m "feat: add in-memory conversation state"
```

---

### Task 5: Wire one responsive async request into the existing demo

**Files:**
- Modify: `src/main.rs`
- Modify: `src/demo.rs`

**Interfaces:**
- Consumes: `CodexProvider::from_env`, `Conversation<CodexProvider>`, `PendingTurn`, and `CompletedTurn` from Task 4; `ChatFuture` remains the `ChatBackend` implementation return type; existing `Tui`, `EventSource`, `Container`, `Input`, and `Text` APIs.
- Produces: async `demo::run`, generic async `run_to_completion`/`run_event_loop` test seams, busy input suppression, and visible answer/error status updates.

- [ ] **Step 1: Replace deterministic-response tests with failing conversation UI state tests**

In `src/demo.rs` tests, remove `deterministic_response_counts_unicode_scalars` and update the scripted application tests to use a `RecordingBackend`. Add these focused cases:

```rust
#[derive(Clone)]
struct ImmediateBackend {
    answers: std::sync::Arc<std::sync::Mutex<VecDeque<Result<String, ProviderError>>>>,
}

impl ImmediateBackend {
    fn new(answers: impl IntoIterator<Item = Result<String, ProviderError>>) -> Self {
        Self {
            answers: std::sync::Arc::new(std::sync::Mutex::new(answers.into_iter().collect())),
        }
    }
}

impl ChatBackend for ImmediateBackend {
    fn complete(&self, _messages: Vec<rig::message::Message>) -> ChatFuture {
        let answer = self.answers.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { answer })
    }
}

#[derive(Clone)]
struct NeverBackend;

impl ChatBackend for NeverBackend {
    fn complete(&self, _messages: Vec<rig::message::Message>) -> ChatFuture {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn successful_request_appends_model_answer_and_returns_to_ready() {
    let backend = ImmediateBackend::new([Ok("model answer".into())]);
    let terminal = RecordingTerminal::new(None);
    let bytes = Rc::clone(&terminal.bytes);
    let (mut tui, mut ids) = build(terminal).unwrap();
    let mut conversation = Conversation::new(backend);
    let mut events = ScriptedEvents {
        events: [
            Ok(InputEvent::Paste("hello".into())),
            Ok(key(Key::Enter)),
            Ok(control('c')),
        ].into_iter().collect(),
    };

    run_to_completion(&mut tui, &mut ids, &mut events, &mut conversation)
        .await
        .unwrap();
    let output = String::from_utf8(bytes.borrow().clone()).unwrap();
    assert!(output.contains("you: hello"));
    assert!(output.contains("thinking..."));
    assert!(output.contains("moh: model answer"));
    assert!(output.contains("ready"));
}

#[tokio::test]
async fn failed_request_is_rendered_and_does_not_block_the_next_submission() {
    let backend = ImmediateBackend::new([
        Err(ProviderError::Request { status: Some(500) }),
        Ok("recovered".into()),
    ]);
    let terminal = RecordingTerminal::new(None);
    let bytes = Rc::clone(&terminal.bytes);
    let (mut tui, mut ids) = build(terminal).unwrap();
    let mut conversation = Conversation::new(backend);
    let mut events = ScriptedEvents {
        events: [
            Ok(InputEvent::Paste("first".into())),
            Ok(key(Key::Enter)),
            Ok(InputEvent::Paste("second".into())),
            Ok(key(Key::Enter)),
            Ok(control('c')),
        ].into_iter().collect(),
    };

    run_to_completion(&mut tui, &mut ids, &mut events, &mut conversation)
        .await
        .unwrap();
    let output = String::from_utf8(bytes.borrow().clone()).unwrap();
    assert!(output.contains("moh: error: Codex request failed with HTTP status Some(500)"));
    assert!(output.contains("moh: recovered"));
    assert_eq!(conversation.turns().len(), 1);
    assert_eq!(conversation.turns()[0].user, "second");
}

#[tokio::test]
async fn exit_remains_responsive_while_a_request_never_completes() {
    let backend = NeverBackend;
    let terminal = RecordingTerminal::new(None);
    let (mut tui, mut ids) = build(terminal).unwrap();
    let mut conversation = Conversation::new(backend);
    let mut events = ScriptedEvents {
        events: [
            Ok(InputEvent::Paste("hello".into())),
            Ok(key(Key::Enter)),
            Ok(control('c')),
        ].into_iter().collect(),
    };
    tokio::time::timeout(
        Duration::from_millis(250),
        run_to_completion(&mut tui, &mut ids, &mut events, &mut conversation),
    ).await.expect("Ctrl+C should cancel the UI wait").unwrap();
}

#[tokio::test]
async fn help_and_resize_remain_responsive_while_a_request_is_pending() {
    let terminal = RecordingTerminal::new(None);
    let bytes = Rc::clone(&terminal.bytes);
    let (mut tui, mut ids) = build(terminal).unwrap();
    let mut conversation = Conversation::new(NeverBackend);
    let mut events = ScriptedEvents {
        events: [
            Ok(InputEvent::Paste("hello".into())),
            Ok(key(Key::Enter)),
            Ok(control('o')),
            Ok(InputEvent::Resize { width: 80, height: 24 }),
            Ok(key(Key::Escape)),
            Ok(control('c')),
        ].into_iter().collect(),
    };

    tokio::time::timeout(
        Duration::from_millis(250),
        run_to_completion(&mut tui, &mut ids, &mut events, &mut conversation),
    ).await.expect("help, resize, and exit should continue polling").unwrap();
    let output = String::from_utf8(bytes.borrow().clone()).unwrap();
    assert!(output.contains("moh help"));
}

#[test]
fn text_input_is_not_dispatched_while_busy() {
    assert!(!should_dispatch_to_component(true, &key(Key::Char('x'))));
    assert!(!should_dispatch_to_component(true, &key(Key::Enter)));
    assert!(should_dispatch_to_component(false, &key(Key::Char('x'))));
}
```

Keep synthetic provider errors free of token-like text.

- [ ] **Step 2: Run demo tests and verify async signatures/behavior are missing**

Run: `cargo test --bin moh demo::tests`

Expected: compilation fails because `run_to_completion` lacks the conversation argument and is synchronous.

- [ ] **Step 3: Make main and terminal lifecycle async without weakening cleanup**

Change `src/main.rs` to:

```rust
mod demo;

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return std::process::ExitCode::FAILURE,
    };
    let result = runtime.block_on(demo::run());
    drop(runtime);
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("moh: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
```

In `src/demo.rs`, introduce an application error that preserves TUI cleanup:

```rust
#[derive(Debug, thiserror::Error)]
pub enum DemoError {
    #[error(transparent)]
    Tui(#[from] moh::tui::RenderError),
    #[error(transparent)]
    Provider(#[from] moh::codex_provider::ProviderError),
    #[error(transparent)]
    Conversation(#[from] moh::conversation::ConversationError),
}

pub async fn run() -> Result<(), DemoError> {
    let provider = CodexProvider::from_env()?;
    let mut conversation = Conversation::new(provider);
    let mut session = TerminalSession::start()?;
    let application_result = run_application(&mut conversation).await;
    let restore_result = session.restore().map_err(DemoError::from);
    application_result.and(restore_result)
}
```

Keep `TerminalSession::restore` explicit and preserve the existing rule that an application error wins while restoration is still attempted.

Thread the conversation through the existing test seams with these signatures:

```rust
async fn run_application<B: ChatBackend>(
    conversation: &mut Conversation<B>,
) -> Result<(), DemoError> {
    let terminal = CrosstermTerminal::new(io::stdout());
    let (mut tui, mut ids) = build(terminal)?;
    let mut events = CrosstermEvents;
    run_to_completion(&mut tui, &mut ids, &mut events, conversation).await
}

async fn run_to_completion<T: Terminal, E: EventSource, B: ChatBackend>(
    tui: &mut Tui<T>,
    ids: &mut DemoIds,
    events: &mut E,
    conversation: &mut Conversation<B>,
) -> Result<(), DemoError> {
    let application_result = run_event_loop(tui, ids, events, conversation).await;
    let finish_result = tui.finish().map_err(DemoError::from);
    application_result.and(finish_result)
}
```

- [ ] **Step 4: Replace echo handling with a single optional pending turn**

Delete `response_for`. Update the introduction/help copy to describe real conversation without adding controls. Add:

```rust
const INTRODUCTION: &str =
    "moh — gpt-5.6-luna · medium reasoning\nEnter sends · Ctrl+O help · Ctrl+C exits";

fn should_dispatch_to_component(busy: bool, event: &InputEvent) -> bool {
    if !busy {
        return true;
    }
    match event {
        InputEvent::Resize { .. } | InputEvent::Key { key: Key::Escape, .. } => true,
        InputEvent::Key {
            key: Key::Char('c' | 'C' | 'o' | 'O'),
            modifiers,
        } => modifiers.control,
        _ => false,
    }
}

fn begin_request<T: Terminal, B: ChatBackend>(
    tui: &mut Tui<T>,
    ids: &DemoIds,
    conversation: &mut Conversation<B>,
    text: String,
) -> Result<moh::conversation::PendingTurn, DemoError> {
    tui.component_mut::<Container>(ids.transcript)?
        .push(Text::new(format!("you: {text}")));
    tui.component_mut::<Text>(ids.status)?.set_text("thinking...");
    tui.request_render();
    Ok(conversation.start_turn(text)?)
}

fn apply_response<T: Terminal, B: ChatBackend>(
    tui: &mut Tui<T>,
    ids: &DemoIds,
    conversation: &mut Conversation<B>,
    completed: moh::conversation::CompletedTurn,
) -> Result<(), DemoError> {
    match conversation.resolve_turn(completed) {
        Ok(answer) => {
            tui.component_mut::<Container>(ids.transcript)?
                .push(Text::new(format!("moh: {answer}")));
            tui.component_mut::<Text>(ids.status)?.set_text("ready");
        }
        Err(error) => {
            tui.component_mut::<Container>(ids.transcript)?
                .push(Text::new(format!("moh: error: {error}")));
            tui.component_mut::<Text>(ids.status)?.set_text("error");
        }
    }
    tui.request_render();
    Ok(())
}

fn apply_non_submit_action<T: Terminal>(
    tui: &mut Tui<T>,
    ids: &mut DemoIds,
    action: DemoAction,
) -> Result<bool, DemoError> {
    match action {
        DemoAction::None => {}
        DemoAction::Submit(_) => {
            return Err(DemoError::Tui(RenderError::Io(io::Error::other(
                "submit action reached non-submit handler",
            ))));
        }
        DemoAction::OpenHelp => {
            if ids.help.is_none() {
                ids.help = Some(tui.show_overlay(Text::new(HELP), help_options()));
            }
        }
        DemoAction::CloseHelp => {
            if let Some(help) = ids.help.take() {
                tui.hide_overlay(help);
                tui.focus(ids.input)?;
            }
        }
        DemoAction::Resize => tui.request_render(),
        DemoAction::Exit => return Ok(false),
    }
    Ok(true)
}
```

Use `DemoError` throughout the demo loop; do not add provider or conversation concerns to the reusable `moh::tui::RenderError` enum.

- [ ] **Step 5: Implement the responsive loop by polling terminal events and the pending turn together**

Use one `Option<PendingTurn>` and never spawn the request:

```rust
async fn run_event_loop<T: Terminal, E: EventSource, B: ChatBackend>(
    tui: &mut Tui<T>,
    ids: &mut DemoIds,
    events: &mut E,
    conversation: &mut Conversation<B>,
) -> Result<(), DemoError> {
    let mut running = true;
    let mut pending: Option<moh::conversation::PendingTurn> = None;
    tui.render_if_dirty()?;

    while running {
        let timeout = if pending.is_some() {
            Duration::ZERO
        } else {
            Duration::from_millis(16)
        };
        if let Some(event) = events.poll_event(timeout)? {
            let mut action = reduce(&event, ids.help.is_some(), &InputOutcome::Ignored);
            if action == DemoAction::None
                && should_dispatch_to_component(pending.is_some(), &event)
            {
                let outcome = tui.dispatch_input(&event)?;
                action = reduce(&event, ids.help.is_some(), &outcome);
            }
            match action {
                DemoAction::Submit(text) if pending.is_none() => {
                    pending = Some(begin_request(tui, ids, conversation, text)?);
                }
                DemoAction::Exit => {
                    if let Some(pending) = pending.take() {
                        conversation.abandon_turn(pending)?;
                    }
                    running = false;
                }
                other => running = apply_non_submit_action(tui, ids, other)?,
            }
        }

        let completed = if let Some(pending_turn) = pending.as_mut() {
            tokio::select! {
                completed = pending_turn => Some(completed),
                () = tokio::time::sleep(Duration::from_millis(16)) => None,
            }
        } else {
            None
        };
        if let Some(completed) = completed {
            pending = None;
            apply_response(tui, ids, conversation, completed)?;
        }
        tui.render_if_dirty()?;
    }
    Ok(())
}
```

Split the existing `apply_action` into `apply_non_submit_action`; `Submit` is owned by the loop because it creates the pending turn. When exiting, `pending.take()` transfers ownership to `Conversation::abandon_turn`, which drops the pending future before releasing the busy gate; `run_to_completion` must still call `tui.finish()`.

- [ ] **Step 6: Make every existing demo test async where required and retain renderer cleanup assertions**

Convert tests that call `run_to_completion` to `#[tokio::test]` and `.await`. Give repeated-submission tests an immediate backend with enough queued answers. Keep the VT100 cursor assertion, application-error precedence assertion, and terminal-control sanitization assertion unchanged in meaning.

- [ ] **Step 7: Run demo tests, full tests, and Clippy**

Run:

```bash
cargo test --bin moh demo::tests
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all commands pass; the never-completing request exits within the test timeout; terminal restoration tests still pass.

- [ ] **Step 8: Commit async conversation UI integration**

```bash
git add src/main.rs src/demo.rs
git commit -m "feat: connect the TUI to Codex conversation"
```

---

### Task 6: Add the opt-in live smoke test, document setup, and verify the complete slice

**Files:**
- Create: `tests/codex_live.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: the public `CodexProvider::from_env` and `ChatBackend::complete` interface.
- Produces: one ignored live test and user-facing setup/compatibility documentation.

- [ ] **Step 1: Add an ignored live smoke test that requires an explicit environment opt-in**

Create `tests/codex_live.rs`:

```rust
use moh::codex_provider::{ChatBackend, CodexProvider};
use rig::message::Message;

#[tokio::test]
#[ignore = "uses the developer's real file-backed Codex login and network quota"]
async fn real_codex_login_returns_a_non_empty_luna_answer() {
    assert_eq!(
        std::env::var("MOH_RUN_CODEX_LIVE").as_deref(),
        Ok("1"),
        "set MOH_RUN_CODEX_LIVE=1 to acknowledge real account usage"
    );
    let provider = CodexProvider::from_env().expect("load file-backed Codex credentials");
    let answer = provider
        .complete(vec![Message::user(
            "Reply with exactly: moh live smoke test",
        )])
        .await
        .expect("send live Codex request");
    assert!(!answer.trim().is_empty());
}
```

- [ ] **Step 2: Run ordinary tests and prove the live test is skipped by default**

Run: `cargo test --all-targets`

Expected: all ordinary tests pass and `real_codex_login_returns_a_non_empty_luna_answer` is reported as ignored.

- [ ] **Step 3: Update README setup and limitations**

Replace the deterministic-demo language and add this concise setup section:

````markdown
## Codex authentication

`moh` currently reuses a ChatGPT login created by Codex CLI. Configure Codex to
use file-backed credentials and sign in before starting `moh`:

```toml
# ~/.codex/config.toml
cli_auth_credentials_store = "file"
```

```bash
codex login
cargo run
```

The current conversation uses `gpt-5.6-luna` with medium reasoning. History is
kept only for the current process, one request runs at a time, and answers are
displayed after completion rather than streamed.

This integration targets Codex's ChatGPT backend and cached credential format,
which are not stable third-party APIs. Keyring-backed Codex credentials are not
supported yet. Treat `$CODEX_HOME/auth.json` like a password and never commit or
share it.
````

Also update the opening status paragraph to say the repository now contains a Pi-inspired renderer plus the first authenticated conversation slice; remove the statement that all agent or model functionality is deferred.

- [ ] **Step 4: Run the live compatibility check with the current developer login**

Run:

```bash
MOH_RUN_CODEX_LIVE=1 cargo test --test codex_live \
  real_codex_login_returns_a_non_empty_luna_answer -- --ignored --nocapture
```

Expected: PASS with a non-empty response. If Rig rejects a Codex-specific request field, capture only the HTTP status and structural error code, never the response's authorization data, then replace the provider internals with a custom Rig `CompletionModel` while preserving `ChatBackend` and every higher-level test.

- [ ] **Step 5: Run the complete repository validation suite**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
git status --short
```

Expected: formatting, Clippy, tests, locked build, and whitespace checks pass; status contains only the intended README and live-test changes before the final commit.

- [ ] **Step 6: Commit documentation and live verification**

```bash
git add README.md tests/codex_live.rs
git commit -m "docs: document Codex conversation setup"
```

- [ ] **Step 7: Review the final diff against the design constraints**

Run:

```bash
git diff --stat main...HEAD
git diff --check main...HEAD
git log --oneline main..HEAD
```

Expected: the diff contains only the spec/plan, dependencies, three focused runtime modules, demo/main integration, tests, and README changes; no keyring support, model picker, persistence, streaming, tools, or credential fixtures are present.
