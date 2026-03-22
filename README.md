# keiros

`keiros` is a local time machine for source code projects.

It records time-based file history inside a project, independently of Git, so you can recover from broken edits, bad refactors, or destructive coding sessions even when you forgot to commit anything.

`keiros` is not a Git replacement. It is a local safety net.

It works inside Git repositories, but it stays independent from Git itself. Current releases partition timeline data by the active runtime context, so branch switches and linked worktrees no longer share one logical file stream by default.

## Why This Exists

Git is excellent for intentional version control. It is less helpful when:

- you forgot to commit before a risky change
- you made a long series of bad edits locally
- you want to restore a file to how it looked earlier today
- you need project-wide recovery after a destructive coding session

`keiros` fills that gap by continuously recording stabilized source-file states into a local `.timeline/` directory inside the project.

## Key Features

- Works inside or outside Git repositories
- Stores history locally inside the project under `.timeline/`
- Uses SQLite metadata plus patch/full-snapshot storage
- Tracks create, modify, and delete events
- Debounces rapid edit bursts to keep only stabilized states
- Supports restoring a single file or the whole project to an earlier timestamp
- Scopes history, recent changes, diffs, and restore planning to the active Git or local context
- Shows file history and diffs between historical versions
- Uses `.gitignore` when present, plus built-in ignore rules for secrets and common noise

## Scope

Current scope:

- Rust CLI
- macOS and Linux
- Local laptop or workstation usage
- Source-code oriented text files
- One project at a time

Current limitation:

- timelines created before the context-aware schema may be migrated conservatively into full snapshots

## How It Works

When `keiros watch` is running, it monitors the project directory recursively. For each tracked file, it records revisions into `.timeline/keiros.db`.

Each revision is stored as one of:

- a full text snapshot
- a patch against the previous revision
- a delete marker

Inside Git repositories, `keiros` stores separate logical streams per runtime context fingerprint. That fingerprint includes the worktree root, shared Git dir, branch name, HEAD, and detached state. Outside Git, `keiros` uses a local context tied to the project root.

This makes it possible to reconstruct file contents at a given timestamp while keeping storage simpler than storing only full copies of everything.

## Installation

### Prerequisites

- Rust toolchain with `cargo`
- macOS or Linux

If you do not have Rust installed yet:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Verify:

```bash
rustc --version
cargo --version
```

### Build From Source

Clone the repository and build it:

```bash
git clone <your-repository-url>
cd keiros
cargo build --release
```

The compiled binary will be available at:

```bash
target/release/keiros
```

### Install as a Global CLI

To install `keiros` into your Cargo bin directory:

```bash
cargo install --path .
```

That installs the binary into `~/.cargo/bin`.

If `keiros` is not found afterward, add this to your shell config:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

Then reload your shell:

```bash
source ~/.zshrc
```

or:

```bash
source ~/.bashrc
```

Verify the install:

```bash
keiros --help
```

## Quick Start

Go to the project you want to protect:

```bash
cd /path/to/your/project
```

Start the watcher:

```bash
keiros watch .
```

Keep that process running while you work.

In another terminal, you can inspect history:

```bash
keiros status
keiros recent --limit 10
keiros history src/main.rs
```

Restore a file to an earlier state:

```bash
keiros restore-file src/main.rs --at "10m ago"
```

Preview a project restore first:

```bash
keiros restore-project --at "1h ago" --dry-run
```

Then apply it if the preview looks right:

```bash
keiros restore-project --at "1h ago"
```

## Command Reference

### `keiros watch [path]`

Starts monitoring a project directory and recording local history into `.timeline/`.

Examples:

```bash
keiros watch .
keiros watch /path/to/project
```

### `keiros history <file>`

Shows known historical revisions for one file.

Example:

```bash
keiros history src/lib.rs
```

### `keiros diff <file> --at <timestamp> --at2 <timestamp>`

Shows the diff between two historical versions of a file.

Example:

```bash
keiros diff src/lib.rs --at "2h ago" --at2 "30m ago"
```

### `keiros restore-file <file> --at <timestamp> [--dry-run] [--allow-cross-context]`

Overwrites a file with the version that existed at the given timestamp.

Examples:

```bash
keiros restore-file src/lib.rs --at "10m ago"
keiros restore-file src/lib.rs --at "2026-03-19T12:00:00+01:00"
keiros restore-file src/lib.rs --at "10m ago" --dry-run
```

If the file did not exist at that time, `keiros` removes it.

In phase 2, file restore reads only from the active runtime context. `--allow-cross-context` is reserved for a later release and currently returns a clear error instead of bypassing context scoping.

### `keiros restore-project --at <timestamp> [--dry-run] [--allow-cross-context]`

Restores all tracked files in the project to their state at the given timestamp.

Examples:

```bash
keiros restore-project --at "30m ago"
keiros restore-project --at "2026-03-19T09:15:00+01:00"
keiros restore-project --at "30m ago" --dry-run
```

