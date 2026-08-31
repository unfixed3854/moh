# Session Management and Main Integration Design

## Goal

Resolve the divergence between `feat/session-management` and `main` without
discarding either branch's user-facing behavior, protocol guarantees, or test
coverage. The result must be a mergeable continuation of PR #42 rather than a
rewritten replacement for its reviewed history.

## Integration Strategy

Merge `origin/main` into `feat/session-management` with a merge commit. Do not
rebase or squash the feature branch: its thirty feature commits are already
published and reviewed, while a rebase would require a force-push and would
repeat conflict resolution across individual commits.

Treat `main` as authoritative for functionality merged after the feature
branch diverged:

- the Clap command-line parser and its exact public forms;
- execution-plan persistence, actor ownership, RPC projection, and TUI;
- multiline prompt editing and responsive welcome/help layout;
- the todo sidebar, contextual `/exit` alias, socket identity hardening, and
  Rust-aware CI caching.

Preserve the session-management branch's authoritative contracts:

- `moh --new` opens an unnamed ephemeral draft;
- `moh --resume <SELECTOR>` resumes by stable ID or title;
- `moh sessions` lists sessions for the current project;
- drafts materialize only on the first nonblank prompt;
- switching detaches without cancelling work;
- session actors exclusively own live lifecycle state;
- rename, deletion, fallback selection, and local/global browsing retain their
  existing semantics;
- failed, cancelled, and interrupted visible turns persist, while only
  successful exchanges enter model context;
- attachment counts come from the actor's authoritative detach response.

## Protocol and State Reconciliation

Cap'n Proto changes from both branches remain additive. Existing ordinals keep
their meanings. Session-management operations and fields retain their current
ordinals, and the execution-plan snapshot field remains `plan @9`. Checked-in
Rust bindings are regenerated from the reconciled schema instead of resolving
generated-code conflicts by hand.

The session actor and persisted projection carry both lifecycle data and the
execution plan. Plan updates remain actor-owned and ordered with other
projection changes. Drafts have no actor or plan; a plan exists only after
materialization. Store migrations and checkpoint code preserve session titles,
visible transcript state, successful model history, and plan state together.

## CLI and TUI Reconciliation

Keep Clap as the parser implementation while adapting it to the approved
session interface. The accepted forms are exactly:

```text
moh
moh --new
moh --resume <SELECTOR>
moh sessions
moh server
moh server --internal-detached
```

The Ratatui application composes the multiline editor, responsive welcome and
help content, todo sidebar, and session browser. Modal/session-browser input
takes precedence over editor input. The editor retains grapheme-safe multiline
cursor behavior, and the sidebar must not obscure the prompt or browser.
Canonical `/quit` remains visible by default; `/exit` appears only for an
`/e...` prefix.

## Conflict Resolution and Error Handling

Resolve shared source files by behavior, not by selecting a complete side.
Lifecycle-action errors stay separate from transient browser refresh warnings.
Plan failures remain session-scoped. Storage, protocol, and runtime failures
continue to use typed errors at their existing boundaries.

Any mismatch discovered between the two feature designs is resolved in favor
of the stricter invariant when the behaviors can coexist. If an actual product
choice is mutually exclusive, stop and request a decision instead of silently
changing either published contract.

## Verification

Conflict resolution must include focused tests for the combined seams:

- Clap parsing for every accepted session-management form and rejected legacy
  or conflicting forms;
- schema ordinals and protocol feature negotiation;
- session actor/store round trips that include plan state;
- draft materialization, detach counts, deletion fallback, and launch modes;
- rendered multiline editor, help, sidebar, session browser, narrow overlay,
  and cursor geometry states.

After focused tests pass, run the complete repository gate:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
git diff --check
```

The merge commit uses the conventional subject
`merge: resolve conflicts with main`. Push only the resolved feature branch;
do not modify or push `main`.
