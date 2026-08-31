use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rig::{
    completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse},
    providers::openai::{self, responses_api},
    streaming::StreamingCompletionResponse,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::Mutex;

use super::{
    CodexConfig,
    auth::{AuthError, AuthFile},
    sse::{CodexHttpClient, CompletionEvidence},
};

const MODEL_CATALOG_CLIENT_VERSION: &str = "99.99.99";

/// File-backed authentication or client-construction failures from the Codex adapter.
#[derive(Debug, Error)]
pub enum CodexModelError {
    /// File-backed credential loading or refresh failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The configured Codex client could not be constructed.
    #[error("could not construct the Codex model client")]
    Client,
    /// The model catalog request could not reach the Codex backend.
    #[error("could not fetch the Codex model catalog")]
    CatalogTransport,
    /// The Codex backend rejected the model catalog request.
    #[error("Codex model catalog request was rejected with HTTP status {status}")]
    CatalogRejected {
        /// Rejected HTTP status.
        status: u16,
    },
    /// The Codex backend returned an incompatible model catalog.
    #[error("Codex returned an incompatible model catalog")]
    CatalogResponse,
}

/// A model that can be selected for Codex agent runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexModelInfo {
    /// Model identifier sent to the Responses API.
    pub id: String,
    /// Human-readable model name.
    pub display_name: String,
    /// Short human-readable model description.
    pub description: String,
    /// Reasoning effort values supported by this model for the current account.
    pub reasoning_efforts: Vec<String>,
    /// Reasoning effort selected by default for this model, when advertised.
    pub default_reasoning_effort: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<RawModelInfo>,
}

#[derive(Deserialize)]
struct RawModelInfo {
    slug: String,
    display_name: String,
    description: String,
    visibility: String,
    #[serde(default)]
    supported_reasoning_levels: Vec<RawReasoningLevel>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
}

#[derive(Deserialize)]
struct RawReasoningLevel {
    effort: String,
}

#[derive(Debug)]
pub(crate) enum CodexTransportError {
    HttpRejected(u16),
    Transport,
    IncompatibleResponse,
}

pub(crate) fn classify_completion_error(error: CompletionError) -> CodexTransportError {
    if let Some(status) = error.provider_response_status() {
        return CodexTransportError::HttpRejected(status.as_u16());
    }

    match error {
        CompletionError::HttpError(_) | CompletionError::UrlError(_) => {
            CodexTransportError::Transport
        }
        other => {
            let display = other.to_string();
            if display
                .strip_prefix(concat!("Provider", "Error: "))
                .is_some_and(is_stream_transport_error)
            {
                CodexTransportError::Transport
            } else {
                CodexTransportError::IncompatibleResponse
            }
        }
    }
}

fn is_stream_transport_error(message: &str) -> bool {
    message.starts_with("Http client error:")
        || message.starts_with("Http error:")
        || message == "Stream ended"
        || message == "Request in error state, cannot access headers"
}

#[cfg(test)]
mod tests {
    use rig::completion::CompletionError;

    use super::{CodexTransportError, classify_completion_error};

    #[test]
    fn response_parse_text_that_resembles_stream_eof_stays_incompatible() {
        let classified =
            classify_completion_error(CompletionError::ResponseError("Stream ended".to_owned()));

        assert!(matches!(
            classified,
            CodexTransportError::IncompatibleResponse
        ));
    }
}

/// Factory for authenticated Codex Responses completion models.
#[derive(Clone)]
pub struct CodexModelFactory {
    inner: Arc<Inner>,
}

struct Inner {
    auth: Mutex<AuthFile>,
    http: reqwest::Client,
    config: CodexConfig,
}

impl CodexModelFactory {
    /// Loads file-backed Codex credentials and builds a factory using `config` endpoints.
    pub async fn from_env(config: CodexConfig) -> Result<Self, CodexModelError> {
        Ok(Self::new(AuthFile::load_from_env().await?, config))
    }

