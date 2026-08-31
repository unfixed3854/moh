//! Codex authentication and Responses completion transport.

mod auth;
mod model;
mod sse;

pub use auth::{AuthError, AuthFile, CodexCredentials, RefreshFailure, resolve_codex_home};
pub(crate) use model::{
    CodexCompletionModel, CodexTransportError, ModelCallBudget, classify_completion_error,
};
pub use model::{CodexModelError, CodexModelFactory, CodexModelInfo};
pub(crate) use sse::CompletionEvidence;

/// Network endpoints used by the Codex adapter.
#[derive(Clone, Debug)]
pub struct CodexConfig {
    /// Base URL for Codex Responses requests.
    pub api_base: String,
    /// OAuth endpoint used to refresh expired ChatGPT credentials.
    pub refresh_url: String,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            api_base: "https://chatgpt.com/backend-api/codex".to_owned(),
            refresh_url: "https://auth.openai.com/oauth/token".to_owned(),
        }
    }
}
