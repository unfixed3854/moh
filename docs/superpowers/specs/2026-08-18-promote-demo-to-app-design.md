# Promote the Demo Module to the Application Module

## Goal

Rename the executable product layer from `demo` to `app`. The module now owns
the real `moh` application shell, event loop, conversation integration, and
terminal lifecycle, so demo terminology no longer describes its role.

This is a behavior-preserving naming change. It does not reorganize the
application, alter public APIs, or change runtime behavior.

## Design

Rename `src/demo.rs` to `src/app.rs` and update the binary entry point to load
and run `app::run`. Rename application-owned identifiers consistently:

- `DemoIds` becomes `AppIds`;
- `DemoAction` becomes `AppAction`;
- `DemoError` becomes `AppError`.

Update current source comments, test names and diagnostics, crate-level
documentation, and README wording so they describe the application rather than
a demo. Preserve the existing controls, output, error messages, and execution
flow.

Historical design specs and implementation plans remain unchanged. Their use of
"demo" accurately records the state and terminology at the time they were
written.

## Boundaries

The reusable `moh::tui` library remains separate from the binary-only
application module. This change does not move application code into the library
or expose the app module publicly.

Although `src/app.rs` is large, splitting it into submodules is outside this
change. That refactor should be considered separately when a concrete ownership
boundary warrants it.

## Verification

Verification will cover:

1. focused binary application tests;
2. the complete test suite;
3. formatting checks;
4. `cargo clippy --all-targets --all-features -- -D warnings`.

No live Codex request is required because the change is strictly structural and
behavior preserving.