    /// Builds a factory from validated credentials and explicit endpoint configuration.
    pub fn new(auth: AuthFile, config: CodexConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                auth: Mutex::new(auth),
                http: reqwest::Client::new(),
                config,
            }),
        }
    }

    /// Fetches the account-specific models that can be selected for API-backed runs.
    pub async fn available_models(&self) -> Result<Vec<CodexModelInfo>, CodexModelError> {
        match self
            .request_available_models(MODEL_CATALOG_CLIENT_VERSION)
            .await
        {
            Err(CodexModelError::CatalogRejected { status: 401 }) => {
                self.refresh().await?;
                self.request_available_models(MODEL_CATALOG_CLIENT_VERSION)
                    .await
            }
            result => result,
        }
    }

    async fn request_available_models(
        &self,
        client_version: &str,
    ) -> Result<Vec<CodexModelInfo>, CodexModelError> {
        let credentials = self.inner.auth.lock().await.credentials()?;
        let response = self
            .inner
            .http
            .get(format!(
                "{}/models?client_version={client_version}",
                self.inner.config.api_base.trim_end_matches('/'),
            ))
            .bearer_auth(credentials.access_token())
            .header("chatgpt-account-id", credentials.account_id())
            .send()
            .await
            .map_err(|_| CodexModelError::CatalogTransport)?;
        if !response.status().is_success() {
            return Err(CodexModelError::CatalogRejected {
                status: response.status().as_u16(),
            });
        }
        let response = response
            .json::<ModelsResponse>()
            .await
            .map_err(|_| CodexModelError::CatalogResponse)?;
        Ok(response
            .models
            .into_iter()
            .filter(|model| model.visibility == "list")
            .map(|model| CodexModelInfo {
                id: model.slug,
                display_name: model.display_name,
                description: model.description,
                reasoning_efforts: model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|level| level.effort)
                    .collect(),
                default_reasoning_effort: model.default_reasoning_level,
            })
            .collect())
    }

    pub(crate) async fn completion_model(
        &self,
        model: impl Into<String>,
        budget: ModelCallBudget,
    ) -> Result<CodexCompletionModel, CodexModelError> {
        let credentials = self.inner.auth.lock().await.credentials()?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_str(credentials.account_id()).map_err(|_| CodexModelError::Client)?,
        );
        let completion_evidence = CompletionEvidence::default();
        let inner_client = openai::Client::builder()
            .http_client(CodexHttpClient::new(
                self.inner.http.clone(),
                completion_evidence.clone(),
            ))
            .api_key(credentials.access_token().to_owned())
            .base_url(&self.inner.config.api_base)
            .http_headers(headers)
            .build()
            .map_err(|_| CodexModelError::Client)?;
        let client = CodexCompletionClient {
            inner: inner_client,
            completion_evidence,
            model_call_budget: budget,
        };
        Ok(CodexCompletionModel::make(&client, model))
    }

    /// Refreshes the file-backed Codex credentials and durably persists any rotation.
    pub async fn refresh(&self) -> Result<(), CodexModelError> {
        self.inner
            .auth
            .lock()
            .await
            .refresh(&self.inner.config.refresh_url)
            .await
            .map(|_| ())
            .map_err(CodexModelError::Auth)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ModelCallBudget {
    remaining: Arc<AtomicUsize>,
}

impl ModelCallBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            remaining: Arc::new(AtomicUsize::new(limit)),
        }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }

    fn consume(&self) -> Result<(), CompletionError> {
        self.remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| {
                CompletionError::ResponseError("Codex model call budget was exhausted".into())
            })
    }
}

#[derive(Clone)]
pub(crate) struct CodexCompletionModel {
    inner: responses_api::ResponsesCompletionModel<CodexHttpClient>,
    completion_evidence: CompletionEvidence,
    model_call_budget: ModelCallBudget,
}

#[derive(Clone)]
pub(crate) struct CodexCompletionClient {
    inner: openai::Client<CodexHttpClient>,
    completion_evidence: CompletionEvidence,
    model_call_budget: ModelCallBudget,
}

impl CodexCompletionModel {
    fn new(client: &CodexCompletionClient, model: impl Into<String>) -> Self {
        Self {
            inner: responses_api::ResponsesCompletionModel::new(client.inner.clone(), model),
            completion_evidence: client.completion_evidence.clone(),
            model_call_budget: client.model_call_budget.clone(),
        }
    }

    fn prepare_request(mut request: CompletionRequest) -> CompletionRequest {
        let mut params = match request.additional_params.take() {
            Some(Value::Object(params)) => params,
            _ => Map::new(),
        };
        params.insert("store".to_owned(), Value::Bool(false));
        request.additional_params = Some(Value::Object(params));
        request
    }

    pub(crate) fn completion_evidence(&self) -> CompletionEvidence {
        self.completion_evidence.clone()
    }
}

impl CompletionModel for CodexCompletionModel {
    type Response = Option<responses_api::streaming::StreamingCompletionResponse>;
    type StreamingResponse = responses_api::streaming::StreamingCompletionResponse;
    type Client = CodexCompletionClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self::new(client, model)
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        self.model_call_budget.consume()?;
        let mut stream = self.inner.stream(Self::prepare_request(request)).await?;
        while let Some(item) = stream.next().await {
            item?;
        }
        if !self.completion_evidence.completed() {
            return Err(CompletionError::ResponseError(
                "Codex SSE stream did not contain a completed response".into(),
            ));
        }
        Ok(stream.into())
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        self.model_call_budget.consume()?;
        self.inner.stream(Self::prepare_request(request)).await
    }
}
