# AGENTS.md

## Cursor Cloud specific instructions

Medusa is a Rust workspace (edition 2024, `rust-version = 1.90`) with two crates:

- `medusa-core` — agent loop, tools, model backends, permissions/sandboxing, workflow engine.
- `medusa-tui` — the terminal UI and the `medusa run` headless CLI (produces the `medusa` binary).

### Toolchain / environment

- The default `rustup` toolchain is pinned to `1.90.0` (the base image ships an older `1.83`, which cannot compile edition 2024). The startup/update script installs and defaults to `1.90.0`; there is no `rust-toolchain.toml`, so the active default matters.
- `bubblewrap` (`bwrap`) is installed. It is only needed for `guarded`/`readonly` sandbox execution and the sandbox integration tests on Linux; `open` mode and the rest of the suite do not require it.

### Build / lint / test / run

Standard commands are already documented in `README.md` (“Development”) and `CONTRIBUTING.md`: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace`, `cargo build --locked --release`. Build/lint/test need no network or credentials.

### Running the app end-to-end (non-obvious)

- Actually executing an agent turn requires a **model backend**, which needs credentials that are not in this environment:
  - Default is Codex OAuth, read from `~/.codex/auth.json` (override dir with `CODEX_HOME`).
  - Alternatively set `MEDUSA_PROVIDER=openai-compatible` with `MEDUSA_OPENAI_API_KEY` (and optional `MEDUSA_OPENAI_BASE_URL`), or `MEDUSA_PROVIDER=deepseek` with `DEEPSEEK_API_KEY`.
- With no credentials, only the live model call fails; build/lint/test still pass. To smoke-test the full loop without real credentials, point `MEDUSA_OPENAI_BASE_URL` at a local OpenAI-compatible chat-completions SSE stub and use `--permission open`.
- The interactive TUI (`medusa`) needs a real TTY — run it in a terminal (or a `tmux` pty), not through a plain piped command. The headless `medusa run [--json] "<task>"` works fully non-interactively (task can also come from stdin).
- Fresh workspaces default to `guarded` permission mode; per-workspace state lives in `.medusa/` (gitignored).
