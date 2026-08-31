# Codex Authentication and Conversation Design

## Goal

Turn the current deterministic `moh` demo into a minimal multi-turn conversation with OpenAI's Codex model while keeping `moh` responsible for its own agent runtime. The application will reuse an existing file-backed Codex CLI ChatGPT login, send requests through Rig, and retain conversation history only for the lifetime of the process.

This milestone deliberately prioritizes the authentication and model-request vertical slice over terminal UI polish.

## Scope

The milestone includes:

- file-backed Codex CLI credentials from `$CODEX_HOME/auth.json`, with `~/.codex/auth.json` as the default;
- ChatGPT-backed Codex authentication only;
- access-token refresh and safe persistence of rotated credentials;
- Rig's OpenAI Responses client configured for the Codex ChatGPT backend;
- the hardcoded `gpt-5.6-luna` model with medium reasoning effort;
- one in-flight request at a time;
- SSE transport required internally by the Codex backend, buffered fully before a completed answer is shown so presentation remains non-streaming;
- in-memory multi-turn user and assistant history;
- minimal transcript and status integration in the existing demo;
- unit, integration, and opt-in live smoke tests.

The milestone excludes:

- OS keyring and Codex encrypted-secrets credential storage;
- API-key authentication and a new login flow;
- model selection or configuration;
- persisted conversations;
- streaming output;
- tools, permissions, or an agent action loop;
- concurrent conversations or requests;
- substantial TUI redesign.

## Architecture

### `codex_auth`

The authentication module owns Codex credential discovery, parsing, refresh, and persistence. It resolves `$CODEX_HOME` when set and otherwise resolves the user's home directory and appends `.codex`. It then reads `auth.json` from that directory.

The parser accepts the documented Codex `auth.json` shape defensively and requires a ChatGPT authentication record containing an access token, refresh token, and account ID. It returns a narrow credential value containing only the fields the provider needs. Secret-bearing types use redacted `Debug` implementations, and errors never include raw credential contents.

Keyring-backed or encrypted-secrets-only installations are unsupported in this milestone. When no usable file exists, the error explains that `moh` currently requires file-backed Codex credentials and directs the user to configure Codex CLI's `cli_auth_credentials_store = "file"` and run `codex login`.

The refresh operation reserves the stable companion credential lock before loading and exchanging the one-time refresh token. Lock acquisition uses a 5-second deadline. The OAuth client uses a 5-second connection timeout, 10-second read timeout, and 30-second overall request timeout, so lock plus network waiting is bounded to 35 seconds before local persistence. Before persistence it reloads the file under the held lock and refuses to overwrite it if either the account ID or refresh token changed concurrently. It then writes the complete rotated credential document back atomically. On Unix, the resulting file has owner-only permissions. A refresh token is used at most once per attempt.

### `codex_provider`

The provider module isolates the less-stable Codex ChatGPT backend contract from the application. It builds Rig's OpenAI Responses client using:

- the Codex ChatGPT backend base URL;
- the cached access token as bearer authentication;
- the ChatGPT account ID request header;
- `gpt-5.6-luna` as the model;
- medium reasoning effort.

Consumers depend on a small asynchronous prompt interface rather than Rig or Codex transport details. Codex requires SSE transport internally, which the provider consumes and buffers completely before returning an answer; the application presentation remains non-streaming. The implementation first uses Rig's existing configurable OpenAI Responses provider. If live compatibility testing finds a request-shape mismatch that Rig cannot configure, only this module may be replaced with a custom Rig completion provider; this is a fallback, not part of the initial implementation scope.

An authentication rejection triggers one refresh and one retry. The retry rebuilds the provider client with the new access token. Other request failures are returned without refreshing.

### `conversation`

The conversation module owns committed in-memory history as ordered user and assistant messages. For a new prompt it constructs a request from the committed history plus the pending user message.

History is transactional at the turn level: the new user message and returned assistant answer are committed together only after a successful response. A failed request therefore remains visible in the demo transcript but does not affect subsequent model context.

This module permits one request at a time and exposes that state explicitly to the application.

### Demo integration

The existing demo remains responsible for input, terminal events, and rendering. Startup creates authentication, provider, and conversation services before entering the interactive loop.

Submitting input immediately appends the user's text to the visible transcript, sets the status to `thinking...`, and starts one asynchronous request. Additional submissions are ignored while the request is active. Resize, help, and exit events remain responsive during the request.

On success, the answer is appended and the status becomes `ready`. On failure, a concise non-secret error is appended and the status becomes `error`; the user may submit another message manually. The interface remains plain and non-streaming. The executable restores the terminal, drops its explicitly owned Tokio runtime so detached refresh persistence completes, and only then reports an application error and returns a failure exit code.

## Data Flow

1. Resolve Codex home and load file-backed ChatGPT credentials.
2. Construct the Rig-backed Codex provider.
3. Build the existing TUI and start the asynchronous terminal event loop.
4. On submission, render the user message and `thinking...` status.
5. Send committed history plus the pending message through the provider.
6. If the server rejects authentication, refresh credentials and retry once.
7. On success, atomically commit the user/assistant exchange and render the answer.
8. On failure, render a safe error without modifying committed model history.

## Error Handling

Errors remain typed across module boundaries and cover:

- inability to resolve a home directory;
- missing, unreadable, or malformed `auth.json`;
- unsupported credential storage or authentication mode;
- missing access token, refresh token, or account ID;
- token refresh rejection, revocation, reuse, or account mismatch;
- safe-persistence failures;
- Rig client construction failures;
- network timeout and transport failures;
- Codex HTTP or API rejection;
- empty or malformed model responses.

User-facing errors are concise and actionable. Diagnostic error chains may identify paths, status codes, and categories, but must never contain access tokens, refresh tokens, ID tokens, authorization headers, or the raw credential document.

The application retries only once and only after an authentication rejection. It does not retry arbitrary network or server failures in this milestone.

## Testing

Automated tests cover:

- `$CODEX_HOME` and default-home path resolution;
- valid credential parsing and every required-field failure;
- malformed documents and secret redaction;
- clear handling of file absence and unsupported storage;
- refresh success and classified refresh failures through a local mock server;
- account-change protection during refresh;
- atomic credential replacement and restrictive Unix permissions;
- provider configuration, including base URL, account header, model, reasoning effort, and request history;
- exactly one refresh/retry after authentication rejection;
- committed multi-turn history and failed-turn rollback;
- demo success and failure rendering;
- suppression of new submissions while a request is active;
- responsive resize and exit handling during a request.

An ignored, explicitly enabled live smoke test uses the developer's real file-backed Codex login to send a small request. Its purpose is to verify the current Rig/Codex backend compatibility without making ordinary test runs depend on personal credentials or network access.

Validation before completion consists of:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

The live smoke test is run separately when usable local credentials and network access are available.

## Security and Compatibility

Codex's ChatGPT backend and cached credential representation are not a stable public third-party API. All assumptions about them are confined to `codex_auth` and `codex_provider`, and compatibility failures must explain the corrective action instead of silently falling back to another billing or authentication mode.

`moh` never prints, logs, or stores credentials outside Codex's existing `auth.json`. It does not copy credentials into environment variables, command arguments, transcripts, fixtures, or snapshots. Tests use synthetic secrets and assert redaction.

Supporting OS keyrings and Codex's encrypted-secrets backend is a separate future milestone. Model selection, streaming, tools, and conversation persistence likewise remain independent follow-up work.
