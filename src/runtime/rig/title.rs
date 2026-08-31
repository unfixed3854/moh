//! Rig-backed Codex session-title generation.

use futures::future::BoxFuture;
use rig::{completion::CompletionError, providers::openai::responses_api::Reasoning};
use rig_agent::{
    AgentBuilder,
    completion::{Prompt, PromptError},
};
use serde_json::json;

use crate::{
    providers::codex::{
        CodexModelError, CodexModelFactory, CodexTransportError, ModelCallBudget,
        classify_completion_error,
    },
    session::{SessionTitleGenerator, TitleGenerationError, TitleRequest},
};

const TITLE_PREAMBLE: &str = "Generate one plain-text title of 3-8 words for the user's message. Return only the title without quotes, markdown, or commentary.";

/// Production generator for independent, model-backed session titles.
#[derive(Clone)]
pub struct CodexTitleGenerator {
    models: CodexModelFactory,
}

impl CodexTitleGenerator {
    /// Creates a title generator sharing authenticated Codex model transport.
    pub fn new(models: CodexModelFactory) -> Self {
        Self { models }
    }
}

impl SessionTitleGenerator for CodexTitleGenerator {
    fn generate(
        &self,
        request: TitleRequest,
    ) -> BoxFuture<'static, Result<String, TitleGenerationError>> {
        let models = self.models.clone();
        Box::pin(async move {
            let model = models
                .completion_model(request.model, ModelCallBudget::new(1))
                .await
                .map_err(map_model_error)?;
            let agent = AgentBuilder::new(model)
                .preamble(TITLE_PREAMBLE)
                .additional_params(json!({
                    "reasoning": Reasoning::new()
                        .with_effort(request.reasoning.as_codex_effort())
                }))
                .default_max_turns(1)
                .build();
            agent
                .prompt(request.first_message)
                .max_turns(1)
                .await
                .map_err(map_prompt_error)
        })
    }
}

fn map_model_error(error: CodexModelError) -> TitleGenerationError {
    match error {
        CodexModelError::Auth(_) => TitleGenerationError::Authentication,
        CodexModelError::Client
        | CodexModelError::CatalogTransport
        | CodexModelError::CatalogRejected { .. }
        | CodexModelError::CatalogResponse => TitleGenerationError::Completion,
    }
}

fn map_prompt_error(error: PromptError) -> TitleGenerationError {
    match error {
        PromptError::CompletionError(error) => map_completion_error(error),
        _ => TitleGenerationError::Completion,
    }
}

fn map_completion_error(error: CompletionError) -> TitleGenerationError {
    match classify_completion_error(error) {
        CodexTransportError::HttpRejected(401) => TitleGenerationError::Authentication,
        CodexTransportError::Transport => TitleGenerationError::Transport,
        CodexTransportError::HttpRejected(_) | CodexTransportError::IncompatibleResponse => {
            TitleGenerationError::Completion
        }
    }
}
