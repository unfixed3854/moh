use futures::StreamExt;
use moh::{
    harness::{EngineEvent, RunContext, RunEngine, RunRequest},
    providers::codex::{CodexConfig, CodexModelFactory},
    runtime::rig::{AgentConfig, CodexRunEngine},
    tools::{ReadConfig, ReadServiceFactory},
};

#[tokio::test]
#[ignore = "uses the developer's real file-backed Codex login and network quota"]
async fn real_codex_login_returns_a_non_empty_luna_answer() {
    assert_eq!(
        std::env::var("MOH_RUN_CODEX_LIVE").as_deref(),
        Ok("1"),
        "set MOH_RUN_CODEX_LIVE=1 to acknowledge real account usage"
    );
    let models = CodexModelFactory::from_env(CodexConfig::default())
        .await
        .expect("load file-backed Codex credentials");
    let reads = ReadServiceFactory::new(
        ReadConfig::platform_default().expect("resolve the durable read-tool store"),
    );
    let engine = CodexRunEngine::new(models, AgentConfig::default(), reads)
        .expect("construct the Codex run engine");
    let request = RunRequest {
        prompt: "Reply with exactly: moh live smoke test".into(),
        history: Vec::new(),
        context: RunContext {
            cwd: std::env::current_dir().expect("resolve current directory"),
            plan: Vec::new(),
        },
    };
    let mut stream = engine.start(request);
    let mut answer = None;
    while let Some(event) = stream.next().await {
        if let EngineEvent::Completed(response) = event.expect("send live Codex request") {
            answer = Some(response);
        }
    }
    assert!(answer.is_some_and(|answer| !answer.trim().is_empty()));
}
