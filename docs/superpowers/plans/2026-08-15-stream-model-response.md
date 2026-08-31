# Stream Model Responses Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render Codex assistant text incrementally in the TUI while preserving transactional conversation history and cancellation safety.

**Architecture:** Add a boxed `ChatStream` to the existing `ChatBackend` boundary, with a default one-shot adapter for existing backends. `CodexProvider` will use Rig's Responses streaming implementation and expose only text deltas, while `PendingTurn` accumulates those deltas and remains the sole owner of an in-flight request. The demo will render a temporary sanitized response component and commit it to the transcript only after successful stream completion.

**Tech Stack:** Rust 2024, Tokio 1.53, futures 0.3, Rig (`rig-core`) 0.41, Reqwest 0.13, Crossterm, existing TUI components, Wiremock.

## Global Constraints

- Use Rig's Responses transport with `store: false`.
- Keep model `gpt-5.6-luna` and medium reasoning.
- Preserve the `chatgpt-account-id` header.
- Refresh and retry at most once after an HTTP 401.
- Never expose credentials or raw provider bodies.
- Keep one opaque, cancellable turn in flight.
- Partial output is display state only and is committed to conversation history only after successful stream completion.
- Reasoning, tool-call, unknown, message-ID, and final-response metadata events are not displayed.
- Preserve resize, help, and exit responsiveness while a request is pending.
- Do not add model selection, tools, persistence, concurrent requests, polling service, or unrelated TUI redesign.
- Validate with `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, `cargo build --locked`, and `git diff --check`.

---

## File map

- Modify `src/codex_provider.rs`: define `ChatStream`, add the default backend stream adapter, and expose the Codex Rig stream with one-time authentication retry.
- Modify `src/conversation.rs`: make `PendingTurn` own and accumulate a stream while preserving opaque completion and abandonment APIs.
- Modify `tests/conversation.rs`: verify streamed accumulation, commit/rollback, and stream-drop ordering.
- Modify `tests/codex_provider.rs`: verify multiple SSE deltas, terminal completion, incomplete streams, and existing auth behavior.
- Modify `src/demo.rs`: add a temporary live-response component and consume one delta per event-loop turn.
- Modify `tests/components.rs`: define the empty `Text` rendering contract used to hide the cleared live-response component.
- Extend `src/demo.rs`'s existing unit-test module: verify intermediate output, sanitization, and cancellation behavior.

### Task 1: Add the normalized stream and transactional pending-turn API

**Files:**

- Modify: `src/codex_provider.rs:1-60`
- Modify: `src/conversation.rs:1-170`
- Modify: `tests/conversation.rs:1-235`

**Interfaces:**

- Produces `pub type ChatStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>`.
- Extends `ChatBackend` with `fn stream(&self, messages: Vec<Message>) -> ChatStream`.
- Preserves `fn complete(&self, messages: Vec<Message>) -> ChatFuture` as the required compatibility method and implements the default `stream` by wrapping its result with `futures::stream::once`.
- Adds `pub async fn next_chunk(&mut self) -> Option<Result<String, ProviderError>>` on `PendingTurn`, plus `PendingTurn::text(&self) -> &str`.
- Adds `PendingTurn::into_completed(self) -> CompletedTurn`, called only after `next_chunk` returns `None`.

- [ ] **Step 1: Add failing conversation tests for chunk accumulation and final commit**

Add a `ChunkedBackend` test double whose `stream` returns:

```rust
Box::pin(futures::stream::iter([
    Ok("first".to_owned()),
    Ok(" second".to_owned()),
]))
```

Test that two `next_chunk().await` calls return the two deltas, that
`pending.text()` becomes `"first second"`, that the final `None` is followed by
`conversation.resolve_turn(pending.into_completed())`, and that the committed
turn contains the complete accumulated answer.

Add a second test where the stream returns one delta and then
`Err(ProviderError::Transport)`. Assert that the caller can abandon the
pending turn, the partial answer is absent from `conversation.turns()`, and a
new turn can start.

- [ ] **Step 2: Run the focused conversation tests and verify they fail**

Run:

```bash
cargo test --test conversation
```

Expected: compilation fails because `ChatStream`, `ChatBackend::stream`,
`PendingTurn::next_chunk`, `PendingTurn::text`, and
`PendingTurn::into_completed` do not exist yet.

- [ ] **Step 3: Define the boxed stream and preserve the compatibility adapter**

In `src/codex_provider.rs`, import `futures::{Stream, StreamExt, stream}` and
add:

```rust
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

