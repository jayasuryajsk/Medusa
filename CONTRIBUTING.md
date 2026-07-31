# Contributing to Medusa

Thanks for helping improve Medusa.

## Development Setup

Install Rust 1.90 or newer. Linux contributors also need `bubblewrap` to run
guarded-mode integration tests.

```sh
git clone https://github.com/jayasuryajsk/Medusa.git
cd Medusa
cargo build --locked
```

Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

The workspace is split into:

- `medusa-core`: model backends, agent loop, tools, permissions, sandboxing,
  checkpoints, sessions, context, workflows, MCP, and hooks.
- `medusa-tui`: terminal UI plus the `medusa run` headless entry point.

Keep behavior and policy in `medusa-core`; the TUI should present core events
without owning agent semantics. Add focused regression tests for bugs and
failure paths. Avoid unrelated formatting or refactors in the same change.

## Security-Sensitive Changes

Changes to command execution, path validation, permissions, OAuth, MCP,
checkpoints, hooks, or persistence need tests for denial and failure behavior,
not only the happy path. Never commit API keys, OAuth tokens, `.medusa/`
session data, or real user prompts.

Report vulnerabilities according to [SECURITY.md](SECURITY.md).
