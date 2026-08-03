# Changelog

All notable changes to Medusa are documented here. The project follows
semantic versioning while the public interfaces stabilize.

## Unreleased

## 0.3.2 - 2026-08-04

### Added

- Local semantic code search powered by a globally cached Nomic Embed Text
  model and persistent llama.cpp server.
- Incremental, quantized repository indexes with hybrid semantic, lexical,
  and path ranking.
- A settings control for showing or hiding provider reasoning traces.

### Changed

- Semantic retrieval participates in Medusa's parallel read-only exploration
  and returns distinct files with focused line ranges.
- Apple Terminal uses ASCII borders automatically while modern terminals keep
  rounded Unicode borders.
- Code blocks use the transcript background instead of a separate grey panel.

### Fixed

- DeepSeek reasoning snapshots and overlapping deltas no longer produce
  duplicated thinking text.
- Reasoning traces are hidden by default and no longer display a redundant
  `thinking` label.
- Repository semantic indexes detect external file changes and refuse to
  traverse symlinks outside the workspace.

## 0.3.1 - 2026-08-03

### Added

- A persistent queued-turn shelf above the composer while an agent is working.
- Subdued reasoning rows before model answers for providers that expose
  reasoning content.
- Model-specific context windows, including Codex catalog metadata and the
  DeepSeek V4 Flash context budget.

### Changed

- Context usage now measures the effective prompt after compaction instead of
  the unmodified transcript.
- Model switches immediately update the context gauge and `/context` report.

### Fixed

- Automatic and manual compaction now show progress, before/after token counts,
  completion feedback, and the resulting lower context usage.
- Queued prompts no longer disappear from view after submission.

## 0.3.0 - 2026-08-01

### Added

- A provider-neutral model gateway with configurable providers, models,
  credentials, endpoints, and reasoning levels.
- Native DeepSeek Responses API support, including streaming responses,
  reasoning content, and tool-call round trips.
- Adaptive workflow orchestration with convergence guards for parallel agents.

### Changed

- Split the core engine, tool runtime, and terminal UI into focused modules to
  make the codebase easier to maintain and extend.
- Preserve terminal theme colors while running in full-screen mode.
- Use the crate version automatically in model-provider HTTP user agents.
- Streamlined the README around installation and the terminal experience.

### Fixed

- Theme colors now remain visible when Medusa is launched in a fresh terminal.

## 0.2.0 - 2026-07-31

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
- Headless `--permission` overrides now govern the actual tool runtime,
  sandbox policy, and MCP tool visibility instead of only the reported mode.