pub trait ChatBackend: Clone + Send + Sync + 'static {
    fn complete(&self, messages: Vec<Message>) -> ChatFuture;

    fn stream(&self, messages: Vec<Message>) -> ChatStream {
        Box::pin(stream::once(self.complete(messages)))
    }
}
```

Keep the existing `ChatFuture` and `complete` signatures so current provider
and test doubles remain valid. The default stream emits one complete answer,
which keeps non-streaming backends functional while the Codex implementation is
upgraded in Task 2.

- [ ] **Step 4: Make `PendingTurn` accumulate deltas without changing turn ownership**

Replace its `ChatFuture` field with `ChatStream` and add an `answer: String` and
`finished: bool`. Implement `futures::Stream` for `PendingTurn` so each
successful item is appended to `answer` before being returned. Mark the turn
finished when the inner stream returns `None`; forward provider errors without
committing anything.

Implement `next_chunk` by awaiting the next item from the `PendingTurn` stream.
Implement `text` as a read-only view of the accumulated answer. Implement
`into_completed` using the accumulated answer and return
`ProviderError::EmptyResponse` if it is empty or whitespace-only. Update
`Future for PendingTurn` to drain its own stream through the same accumulation
path, preserving existing `pending.await` callers and opaque `CompletedTurn`
identity binding.

Change `Conversation::start_turn` to call `self.backend.stream(messages)`.
Leave `resolve_turn`, `abandon_turn`, `abandon_completed`, and
`take_matching_pending` identity checks intact. `abandon_turn` must still drop
the entire `PendingTurn` before taking the matching pending state.

- [ ] **Step 5: Run the focused conversation tests and commit**

Run:

```bash
cargo test --test conversation
cargo fmt --all -- --check
```

Expected: all conversation tests pass, including the pre-existing stale-turn
and drop-order tests. Commit the focused change:

```bash
git add src/codex_provider.rs src/conversation.rs tests/conversation.rs
git commit -m "feat: add transactional chat streams"
```

### Task 2: Expose the Rig-backed Codex text stream

**Files:**

- Modify: `src/codex_provider.rs` near `CodexCompletionModel`, `CodexProvider::attempt`, and the `ChatBackend for CodexProvider` implementation
- Modify: `tests/codex_provider.rs` near the existing SSE helpers and provider request tests

**Interfaces:**

- Produces an internal `AttemptStream` whose items retain `AttemptError` until the provider stream state machine applies auth retry and redacted error mapping.
- `CodexProvider::stream` returns the public `ChatStream` from Task 1.
- `CodexProvider::complete` remains behaviorally compatible and continues to return the final answer for existing callers.

- [ ] **Step 1: Add failing provider stream tests**

Add an SSE helper that emits two separate text deltas followed by the existing
successful `response.completed` event:

```rust
fn chunked_success_sse() -> String {
    ["first", " second"]
        .into_iter()
        .map(|delta| {
            format!(
                "data: {}\n\n",
                json!({
                    "type": "response.output_text.delta",
                    "delta": delta,
                    "item_id": "msg_test",
                    "output_index": 0,
                    "content_index": 0,
                    "sequence_number": 0
                })
            )
        })
        .chain(std::iter::once(format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": success_response("first second"),
                "sequence_number": 2
            })
        )))
        .collect()
}
```

Test `provider.stream(...).collect::<Vec<_>>().await` and assert the exact
items are `Ok("first")` and `Ok(" second")`. Add a test with a terminal
`response.incomplete` event and assert the collected stream contains
`Err(ProviderError::IncompatibleResponse)` without exposing the mock body.

- [ ] **Step 2: Run the focused provider tests and verify the new tests fail**

Run:

```bash
cargo test --test codex_provider sends_history_model_and_medium_reasoning_to_codex_responses
cargo test --test codex_provider streams_text_deltas_before_completion
cargo test --test codex_provider rejects_incomplete_streams
```

Expected: the existing request test passes, while the new stream tests fail to
compile or fail because `CodexProvider::stream` still uses the default
one-shot adapter.

- [ ] **Step 3: Build a Rig stream request without duplicating SSE parsing**

Refactor the shared request setup from `CodexProvider::attempt` into a helper
that loads credentials, creates `CodexHttpClient`, builds the OpenAI client,
creates `CodexCompletionModel`, and translates the final `Message` plus prior
history into a completion request. Preserve these exact request fields:

```rust
additional_params(json!({
    "reasoning": Reasoning::new().with_effort(ReasoningEffort::Medium)
}))
```

`CodexCompletionModel::prepare_request` must continue inserting
`"store": false`. The stream path must call Rig's `.stream().await` rather
than `.send().await` and must preserve the existing response-content-type shim.

- [ ] **Step 4: Map Rig stream items to text deltas and validate terminal state**

Add an internal stream wrapper around
`StreamingCompletionResponse<responses_api::streaming::StreamingCompletionResponse>`.
For each item:

- yield `Ok(text.text)` for `StreamedAssistantContent::Text(text)` when the text is non-empty;
- consume `Reasoning`, `ReasoningDelta`, tool-call, final-response, and unknown items without yielding them;
- map a Rig `Err(CompletionError)` through the existing `map_completion_error` path;
- when the stream ends, require the `CompletionObserver` to have seen a
  successful completed response and require at least one non-whitespace text
  delta, otherwise yield `AttemptError::Completion` or `AttemptError::Empty`.

Keep `AttemptError` private and map it to `ProviderError` only at the public
`ChatStream` boundary. Do not include response bodies, request headers, or
credential values in errors.

- [ ] **Step 5: Preserve one-time 401 refresh/retry in the streaming state machine**

Implement `CodexProvider::stream` as a boxed `futures::stream::try_unfold`
state machine with `Start` and `Active` states. `Start` calls
`attempt_stream(messages.clone())`; `Active` polls the internal stream. If the
initial attempt (or a pre-delta stream error) is HTTP 401 and refresh has not
yet been attempted, refresh through the existing locked `AuthFile` and restart
once. If any text delta has already been emitted, return the error rather than
starting a second response. Map every final `AttemptError` with
`map_attempt_error`.

Dropping the returned stream must drop the Rig streaming response immediately;
do not spawn a detached producer task. Keep `complete`'s current buffered
implementation and its one-time retry behavior unchanged unless the shared
request helper requires a mechanical refactor.

- [ ] **Step 6: Run provider tests, Clippy, and commit**

Run:

```bash
cargo test --test codex_provider
cargo test --test codex_live -- --list
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Expected: all provider mock tests pass; the ignored live test remains ignored
and listed without requiring credentials. Commit:

