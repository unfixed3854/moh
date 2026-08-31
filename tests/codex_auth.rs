use std::{
    ffi::OsString,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use moh::providers::codex::{
    AuthError, AuthFile, CodexConfig, CodexCredentials, RefreshFailure, resolve_codex_home,
};
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path},
};

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

async fn wait_for_request_count(server: &MockServer, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if server.received_requests().await.unwrap().len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mock server did not receive the expected request");
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
fn empty_home_values_do_not_resolve_a_cwd_relative_codex_directory() {
    assert!(matches!(
        resolve_codex_home(Some(OsString::new()), Some(PathBuf::new())),
        Err(AuthError::HomeDirectoryUnavailable)
    ));
}

#[test]
fn codex_config_uses_current_production_endpoints_by_default() {
    let config = CodexConfig::default();
    assert_eq!(config.api_base, "https://chatgpt.com/backend-api/codex");
    assert_eq!(config.refresh_url, "https://auth.openai.com/oauth/token");
}

#[tokio::test]
async fn loads_chatgpt_credentials_without_exposing_secrets() {
    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let auth = AuthFile::load(path).await.unwrap();
    let credentials: CodexCredentials = auth.credentials().unwrap();

    assert_eq!(credentials.account_id(), "account-123");
    let debug = format!("{credentials:?}");
    assert!(!debug.contains("synthetic-access-secret"));
    assert!(!debug.contains("synthetic-refresh-secret"));
    assert!(debug.contains("[REDACTED]"));
}

#[tokio::test]
async fn rejects_missing_file_malformed_json_and_non_chatgpt_auth() {
    let directory = tempdir().unwrap();
    assert!(matches!(
        AuthFile::load(directory.path().join("auth.json")).await,
        Err(AuthError::FileRequired { .. })
    ));

    let malformed = directory.path().join("malformed.json");
    fs::write(&malformed, b"{not json").unwrap();
    assert!(matches!(
        AuthFile::load(malformed).await,
        Err(AuthError::Malformed { .. })
    ));

    let api_key = write_auth(
        &directory.path().join("api"),
        json!({ "auth_mode": "api", "OPENAI_API_KEY": "synthetic-api-secret" }),
    );
    let error = AuthFile::load(api_key).await.unwrap_err();
    assert!(matches!(error, AuthError::UnsupportedAuthMode { .. }));
    assert!(!error.to_string().contains("synthetic-api-secret"));
}

#[tokio::test]
async fn reports_each_missing_chatgpt_field_without_secret_values() {
    for field in ["access_token", "refresh_token", "account_id"] {
        let directory = tempdir().unwrap();
        let mut value = valid_auth();
        value["tokens"].as_object_mut().unwrap().remove(field);
        let path = write_auth(directory.path(), value);
        let error = AuthFile::load(path).await.unwrap_err();
        assert!(matches!(error, AuthError::MissingCredentialField { .. }));
        assert!(error.to_string().contains(field));
        assert!(!error.to_string().contains("synthetic-"));
    }
}

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
    let mut auth = AuthFile::load(&path).await.unwrap();
    let credentials = auth
        .refresh(&format!("{}/oauth/token", server.uri()))
        .await
        .unwrap();

    assert_eq!(credentials.account_id(), "account-123");
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["tokens"]["access_token"], "rotated-access-secret");
    assert_eq!(stored["tokens"]["refresh_token"], "rotated-refresh-secret");
    assert_eq!(stored["tokens"]["future_token_field"], "preserve-me");
    assert_eq!(stored["future_top_level_field"]["enabled"], true);
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn refresh_classifies_permanent_failures_without_leaking_the_body() {
    for (code, expected, expected_failure) in [
        ("refresh_token_expired", "expired", RefreshFailure::Expired),
        (
            "refresh_token_reused",
            "already used",
            RefreshFailure::Reused,
        ),
        (
            "refresh_token_invalidated",
            "revoked",
            RefreshFailure::Revoked,
        ),
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
        let mut auth = AuthFile::load(path).await.unwrap();
        let error = auth.refresh(&server.uri()).await.unwrap_err();
        assert!(matches!(
            (&error, expected_failure),
            (
                AuthError::RefreshFailed(RefreshFailure::Expired),
                RefreshFailure::Expired
            ) | (
                AuthError::RefreshFailed(RefreshFailure::Reused),
                RefreshFailure::Reused
            ) | (
                AuthError::RefreshFailed(RefreshFailure::Revoked),
                RefreshFailure::Revoked
            )
        ));
        let error_text = error.to_string();
        assert!(error_text.contains(expected));
        assert!(!error_text.contains("synthetic-refresh-secret"));
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
    let mut auth = AuthFile::load(&path).await.unwrap();
    assert!(matches!(
        auth.refresh(&server.uri()).await,
        Err(AuthError::ConcurrentCredentialChange)
    ));
    let stored = fs::read_to_string(path).unwrap();
    assert!(stored.contains("newer-refresh-secret"));
    assert!(!stored.contains("rotated-refresh-secret"));
}

#[tokio::test]
async fn dispatched_refresh_survives_outer_future_cancellation_and_persists_rotation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({
                    "access_token": "rotated-access-secret",
                    "refresh_token": "rotated-refresh-secret"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let auth = AuthFile::load(&path).await.unwrap();
    let endpoint = format!("{}/oauth/token", server.uri());
    let (abort_handle_tx, abort_handle_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let runtime_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut auth = auth;
            let refresh = tokio::spawn(async move { auth.refresh(&endpoint).await });
            abort_handle_tx.send(refresh.abort_handle()).unwrap();
            let _ = shutdown_rx.await;
            AuthFile::drain_pending_refreshes().await;
        });
        drop(runtime);
    });
    let abort_handle = abort_handle_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    wait_for_request_count(&server, 1).await;
    abort_handle.abort();
    shutdown_tx.send(()).unwrap();
    tokio::task::spawn_blocking(move || runtime_thread.join().unwrap())
        .await
        .unwrap();

    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["tokens"]["access_token"], "rotated-access-secret");
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_refresh_cancellation_does_not_block_the_executor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(300))
                .set_body_json(json!({
                    "access_token": "rotated-access-secret",
                    "refresh_token": "rotated-refresh-secret"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let auth = AuthFile::load(&path).await.unwrap();
    let endpoint = server.uri();
    let refresh = tokio::spawn(async move {
        let mut auth = auth;
        auth.refresh(&endpoint).await
    });
    wait_for_request_count(&server, 1).await;

    let cancelled_at = std::time::Instant::now();
    refresh.abort();
    assert!(refresh.await.unwrap_err().is_cancelled());
    assert!(
        cancelled_at.elapsed() < Duration::from_millis(100),
        "cancelling refresh blocked the current-thread executor"
    );
    AuthFile::drain_pending_refreshes().await;
}

#[tokio::test]
async fn immediate_cancellation_persists_when_the_application_drains_refreshes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(300))
                .set_body_json(json!({
                    "access_token": "rotated-access-secret",
                    "refresh_token": "rotated-refresh-secret"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let auth = AuthFile::load(&path).await.unwrap();
    let endpoint = format!("{}/oauth/token", server.uri());
    let runtime_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (refresh_started_tx, refresh_started_rx) = tokio::sync::oneshot::channel();
            let refresh = tokio::spawn(async move {
                let mut auth = auth;
                refresh_started_tx.send(()).unwrap();
                auth.refresh(&endpoint).await
            });
            refresh_started_rx.await.unwrap();
            refresh.abort();
            assert!(refresh.await.unwrap_err().is_cancelled());
            AuthFile::drain_pending_refreshes().await;
        });
        drop(runtime);
    });

    tokio::task::spawn_blocking(move || runtime_thread.join().unwrap())
        .await
        .unwrap();

    wait_for_request_count(&server, 1).await;
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["tokens"]["access_token"], "rotated-access-secret");
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_lock_wait_does_not_block_the_async_executor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(50))
                .set_body_json(json!({
                    "access_token": "rotated-access-secret",
                    "refresh_token": "rotated-refresh-secret"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let path = write_auth(directory.path(), valid_auth());
    let lock_path = directory.path().join("auth.json.lock");
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let lock_timed_out = Arc::new(AtomicBool::new(false));
    let holder_timed_out = Arc::clone(&lock_timed_out);
    let holder = thread::spawn(move || {
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        lock_file.lock().unwrap();
        locked_tx.send(()).unwrap();
        if release_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            holder_timed_out.store(true, Ordering::SeqCst);
        }
    });
    locked_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let mut auth = AuthFile::load(&path).await.unwrap();
    let endpoint = server.uri();
    let refresh = tokio::spawn(async move { auth.refresh(&endpoint).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = release_tx.send(());
    holder.join().unwrap();
    assert!(
        !lock_timed_out.load(Ordering::SeqCst),
        "the current-thread executor could not release the held credential lock"
    );
    refresh.await.unwrap().unwrap();
    wait_for_request_count(&server, 1).await;
}

#[tokio::test]
async fn malformed_success_tokens_leave_the_existing_auth_file_byte_identical() {
    for response in [
        json!({
            "access_token": "   ",
            "refresh_token": "rotated-refresh-secret"
        }),
        json!({
            "access_token": "rotated-access-secret",
            "refresh_token": "\t"
        }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempdir().unwrap();
        let path = write_auth(directory.path(), valid_auth());
        let original = fs::read(&path).unwrap();
        let mut auth = AuthFile::load(&path).await.unwrap();

        assert!(matches!(
            auth.refresh(&server.uri()).await,
            Err(AuthError::MissingCredentialField { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}
