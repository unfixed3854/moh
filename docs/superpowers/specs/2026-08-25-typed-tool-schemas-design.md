# Typed Tool Schemas Design

## Goal

Replace Moh's hand-written JSON Schema values for model-visible tool arguments
with schemas derived from the Rust argument types. The same declarations should
also validate structural argument constraints at runtime, so a new field or
constraint cannot silently make the advertised schema and accepted input drift
apart.

## Scope

This refactor applies to the current seven Rig-exposed tools:

- `read`;
- `write`;
- `edit`;
- `bash`;
- `job_status`;
- `job_wait`; and
- `job_cancel`.

It does not change tool names, descriptions, domain behavior, filesystem
safety checks, job lifecycle semantics, or model-visible error codes. It also
does not add a generic tool-registration abstraction; the existing thin Rig
adapters remain the registration boundary.

## Selected approach

Moh will use two direct dependencies:

- `schemars` derives the JSON Schema supplied to Rig; and
- `garde` derives structural validation on the same argument types.

Each argument type derives `Deserialize`, `JsonSchema`, and `Validate`, along
with existing useful derives such as `Debug`. Existing Rust doc comments stay
on fields so Schemars uses them as model-facing descriptions. The Rig adapters
serialize `schemars::schema_for!(Args)` to the `serde_json::Value` required by
Rig's `PortableTool` trait.

This removes the manually assembled `serde_json::json!` schema values. It also
makes the validator attributes the single declaration of constraints that the
model receives in schema form and Moh enforces after parsing.

## Tool contracts

All argument structs keep `#[serde(deny_unknown_fields)]`, preserving strict
rejection of unknown properties. Required non-`Option` fields and optional
`Option` fields are inferred by Serde and Schemars. `#[serde(default)]` on
`bash.background` continues to supply `false` when the model omits it.

### Read

`read` accepts one required, non-empty `path` string and optional positive
`offset` and `limit` integers. The legacy `file_path` alias is removed.

This intentionally simplifies the public tool contract. It eliminates the
current custom `Deserialize` implementation, its raw intermediate struct, and
the manually authored `oneOf` schema. An old model transcript that calls
`file_path` receives the existing strict unknown-field failure and can retry
with `path`; current prompts and tool descriptions refer to `path` already.

### Write and edit

`write.path` is a non-empty string. `write.content` remains any string,
including an empty string.

`edit.path` is non-empty. `remove_from` and `remove_to` each require exactly
three ASCII alphanumeric characters. Every `replacement_lines` element must
exclude carriage-return and line-feed characters. The ordering check
(`remove_from` must not follow `remove_to`) remains in `EditService`, because
it depends on the observed file snapshot rather than a property of one field.

### Bash and jobs

`bash.command` is non-empty; `timeout_ms`, when present, is between 1 and
3,600,000 inclusive. `background` remains optional with a false default.

`job_wait.job_ids` requires at least one item and its optional `timeout_ms` is
between 0 and 300,000 inclusive; zero is the explicit immediate-timeout
value. `job_status.job_id` remains optional and
`job_cancel.job_id` remains required. Canonical `job-N` format and target
existence checks remain in `JobService`, because they require parsing or
registry state beyond structural payload validation.

## Validation flow and errors

Rig continues to deserialize the model's JSON into each tool's typed `Args`.
Each adapter calls the derived Garde validator before entering the service and
maps a validation failure to the existing model-visible invalid-argument path.
Service methods retain only checks that depend on runtime state, relationships
between observed resources, or values parsed from accepted strings.

This preserves the distinction between a recoverable bad tool call and an
operational runtime failure. Validation failures do not gain new error codes;
they remain ordinary model-visible argument errors that let the agent retry.

## Schema generation boundary

The adapter's `parameters()` method remains because Rig currently requires it,
but it delegates generically to the tool's `Args` type rather than a service
method. Service-level `parameters`, `status_parameters`, `wait_parameters`,
and `cancel_parameters` methods are deleted.

Schemas are regenerated for each tool registration, as they are small static
values and registration happens once per run. No schema cache or new shared
trait is introduced in this change.

## Testing

Tests will assert externally important generated-schema behavior rather than
the full derived JSON representation:

- every tool schema rejects additional properties;
- required fields, descriptions, defaulted `background`, numeric bounds, and
  non-empty `job_ids` appear as expected;
- `read` exposes required `path` and no longer exposes `file_path` or a
  `oneOf` alias contract;
- invalid typed tool calls are rejected by the derived validator;
- existing service tests still cover domain-dependent validation and safety
  behavior; and
- runtime request tests verify that the Codex payload contains all seven
  generated schemas and the updated read contract.

The implementation must pass `cargo fmt --all`,
`cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --all-targets`, and `cargo build --locked`.

## Compatibility and future work

Removing `read.file_path` is the only intentional public-input simplification.
It is documented in the tool description and covered by tests. If future
compatibility pressure requires aliases or cross-field alternatives, Moh should
model them as typed enum variants and derive their schema; it should not
reintroduce manually assembled JSON Schema.

The current refactor does not attempt to convert every domain invariant into a
validator annotation. Runtime state and semantic invariants stay close to the
services that own the required state.
