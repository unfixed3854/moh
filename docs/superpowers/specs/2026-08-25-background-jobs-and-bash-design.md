# Background Jobs and Bash Design

## Goal

Add a non-interactive `bash` tool that supports foreground and background
execution, backed by a reusable process-local job subsystem. Bash is the first
job producer; later producers such as subagent runs and monitors can share job
identity, status, waiting, cancellation, retention, and shutdown behavior
without sharing Bash-specific controls or result types.

## Context

Moh currently exposes async `read`, `write`, and `edit` tools through thin Rig
adapters. Each agent run is bound to a working directory, only one run is
active at a time, and dropping the run stream cancels its in-flight tool loop.
There is no command-execution tool and no owner for work that intentionally
outlives the tool call that started it.

Established agent harnesses suggest sharing the lifecycle substrate without
forcing every producer into one generic tool. Claude Code gives background
shell commands, monitors, and subagents common task identity and termination;
OpenAI keeps shell and agent-specific operations separate; OpenCode has a
typed background-job registry beneath its experimental background subagents;
and Pi keeps its foreground Bash tool and extension-provided subagents
separate. Across these designs, completion-driven waiting is preferred to
repeated polling, output is bounded before it enters model context, and
producer-specific interaction remains producer-specific.

## Design principles

- A job represents one execution, not the durable identity of its producer.
  A future subagent may retain an `agent_id` across several turns, while each
  active subagent turn has its own `job_id`.
- Common lifecycle behavior is generic. Starting work and interacting with a
  running producer remain type-specific.
- Foreground and background execution use the same job machinery. The only
  difference is whether the starting tool waits for the terminal snapshot.
- Waiting is notification-driven inside the runtime. A non-blocking status
  lookup exists for inspection, not as an invitation to busy-poll.
- Command failure is data. A nonzero exit status is a completed Bash job, not
  a tool-infrastructure failure.

## Architecture

### Job registry

`JobRegistry` is shared by the run engine and all job-producing service
factories. It is process-local and lives for the engine's lifetime, so jobs and
their terminal results remain visible across prompt turns but do not survive
application restart.

Each registry entry has:

- an opaque, monotonically assigned `job_id`;
- a typed `kind`, initially `bash`;
- `running`, `completed`, `failed`, or `cancelled` state;
- creation and optional completion timestamps;
- a producer-owned typed result or failure summary;
- a cancellation handle;
- an async change notification used by waiters.

The generic registry never interprets Bash exit codes, output streams, or
future subagent messages. Producers translate their result into a typed job
payload, and the corresponding model-facing adapter renders it.

The registry enforces two fixed safety bounds in the first version:

- at most 16 simultaneously running jobs;
- at most 64 retained terminal jobs, evicting the oldest terminal entry when
  another terminal job is retained.

Running jobs are never evicted. These constants are internal and can become
configuration later if real usage requires it.

### Bash producer

`BashServiceFactory` owns a clone of the shared registry. `for_cwd` binds a
service to the `RunContext` working directory, matching the existing file-tool
pattern.

The service starts `bash -lc <command>` with that working directory, inherited
environment, null stdin, and piped stdout and stderr. Execution is
non-interactive: PTYs, prompts, and stdin writes are not supported.

Stdout and stderr are drained concurrently so a child cannot block on a full
pipe. The service retains bounded tail output for model-facing results and
spools the full output to a private, application-owned temporary file. The
combined log records chunks in the order Moh observes them and labels their
source; exact ordering between separate operating-system stdout and stderr
pipes is not guaranteed. Invalid UTF-8 is decoded lossily for display and the
log.

Model-facing Bash output is capped at 50 KiB or 2,000 lines, whichever is
reached first. A truncated result names the full log path so the existing
`read` tool can inspect it selectively. Log files are
created with user-only permissions where the platform supports them and are
removed when their retained job entry is evicted or the registry shuts down.

On Unix, Bash starts in its own process group so timeout, cancellation, and
shutdown terminate descendants as well as the shell. Other platforms receive
best-effort child termination until equivalent process-tree support is added.

### Foreground and background behavior

The `bash` arguments are strict and contain:

- required `command: string`;
- optional `background: boolean`, defaulting to `false`;
- optional positive `timeout_ms`, with no timeout when omitted and a maximum
  accepted value of one hour.

Every invocation creates a job before starting the child process.

In foreground mode, `bash` waits for the job's terminal snapshot and returns
the job ID, terminal state, exit status when available, and bounded output.
Cancelling the originating agent run cancels the foreground job and its process
tree.

In background mode, `bash` returns the job ID and initial running snapshot as
soon as the child has started. The job intentionally survives cancellation or
completion of the originating agent run and remains owned by the engine.
Startup failure is reported synchronously instead of returning a job that
never ran. It records a failed terminal snapshot and returns a model-visible
error containing that job ID.

When `timeout_ms` elapses, the supervisor terminates the process tree, drains
remaining output, and records a failed terminal job with a timeout reason.
Explicit cancellation records `cancelled`. A normal child exit, including a
nonzero exit code, records `completed` with its exit status.

### Generic lifecycle tools

Three strict tools expose common lifecycle operations:

- `job_status` accepts an optional `job_id`. With an ID it returns that job's
  current snapshot and producer-rendered details. Without an ID it lists
  compact summaries for all retained jobs.
