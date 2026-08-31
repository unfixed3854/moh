# Stream Model Responses Design

**Date:** 2026-08-15
**Status:** Approved

## Goal

Show assistant text in the terminal as Codex produces it instead of keeping the
user on `thinking...` until the complete response has arrived. A response must
remain transactional: partial output is display state only and is committed to
conversation history only after a successful stream completion.

## Current boundary

`CodexProvider` already requests the Codex Responses API over SSE, but its Rig
adapter drains the stream inside `completion` and returns one `String` through
`ChatBackend::complete`. `Conversation::PendingTurn` therefore represents only
a final completion, and `demo` can render only the loading state followed by a
finished assistant message.

The existing compatibility constraints remain unchanged:

- use Rig's Responses transport with `store: false`;
- keep model `gpt-5.6-luna` and medium reasoning;
- preserve the `chatgpt-account-id` header;
- refresh and retry at most once after an HTTP 401;
- never expose credentials or raw provider bodies;
- keep one opaque, cancellable turn in flight.

## Chosen approach

Expose a normalized text stream at the existing provider boundary and use Rig's
streaming response implementation underneath it.

`ChatBackend` gains a `stream` method returning a boxed `ChatStream` whose items
are either assistant text deltas or a redacted `ProviderError`. Existing
backends remain source-compatible through a default one-shot implementation
based on `complete`, so non-streaming test doubles do not need to become
network-aware. `CodexProvider` overrides `stream` and maps Rig's streamed text
events to deltas. Reasoning, tool-call, unknown, message-ID, and final-response
metadata events are consumed without being displayed.

The implementation will not parse a second raw SSE protocol. Rig continues to
own SSE decoding and response aggregation, while the Codex-specific HTTP client
continues to provide the successful-content-type shim and completion validation.
The provider's stream setup retains the existing one-time authentication
refresh/retry policy. A stream that ends without a successful completed event,
or without assistant text, is classified as an existing redacted provider
failure.

## Conversation state

`PendingTurn` owns the boxed stream and an in-flight text buffer. It exposes each
text delta to the event loop and appends it to that buffer. It may still be
awaited as a convenience for existing callers; awaiting it drains the stream
and produces the same opaque `CompletedTurn` contract as before.

On normal end-of-stream, the buffered answer becomes a `CompletedTurn` and
`Conversation::resolve_turn` commits the user/assistant pair. If the provider
fails or the turn is abandoned, the pending stream is dropped before the busy
state is cleared. Any partial answer remains uncommitted and cannot resolve a
newer turn.

## TUI behavior

The demo keeps its current event-loop cadence and input rules. While a request
is pending, it polls for either terminal input or the next model delta. Each
delta updates a temporary live-response `Text` component and marks the TUI
dirty, so resize/help/exit remain responsive and the terminal renders the
partial answer incrementally.

When the stream completes, the final answer is appended to the normal
transcript and the temporary component is cleared. On a provider error, the
temporary partial response is discarded from history, a sanitized error line is
shown, and the status becomes `error`. Ctrl+C and other loop failures continue
to consume and drop the pending turn before cleanup returns.

## Testing

Add or update focused tests for:

- Rig-backed provider streams that emit multiple SSE text deltas, terminal
  completion, malformed/incomplete streams, and the existing auth retry path;
- `PendingTurn` accumulation, successful commit, partial-output rollback, and
  cancellation/drop ordering;
- TUI output that contains an intermediate assistant delta before the final
  response, while preserving responsive exit/help/resize behavior;
- sanitization of every displayed untrusted delta and final/error string.

Run the repository's complete validation sequence before claiming completion:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

No model selection, tools, persistence, concurrent requests, polling service,
or unrelated TUI redesign is included.
