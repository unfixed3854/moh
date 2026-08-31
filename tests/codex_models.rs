use moh::providers::codex::{AuthFile, CodexConfig, CodexModelFactory};
use serde_json::json;
use tempfile::{TempDir, tempdir};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

async fn synthetic_auth_file() -> (TempDir, AuthFile) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "synthetic-id-secret",
                "access_token": "synthetic-access-secret",
                "refresh_token": "synthetic-refresh-secret",
                "account_id": "account-123"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let auth = AuthFile::load(path).await.unwrap();
    (directory, auth)
}

#[tokio::test]
async fn lists_picker_visible_models_for_this_chatgpt_account() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", "99.99.99"))
        .and(header("authorization", "Bearer synthetic-access-secret"))
        .and(header("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {
                    "slug": "gpt-visible",
                    "display_name": "GPT Visible",
                    "description": "Selectable model",
                    "visibility": "list",
                    "supported_reasoning_levels": [
                        {"effort": "none", "description": "No reasoning"},
                        {"effort": "low", "description": "Light reasoning"},
                        {"effort": "medium", "description": "Balanced reasoning"},
                        {"effort": "high", "description": "Deep reasoning"},
                        {"effort": "xhigh", "description": "Extra deep reasoning"},
                        {"effort": "max", "description": "Maximum reasoning"},
                        {"effort": "ultra", "description": "Maximum reasoning with delegation"}
                    ],
                    "default_reasoning_level": "medium",
                    "supported_in_api": true,
                    "priority": 1
                },
                {
                    "slug": "gpt-hidden",
                    "display_name": "GPT Hidden",
                    "description": "Internal model",
                    "visibility": "hide",
                    "supported_in_api": true,
                    "priority": 2
                },
                {
                    "slug": "gpt-unsupported",
                    "display_name": "GPT Unsupported",
                    "description": "Unavailable through this transport",
                    "visibility": "list",
                    "supported_in_api": false,
                    "priority": 3
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (_directory, auth) = synthetic_auth_file().await;
    let factory = CodexModelFactory::new(
        auth,
        CodexConfig {
            api_base: server.uri(),
            refresh_url: format!("{}/oauth/token", server.uri()),
        },
    );

    let models = factory.available_models().await.unwrap();

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "gpt-visible");
    assert_eq!(models[0].display_name, "GPT Visible");
    assert_eq!(models[0].description, "Selectable model");
    assert_eq!(
        models[0].reasoning_efforts,
        vec!["none", "low", "medium", "high", "xhigh", "max", "ultra"]
    );
    assert_eq!(
        models[0].default_reasoning_effort.as_deref(),
        Some("medium")
    );
    assert_eq!(models[1].id, "gpt-unsupported");
}

#[tokio::test]
async fn refreshes_expired_credentials_once_before_retrying_the_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer synthetic-access-secret"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
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
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer rotated-access-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{
                "slug": "gpt-refreshed",
                "display_name": "GPT Refreshed",
                "description": "Available after refresh",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (_directory, auth) = synthetic_auth_file().await;
    let factory = CodexModelFactory::new(
        auth,
        CodexConfig {
            api_base: server.uri(),
            refresh_url: format!("{}/oauth/token", server.uri()),
        },
    );

    let models = factory.available_models().await.unwrap();

    assert_eq!(models[0].id, "gpt-refreshed");
}
