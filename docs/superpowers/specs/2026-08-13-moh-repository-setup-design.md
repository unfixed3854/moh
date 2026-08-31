# `moh` Repository Setup Design

## Goal

Prepare Maciej's Rust coding harness for public-quality development workflows while creating it as a private GitHub repository under `unfixed3854`.

## Scope

The setup covers repository documentation, reproducible Rust tooling, basic GitHub Actions validation, and initial GitHub remote publication. It does not add harness functionality, release automation, dependency management, issue templates, or other project-management features.

## Repository identity

- GitHub owner: `unfixed3854`
- Repository name: `moh`
- Visibility: private
- Description: a concise description identifying `moh` as Maciej's personal Rust coding harness
- Remote: the created GitHub repository is configured as the local `origin`

## Documentation

Create `README.md` with:

- the project name and a short description
- a clear early-stage status statement
- prerequisites and local development commands
- the validation convention: `cargo fmt`, `cargo clippy`, `cargo test`, and `cargo build`
- a brief contribution/development note appropriate for a personal private project

The README must describe only capabilities present in the repository and must not imply that the harness already implements functionality beyond the current binary.

## Toolchain

Create `rust-toolchain.toml` using the stable channel and explicitly request the `rustfmt` and `clippy` components. This keeps local commands and CI aligned without pinning a specific compiler version.

## Continuous integration

Create `.github/workflows/ci.yml` for pushes and pull requests. Use the stable toolchain from `rust-toolchain.toml` and run these independent validation steps:

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test --all-targets`
4. `cargo build --locked`

Clippy is the primary static validation command; `cargo check` is not a required CI step. The workflow should use the standard Rust toolchain action and dependency caching, while remaining minimal and readable.

## Publication flow

After local files are implemented and validated:

1. create the private GitHub repository if it does not already exist;
2. configure its SSH URL as `origin`;
3. commit the repository setup as one intentional commit if the working tree contains only this task's changes;
4. push the existing `main` branch and set its upstream.

Do not alter or discard unrelated working-tree changes. If unrelated changes appear, stage only the setup files and preserve the rest.

## Verification

Before claiming completion, run the same commands represented in CI locally, confirm the workflow and README are present, verify the remote points to the intended private repository, and verify the pushed branch is available on GitHub.

