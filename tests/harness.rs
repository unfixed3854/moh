use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use futures::{Stream, stream};
use moh::harness::{
    EngineEvent, Harness, HarnessError, Message, Role, RunContext, RunEngine, RunEvent, RunFailure,
    RunFailureKind, RunRequest, RunStage, RunStream,
};
use serde_json::json;

#[derive(Clone, Default)]
struct FakeEngine {
    requests: Arc<Mutex<Vec<RunRequest>>>,
    streams: Arc<Mutex<VecDeque<RunStream>>>,
}

impl FakeEngine {
    fn with_stream(events: Vec<Result<EngineEvent, RunFailure>>) -> Self {
        Self::with_streams(vec![events])
    }

    fn with_streams(streams: Vec<Vec<Result<EngineEvent, RunFailure>>>) -> Self {
        Self {
            requests: Arc::default(),
            streams: Arc::new(Mutex::new(
                streams
                    .into_iter()
                    .map(|events| Box::pin(stream::iter(events)) as RunStream)
                    .collect(),
            )),
        }
    }

    fn requests(&self) -> Vec<RunRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl RunEngine for FakeEngine {
    fn start(&self, request: RunRequest) -> RunStream {
        self.requests.lock().unwrap().push(request);
        self.streams.lock().unwrap().pop_front().unwrap_or_else(|| {
            Box::pin(stream::once(async {
                Err(RunFailure::new(
                    RunStage::Startup,
                    RunFailureKind::RuntimeInfrastructure,
                    false,
                    "fake engine has no scripted stream",
                ))
            }))
        })
    }
}

fn context() -> RunContext {
    RunContext {
        cwd: PathBuf::from("/workspace"),
        plan: Vec::new(),
    }
}

fn messages(entries: &[(Role, &str)]) -> Vec<Message> {
    entries
        .iter()
        .map(|(role, text)| Message::new(*role, *text))
        .collect()
}

fn assert_request(request: &RunRequest, prompt: &str, history: Vec<Message>) {
    assert_eq!(request.prompt, prompt);
    assert_eq!(request.history, history);
    assert_eq!(request.context, context());
}

fn assert_started(event: RunEvent, run_id: u64) {
    match event {
        RunEvent::Started { run_id: actual } => assert_eq!(actual.get(), run_id),
        other => panic!("expected Started, got {other:?}"),
    }
}

fn assert_delta(event: RunEvent, run_id: u64, text: &str) {
    match event {
        RunEvent::AssistantDelta {
            run_id: actual,
            text: actual_text,
        } => {
            assert_eq!(actual.get(), run_id);
            assert_eq!(actual_text, text);
        }
        other => panic!("expected AssistantDelta, got {other:?}"),
    }
}

fn assert_completed(event: RunEvent, run_id: u64, response: &str) {
    match event {
        RunEvent::Completed {
            run_id: actual,
            response: actual_response,
        } => {
            assert_eq!(actual.get(), run_id);
            assert_eq!(actual_response, response);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

fn assert_failed(
    event: RunEvent,
    run_id: u64,
    stage: RunStage,
    kind: RunFailureKind,
    message: &str,
) {
    match event {
        RunEvent::Failed {
            run_id: actual,
            failure,
        } => {
            assert_eq!(actual.get(), run_id);
            assert_eq!(failure.stage(), stage);
            assert_eq!(failure.kind(), &kind);
            assert!(!failure.retryable());
            assert_eq!(failure.message(), message);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn completed_run_commits_one_user_assistant_exchange() {
    let engine = FakeEngine::with_stream(vec![
        Ok(EngineEvent::AssistantDelta("Hel".into())),
        Ok(EngineEvent::Completed("Hello".into())),
    ]);
    let mut harness = Harness::new(engine.clone());

    assert_started(harness.submit("Hi", context()).unwrap(), 0);
    assert!(harness.is_running());
    assert_delta(harness.next_event().await.unwrap(), 0, "Hel");
    assert_completed(harness.next_event().await.unwrap(), 0, "Hello");
    assert!(!harness.is_running());
    assert!(harness.next_event().await.is_none());
    assert_eq!(
        harness.history(),
        messages(&[(Role::User, "Hi"), (Role::Assistant, "Hello")])
    );
    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    assert_request(&requests[0], "Hi", vec![]);
}

#[tokio::test]
async fn failure_after_delta_does_not_commit_partial_history() {
    let engine = FakeEngine::with_stream(vec![
        Ok(EngineEvent::AssistantDelta("partial".into())),
        Err(RunFailure::new(
            RunStage::ModelRequest,
            RunFailureKind::Transport,
            false,
            "connection lost",
        )),
    ]);
    let prior = messages(&[(Role::User, "Earlier"), (Role::Assistant, "Answer")]);
    let mut harness = Harness::with_history(engine.clone(), prior.clone());

    assert_started(harness.submit("New", context()).unwrap(), 0);
    assert_delta(harness.next_event().await.unwrap(), 0, "partial");
    assert_failed(
        harness.next_event().await.unwrap(),
        0,
        RunStage::ModelRequest,
        RunFailureKind::Transport,
        "connection lost",
    );
    assert!(!harness.is_running());
    assert_eq!(harness.history(), prior);
    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    assert_request(
        &requests[0],
        "New",
        messages(&[(Role::User, "Earlier"), (Role::Assistant, "Answer")]),
    );
}

#[tokio::test]
async fn second_submit_while_running_returns_busy() {
    let engine = FakeEngine::with_stream(vec![Ok(EngineEvent::Completed("Answer".into()))]);
    let mut harness = Harness::new(engine.clone());

    assert_started(harness.submit("First", context()).unwrap(), 0);
    match harness.submit("Second", context()) {
        Err(HarnessError::Busy) => {}
        other => panic!("expected Busy, got {other:?}"),
    }
    assert_completed(harness.next_event().await.unwrap(), 0, "Answer");
    assert_eq!(
        harness.history(),
        messages(&[(Role::User, "First"), (Role::Assistant, "Answer")])
    );
    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    assert_request(&requests[0], "First", vec![]);
}

#[tokio::test]
async fn premature_engine_eof_becomes_protocol_failure() {
    let engine = FakeEngine::with_stream(vec![Ok(EngineEvent::AssistantDelta("partial".into()))]);
    let mut harness = Harness::new(engine.clone());

    assert_started(harness.submit("Question", context()).unwrap(), 0);
    assert_delta(harness.next_event().await.unwrap(), 0, "partial");
    assert_failed(
        harness.next_event().await.unwrap(),
        0,
        RunStage::Finalization,
        RunFailureKind::Protocol,
        "engine stream ended before completion",
    );
    assert!(!harness.is_running());
    assert_eq!(harness.history(), Vec::<Message>::new());
    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    assert_request(&requests[0], "Question", vec![]);
}

#[tokio::test]
async fn blank_completion_becomes_empty_response_failure() {
    let engine = FakeEngine::with_stream(vec![Ok(EngineEvent::Completed(" \n\t ".into()))]);
    let mut harness = Harness::new(engine.clone());

    assert_started(harness.submit("Question", context()).unwrap(), 0);
    assert_failed(
        harness.next_event().await.unwrap(),
        0,
        RunStage::Finalization,
        RunFailureKind::EmptyResponse,
        "engine completed with an empty response",
    );
    assert!(!harness.is_running());
    assert_eq!(harness.history(), Vec::<Message>::new());
    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    assert_request(&requests[0], "Question", vec![]);
}

struct DropTrackingStream {
    dropped: Arc<AtomicBool>,
}

impl Stream for DropTrackingStream {
    type Item = Result<EngineEvent, RunFailure>;

    fn poll_next(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for DropTrackingStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancel_drops_the_stream_before_releasing_busy_state() {
    let dropped = Arc::new(AtomicBool::new(false));
    let engine = FakeEngine {
        requests: Arc::default(),
        streams: Arc::new(Mutex::new(VecDeque::from([
            Box::pin(DropTrackingStream {
                dropped: dropped.clone(),
            }) as RunStream,
            Box::pin(stream::iter(vec![Ok(EngineEvent::Completed(
                "Retried answer".into(),
            ))])) as RunStream,
        ]))),
    };
    let mut harness = Harness::new(engine.clone());

    assert_started(harness.submit("Question", context()).unwrap(), 0);
    match harness.cancel().unwrap() {
        RunEvent::Cancelled { run_id } => assert_eq!(run_id.get(), 0),
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(dropped.load(Ordering::SeqCst));
    assert!(!harness.is_running());
    assert_started(harness.submit("Retry", context()).unwrap(), 1);
    assert_completed(harness.next_event().await.unwrap(), 1, "Retried answer");
    assert_eq!(
        harness.history(),
        messages(&[(Role::User, "Retry"), (Role::Assistant, "Retried answer")])
    );
    let requests = engine.requests();
    assert_eq!(requests.len(), 2);
    assert_request(&requests[0], "Question", vec![]);
    assert_request(&requests[1], "Retry", vec![]);
}

#[tokio::test]
async fn harness_assigns_monotonic_run_ids_and_preserves_tool_call_ids() {
    let engine = FakeEngine::with_streams(vec![
        vec![
            Ok(EngineEvent::ToolStarted {
                call_id: "engine-call-7".into(),
                name: "read".into(),
                arguments: json!({"path": "README.md"}),
            }),
            Ok(EngineEvent::ToolFinished {
                call_id: "engine-call-7".into(),
                name: "read".into(),
            }),
            Ok(EngineEvent::Completed("First answer".into())),
        ],
        vec![Ok(EngineEvent::Completed("Second answer".into()))],
    ]);
    let mut harness = Harness::new(engine.clone());

    assert_started(harness.submit("First", context()).unwrap(), 0);
    match harness.next_event().await.unwrap() {
        RunEvent::ToolStarted {
            run_id,
            call_id,
            name,
            arguments,
        } => {
            assert_eq!(run_id.get(), 0);
            assert_eq!(call_id, "engine-call-7");
            assert_eq!(name, "read");
            assert_eq!(arguments, json!({"path": "README.md"}));
        }
        other => panic!("expected ToolStarted, got {other:?}"),
    }
    match harness.next_event().await.unwrap() {
        RunEvent::ToolFinished {
            run_id,
            call_id,
            name,
        } => {
            assert_eq!(run_id.get(), 0);
            assert_eq!(call_id, "engine-call-7");
            assert_eq!(name, "read");
        }
        other => panic!("expected ToolFinished, got {other:?}"),
    }
    assert_completed(harness.next_event().await.unwrap(), 0, "First answer");
    assert_started(harness.submit("Second", context()).unwrap(), 1);
    assert_completed(harness.next_event().await.unwrap(), 1, "Second answer");
    assert_eq!(
        harness.history(),
        messages(&[
            (Role::User, "First"),
            (Role::Assistant, "First answer"),
            (Role::User, "Second"),
            (Role::Assistant, "Second answer"),
        ]),
    );
    let requests = engine.requests();
    assert_eq!(requests.len(), 2);
    assert_request(&requests[0], "First", vec![]);
    assert_request(
        &requests[1],
        "Second",
        messages(&[(Role::User, "First"), (Role::Assistant, "First answer")]),
    );
}

#[tokio::test]
async fn next_request_receives_a_snapshot_of_committed_history() {
    let engine = FakeEngine::with_streams(vec![
        vec![Ok(EngineEvent::Completed("Answer one".into()))],
        vec![Ok(EngineEvent::Completed("Answer two".into()))],
    ]);
    let mut harness = Harness::with_history(
        engine.clone(),
        messages(&[(Role::User, "Earlier"), (Role::Assistant, "Earlier answer")]),
    );

    assert_started(harness.submit("Question one", context()).unwrap(), 0);
    assert_completed(harness.next_event().await.unwrap(), 0, "Answer one");
    assert_started(harness.submit("Question two", context()).unwrap(), 1);
    assert_completed(harness.next_event().await.unwrap(), 1, "Answer two");
    assert_eq!(
        harness.history(),
        messages(&[
            (Role::User, "Earlier"),
            (Role::Assistant, "Earlier answer"),
            (Role::User, "Question one"),
            (Role::Assistant, "Answer one"),
            (Role::User, "Question two"),
            (Role::Assistant, "Answer two"),
        ]),
    );
    let requests = engine.requests();
    assert_eq!(requests.len(), 2);
    assert_request(
        &requests[0],
        "Question one",
        messages(&[(Role::User, "Earlier"), (Role::Assistant, "Earlier answer")]),
    );
    assert_request(
        &requests[1],
        "Question two",
        messages(&[
            (Role::User, "Earlier"),
            (Role::Assistant, "Earlier answer"),
            (Role::User, "Question one"),
            (Role::Assistant, "Answer one"),
        ]),
    );
}