- `job_wait` accepts one or more `job_ids` and an optional `timeout_ms`. It
  returns immediately when any requested job is already terminal; otherwise
  it waits until one changes to a terminal state or the timeout expires. The
  default wait is 30 seconds and the maximum is five minutes.
- `job_cancel` accepts one `job_id`. Cancellation is idempotent: a terminal job
  returns its existing snapshot, while a running job is asked to stop and the
  tool waits for the terminal snapshot.

Unknown or evicted IDs produce a model-visible not-found error. Waiting on an
empty list or supplying an invalid timeout is rejected before registry access.

There is no separate polling tool. `job_status` is the non-blocking snapshot
operation; `job_wait` is the efficient coordination operation.

Future producers register the same lifecycle tools but keep their own
interfaces. For example, shell stdin would belong to a future Bash-specific
tool, while agent messages and follow-up turns would use agent-specific tools
and `agent_id`, not `job_cancel` arguments overloaded with message data.

### Rig runtime integration

The run engine constructs one registry, shares it with `BashServiceFactory`,
and creates cwd-bound Bash adapters for each run. It exposes a cloneable
shutdown handle before the engine is moved into `Harness`; `main` retains that
handle and awaits registry shutdown after `app::run` returns and before the
Tokio runtime is torn down. The four new Rig adapters (`bash` plus the three
lifecycle tools) are registered alongside `read`, `edit`, and `write`.

Ordinary job and command errors remain model-visible so the agent can inspect,
wait, cancel, or recover. A poisoned or unavailable registry is a runtime
failure with its own stable Rig error code and stops the tool loop through the
existing runtime hook.

The existing `ToolStarted` and `ToolFinished` events are sufficient for the
initial TUI. Starting a background command completes the `bash` tool call; a
later status, wait, or cancel operation appears as its own tool call.

## Data flow

```text
bash tool
  |
  v
cwd-bound BashService --> JobRegistry::start(kind = bash)
                              |
                              v
                         Bash supervisor
                    spawn / drain / timeout / kill
                              |
                              v
                   terminal typed Bash result

foreground: bash awaits terminal snapshot --> model
background: bash returns running snapshot --> model continues
                                            |
                         job_status / job_wait / job_cancel
                                            |
                                            v
                                      shared registry
```

The registry's change notification wakes `job_wait`; waiting never loops on a
timer. Several concurrent waiters may observe the same terminal snapshot.

## Errors, cancellation, and shutdown

The model-visible results and errors distinguish:

- invalid arguments;
- unknown or evicted job IDs;
- active-job capacity exhaustion;
- Bash spawn failure;
- job timeout or cancellation recorded in a terminal snapshot;
- runtime registry or supervisor failure.

Nonzero command exit is not an error category. Its stdout, stderr, and exit
status are returned as a normal completed result.

Dropping a foreground `bash` future requests cancellation before it releases
its job handle. Dropping a `job_wait` future only stops that waiter. Dropping a
background-starting tool future after the child has started does not cancel the
job.

Application shutdown requests cancellation for every running job, allows a
two-second grace period for supervisors to drain and reap their children, then
force-kills remaining process trees before the Tokio runtime is torn down.
`job_cancel` uses the same bounded terminate-then-kill path. This cleanup joins
the existing application shutdown path; it does not rely only on `Child::drop`.

## Completion delivery

The first version exposes completion through `job_wait` and `job_status`. It
does not inject unsolicited model messages after a tool call or wake an idle
conversation when a job completes. The registry's notification boundary is
deliberately reusable by a later host-level completion event feature, which can
surface job completion in the TUI or inject it into a subsequent model turn
without changing producer implementations.

This keeps issue #2 focused while avoiding a polling-only internal design.

## Testing and validation

Registry tests cover:

- monotonic IDs and typed snapshots;
- running-to-terminal transitions;
- notification-driven waits, immediate terminal waits, and wait timeout;
- idempotent cancellation;
- active-job capacity and terminal-retention eviction;
- concurrent status and wait access;
- shutdown cancellation.

Bash service tests cover:

- cwd-relative execution and inherited environment;
- stdout, stderr, zero exit, and nonzero exit;
- background start returning before process completion;
- status and wait observing partial and terminal output;
- explicit timeout and cancellation of the process tree;
- foreground run cancellation versus background survival;
- large-output draining, truncation, and full-log access;
- spawn and invalid-argument failures;
- non-stalling execution on the current-thread Tokio runtime.

Rig runtime tests verify the four new tool schemas, tool registration, tool
events, foreground continuation, background start followed by wait, ordinary
model-visible errors, and fatal registry-error projection.

Run the repository's standard gates after implementation:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

## Non-goals

- Implementing subagents, monitors, job dependencies, retries, or scheduling.
- Treating a future durable `agent_id` as a `job_id`.
- Persisting or recovering jobs across application restarts.
- Injecting unsolicited completion messages or waking idle conversations.
- PTY allocation, interactive programs, stdin forwarding, or terminal resize.
- User-driven promotion of an already-running foreground command to the
  background.
- Shell sandboxing, command approval policy, or remote execution.
- User-configurable concurrency, retention, timeout, or output limits.
