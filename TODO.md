# TODO

## High Priority

- Make installation easier for end users.
  Current installation via `cargo install --path .` is acceptable for development, but not ideal for open-source adoption.
  Evaluate and implement one or more of the following:
  - publish the crate to crates.io for `cargo install keiros`
  - provide prebuilt binaries through GitHub Releases
  - add a simple cross-platform install script
  - add Homebrew support for macOS

## Future Work

- Improve rename tracking so renames are preserved as first-class events instead of delete plus create.
- Add an optional safe restore mode with preview, confirmation, or automatic backups before overwrite.
- Support global storage across projects instead of only per-project `.timeline/` storage.
- Improve delta compression and compaction for better long-running storage efficiency.
- Explore a TUI or GUI for browsing timeline history visually.
- Add Git-aware helper commands without making Git a dependency for core storage or restore behavior.
