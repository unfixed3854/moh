# Agent Skills and Project Root Discovery Design

## Goal

Moh will discover Agent Skills from a global directory and the active project,
advertise their metadata in every Codex system prompt, and let the agent load a
matching skill's full instructions only when needed. It will follow the Agent
Skills `SKILL.md` format and progressive-disclosure model.

At the same time, project-root discovery will become a shared runtime
abstraction rather than an implementation detail of `AGENTS.md` loading. The
initial policy remains Git-aware, while leaving one explicit extension point for
future non-Git and alternative-VCS project markers.

## Scope

This change adds:

- global discovery under `~/.agents/skills`;
- project discovery under `<project-root>/.agents/skills`;
- Agent Skills frontmatter parsing and validation;
- deterministic project-over-global skill precedence;
- an available-skills prompt inventory and activation guidance; and
- shared Git-based project-root discovery used by both skills and `AGENTS.md`.

It does not add automatic skill activation, a skills UI, installation commands,
script execution, resource-specific tools, a configuration format for root
markers, or non-Git markers. Moh already gives the agent an absolute-path-aware
`read` tool, which is sufficient for on-demand loading of a selected
`SKILL.md` and its bundled files.

## Project-root discovery

`runtime::project_root` will own a `ProjectRootLocator` whose single public
operation resolves a working directory to a project root. It will search the
working directory and then each ancestor from nearest to farthest, returning
the first directory that contains Git metadata named `.git`. The marker may be
either a directory (a regular repository) or a file (a linked Git worktree).
If no project marker is found, the locator returns the working directory.

The locator will own the marker-matching policy instead of exposing a generic
filesystem predicate at each call site. Its initial marker set contains only
the Git rule. This is deliberately a narrow abstraction, not a user-configured
root-detector framework: a future change can add `Cargo.toml`, `package.json`,
or another VCS marker in this one module after defining precedence when markers
are nested or disagree.

`AGENTS.md` discovery will call the locator, then collect `AGENTS.md` from the
resolved root through the working directory, preserving its current order and
global-first behavior. A non-project working directory continues to load only
its own `AGENTS.md`.

## Skill sources and precedence

`AgentConfig` will gain an optional `global_skills` path. Its default is
`~/.agents/skills`, next to the existing default global `AGENTS.md` path. For a
run, the engine derives the project skills parent as
`ProjectRootLocator::locate(cwd)/.agents/skills`.

Each source is scanned one level deep. A direct child is a candidate only when
it is a directory with a readable `SKILL.md` file; resources below that child
are not scanned at startup. Global candidates are discovered first. Project
candidates are then applied by name, replacing a global candidate of the same
name. Invalid project candidates are ignored and therefore do not mask a valid
global skill. The final inventory is sorted by name so requests are stable
across filesystem traversal orders.

No skills directory, unreadable entries, or invalid candidates are normal local
configuration conditions and will not fail an agent run. Moh will simply omit
them from the inventory.

## `SKILL.md` validation

Moh will parse only the YAML frontmatter at discovery time, using the maintained
`yaml_serde` crate rather than hand-parsing YAML. The body after the closing
frontmatter delimiter is retained only on disk and is never copied into the
startup prompt.

A discovered skill is accepted only when its frontmatter is valid YAML and
satisfies the Agent Skills specification:

- `name` and `description` are present strings;
- `name` is 1-64 lowercase ASCII letters, digits, or single hyphens; it does
  not begin or end with a hyphen and exactly matches its parent directory;
- `description` is 1-1024 characters and non-empty;
- if present, `compatibility` is a non-empty string of at most 500 characters;
- if present, `license` and `allowed-tools` are strings; and
- if present, `metadata` is a mapping of string keys to string values.

Unknown frontmatter fields remain allowed for forward-compatible client
metadata. Moh does not use `allowed-tools`, because the current tool runtime
does not have per-skill approval semantics.

## Prompt and activation flow

The existing coding system prompt remains first, followed by the literal
working directory and layered `AGENTS.md` instructions. When one or more skills
are available, Moh appends an `Available skills` section. It states that the
inventory is metadata only; when a task matches a listed description, the agent
must read that skill's listed absolute `SKILL.md` path before following it.

Each inventory entry includes exactly the validated name, description, and
absolute `SKILL.md` path. It does not include the Markdown instruction body,
scripts, references, or assets. After activation, a skill can point the agent
to its package-relative resources; the agent accesses them through the current
read, write, edit, and bash tools. This implements progressive disclosure:
metadata at startup, full instructions on activation, then resources as needed.

## Components and data flow

`ProjectRootLocator` is called with the run's working directory. Its result is
shared by the `AGENTS.md` loader and a new `SkillCatalog` discovery function.
The catalog returns small validated records containing a name, description, and
`SKILL.md` path. `RunAttempt::attempt_stream` renders those records into the
system prompt immediately before constructing the Rig agent. No harness or
provider interface changes are necessary.

```
Run context cwd
    |
    v
ProjectRootLocator ----> AGENTS.md layering
    |
    v
project .agents/skills --\
                         > SkillCatalog -> available-skills prompt inventory
global ~/.agents/skills -/
```

## Testing

Unit tests will cover:

- nearest Git root, linked-worktree `.git` files, nested working directories,
  and the working-directory fallback;
- `AGENTS.md` layering retaining its current behavior through the shared root
  locator;
- valid skill discovery, frontmatter parsing, parent-name matching, and
  alphabetical output;
- skipped missing, unreadable, malformed, and specification-invalid skills;
- project skills overriding equally named global skills without invalid project
  skills masking valid global skills; and
- project skills resolving from the shared project root, not the nested current
  directory.

A mock Codex transport integration test will assert that the system prompt
contains the available-skill name, description, and absolute `SKILL.md` path,
while it omits the skill's instruction body. It will also preserve the existing
tests for a prompt with no skills and `AGENTS.md` instructions.

The implementation must pass `cargo fmt --all`,
`cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --all-targets`, and `cargo build --locked`.

## Compatibility and future work

Existing users without either skills directory see no behavioral change beyond
the shared root-lookup implementation. Existing `AGENTS.md` path order and the
global `~/.agents/AGENTS.md` default remain intact. Agent Skills are read-only
discovery metadata in this milestone; their optional `allowed-tools` field is
intentionally not enforced.

Future root-marker work will extend `ProjectRootLocator` after deciding how
Git, language manifests, and alternative VCS markers interact in nested
projects. Future skill work may add installation, validation reporting, an
inventory UI, or an activation helper, but must keep full skill bodies out of
the startup prompt.
