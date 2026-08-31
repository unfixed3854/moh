# Agent Skills and Project Root Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Discover valid global and project Agent Skills, advertise their metadata in Moh's system prompt, and share project-root detection with `AGENTS.md` layering.

**Architecture:** `runtime::project_root` resolves the active project boundary from the nearest Git marker and is the sole owner of that policy. `runtime::skills` scans global and project skill directories, validates only YAML frontmatter, resolves project-over-global collisions, and returns a sorted metadata catalog. `runtime::rig::codex` uses both components while assembling the existing per-run system prompt; full skill bodies stay on disk until the model elects to read one.

**Tech Stack:** Rust 2024, Rust standard filesystem APIs, Serde 1.0, `yaml_serde` 0.10.7, Tokio 1.53, Rig 0.41, tempfile 3.27, wiremock 0.6.

**Spec:** `docs/superpowers/specs/2026-08-26-agent-skills-design.md`

## Global Constraints

- Discover global skills only in `~/.agents/skills` and project skills only in `<project-root>/.agents/skills`.
- Resolve the project root as the nearest ancestor with a `.git` entry, accepting both directory and file forms; use the working directory when no such entry exists.
- Keep root detection behind `ProjectRootLocator`; do not add non-Git markers or a user-configured detector framework in this change.
- Scan direct child skill directories only. A candidate requires a readable `SKILL.md`; never scan resources at startup.
- Parse YAML frontmatter only. Validated startup metadata is `name`, `description`, and the absolute `SKILL.md` path; do not add the Markdown instruction body to the system prompt.
- Enforce the Agent Skills `name`, `description`, optional `compatibility`, `license`, `metadata`, and `allowed-tools` contracts defined by the spec. Allow unknown frontmatter fields.
- Apply global candidates first, then replace same-named entries with valid project candidates; sort the final inventory by name.
- Missing, unreadable, malformed, and invalid skill candidates must be silently omitted and must not fail a run or hide a valid global skill.
- Keep existing `AGENTS.md` global-first order and behavior. Use the shared locator for its project boundary.
- Keep `allowed-tools` informational; Moh has no per-skill tool-approval enforcement in this change.
- Preserve all harness, provider, tool, authentication, and TUI behavior not named here.
- Every new public item needs rustdoc because the crate enables `#![warn(missing_docs)]`.
- Use conventional commits and stage only files named by the current task.
- Complete validation with `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `cargo build --locked`.

---

## File Structure

- `src/runtime/project_root.rs` — canonical project-root location policy and focused unit tests.
- `src/runtime/skills.rs` — Agent Skills frontmatter parsing, validation, source discovery, precedence, and prompt rendering tests.
- `src/runtime/mod.rs` — exposes the two runtime-internal modules.
- `src/runtime/rig/codex.rs` — resolves a root once per run, retains `AGENTS.md` order through that root, configures global skills, and appends the prompt inventory.
- `tests/rig_runtime.rs` — mocked Codex request assertions for metadata-only inventory and shared project root behavior.
- `Cargo.toml` and `Cargo.lock` — add the maintained YAML parser.
- `README.md` — documents Agent Skills source directories, precedence, and progressive activation.

### Task 1: Centralize Git project-root location

**Files:**

- Create: `src/runtime/project_root.rs`
- Modify: `src/runtime/mod.rs:1-4`
- Modify: `src/runtime/rig/codex.rs:1-90, 540-550, 836-905`

**Interfaces:**

- Produces `crate::runtime::project_root::ProjectRootLocator`.
- `ProjectRootLocator::locate(&self, cwd: &Path) -> PathBuf` returns the nearest ancestor containing `.git`, including a linked-worktree `.git` file, or returns `cwd.to_owned()`.
- `agents_md_instructions_from(cwd: &Path, project_root: &Path, global_agents_md: Option<&Path>) -> String` consumes the resolved boundary and retains global-first, root-to-cwd ordering.

- [ ] **Step 1: Write failing locator and shared-boundary unit tests**

Add `pub(crate) mod project_root;` to `src/runtime/mod.rs`, then create `src/runtime/project_root.rs` with a `#[cfg(test)]` module that imports the not-yet-defined `ProjectRootLocator`. Exercise a regular repository, a nested directory, a `.git` file, and a non-project directory. Update the existing `codex.rs` unit tests to pass a root explicitly and add a regression that a `parent/AGENTS.md` above the resolved root is not read.

