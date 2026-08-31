//! Production foreground/detached commands and deferred Codex runtime initialization.

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    sync::Arc,
};

use thiserror::Error;

use crate::{
    backend::{BackendError, BackendOptions, BackendRuntimeFactory, ShutdownReason, run_backend},
    local::{BackendCommand, LocalPathError, LocalPaths, MohConfig},
    providers::codex::{AuthError, AuthFile, CodexConfig, CodexModelFactory},
    runtime::rig::{AgentConfig, CodexSessionEngineFactory, ReasoningLevel},
    session::{ModelCatalogState, ModelInfoDto, SessionStore, SessionStoreError},
    tools::{ReadConfig, ReadServiceFactory},
};

type CodexInitialization =
    Pin<Box<dyn Future<Output = Result<CodexSessionEngineFactory, AuthError>> + 'static>>;

/// Deferred production initializer for shared Codex transport and isolated session runtimes.
pub struct CodexBackendRuntimeFactory {
    auth: AuthSource,
    codex: CodexConfig,
    agent: AgentConfig,
    reads: ReadServiceFactory,
}

enum AuthSource {
    Path(PathBuf),
    Environment,
}

impl CodexBackendRuntimeFactory {
    /// Creates an initializer whose file-backed authentication is loaded only when polled.
    pub fn new(
        auth_path: impl Into<PathBuf>,
        codex: CodexConfig,
        agent: AgentConfig,
        reads: ReadServiceFactory,
    ) -> Self {
        Self {
            auth: AuthSource::Path(auth_path.into()),
            codex,
            agent,
            reads,
        }
    }

    /// Creates a production initializer using the environment-resolved Codex credential file.
    pub fn from_env(codex: CodexConfig, agent: AgentConfig, reads: ReadServiceFactory) -> Self {
        Self {
            auth: AuthSource::Environment,
            codex,
            agent,
            reads,
        }
    }
}

impl BackendRuntimeFactory for CodexBackendRuntimeFactory {
    type SessionFactory = CodexSessionEngineFactory;
    type Error = AuthError;
    type Future = CodexInitialization;

    fn initialize(self) -> Self::Future {
        Box::pin(async move {
            let auth = match self.auth {
                AuthSource::Path(path) => AuthFile::load(path).await?,
                AuthSource::Environment => AuthFile::load_from_env().await?,
            };
            let models = CodexModelFactory::new(auth, self.codex);
            let catalog = match models.available_models().await {
                Ok(models) => ModelCatalogState::Ready(
                    models
                        .into_iter()
                        .map(|model| ModelInfoDto {
                            id: model.id,
                            display_name: model.display_name,
                            description: model.description,
                            reasoning_efforts: model
                                .reasoning_efforts
                                .into_iter()
                                .filter_map(|effort| ReasoningLevel::parse(&effort))
                                .collect(),
                            default_reasoning: model
                                .default_reasoning_effort
                                .as_deref()
                                .and_then(ReasoningLevel::parse),
                        })
                        .collect(),
                ),
                Err(error) => ModelCatalogState::Failed(error.to_string()),
            };
            Ok(
                CodexSessionEngineFactory::new(models, self.agent, self.reads)
                    .with_catalog(catalog),
            )
        })
    }
}

/// Failure while composing or running the production local backend.
#[derive(Debug, Error)]
pub enum ServerRunError {
    /// The shared state directory could not be created or validated safely.
    #[error(transparent)]
    Paths(#[from] LocalPathError),
    /// The durable session store could not be opened safely.
    #[error(transparent)]
    SessionStore(#[from] SessionStoreError),
    /// The listener, runtime, or orderly backend shutdown failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Composes and runs the production backend for foreground and detached server modes.
pub async fn run(paths: LocalPaths, config: MohConfig) -> Result<ShutdownReason, ServerRunError> {
    let reads =
        ReadServiceFactory::new(ReadConfig::at(paths.state_dir().join("hash-store.sqlite")));
    let runtime_factory =
        CodexBackendRuntimeFactory::from_env(CodexConfig::default(), AgentConfig::default(), reads);
    run_with_runtime(paths, config, runtime_factory).await
}

async fn run_with_runtime<F>(
    paths: LocalPaths,
    config: MohConfig,
    runtime_factory: F,
) -> Result<ShutdownReason, ServerRunError>
where
    F: BackendRuntimeFactory,
{
    paths.prepare_state_dir()?;
    let state_dir = paths.state_dir().to_path_buf();
    let opened = SessionStore::open_at(&state_dir.join("sessions.sqlite")).await?;
    let repository = Arc::new(opened.store);
    Ok(run_backend(BackendOptions {
        paths,
        config: config.server,
        runtime_factory,
        repository,
    })
    .await?)
}

/// Builds the diagnostic foreground `moh server` process command.
pub fn foreground_server_command(executable: impl AsRef<Path>) -> Command {
    let mut command = Command::new(executable.as_ref());
    command.arg("server");
    command
}

/// Builds the private detached `moh server --internal-detached` process command.
pub fn detached_server_command(executable: impl AsRef<Path>) -> BackendCommand {
    BackendCommand::detached(executable.as_ref().to_path_buf())
        .args(["server", "--internal-detached"])
}

#[cfg(test)]
mod tests {
    use std::{future::Ready, os::unix::fs::PermissionsExt};

    use crate::local::PathRoots;

    use super::*;

    struct FailingRuntimeFactory;

    impl BackendRuntimeFactory for FailingRuntimeFactory {
        type SessionFactory = CodexSessionEngineFactory;
        type Error = std::io::Error;
        type Future = Ready<Result<Self::SessionFactory, Self::Error>>;

        fn initialize(self) -> Self::Future {
            std::future::ready(Err(std::io::Error::other("deferred runtime reached")))
        }
    }

    #[tokio::test]
    async fn foreground_first_run_prepares_private_state_before_deferred_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let state_root = directory.path().join("missing-state-root");
        let paths = LocalPaths::from_roots(PathRoots {
            runtime_dir: Some(directory.path().join("runtime")),
            temp_dir: directory.path().join("tmp"),
            config_dir: directory.path().join("config"),
            state_dir: state_root.join("nested/moh"),
            effective_uid: nix::unistd::Uid::effective().as_raw(),
        });
        assert!(!paths.state_dir().exists());

        let error = run_with_runtime(paths.clone(), MohConfig::default(), FailingRuntimeFactory)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ServerRunError::Backend(BackendError::RuntimeInitialization { .. })
        ));
        for path in [
            state_root.clone(),
            state_root.join("nested"),
            paths.state_dir().to_path_buf(),
        ] {
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} must remain private",
                path.display()
            );
        }
        for path in [
            paths.state_dir().join("sessions.sqlite"),
            paths.state_dir().join("sessions.sqlite.lock"),
        ] {
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "{} must remain private",
                path.display()
            );
        }
    }
}