```bash
git add src/codex_provider.rs tests/codex_provider.rs
git commit -m "feat: stream Codex text deltas"
```

### Task 3: Render live deltas in the TUI

**Files:**

- Modify: `src/demo.rs:17-266` and its existing test module
- Modify: `src/tui/components/text.rs:32-43`
- Modify: `tests/components.rs:35-55`

**Interfaces:**

- Adds `DemoIds::live_response: ComponentId` for the temporary assistant text.
- Adds a private helper that appends sanitized deltas to the live response
  `Text` component and requests a render.
- Keeps transcript commits and error rendering in the existing demo flow.

- [ ] **Step 1: Add the empty-text component test and a failing incremental UI test**

Extend the component test to assert that clearing a text component renders no
rows:

```rust
let mut text = Text::new("visible");
text.set_text("");
assert!(text.render(80).unwrap().is_empty());
```

Add a `ChunkedBackend` to the demo test module whose `stream` yields
`Ok("partial")`, then `Ok(" response")`. Drive the existing scripted event
loop with one prompt and Ctrl+C after the response completes. Assert that the
recorded terminal output contains both `moh: partial` and
`moh: partial response`, and that the final conversation turn contains only
the complete answer.

- [ ] **Step 2: Run the focused UI tests and verify they fail**

Run:

