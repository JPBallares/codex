# Repository Guidelines

In the codex-rs folder where the rust code lives:
- Crate names are prefixed with `codex-`. For examole, the `core` folder's crate is named `codex-core`
- When using format! and you can inline variables into {}, always do that.
- Never add or modify any code related to `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` or `CODEX_SANDBOX_ENV_VAR`.
  - You operate in a sandbox where `CODEX_SANDBOX_NETWORK_DISABLED=1` will be set whenever you use the `shell` tool. Any existing code that uses `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` was authored with this fact in mind. It is often used to early exit out of tests that the author knew you would not be able to run given your sandbox limitations.
  - Similarly, when you spawn a process using Seatbelt (`/usr/bin/sandbox-exec`), `CODEX_SANDBOX=seatbelt` will be set on the child process. Integration tests that want to run Seatbelt themselves cannot be run under Seatbelt, so checks for `CODEX_SANDBOX=seatbelt` are also often used to early exit out of tests, as appropriate.

Before creating a pull request with changes to `codex-rs`, run `just fmt` (in `codex-rs` directory) to format the code and `just fix` (in `codex-rs` directory) to fix any linter issues in the code, ensure the test suite passes by running `cargo test --all-features` in the `codex-rs` directory.

## Project Structure & Module Organization

- `codex-rs/`: Primary Rust workspace (crates like `core/`, `tui/`, `exec/`, `cli/`, utilities). Each crate is named with the `codex-` prefix (e.g., `core` → `codex-core`).
- `codex-cli/`: Legacy TypeScript CLI (kept for reference); new work happens in `codex-rs/`.
- `docs/`, `.github/`: Documentation and CI workflows.

## Build, Test, and Development Commands

- Rust build: `cargo build --workspace` (in `codex-rs/`).
- Run TUI: `just tui` or `cargo run --bin codex -- tui` (in `codex-rs/`).
- Format Rust: `just fmt`; lint/autofix: `just fix` (clippy fix, all features).
- Test Rust: `cargo test --all-features` (prefer targeted runs while iterating).
- Repo formatting (Markdown/JSON/JS): `pnpm format` or `pnpm format:fix` at repo root.

## Coding Style & Naming Conventions

- Rust: follow `rustfmt` (see `codex-rs/rustfmt.toml`). Use `format!` with inline variables in `{}` whenever possible. Crate names use `codex-*`; modules use `snake_case`.
- TypeScript (legacy): follow Prettier config (`.prettierrc.toml`).

## Testing Guidelines

- Framework: Rust `cargo test` across the workspace; unit tests live alongside code and in `tests/` dirs per crate.
- Targeted runs: `cargo test -p codex-core` or `cargo test -p codex-core some_test_name` for faster feedback.
- Avoid adding tests that require external network access. Some integration tests may early-exit under sandboxed environments used by CI and local tooling.

## Commit & Pull Request Guidelines

- Commits: Use Conventional Commits to drive changelogs (e.g., `feat(core): add X`, `fix(tui): resolve panic`). See `cliff.toml`.
- PRs: Include a clear description, rationale, and linked issues. For TUI or UX-visible changes, add short clips or screenshots when helpful. Ensure `just fmt`, `just fix`, and `cargo test --all-features` pass in `codex-rs/`.

## Security & Configuration Tips

- Do not modify code related to `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` or `CODEX_SANDBOX_ENV_VAR`. These are used to control/skip tests in sandboxed runs.
- Prefer workspace-write behavior during development; never rely on network access in tests.

## Before You Submit

1) From `codex-rs/`: `just fmt` → `just fix` → `cargo test --all-features`.
2) Run targeted tests for changed crates first; expand to workspace tests before opening the PR.
