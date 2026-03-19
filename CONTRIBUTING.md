# Contributing

Thanks for your interest in contributing to `keiros`.

## Project Priorities

Please optimize changes in this order:

1. Reliability of restore behavior
2. Predictability and safety
3. Simplicity and maintainability
4. Performance after correctness

`keiros` is a recovery tool. A simple and correct implementation is better than a clever one that risks incorrect restores.

## Getting Started

Clone the repository and run:

```bash
cargo check
cargo test
```

Optional:

```bash
cargo fmt
```

## Before Opening a Pull Request

Please make sure that:

- the code builds successfully
- tests pass
- new behavior is covered by tests when appropriate
- the README is updated if the user-facing behavior changes
- command-line behavior stays clear and backward-compatible unless there is a strong reason to change it

## Coding Guidelines

- Keep modules focused and small when possible.
- Prefer straightforward logic over heavily abstracted designs.
- Fail fast on store corruption or impossible reconstruction states.
- Avoid silent data loss or ambiguous restore behavior.
- Be explicit about tradeoffs in pull request descriptions.

## Areas That Need Care

These parts of the codebase are especially sensitive:

- SQLite schema and revision reconstruction
- pruning and retention logic
- restore behavior
- ignore filtering
- filesystem event coalescing and debounce behavior

If you change one of these areas, include tests that exercise the new behavior.

## Bug Reports

A good bug report includes:

- operating system
- Rust version
- exact command used
- expected behavior
- actual behavior
- steps to reproduce
- whether the project was inside a Git repository

## Feature Requests

Feature requests are welcome. If the request changes restore semantics, storage layout, or safety guarantees, describe the expected behavior as concretely as possible.

## Discussion

When in doubt, prefer opening an issue before a large change. That is especially useful for:

- storage format changes
- retention model changes
- new restore modes
- global storage support
- background daemon or service behavior
