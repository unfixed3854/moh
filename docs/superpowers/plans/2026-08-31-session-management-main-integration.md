# Session Management and Main Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Merge `origin/main` into `feat/session-management` while preserving the complete session-management, execution-plan, CLI, and Ratatui contracts.

**Architecture:** Create one merge commit because the feature branch is already published. Resolve each shared layer against the approved integration design, regenerate Cap'n Proto bindings from the combined schema, and use focused seam tests before the full repository gate.

**Tech Stack:** Rust 2024, Clap, Tokio, SQLite/rusqlite, Cap'n Proto 1.5.0 with capnpc 0.27.0, Crossterm, Ratatui, and Git.

**Spec:** `docs/superpowers/specs/2026-08-31-session-management-main-integration-design.md`

## Global Constraints

- Preserve the exact CLI forms `moh`, `moh --new`, `moh --resume <SELECTOR>`, `moh sessions`, `moh server`, and `moh server --internal-detached`.
- Preserve ephemeral draft materialization, actor-owned lifecycle state, exact detach counts, deletion fallback, and durable visible transcript behavior.
- Preserve actor-owned execution-plan state and `SessionSnapshot.plan @9`.
- Preserve the multiline editor, responsive welcome/help, todo sidebar, and contextual `/exit` alias.
- Never hand-edit `src/rpc/moh_capnp.rs`; regenerate it from `schema/moh.capnp`.
- Do not rebase, squash, modify `main`, or stage unrelated paths.
- The only integration commit is `merge: resolve conflicts with main`.

## File Map

- `schema/moh.capnp`, `src/rpc/moh_capnp.rs`, `src/rpc/{client,convert}.rs`: combined lifecycle and plan protocol.
- `src/session/{actor,mod,projection,runtime,store,types}.rs`: actor-owned lifecycle plus plan persistence/projection.
- `src/runtime/rig/{codex,mod}.rs`: title generation and plan-tool registration in one runtime.
- `src/cli.rs`, `src/main.rs`, `tests/{cli,local_launch}.rs`: Clap-based session launch interface.
- `src/client/{app,app_tests}.rs`, `src/client/ui/{mod,view}.rs`: session controller/browser composed with multiline editing and the sidebar.
- `README.md`: combined current usage, interaction, and maintenance documentation.
- `tests/{client_server,rig_runtime,rpc_schema,rpc_transport,session_actor,session_projection,session_store,support}.rs`: cross-layer regression coverage.

---

### Task 1: Start the merge and inventory the exact conflicts

**Files:**
- Inspect: every unmerged path reported by Git

**Interfaces:**
- Consumes: clean `feat/session-management` and fetched `origin/main`
- Produces: one in-progress merge with a fixed list of stage-1/2/3 conflict blobs

- [ ] **Step 1: Confirm the branch and merge base**

```bash
git status --short --branch
git merge-base HEAD origin/main
git rev-list --left-right --count origin/main...HEAD
```

Expected: the worktree is clean, the current branch is `feat/session-management`, and both sides have unique commits.

- [ ] **Step 2: Start one non-fast-forward merge without committing**

```bash
git merge --no-ff --no-commit origin/main
```

Expected: Git stops with content conflicts and leaves all clean auto-merges staged.

- [ ] **Step 3: Record the conflict inventory**

```bash
git diff --name-only --diff-filter=U
git ls-files -u
```

Expected: conflicts are limited to the shared documentation, protocol, CLI, client, runtime, session, and test files identified in the design review.

### Task 2: Reconcile CLI, entrypoint, and documentation

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `README.md`
- Modify: `tests/cli.rs`
- Modify: `tests/local_launch.rs`

**Interfaces:**
- Consumes: main's Clap parser and session management's `CliMode`, `SessionSelector`, and terminal-controller launch intent
- Produces: `CliMode::{Default, New, Session, Sessions, Server}` parsed through Clap with `--resume`

- [ ] **Step 1: Resolve the parser around the approved forms**

Keep main's `clap::{Parser, Subcommand, Args}` implementation. Rename the selector option to `resume`, keep it mutually exclusive with `new`, parse `session-*` as `SessionId`, and otherwise validate through `SessionTitle`. Do not restore the handwritten `CliError` parser or the retired `--session` spelling.

- [ ] **Step 2: Resolve binary dispatch**

