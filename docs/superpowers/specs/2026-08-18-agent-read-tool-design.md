# Agent Read Tool Design

## Goal

Give Moh's Codex agent a real, local `read` tool that returns Pi-compatible
hashline anchors and is executed by Rig's agent runtime during a submitted
prompt.

## Scope

This change adds only the text-file `read` tool and its agent execution path.
It does not add write, replace, undo, tool-result image rendering, workspace
confinement, conversation persistence, or a new TUI tool-activity view.

Supported image files are reported as text-only unsupported input. Visual image
attachments are tracked separately in GitHub issue #1.

## Dependencies

Add the following direct dependencies:

- `rig-agent` version `0.41.0`, matching the existing `rig-core` dependency;
- `directories` version `6.0.0` for platform-standard application-state
  locations;
- SQLite support for durable anchor snapshots;
- an xxHash implementation that provides xxHash32 and xxHash64.

## Agent Architecture

`CodexProvider` continues to implement the existing `ChatBackend` interface.
For each submitted prompt, it creates a request-scoped Rig `Agent` around the
existing `CodexCompletionModel`, adds the `read` tool, preserves the current
medium reasoning setting and `store: false` request policy, supplies the
committed Moh conversation history, and streams the agent run.

The agent's total model-call limit is **512 calls per submitted prompt**. The
count includes the initial response and every continuation after a tool result.
It is a runaway-loop guard, not a short task limit. Dropping Moh's active
pending turn continues to cancel the underlying stream as it does today.

Rig owns function-tool registration, argument validation, function-call/result
correlation, and continuation requests. Moh filters Rig's streamed output to
the existing assistant-text delta stream, so the current TUI remains text-only
and does not render raw tool calls or tool results.

The existing one-refresh OAuth behavior remains: an unauthorized provider
response may refresh credentials and retry only before the user has received
assistant text. Provider errors after visible text remain terminal, preserving
the current no-duplicate-output behavior.

## Tool Contract

`read` is a Rig runtime tool. Its JSON arguments are:

```json
{
  "path": "relative-or-absolute-path",
  "offset": 1,
  "limit": 50
}
```

`path` is required unless its `file_path` alias is present; supplying both is a
bad request. `offset` and `limit` are optional positive integers. `offset` is
one-indexed. Unknown fields and invalid types are rejected as malformed tool
arguments.

Relative paths resolve against the process current working directory. Absolute
paths are permitted. Canonical target paths, including paths reached through
symlinks, are permitted; Moh deliberately does not impose a workspace root.

Successful text output renders each displayed line as:

```text
ABC│line contents
```

`ABC` is a unique three-character `A-Za-z0-9` anchor. The full output is the
tool's textual result delivered to the model.

## Read Semantics

The tool follows the text-read contract of
`pi-hashline-edit-pro` 2.6.1:

- Read a maximum of 100 MiB and 238,328 lines. Larger files return
  `[E_FILE_TOO_LARGE]`.
- Reject directories, non-regular files, binary inputs, UTF-16/UTF-32 BOM text,
  and image files with `[E_NOT_TEXT]`. JPEG, PNG, GIF, WebP, and BMP image
  attachment support is intentionally deferred to issue #1.
- Do not misclassify a UTF-8 text file merely because its leading bytes match a
  known binary signature. A NUL byte makes the file binary.
- Strip a UTF-8 BOM from display and normalize CRLF/CR to LF for line and anchor
  processing. Decode other invalid UTF-8 bytes with replacement characters and
  append the extension-compatible warning that a future edit would rewrite the
  file as UTF-8.
- Page according to `offset` and `limit`; when more lines remain, append
  `[Showing lines start-end of total. Use offset=next to continue.]`.
- A requested offset past the end returns a descriptive, non-error text result.
- An empty file returns one anchored empty row followed by the extension's
  empty-file guidance.
- A rendered row exceeding 200 KiB is not partially shown. Replace it with the
  extension-compatible line-size marker and `sed ... | head -c 204800`
  inspection guidance, because a hashline anchor requires the full line.

Filesystem failures are returned to the model as descriptive tool failures:
`[E_NOT_FOUND]` for absence and `[E_ACCESS]` for unavailable read access. They
do not terminate the whole agent turn; Rig sends the failure as the called
tool's result and permits the model to choose another path or respond.

## Durable Hashline Anchors

Anchor state location is derived through `directories::ProjectDirs` for Moh.
Moh uses `ProjectDirs::state_dir()` when it exists, producing
`$XDG_STATE_HOME/moh` (or `~/.local/state/moh`) on Linux. Platforms without a
separate state directory use `ProjectDirs::data_local_dir()` instead. The
database is `hash-store.sqlite` inside that selected directory. This is durable
application state, not configuration. The store is private to Moh; it does not
share Pi's data files.

For every canonical target path, the store records the xxHash64 content
checksum, normalized line count, and unique anchor list. Store writes are
atomic. A corrupt store is quarantined with a timestamp suffix and rebuilt; a
busy store is retried with bounded backoff. If a usable, durable snapshot
cannot be produced, the read call fails instead of serving unstable anchors.

Each normalized line is canonicalized for anchor hashing by removing carriage
returns and trimming trailing whitespace. Its base anchor is xxHash32 reduced
into a 62-character alphabet (`A-Z`, `a-z`, `0-9`). Collisions, including
identical repeated lines, are allocated uniquely using the extension's
coprime-probe stride. Thus every file line has a distinct three-character
anchor.

When file content changed since the stored snapshot, unchanged lines retain
their existing anchors. Matching is position-aware so duplicated content never
borrows an anchor from outside its surviving occurrence. New lines receive new
unique anchors. This state survives process restarts and external file edits,
which prepares Moh for later anchor-based replace support.

## Validation

Add focused read-tool tests for:

- schema validation, `file_path` alias handling, and relative/absolute paths;
- ordinary anchored output, paging, offsets beyond EOF, empty files, and
  oversized rendered lines;
- binary, image, directory, missing, access-denied, UTF-16/32, UTF-8 BOM, and
  lossy UTF-8 behavior;
- unique repeated-line anchors, deterministic collision resolution, trailing
  whitespace stability, external insert/delete/change preservation, and a
  reopened SQLite store;
- max-byte and max-line rejection plus store-corruption/reopen behavior.

Extend Codex provider tests with scripted Responses SSE turns that prove the
first request advertises the `read` function, Rig executes its request, the
follow-up request contains the exact correlated function-call output, and the
final assistant text is delivered through `ChatBackend::stream`. Retain
coverage for auth refresh before emitted text and cancellation. Existing demo
and conversation tests must continue to pass without UI changes.

## Verification

Run formatting, Clippy with warnings denied, the new focused tests, the full
test suite, and a release build. Inspect the recorded HTTP request fixtures to
confirm tool schema and function-result correlation without printing
credentials, authorization headers, or provider response bodies.