```rust
#[test]
fn locates_the_nearest_git_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("repository");
    let nested = root.join("crates").join("cli");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();

    assert_eq!(ProjectRootLocator::default().locate(&nested), root);
}

#[test]
fn accepts_a_git_worktree_marker_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("worktree");
    let nested = root.join("src");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(root.join(".git"), "gitdir: /elsewhere/worktrees/moh").unwrap();

    assert_eq!(ProjectRootLocator::default().locate(&nested), root);
}

#[test]
fn uses_the_working_directory_when_no_marker_exists() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("plain").join("nested");
    std::fs::create_dir_all(&cwd).unwrap();

    assert_eq!(ProjectRootLocator::default().locate(&cwd), cwd);
}
```

- [ ] **Step 2: Run the affected unit tests and confirm the new API is absent**

Run: `cargo test runtime::project_root --lib`

Expected: FAIL because `ProjectRootLocator` and the `runtime::project_root` module do not exist.

- [ ] **Step 3: Implement the narrow root-locator module**

Create `src/runtime/project_root.rs` and expose it to sibling runtime modules with `pub(crate) mod project_root;` in `src/runtime/mod.rs`. Keep the marker rule in this module, represented by a private `ProjectRootMarker::Git` variant, so later language-manifest or alternative-VCS rules have one deliberate extension point rather than duplicated `cwd.ancestors()` loops.

```rust
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProjectRootLocator;

#[derive(Clone, Copy, Debug)]
enum ProjectRootMarker {
    Git,
}

impl ProjectRootLocator {
    pub(crate) fn locate(&self, cwd: &Path) -> PathBuf {
        cwd.ancestors()
            .find(|directory| ProjectRootMarker::Git.matches(directory))
            .map(Path::to_owned)
            .unwrap_or_else(|| cwd.to_owned())
    }
}

impl ProjectRootMarker {
    fn matches(self, directory: &Path) -> bool {
        match self {
            Self::Git => directory.join(".git").exists(),
        }
    }
}
```

Use `Path::exists`, rather than `is_dir`, because Git linked worktrees use a `.git` file. The implementation must not canonicalize the working directory or follow an ancestor beyond the first matching marker.

- [ ] **Step 4: Route `AGENTS.md` through the resolved root**

In `src/runtime/rig/codex.rs`, replace the private root-search block in `agents_md_instructions_from` with the supplied `project_root`. Build the layered path vector from `project_root` through `cwd`, after the optional global path, exactly as the current function does. In `RunAttempt::attempt_stream`, resolve once before assembling instructions and pass that same `PathBuf` into `agents_md_instructions_from`.

```rust
let project_root = ProjectRootLocator::default().locate(&self.request.context.cwd);
let agents_instructions = agents_md_instructions_from(
    &self.request.context.cwd,
    &project_root,
    self.agent.global_agents_md.as_deref(),
);
```

Update every existing unit-test call to pass `ProjectRootLocator::default().locate(cwd)` so the tests preserve their former observable order while exercising the shared boundary.

- [ ] **Step 5: Run root and instruction-layering tests**

Run: `cargo test project_root --lib && cargo test agents_md --lib`

Expected: PASS. The regular-repository, worktree-file, nested-path, non-project fallback, global-first, and root-to-cwd layering assertions all pass.

- [ ] **Step 6: Commit the independently testable root-locator change**

```bash
git add src/runtime/project_root.rs src/runtime/mod.rs src/runtime/rig/codex.rs
git commit -m "refactor(runtime): centralize project root discovery"
```

### Task 2: Discover and validate Agent Skills metadata

**Files:**

- Create: `src/runtime/skills.rs`
- Modify: `src/runtime/mod.rs:1-5`
- Modify: `Cargo.toml:6-28`
- Modify: `Cargo.lock`

**Interfaces:**

