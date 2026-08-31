# `moh` Repository Setup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Document the minimal Rust harness, standardize its stable toolchain, add Clippy-first GitHub Actions CI, and publish the existing `main` branch to a new private `unfixed3854/moh` repository.

**Architecture:** Keep the current binary untouched. Add repository-level documentation and toolchain configuration, then a single readable GitHub Actions workflow with separate formatting, Clippy, test, and build steps. Finish by creating/configuring the GitHub remote and pushing `main`.

**Tech Stack:** Rust 2024 edition, Cargo, rustup, GitHub Actions, `actions/checkout`, `dtolnay/rust-toolchain`, and `gh`.

## Global Constraints

- GitHub repository: `unfixed3854/moh`.
- Repository visibility: private.
- Rust channel: `stable`.
- Static validation command: `cargo clippy --all-targets --all-features -- -D warnings`.
- Do not add a required `cargo check` CI step.
- Do not change `src/main.rs` or invent harness capabilities in documentation.
- Preserve unrelated working-tree changes and stage explicit paths.

---

### Task 1: Add project documentation and stable toolchain configuration

**Files:**
- Create: `README.md`
- Create: `rust-toolchain.toml`

**Interfaces:**
- Produces the documented local commands and stable toolchain used by the CI workflow in Task 2.

- [ ] **Step 1: Write `README.md`**

Include the title `moh`, a concise description of Maciej's personal Rust coding harness, an early-stage status note, prerequisites, and these commands:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
cargo run
```

State that the project is personal and currently intentionally small; do not claim functionality beyond the existing hello-world binary.

- [ ] **Step 2: Add `rust-toolchain.toml`**

Create:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Validate documentation/configuration syntax**

Run:

```bash
rustup show active-toolchain
sed -n '1,240p' README.md
sed -n '1,80p' rust-toolchain.toml
```

Expected: the active toolchain resolves to stable and both files contain the approved project description and commands.

- [ ] **Step 4: Commit the task**

```bash
git add README.md rust-toolchain.toml
git commit -m "docs: add project README and toolchain"
```

### Task 2: Add Clippy-first GitHub Actions CI

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `Cargo.lock` if it does not already exist

**Interfaces:**
- Consumes `rust-toolchain.toml` from Task 1.
- Produces a workflow for `push` and `pull_request` events with four required validation steps.

- [ ] **Step 1: Create `.github/workflows/ci.yml`**

Use one `validate` job on `ubuntu-latest` with checkout, stable Rust plus `rustfmt` and `clippy`, Cargo caching, and these four run steps:

```yaml
name: CI

on:
  push:
  pull_request:

permissions:
  contents: read

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/bin/
            ~/.cargo/registry/index/
            ~/.cargo/registry/cache/
            ~/.cargo/git/db/
            target/
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-targets --all-features -- -D warnings
      - run: cargo test --all-targets
      - run: cargo build --locked
```

If no `Cargo.lock` exists, generate and commit it before relying on `--locked`.

- [ ] **Step 2: Inspect workflow contents**

Run:

```bash
sed -n '1,240p' .github/workflows/ci.yml
rg -n 'cargo (fmt|clippy|test|build)|cargo check' .github/workflows/ci.yml
```

Expected: all four required commands are present and `cargo check` is absent.

- [ ] **Step 3: Commit the task**

```bash
git add .github/workflows/ci.yml Cargo.lock
git commit -m "ci: add Rust validation workflow"
```

### Task 3: Run validation and publish the private GitHub repository

**Files:**
- Modify: `.git/config` through Git remote commands only
- Existing history: push `main` to the new remote

**Interfaces:**
- Consumes the README, toolchain, workflow, and lockfile from Tasks 1–2.
- Produces private repository `https://github.com/unfixed3854/moh` with SSH `origin` and pushed `main`.

- [ ] **Step 1: Confirm scope and repository state**

Run:

```bash
git status -sb
git diff --stat
git remote -v
```

Expected: only this task's changes are present and no existing remote needs to be preserved. If unrelated changes are present, stage only the setup files and preserve the rest.

- [ ] **Step 2: Run the exact CI commands locally**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --locked
```

Expected: all commands exit successfully.

- [ ] **Step 3: Create the private GitHub repository**

Verify authentication and create the repository:

```bash
gh auth status
gh repo create unfixed3854/moh --private --description "Maciej's personal Rust coding harness"
```

If GitHub reports that it already exists, inspect it with `gh repo view unfixed3854/moh`; do not overwrite it or delete anything.

- [ ] **Step 4: Configure the SSH remote**

```bash
git remote add origin git@github.com:unfixed3854/moh.git
git remote -v
```

- [ ] **Step 5: Push `main` with tracking**

```bash
git push -u origin main
```

- [ ] **Step 6: Verify the remote repository**

```bash
gh repo view unfixed3854/moh --json nameWithOwner,isPrivate,description,defaultBranchRef,url
git status -sb
```

Expected: the repository is private, has the requested description, its default branch is `main`, and the local branch tracks `origin/main` with a clean worktree.

