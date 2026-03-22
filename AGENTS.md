# Repository Guidelines

## Project Structure & Module Organization

`keiros` is a single-crate Rust CLI. Core code lives in `src/`, with `src/main.rs` as the thin binary entrypoint and `src/lib.rs` wiring commands to focused modules such as `store.rs`, `restore.rs`, `watcher.rs`, `filter.rs`, and `ui.rs`. Integration coverage lives in `tests/integration.rs`. Generated artifacts belong in `target/`. Local timeline data is stored under `.timeline/` when the tool runs; treat that as runtime state, not source.

## Build, Test, and Development Commands

- `cargo check` validates the crate quickly without producing a release binary.
- `cargo test` runs the full test suite, including `tests/integration.rs`.
- `cargo fmt --check` verifies formatting; run `cargo fmt` before committing if needed.
- `cargo run -- watch .` starts the watcher against the current project.
- `cargo build --release` produces the optimized CLI in `target/release/keiros`.

Use `cargo run -- --help` or `cargo run -- history src/lib.rs` when validating CLI behavior locally.

## Coding Style & Naming Conventions

Follow standard Rust style with rustfmt defaults: 4-space indentation, trailing commas where rustfmt adds them, and small, focused modules. Use `snake_case` for files, functions, and test names, and `UpperCamelCase` for types. Prefer direct, readable control flow over abstraction-heavy designs; this repository prioritizes restore correctness and predictable behavior over cleverness.

## Testing Guidelines

Add `#[test]` coverage for behavior changes, especially around restore logic, SQLite-backed storage, pruning, ignore filtering, and debounce behavior. Match the existing test naming pattern: descriptive, scenario-based names such as `restore_project_recovers_multiple_files_and_removes_newer_ones`. Use `tempfile::tempdir()` for filesystem-backed tests so cases stay isolated and reproducible. There is no explicit coverage threshold, but risky behavior changes should ship with tests.

## Commit & Pull Request Guidelines

History is currently sparse, but it already uses Conventional Commit style, for example `feat(cli): improve terminal output styling and readability`. Follow `type(scope): summary` in imperative mood. Pull requests should explain behavior changes, call out any restore or storage risks, list validation steps, and update `README.md` when CLI behavior or user-facing workflows change.

## Safety Notes

This project is a recovery tool. Avoid silent data loss, ambiguous restore semantics, or schema changes without corresponding tests and explicit rationale in the PR.