- Produces `crate::runtime::skills::SkillMetadata { name: String, description: String, instructions_path: PathBuf }` with `Clone`, `Debug`, `Eq`, and `PartialEq`.
- Produces `crate::runtime::skills::SkillCatalog`.
- `SkillCatalog::discover(global_skills: Option<&Path>, project_root: &Path) -> SkillCatalog` reads `global_skills` then `project_root/.agents/skills`.
- `SkillCatalog::entries(&self) -> &[SkillMetadata]` returns entries sorted by name.
- `SkillCatalog::prompt_section(&self) -> Option<String>` renders only activation guidance and metadata.

- [ ] **Step 1: Write failing catalog tests for validation, precedence, and disclosure**

Add `pub(crate) mod skills;` to `src/runtime/mod.rs`, then create `src/runtime/skills.rs` with its `#[cfg(test)]` module. Add this fixture helper, then assert valid metadata, sorted output, global/project overrides, malformed YAML, mismatched directory names, invalid names, empty or overlong descriptions, invalid optional fields, missing files, and a project skill that is invalid while its global counterpart is valid.

```rust
fn write_skill(source: &std::path::Path, directory_name: &str, frontmatter: &str, body: &str) {
    let skill = source.join(directory_name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n{body}\n"),
    )
    .unwrap();
}
```

```rust
#[test]
fn project_skills_replace_global_skills_and_output_is_sorted() {
    let directory = tempfile::tempdir().unwrap();
    let global = directory.path().join("global");
    let project = directory.path().join("project");
    write_skill(&global, "pdf", "name: pdf\ndescription: Global PDF help", "global body");
    write_skill(&global, "code-review", "name: code-review\ndescription: Review code", "review body");
    write_skill(
        &project.join(".agents/skills"),
        "pdf",
        "name: pdf\ndescription: Project PDF help",
        "project body",
    );

    let catalog = SkillCatalog::discover(Some(&global), &project);
    let names: Vec<_> = catalog.entries().iter().map(|skill| skill.name.as_str()).collect();

    assert_eq!(names, ["code-review", "pdf"]);
    assert_eq!(catalog.entries()[1].description, "Project PDF help");
    assert!(catalog.entries()[1].instructions_path.is_absolute());
    assert!(catalog.prompt_section().unwrap().contains("SKILL.md"));
    assert!(!catalog.prompt_section().unwrap().contains("project body"));
}
```

Add exact boundary assertions: nested files under `references/` must not appear as candidates; a valid global `pdf` remains after a malformed project `pdf`; an empty catalog returns `None` from `prompt_section`; and every rendered entry includes `instructions_path.display()`.

- [ ] **Step 2: Run the catalog tests and confirm discovery has not been implemented**

Run: `cargo test runtime::skills --lib`

Expected: FAIL because the `runtime::skills` module, `SkillCatalog`, and `SkillMetadata` do not exist.

- [ ] **Step 3: Add YAML parsing and metadata types**

Add `yaml_serde = "0.10.7"` to `[dependencies]` in `Cargo.toml`; let Cargo refresh `Cargo.lock` during the first test run. Declare the runtime-internal module with `pub(crate) mod skills;`.

Implement a frontmatter-only parser. It must require the first line to be `---`, collect lines until the next standalone `---` line (accepting LF and CRLF), and deserialize only that collected YAML into this typed representation. Return `None` for an absent closing delimiter, YAML parse failure, or any failed validation; do not inspect the Markdown body.

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) instructions_path: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SkillCatalog {
    entries: Vec<SkillMetadata>,
}