```bash
cargo test --test components text
cargo test --bin moh successful_request_streams_intermediate_text
```

Expected: the empty-text assertion and new incremental UI test fail because
`Text` still renders an empty row and the demo does not consume stream items.

- [ ] **Step 3: Hide cleared live text without changing non-empty rendering**

In `Text::render`, return `Ok(Vec::new())` before `wrap_ansi` when
`self.text.is_empty()`. Keep width/revision caching unchanged for non-empty
text and add the focused component test from Step 1.

- [ ] **Step 4: Add the live-response component and consume deltas**

In `build`, add `Text::new("")` after the transcript and store its component
ID in `DemoIds::live_response`, before the prompt input. During each pending
turn, replace the current final-only `tokio::select!` branch with a branch that
awaits `pending_turn.next_chunk()`.

Use this update flow:

```rust
match chunk {
    Some(Ok(_delta)) => update_live_response(tui, ids, pending_turn.text())?,
    Some(Err(error)) => {
        let pending_turn = pending.take().expect("pending turn exists");
        conversation.abandon_turn(pending_turn)?;
        apply_provider_error(tui, ids, error)?;
    }
    None => {
        let pending_turn = pending.take().expect("pending turn exists");
        apply_response(tui, ids, conversation, pending_turn.into_completed())?;
    }
}
```

`update_live_response` must sanitize the complete accumulated text with
`Input::sanitize_plain_text` and set the component text to
`format!("moh: {sanitized}")`. Request a render after each delta. On success,
`apply_response` must append the final answer to the transcript, clear the
live component, and set status to ready. On error, clear the live component,
append the existing sanitized `moh: error: ...` line, and set status to error.

Keep the current 16ms sleep branch, input suppression, help/resize handling,
and unconditional cleanup. If Ctrl+C or an event/render error exits while a
stream is active, `pending.take()` must reach `conversation.abandon_turn`
before the original application error is returned.

- [ ] **Step 5: Add partial sanitization and responsiveness tests**

Extend the existing backend-sanitization test to emit control sequences across
two deltas and assert none of the raw escape bytes appear in terminal output.
Keep the existing tests for never-completing requests, help/resize, event-error
cleanup, and no post-exit polling; they must continue to pass with streams.
Add an assertion that a stream failure after a visible partial delta leaves
`conversation.turns()` empty and permits the next submission.

- [ ] **Step 6: Run demo/component tests and commit**

Run:

```bash
cargo test --lib tui::components
cargo test --bin moh
cargo fmt --all -- --check
```

Expected: all existing and new component/demo tests pass. Commit:

```bash
git add src/demo.rs src/tui/components/text.rs tests/components.rs
git commit -m "feat: render streamed model responses"
```

### Task 4: Full verification and handoff

**Files:**

- Modify: none unless a validation-discovered issue is directly caused by this feature

- [ ] **Step 1: Inspect the final scope**

Run:

```bash
git status --short --branch
git log --oneline --decorate -6
git diff HEAD~3..HEAD --stat
git diff HEAD~3..HEAD --check
```

Confirm that the only commits after the approved spec are the three focused
feature commits and that no credential, generated, or unrelated UI files are
included.

- [ ] **Step 2: Run the complete validation sequence**

Run each command separately so failures identify their layer:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

Expected: every command exits successfully. Do not run the live Codex probe as
part of the ordinary suite; it remains the explicit ignored command:

```bash
MOH_RUN_CODEX_LIVE=1 cargo test --test codex_live real_codex_login_returns_a_non_empty_luna_answer -- --ignored
```

- [ ] **Step 3: Commit any directly required validation fix**

If a feature-caused validation failure remains, add only the smallest fix,
rerun the affected focused test and the complete validation sequence, then
commit it with a specific conventional message. Otherwise leave the worktree
clean and report the exact commands and results.