Keep main's error rendering and detached-server handling, but route `Default`, `New`, and `Session` through session management's launch intent/controller. Keep `Sessions` as the current-project listing command.

- [ ] **Step 3: Merge CLI and launch tests**

Retain main's Clap help/error snapshots and session management's draft/resume/list launch assertions. Update expected copy only for `--resume` and the approved unnamed `--new` behavior.

- [ ] **Step 4: Run the focused CLI tests**

```bash
cargo test --test cli
cargo test --test local_launch
```

Expected: both test binaries pass with no legacy `--session` acceptance.

### Task 3: Reconcile Cap'n Proto, RPC conversion, and transport

**Files:**
- Modify: `schema/moh.capnp`
- Regenerate: `src/rpc/moh_capnp.rs`
- Modify: `src/rpc/convert.rs`
- Inspect auto-merge: `src/rpc/client.rs`
- Modify: `tests/rpc_schema.rs`
- Modify: `tests/rpc_transport.rs`
- Modify: `tests/client_server.rs`

**Interfaces:**
- Consumes: session lifecycle RPC operations and execution-plan projection
- Produces: protocol v2 schema with stable session ordinals and `SessionSnapshot.plan @9`

- [ ] **Step 1: Resolve only the source schema**

Start from the session-management schema and add main's `PlanStatus`, `PlanItem`, and `SessionSnapshot.plan @9`. Preserve lifecycle backend methods `startup @1` through `draftDefaults @7`, `Session.detach @6`, title-based selectors, lifecycle errors, and `CommandResult.attachedClients @2`.

- [ ] **Step 2: Regenerate checked-in bindings**

```bash
capnp --version
scripts/generate-rpc.sh
```

Expected: Cap'n Proto reports 1.5.0, the generated header reports capnpc 0.27.0, and `src/rpc/moh_capnp.rs` has no conflict markers.

- [ ] **Step 3: Resolve conversions and transport tests**

Retain lifecycle selectors/errors/snapshots and add bidirectional `PlanStatus`/`PlanItem` conversion. Keep protocol feature negotiation for both session-management and plan capabilities. Merge observer/transport assertions so snapshots and updates carry plan state without changing attachment behavior.

- [ ] **Step 4: Run focused protocol tests**

```bash
cargo test --test rpc_schema
cargo test --test rpc_transport
cargo test --test client_server
```

Expected: schema ordinals, generated conversions, attachment cleanup, and plan projection all pass.

### Task 4: Reconcile session domain, actor, projection, and store