#[derive(serde::Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<std::collections::BTreeMap<String, String>>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
}
```

Validate `name` with `^[a-z0-9]+(?:-[a-z0-9]+)*$`, `name.chars().count() <= 64`, and an exact parent-directory match. Validate `description` as 1 through 1024 characters. Reject empty or longer-than-500 `compatibility`; accept optional `license` and `allowed_tools` only as strings, which Serde already enforces. Deserializing `metadata` as `BTreeMap<String, String>` enforces its string-to-string contract. Do not add `deny_unknown_fields`, because unknown top-level frontmatter remains forward compatible.

- [ ] **Step 4: Implement source scanning, merge policy, and prompt rendering**

Use `fs::read_dir` and `filter_map(Result::ok)` so missing or unreadable source directories are empty sources. For each direct child directory, read only `child/SKILL.md`; successful parsing produces one `SkillMetadata`. Canonicalize the readable `SKILL.md` before storing it and omit that candidate if canonicalization fails, ensuring every advertised path is absolute. Insert global skills into `BTreeMap<String, SkillMetadata>`, then project skills into the same map, and collect its values for deterministic sorting and replacement behavior.

Render an inventory only when the catalog is non-empty. The text must explicitly tell the model to use the existing `read` tool to load the listed full `SKILL.md` when the task matches its description, before applying that skill's instructions. Render the absolute path with debug formatting so paths containing whitespace or control characters remain literal data.

```rust
pub(crate) fn prompt_section(&self) -> Option<String> {
    (!self.entries.is_empty()).then(|| {
        let mut prompt = String::from(
            "Available skills:\nThese entries are metadata only. When a task matches a skill description, use the read tool to load that skill's full SKILL.md before following its instructions.\n",
        );
        for skill in &self.entries {
            prompt.push_str(&format!(
                "- {}: {}\n  SKILL.md (literal path): {:?}\n",
                skill.name, skill.description, skill.instructions_path
            ));
        }
        prompt.trim_end().to_owned()
    })
}
```

- [ ] **Step 5: Run discovery tests and lint the new module**

Run: `cargo fmt --all && cargo test runtime::skills --lib && cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS. The catalog accepts valid YAML metadata, skips every invalid fixture, retains valid global skills when a project override is invalid, orders entries by name, and never renders a body or nested resource.

- [ ] **Step 6: Commit the catalog as a self-contained runtime component**

```bash
git add Cargo.toml Cargo.lock src/runtime/mod.rs src/runtime/skills.rs
git commit -m "feat(runtime): discover agent skill metadata"
```

### Task 3: Add the inventory to Codex runs and document activation

**Files:**

- Modify: `src/runtime/rig/codex.rs:140-235, 535-565, 836-905`
- Modify: `tests/rig_runtime.rs:52-82, 374-510`
- Modify: `README.md:96-132`

**Interfaces:**

- Extends `AgentConfig` with `global_skills: Option<PathBuf>`; `Default` supplies `UserDirs::home_dir().join(".agents/skills")`.
- `RunAttempt::attempt_stream` calls `SkillCatalog::discover(self.agent.global_skills.as_deref(), &project_root)` and appends `catalog.prompt_section()` after the existing `AGENTS.md` section.
- Existing test constructors set `global_skills: None` unless a test deliberately supplies a fixture source.

- [ ] **Step 1: Write failing mocked-request tests for metadata-only prompt injection**

In `tests/rig_runtime.rs`, factor a helper that accepts an `AgentConfig` so a test can pass fixture global-skill and global-`AGENTS.md` paths while other tests keep both `None`. Add this local fixture helper and a mock request test using a Git-root project, a nested working directory, one global skill, and an overriding project skill with a distinctive body.

```rust
fn write_skill(source: &std::path::Path, directory_name: &str, frontmatter: &str, body: &str) {
    let skill = source.join(directory_name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n{body}\n"),
    )
    .unwrap();
}

fn test_agent_config() -> AgentConfig {
    AgentConfig {
        model: "gpt-5.6-luna".into(),
        reasoning: ReasoningLevel::Medium,
        max_model_calls: AgentConfig::default().max_model_calls,
        global_agents_md: None,
        global_skills: None,
    }
}

fn test_engine_with_agent_config(
    directory: &TempDir,
    auth: AuthFile,
    config: CodexConfig,
    agent: AgentConfig,
) -> CodexRunEngine {
    CodexRunEngine::new(
        CodexModelFactory::new(auth, config),
        agent,
        ReadServiceFactory::new(ReadConfig::at(directory.path().join("hash-store.sqlite"))),
    )
    .unwrap()
}
```

