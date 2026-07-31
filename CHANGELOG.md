# Changelog

All notable changes to Medusa are documented here. The project follows
semantic versioning while the public interfaces stabilize.

## Unreleased

### Added

- Linux bubblewrap sandboxing with fail-closed guarded execution.
- Crash-safe atomic persistence for sessions, settings, permissions,
  checkpoints, attachments, and workflow reports.
- Recoverable session backups and append-only workflow run journals.
- Hook deadlines and bounded child-process output.
- Release checksums, build provenance, RustSec auditing, and dependency update
  automation.

### Changed

- Fresh workspaces now default to guarded permission mode.
- Codex-style multi-file patches roll back completely when any operation fails.
- Background commands use managed process groups and bounded output capture.
- The supported Rust version is now explicit and enforced in CI.

### Fixed

- Terminal state is restored when TUI startup or execution unwinds with an
  error.
- Workflow workers are joined even when the UI event consumer exits early.