**Files:**
- Modify: `src/session/actor.rs`
- Modify: `src/session/mod.rs`
- Modify: `src/session/projection.rs`
- Modify: `src/session/runtime.rs`
- Modify: `src/session/store.rs`
- Inspect auto-merge: `src/session/types.rs`
- Modify: `tests/session_actor.rs`
- Modify: `tests/session_projection.rs`
- Modify: `tests/session_store.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Consumes: lifecycle commands/events, `PlanItem`, `PlanStatus`, and persisted session records
- Produces: one actor-owned snapshot containing lifecycle and plan state, with atomic persistence

- [ ] **Step 1: Resolve domain exports and projection state**

Export both lifecycle/title types and plan types. Keep plan as authoritative projection state rather than transcript inference. Preserve sequence ordering, running-state derivation, durable visible terminal events, and successful-only model history.

- [ ] **Step 2: Resolve actor commands and checkpointing**

Keep rename/generated-title/delete/detach lifecycle commands and add main's sequenced plan update command. Ensure plan updates are rejected when unsequenced, checkpointed with the rest of the snapshot, and broadcast in actor order. Preserve authoritative post-detach counts.

- [ ] **Step 3: Resolve store migrations and round trips**

Combine the session-management schema migration with plan persistence. A loaded session must retain title metadata, visible transcript, successful history, settings, turn state, and plan items. Drafts remain absent from storage.

- [ ] **Step 4: Merge seam tests before implementation verification**

Retain tests for blank materialization, bounded-observer detach, delete fallback, interruption recovery, plan validation, plan checkpointing, and plan ordering. Add combined assertions to existing snapshot/store tests where neither parent alone covers lifecycle plus plan state.

- [ ] **Step 5: Run focused session tests**

```bash
cargo test --test session_actor
cargo test --test session_projection
cargo test --test session_store
```

Expected: all actor, projection, migration, recovery, and persistence tests pass.

### Task 5: Reconcile runtime tools and Ratatui composition

**Files:**
- Modify: `src/runtime/rig/codex.rs`
- Modify: `src/runtime/rig/mod.rs`
- Modify: `tests/rig_runtime.rs`
- Modify: `src/client/app.rs`
- Modify: `src/client/app_tests.rs`
- Modify: `src/client/ui/mod.rs`
- Modify: `src/client/ui/view.rs`

**Interfaces:**
- Consumes: title generator, plan tool, session workspace/controller, multiline editor, sidebar, and session browser
- Produces: one runtime registering plan updates and title generation, plus one composed TUI

- [ ] **Step 1: Resolve runtime registration**

Keep session management's title-generation transport and main's update-plan tool. Preserve the session-scoped plan sink and ensure title-only requests cannot mutate conversation or plan state.

- [ ] **Step 2: Resolve application state and input precedence**

Preserve the session workspace/controller and browser states while using main's multiline `PromptEditor` and todo-sidebar state. Browser and confirmation modal input wins over editor input; background refresh and lifecycle feedback keep their existing separation.

- [ ] **Step 3: Resolve rendering composition**

Start from main's responsive layout and multiline prompt geometry, then compose session management's browser/rename/delete overlays. Keep the prompt visible beside the sidebar, prevent narrow overlay clipping, and retain contextual `/exit` suggestions.

- [ ] **Step 4: Merge runtime and rendered-buffer tests**

Retain tests for plan-tool registration, title isolation, grapheme/multiline cursor positions, sidebar breakpoints, help layouts, browser empty/filter/confirmation states, and lifecycle feedback persistence.

- [ ] **Step 5: Run focused runtime and client tests**

```bash
cargo test --test rig_runtime
cargo test client::app_tests --lib
```

Expected: runtime registration and all client state/rendering tests pass.

### Task 6: Resolve remaining markers and run the full gate

**Files:**
- Modify: any remaining conflicted paths from Task 1
- Verify: entire repository

**Interfaces:**
- Consumes: resolved protocol, session, runtime, CLI, and client layers
- Produces: one marker-free, formatted, fully tested merge tree

- [ ] **Step 1: Prove all conflicts are resolved**

```bash
git diff --name-only --diff-filter=U
rg -n '^(<<<<<<<|=======|>>>>>>>)' --glob '!docs/superpowers/plans/2026-08-31-session-management-main-integration.md' .
```

Expected: both commands produce no conflict paths or markers.

- [ ] **Step 2: Format and inspect formatting changes**

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
```

Expected: formatting and whitespace checks pass.

- [ ] **Step 3: Run strict linting**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit 0 with no warnings.

- [ ] **Step 4: Run the complete test suite**

```bash
cargo test --all-targets
```

Expected: every non-ignored test passes; record the exact pass/ignore counts.

- [ ] **Step 5: Run the locked build**

```bash
cargo build --locked
```

Expected: exit 0 without changing `Cargo.lock`.

### Task 7: Review, commit, and publish the merge

**Files:**
- Stage: all reviewed merge paths

**Interfaces:**
- Consumes: fully verified merge tree
- Produces: conventional merge commit on the feature branch and updated PR branch

- [ ] **Step 1: Review the complete staged result**

```bash
git status --short
git diff --stat --cached
git diff --check --cached
git diff --name-only --diff-filter=U
```

Expected: only intended merge paths are staged, no paths are unmerged, and the plan/spec commits remain in history.

- [ ] **Step 2: Create the merge commit**

```bash
git commit -m "merge: resolve conflicts with main"
```

Expected: Git creates a two-parent merge commit whose second parent is `origin/main`.

- [ ] **Step 3: Verify commit topology and cleanliness**

```bash
git rev-list --parents -n 1 HEAD
git status --short --branch
git log -3 --oneline --decorate
```

Expected: HEAD has two parents and the worktree is clean.

- [ ] **Step 4: Push only the feature branch**

```bash
git push origin feat/session-management
```

Expected: the remote feature branch advances without force and `main` is untouched.

- [ ] **Step 5: Confirm remote mergeability when authentication permits**

```bash
gh pr view 42 --json mergeable,mergeStateStatus,statusCheckRollup,url
```

Expected: PR #42 no longer reports branch conflicts. Hosted checks may remain pending immediately after the push.