```rust
#[tokio::test]
async fn codex_request_lists_project_skills_without_loading_their_bodies() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(success_sse("ready to code"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (_auth_directory, auth) = synthetic_auth_file().await;
    let config = CodexConfig {
        api_base: server.uri(),
        refresh_url: format!("{}/oauth/token", server.uri()),
    };
    let directory = tempdir().unwrap();
    let project = directory.path().join("project");
    let nested = project.join("crates").join("cli");
    let global_skills = directory.path().join("global-skills");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    write_skill(&global_skills, "release", "name: release\ndescription: Global release", "global-only body");
    write_skill(
        &project.join(".agents/skills"),
        "release",
        "name: release\ndescription: Prepare project releases",
        "DO NOT PUT THIS BODY IN THE STARTUP PROMPT",
    );

    let engine = test_engine_with_agent_config(
        &directory,
        auth,
        config,
        AgentConfig { global_skills: Some(global_skills), ..test_agent_config() },
    );
    let chunks = run(&engine, run_request(&nested, "prepare a release")).await;
    assert!(matches!(&chunks[2], Ok(EngineEvent::Completed(text)) if text == "ready to code"));
    let request = server.received_requests().await.unwrap().pop().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    let instructions = body["instructions"].as_str().unwrap();
    assert!(instructions.contains("Available skills:"));
    assert!(instructions.contains("Prepare project releases"));
    assert!(instructions.contains(&project.join(".agents/skills/release/SKILL.md").display().to_string()));
    assert!(!instructions.contains("Global release"));
    assert!(!instructions.contains("DO NOT PUT THIS BODY IN THE STARTUP PROMPT"));
}
```

Also add a no-skills assertion to `codex_request_includes_coding_system_prompt_and_working_directory`: it must not contain `Available skills:`. Keep the existing AGENTS request test and update it to verify its text occurs before the skill inventory when both sources are configured.

- [ ] **Step 2: Run the integration target and confirm configuration support is absent**

Run: `cargo test --test rig_runtime codex_request_lists_project_skills_without_loading_their_bodies -- --exact`

Expected: FAIL because `AgentConfig` has no `global_skills` field and `RunAttempt` does not render a skill catalog.

- [ ] **Step 3: Add configuration and prompt composition**

Extend `AgentConfig` and its default without changing model, reasoning, or model-call-budget defaults. Import `ProjectRootLocator` and `SkillCatalog` in `src/runtime/rig/codex.rs`. Resolve the root once, pass it to both loaders, preserve the current AGENTS heading, and append the skill section only when present.

```rust
let project_root = ProjectRootLocator::default().locate(&self.request.context.cwd);
let agents_instructions = agents_md_instructions_from(
    &self.request.context.cwd,
    &project_root,
    self.agent.global_agents_md.as_deref(),
);
if !agents_instructions.is_empty() {
    system_prompt.push_str("\n\nInstructions from AGENTS.md:\n");
    system_prompt.push_str(&agents_instructions);
}
let skills = SkillCatalog::discover(self.agent.global_skills.as_deref(), &project_root);
if let Some(section) = skills.prompt_section() {
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&section);
}
```

Update every explicit `AgentConfig` literal in the test suite to set `global_skills: None`, preventing the developer machine's real global skills from contaminating fixture expectations.

- [ ] **Step 4: Document user-visible skill behavior**

Add an `## Agent skills` subsection after `## Agent file access` in `README.md`. State that a skill is a direct child directory containing `SKILL.md` YAML frontmatter; Moh discovers global `~/.agents/skills` first and project `<project-root>/.agents/skills` second; a valid project skill replaces a global skill of the same name; and startup sends only names, descriptions, and literal paths. Explain that Moh loads the full instructions only when the agent reads the selected `SKILL.md`, and that resources are available on demand through their relative paths. State that the current project root is the nearest `.git` directory or linked-worktree file, otherwise the working directory, and that additional root markers are not supported yet.

- [ ] **Step 5: Run focused and full validation**

Run:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

Expected: every command exits successfully. The focused mocked-request assertions prove discovery uses the shared Git boundary, applies project precedence, exposes only metadata and literal paths, preserves AGENTS ordering, and leaves no-skills prompts unchanged.

- [ ] **Step 6: Review the scoped diff and commit the integration**

Run: `git diff --check && git diff -- src/runtime/rig/codex.rs tests/rig_runtime.rs README.md`

Expected: no whitespace errors; the diff contains only the configuration, prompt, test, and documentation changes above.

```bash
git add src/runtime/rig/codex.rs tests/rig_runtime.rs README.md
git commit -m "feat(runtime): advertise available agent skills"
```