Tracked files that did not exist at that timestamp are removed.

`--dry-run` is the recommended first step before a project restore.

In phase 2, project restore only reads tracked files from the active runtime context. Files that only exist in another branch or worktree context are ignored by default instead of being mixed into the restore plan.

### `keiros status`

Shows:

- whether the watcher appears active
- tracked file counts
- revision count
- retention settings
- debounce settings
- storage usage

Example:

```bash
keiros status
```

### `keiros recent --limit <n>`

Shows the most recent captured project changes.

Example:

```bash
keiros recent --limit 20
```

### `keiros prune`

Removes expired history according to the retention policy.

Example:

```bash
keiros prune
```

## Timestamp Formats

`keiros` accepts several timestamp formats:

- RFC3339:
  `2026-03-19T12:00:00+01:00`
- Unix timestamp in seconds:
  `1774922400`
- Unix timestamp in milliseconds:
  `1774922400000`
- `now`
- Relative time:
  `10m ago`, `2h ago`, `1d ago`, `1w ago`

Displayed timestamps include both:

- exact local datetime
- human-readable relative time

## Typical Workflows

### Restore One File After a Bad Refactor

```bash
keiros history src/parser.rs
keiros diff src/parser.rs --at "20m ago" --at2 "now"
keiros restore-file src/parser.rs --at "20m ago"
```

### Recover a Whole Project

```bash
keiros recent --limit 30
keiros restore-project --at "45m ago"
```

### Inspect What Changed Recently

```bash
keiros status
keiros recent --limit 15
keiros history src/main.rs
```

## What Gets Tracked

`keiros` is intentionally focused on source-code relevant files.

Tracked:

- text-based source files
- files below the max tracked size
- files not excluded by `.gitignore` or built-in filters

Ignored:

- `.gitignore` matches when `.gitignore` exists
- `.timeline/`
- `.git/`
- `node_modules/`
- `dist/`
- `build/`
- `.next/`
- `target/`
- `.env`
- `.env.*`
- common private key and certificate files
- common editor temp and swap files
- files larger than 1 MB by default
- binary or non-UTF-8 files

## Storage Layout

All local timeline data stays inside the project:

```text
your-project/
  .timeline/
    keiros.db
```

The SQLite database stores:

- context rows for local or Git runtime states
- tracked file paths scoped by context
- revision timestamps
- event types
- patch or full-snapshot payloads
- watcher heartbeat metadata

## Defaults

- Retention period: 7 days
- Debounce window: 1200 ms
- Max tracked file size: 1 MB
- Storage location: `.timeline/`

## Architecture Overview

- CLI: `src/cli.rs`
- Command dispatch: `src/lib.rs`
- Project config and path handling: `src/config.rs`
- Watcher loop: `src/watcher.rs`
- Debounce logic: `src/debounce.rs`
- Ignore filtering: `src/filter.rs`
- Initial scan and event application: `src/snapshot.rs`
- Delta storage helpers: `src/delta.rs`
- SQLite metadata and reconstruction: `src/store.rs`
- Restore logic: `src/restore.rs`
- Timestamp parsing and formatting: `src/time.rs`

## Safety and Behavior Notes

- Restore commands overwrite tracked files directly.
- `keiros` is independent from Git and does not require commits.
- `keiros` works inside Git repositories, but it still records and restores through its own local timeline data.
- `history`, `diff`, `recent`, `restore-file`, and `restore-project` all default to the active runtime context.
- `keiros restore-project --dry-run` is the safest way to preview a large recovery before touching the filesystem.
- If SQLite integrity checks fail, the tool is designed to fail fast instead of silently restoring incorrect content.

## Current Limitations

- Only UTF-8 text files are tracked in this MVP.
- Rename tracking is not implemented as a first-class event. In practice, renames are treated as delete plus create.
- Project restore only affects files known to `keiros`; unrelated files are not touched.
- The watcher is launched manually and is not yet managed as a background service.
- Patch storage is intentionally simple and not yet aggressively optimized.
- Intentional cross-context restore is not implemented yet; `--allow-cross-context` is reserved for future work.

## Development

Run a local build:

```bash
cargo check
```

Run tests:

```bash
cargo test
```

Format the code:

```bash
cargo fmt
```

## Test Coverage

The current test suite covers:

- debounce stabilization behavior
- ignore filtering
- max file size filtering
- file restore correctness
- project restore correctness
- context partitioning across branches and worktrees
- v1 to v2 schema migration and full-snapshot replay
- delete handling
- pruning behavior
- timestamp lookup correctness

## Roadmap

Planned improvements:

- better rename tracking
- optional safe restore mode with preview or backup
- global storage mode across projects
- improved delta compression and compaction
- TUI or GUI browsing
- Git-aware helpers without Git dependency for core restore/storage behavior

## Contributing

Issues and pull requests are welcome.

If you want to contribute:

- keep changes focused
- add or update tests when behavior changes
- prefer reliability and predictable restore behavior over clever optimizations

## License

MIT
