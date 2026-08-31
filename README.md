# Moh

Moh is an experimental, local-first coding agent written in Rust. A thin
Ratatui client stays responsive while one per-user backend owns model runs,
background jobs, and durable sessions, so work can continue when a terminal
disconnects and can be observed again from another client.

> **Status:** active prototype. The client/server lifecycle, session durability,
> tools, and test harness are implemented, but the provider integration depends
> on Codex's private ChatGPT backend and credential format. Those are explicitly
> unstable and are not a supported public API.

## Project tour

- [Try it](#try-it): build, inspect, and run Moh with synthetic example data.
- [Architecture](#architecture): follow a request from the terminal to the
  provider and durable store.
- [Security boundaries](#security-boundaries): understand what Moh isolates
  and what still runs with the invoking user's authority.
- [Usage and sessions](#usage-and-sessions): understand session lifecycle and
  client commands.
- [Terminal interface](#terminal-interface): see navigation, editing, and
  process controls.
- [Testing](#testing): reproduce the automated quality gate and see what it
  does not cover.
- [Current limitations](#current-limitations): review the prototype boundaries.

## Try it

Moh requires Rust via [rustup](https://rustup.rs/). Clone the repository, then
build and inspect the public CLI without provider credentials (Cargo may need
network access to download dependencies on the first build):

```bash
cargo build --locked
cargo run --locked -- --help
cargo test --all-targets
```

For an authenticated interactive demonstration, first sign in with Codex CLI
using the file-backed credential store described in
[Codex authentication](#codex-authentication), then run:

```bash
cargo run --locked -- --new
```

The following transcript is illustrative and uses synthetic names and paths;
the actual conversation is rendered in the fullscreen terminal UI:

```text
working directory: /tmp/moh-demo
you: Summarize the Rust modules in this repository.
moh: [read] src
moh: [read] src/lib.rs
assistant: The crate separates the terminal client, backend, session runtime,
           provider adapter, RPC transport, and agent tools.
status: ready · gpt-5.6-luna · medium
```

Exit the client with Ctrl+C, then demonstrate durable discovery and reattachment:

```bash
cargo run --locked -- sessions
cargo run --locked -- --resume session-1
```

The exact generated session ID may differ. Do not record or publish a live demo
that contains credentials, personal paths, private source, or user data.

## Usage and sessions

Moh uses one local backend for all clients and working directories. Client modes connect to that backend, starting it automatically when needed. The supported commands are:

```text
moh
moh --new
moh --resume SELECTOR
moh sessions
moh server
```

- `moh` resumes the most recently active running session in the invocation directory, or opens a local, non-durable empty chat when none is running. Idle saved sessions remain available through explicit selection.
- `moh --new` always obtains fresh backend defaults for a non-durable chat without selecting, attaching, or inheriting settings from another running session. It accepts no title or other argument; `moh --new NAME` is rejected.
- `moh --resume session-N` opens that stable ID globally. Any other valid `SELECTOR` is an exact title match in the invocation directory; duplicate matches report the stable IDs to choose explicitly.
- `moh sessions` lists the invocation directory's durable sessions and live state without creating a session for an empty chat.
- `moh server` runs the backend in the foreground for diagnosis. Ordinary client commands instead spawn a detached backend when one is not already available.

An empty startup chat or `/new` chat has no session ID, actor, database row, job registry, or browser entry. Its first nonblank message is persisted together with its settings, deterministic fallback title, visible user transcript entry, and running turn before model execution begins. A failure before that transaction leaves the draft and prompt intact; a run-start failure afterward remains visible as a durable failed turn.

Moh asynchronously asks AI for a concise title from the first message. The deterministic shortened first-message title is already durable and remains when generation fails or returns invalid output. Titles are non-unique display metadata; `F2` in the session browser edits a title, and a manual rename always wins over delayed generation.

Ctrl+C and `/quit` exit only the client and detach it from the session. Switching chats also detaches without cancellation, so an active model run or background job continues in the backend. Escape during an active run and `/cancel` explicitly cancel the shared run while keeping the client attached.

The visible transcript is durable for successful, failed, cancelled, and backend-interrupted turns, while only successful user/assistant exchanges become future model context. Settings and titles are durable too. Active model requests and background jobs survive client exit, detachment, and switching, but they are process-local and do not survive backend death or machine restart; a durable running turn is restored once as interrupted.

## Terminal interface

Moh's private Ratatui client uses the alternate screen for a fullscreen transcript, prompt, and status display. PageUp and PageDown scroll by one page minus one row, the mouse wheel scrolls by three rows, and End from the end of the prompt resumes following the latest transcript content. The interface reflows when resized and reports when the terminal is too small.

Use Left/Right, Up/Down, Home/End, Delete/Backspace, Ctrl+Left/Right, and Ctrl+Backspace/Delete to edit the prompt. Enter submits; Shift+Enter adds a line. Ctrl+O opens help, and Escape closes menus and help or cancels an active run. Typing `/` shows matching commands above the prompt; Up/Down selects a suggestion, Tab completes it, and Enter runs it. `/model [model-id]` changes the model using fuzzy matching, while bare `/model` or Ctrl+L opens a searchable model selector. `/effort [level]` selects an effort advertised for the active model; bare `/effort` or Ctrl+R opens its searchable selector, and Shift+Tab cycles supported efforts. `/ps` opens a selector for running background processes; choosing one prepares its `/kill job-N` command, which terminates it after confirmation. Slash input without a matching command is submitted to the model normally. The bottom status line shows `new chat` or the current durable title beside the working directory, plus the active model and effort, ready/thinking.../error state, and running background-process count. The client requires an interactive terminal.

`/new` opens a fresh draft and accepts no arguments. `/sessions` opens the session browser in local mode; `Tab` toggles a global view grouped by working directory. Typing fuzzy-filters titles, stable IDs, and displayed paths. Up/Down, PageUp/PageDown, and the mouse wheel navigate rows; Enter switches to the selected session, `F2` edits its title, Ctrl+D opens deletion confirmation, and Escape closes the nested dialog before closing the browser. The browser refreshes once per second while open, retains its last good rows after a refresh failure, and has no create action.

A switch opens and validates the target before detaching the previous attachment. A failed switch therefore leaves the old chat attached and the browser open. A successful global switch adopts the target's working directory for later local browsing and `/new`; returning to a detached session recovers its latest durable or still-live transcript.

Deletion is permanent and always requires confirmation naming the title and stable ID and warning that active model work will be cancelled, background jobs terminated, and attached clients disconnected. Deleting another row keeps the browser open. Deleting the current session—or receiving its deletion from another client—selects the most recently active running session in that project or returns to a non-durable draft.

This milestone intentionally defers terminal images, themes, general-purpose autocomplete, nested focus traversal, overlay scrolling, and clipboard integration.

## Execution plans

Moh gives the model one execution-plan tool, `update_plan`. It replaces the
complete ordered plan for the current session; it does not support individual
task mutation or a separate plan-reading tool. The model receives the current
non-empty plan in the next run's generated context, and uses this tool again
whenever the plan changes.

```json
{
  "explanation": "Optional reason for changing the plan",
  "plan": [
    {
      "step": "Run the focused tests",
      "status": "in_progress"
    }
  ]
}
```

`plan` is required and `explanation` is optional. Each item has only `step`
and `status`; unknown properties are rejected. Status is exactly one of
`pending`, `in_progress`, `completed`, `blocked`, or `cancelled`. A replacement
may contain at most 32 ordered items and at most one `in_progress` item. Step
text must be trimmed, contain no control characters, and be 1 through 256
Unicode scalar values. Duplicate step text is allowed. Sending an empty plan
clears the current plan, and an invalid request leaves the prior plan unchanged.

Plans are durable session application state in Moh's state directory (in the
session SQLite database), not conversation history or workspace data. A
successfully checkpointed plan survives context compaction, client detachment,
completed, failed, and cancelled runs, and backend restart. A checkpoint
failure keeps the accepted plan live but marks its persistence as pending;
attached clients receive a warning while a later checkpoint or clean shutdown
retries it. If the backend dies or restarts before that retry succeeds, Moh
restores the previously checkpointed plan instead.

The status row shows `plan C/T` for a non-empty plan: `C` is completed items and
`T` is all non-cancelled items. A 42-column Todo sidebar appears automatically
when the terminal is wider than 120 columns; Ctrl+T toggles it at any width. On
narrower terminals it overlays the right side without becoming modal. The
sidebar labels an empty plan and explains when todos will appear. Todo text
word-wraps within the panel, and completed and cancelled items remain visible;
cancelled items are excluded from `T`.

An execution plan is intentionally not a task graph: it has no stable task
IDs, dependencies, nesting, priorities, owners, scheduling, or subagents. It
is also not Plan Mode: Moh has no plan-mode permissions or
`enter_plan_mode`/`exit_plan_mode` commands. Plans never create or modify files
in the workspace.

## Architecture

Moh separates presentation from long-lived work:

```text
Ratatui client(s)
       │  versioned Cap'n Proto RPC over an owner-only Unix socket
       ▼
per-user backend ──► lazy per-session actor ──► harness + isolated tools
       │                       │                         │
       │                       └──► SQLite              └──► RunEngine
       │                            sessions                  │
       └──► lifecycle and job registry                       ▼
                                                   Rig adapter ──HTTP/SSE──►
                                                   Codex ChatGPT backend
```

The terminal process is a thin client: it owns input, rendering, and local
projections of backend events. One per-user backend owns the durable store and
a lazy actor for each open session. They communicate through a versioned Cap'n
Proto protocol over an owner-only Unix-domain socket. Multiple clients can
therefore observe and control the same session, while separate session actors
serialize their work independently.

Within each live session, `moh::harness` owns monotonically assigned run IDs, the lifecycle and terminal outcome of its single active run, and successful text-only conversation history. `RunEngine` is its model-neutral execution port: an engine starts a request and yields engine events, while the harness projects those into run events and commits history only after a nonblank completion.

`runtime::rig` adapts Rig and isolated per-session tools to that port. `providers::codex` owns Codex authentication plus HTTP/SSE completion transport. The backend session actor owns model and reasoning settings, submissions, cancellation, observers, and the session's job registry; the terminal client owns input, rendering, and local projections of backend snapshots and events.

The backend shuts down after it has no connected clients, active runs, or running jobs for the configured idle timeout, which defaults to 15 minutes. Detaching the last client starts that idle window only when no work remains. Shutdown proceeds only after dirty sessions checkpoint successfully; a checkpoint failure vetoes shutdown and starts a new idle wait.

## Security boundaries

Moh is a single-user local prototype, not a sandbox or a privilege boundary.
The backend accepts local Unix-socket clients only; runtime directories are
owner-only and the socket, lock, log, and database files are created with
owner-only permissions. These controls separate local OS users, but they do
not restrict what the model can do as the user who launched Moh.

The model-facing `read` tool accepts absolute paths, and the `bash` tool runs
non-interactive commands with the invoking user's normal filesystem and
process permissions. The read-before-write checks described below protect the
dedicated `write` and `edit` tools from stale replacements; they do not apply
to shell commands. Repository instructions, source files, and tool output can
also contain prompt injection. Run Moh only in repositories and accounts you
trust, review requested tool actions, and use ordinary OS isolation when a
task needs a stronger boundary.

Moh reads Codex credentials from `$CODEX_HOME/auth.json` and may refresh that
file atomically with owner-only permissions. Credentials and raw provider
responses are not intended for transcript or diagnostic output, but the
provider protocol and credential format are private, unstable interfaces.
Session transcripts and settings are stored unencrypted in the per-user state
directory, so their confidentiality depends on the host account and
filesystem permissions.

## Configuration and local paths

Moh reads optional strict TOML configuration once when the backend starts. The default configuration is:

```toml
[server]
idle_timeout = "15m"
```

Paths follow the host platform's user directories:

- On Linux, configuration is `$XDG_CONFIG_HOME/moh/config.toml` or `~/.config/moh/config.toml`, and durable state is under `$XDG_STATE_HOME/moh` or `~/.local/state/moh`.
- On macOS, configuration is `~/Library/Application Support/moh/config.toml`, and the same `~/Library/Application Support/moh` directory is used for durable state.
- The runtime directory is `$XDG_RUNTIME_DIR/moh` when `XDG_RUNTIME_DIR` is set; otherwise it is `moh-<effective-uid>` below the system temporary directory. It contains `backend.sock` and the `backend.lock` startup lock.

The state directory contains `sessions.sqlite`, the durable `hash-store.sqlite` read-anchor database, and `server.log`. An automatically spawned backend appends its stdout and stderr to the private `server.log`; a foreground `moh server` writes diagnostics to its terminal. Spawn and five-second readiness-timeout errors report the exact socket, lock, and log paths, so inspect the reported `server.log` first. If no backend currently owns the socket, running `moh server` in the foreground keeps its sanitized top-level startup diagnostics attached to the terminal.

## Prerequisites

- Rust via [rustup](https://rustup.rs/)

The repository includes `rust-toolchain.toml`, which selects the stable toolchain and the `rustfmt` and `clippy` components.

## Codex authentication

`moh` currently reuses a ChatGPT login created by Codex CLI. Configure Codex to
use file-backed credentials and sign in before starting `moh`:

```toml
# ~/.codex/config.toml
cli_auth_credentials_store = "file"
```

```bash
codex login
cargo run
```

New sessions use `gpt-5.6-luna` with medium reasoning by default, and one request
runs at a time in each session. The Codex backend uses SSE transport internally;
`moh` renders each tool call as a compact dim transcript line and renders the
terminal assistant response once it is complete. Provisional text from
intermediate tool turns is not shown, tool calls are not added to conversation
history, and history is committed only after the agent run completes
successfully.

This integration targets Codex's ChatGPT backend and cached credential format,
which are not stable third-party APIs. Keyring-backed Codex credentials are not
supported yet. Treat `$CODEX_HOME/auth.json` like a password and never commit or
share it.

Credential refresh reserves Codex's companion credential lock before exchanging
the one-time refresh token. Lock contention is bounded to 5 seconds, and the
OAuth exchange has a 30-second overall timeout (including 5-second connect and
10-second read bounds), for at most 35 seconds of bounded waiting before local
atomic persistence. Backend shutdown waits for an already-dispatched refresh
to finish so rotated credentials are not lost.

## Agent file access

Moh's Codex agent can read text files and list directories through its `read`
tool. Successful file rows use durable `HASH│content` anchors; directory rows
list sorted direct children, with `/` marking child directories. Relative paths
resolve from the current working directory, and absolute paths are allowed.

The `write` tool creates text files and performs intentional whole-file
rewrites. An existing file must first be read in the current live session; even
a partial read is sufficient because Moh records the complete file checksum.
The rewrite is rejected if the file changes or is deleted after that read.
These authorization observations are isolated from other sessions and live
outside model-visible history. They survive prompt turns, conversation
compaction, client detachment, and client restart while the backend remains
alive, but they are not restored after backend death or restart. Writes are
staged beside the target and installed atomically where the platform permits.
Moh rechecks the checksum immediately before replacement; the filesystem does
not provide an atomic compare-and-swap, so an external writer could still race
that final check.

The `edit` tool performs surgical changes to an existing text file. It accepts
one inclusive range identified by the three-character anchors returned by
`read`, plus replacement content as one string per logical line. An empty
replacement array deletes the range. The edit requires a read in the current
live session, rejects stale files and anchors, preserves the file's newline
convention, UTF-8 BOM, and permissions, and stages the replacement beside the
target before installing it. A successful edit returns a refreshed anchored
read of the file so later edits can use the new anchors directly.

Anchor snapshots are durable application state, not configuration. Moh stores
them through `directories` in its platform state directory: on Linux,
`$XDG_STATE_HOME/moh` or `~/.local/state/moh`. The shared durable snapshots
support anchor reuse, but do not grant another or restarted session write/edit
authority. Image attachments are not yet supported.

## Agent skills

A skill is a direct child directory containing a `SKILL.md` file with YAML
frontmatter. Moh discovers global skills from `~/.agents/skills` first, then
project skills from `<project-root>/.agents/skills`. A valid project skill
replaces a global skill with the same name.

At startup, Moh sends only each skill's name, description, and literal
`SKILL.md` path to the agent. Moh loads the full instructions only when the
agent reads the selected `SKILL.md`; any skill resources are then available on
demand through their relative paths.

The current project root is the nearest directory containing a `.git`
directory or linked-worktree file. When neither exists, Moh uses the working
directory. Additional root markers are not supported yet.

## Agent command execution

Moh's `bash` tool runs non-interactive `bash -lc` commands from the current
working directory. Foreground execution is the default; `background: true`
returns a job ID after the child starts. Optional timeouts are explicit and
limited to one hour.

`job_status`, `job_wait`, and `job_cancel` inspect and control only the current
session's backend-resident job registry. Moh retains at most 16 running and 64
terminal jobs per session, bounds model-visible output to 50 KiB or 2,000
lines, and provides a temporary path for truncated full output. Jobs survive
client detachment or restart while the backend remains alive, but are not
restored after backend death or restart. This milestone does not provide PTYs,
stdin forwarding, or unsolicited completion notifications.

Backend shutdown cancels and reaps remaining background processes before
runtime teardown.

## Testing

The local quality gate covers formatting, strict Clippy checks, unit and
integration tests, a locked build, and whitespace errors:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

The automated suite exercises the CLI, Unix-socket RPC transport, concurrent
clients, session actors and SQLite recovery, provider HTTP/SSE parsing, tools,
and Ratatui rendering. `tests/codex_live.rs` is ignored by default because it
uses a developer's real login, network, and provider quota. Passing the default
gate therefore does not prove that the current private provider integration is
live-compatible.

RPC bindings are checked in, so ordinary builds do not require the Cap'n Proto compiler. Maintainers with `capnp` and `capnpc-rust` installed can regenerate them with:

```bash
scripts/generate-rpc.sh
```

## Current limitations

- Moh supports local Unix-domain sockets, not TCP, remote clients, Windows
  transport, or cross-user access.
- Active requests and background jobs survive client detachment but not a
  backend process or machine restart. Durable in-flight turns are restored as
  interrupted.
- The only provider adapter currently targets Codex's private ChatGPT HTTP/SSE
  backend and file-backed credentials. Neither is a stable public contract.
- Conversations are text-only; image attachments are not implemented.
- The terminal UI intentionally omits themes, general-purpose autocomplete,
  overlay scrolling, nested focus traversal, and clipboard integration.

## Contributing

This is a personal project, so development direction is intentionally opinionated and may change quickly. Small, focused changes with passing formatting, Clippy, tests, and builds are welcome.

## License

Moh is licensed under either the [Apache License, Version 2.0](LICENSE-APACHE)
or the [MIT License](LICENSE-MIT), at your option.

Third-party dependencies retain their own copyright and license terms; Moh's
license does not relicense them.
