# Medusa

A terminal-native, autonomous coding agent. Medusa inspects, edits, tests, and
debugs the current workspace through a tight tool loop, rendered in a fast TUI
that follows the Claude Code / Codex CLI visual language — one block per tool
call, inline colored diffs, a live plan strip above the composer.

Built in Rust. Single binary, no runtime dependencies.

```
 MEDUSA   ● ready   workspace ~/code/myproject                      perm open  git
─────────────────────────────────────────────────────────────────────────────────
 • read src/parser.rs, src/lexer.rs +3 more
   ⎿ 5 files, 1.2k lines
 • edit src/parser.rs (1 replacement)
   ⎿ - fn parse(&self) -> Ast {
     + fn parse(&mut self) -> Result<Ast> {

 plan · 2/4 · Rework the parser
   ✓ 2 done
   ● Wire the new lexer in
   · Run the full suite
 ╭─────────────────────────────────────────────────────────────────────────────╮
 │ █ Type a task or ask a question…                                             │
 ╰─────────────────────────────────────────────────────────────────────────────╯
```

## Features

- **Agent tool loop** — read, search, glob, list, edit, patch, and run terminal
  commands. Independent read-only calls execute in parallel.
- **Live plan strip** — the model's plan docks above the composer and updates in
  place, instead of scrolling away in chat history.
- **Inline diffs & syntax highlighting** — edits render as colored diffs; code
  blocks are syntax-highlighted; terminal output keeps its ANSI colors.
- **Interactive permissions** — approve, always-allow, or deny each mutating
  action, with three modes: `open`, `guarded`, `readonly`.
- **JS workflow engine** — the model (or you, via `/workflow`) can author and run
  deterministic multi-agent scripts with `agent()` / `parallel()` / `phase()`.
- **Context engineering** — token budgeting with automatic LLM compaction of
  older turns so long sessions stay within the model's window.
- **Headless mode** — `medusa run` for one-shot, non-interactive turns and
  scripting.
- **Ghostty-tuned** — synchronized output (no tearing), Kitty keyboard protocol,
  fast change-driven rendering.

## Requirements

- **Rust 1.85+** (2024 edition) — install via [rustup](https://rustup.rs).
- **A model backend.** By default Medusa reuses your **Codex CLI** OAuth login
  (see below). OpenAI-compatible and DeepSeek backends are also supported via
  environment variables.

## Install

### From source (recommended)

```sh
git clone https://github.com/jayasuryajsk/Medusa.git
cd Medusa
cargo install --path crates/medusa-tui
```

This builds an optimized binary and places `medusa` on your `PATH` (usually
`~/.cargo/bin`). Make sure that directory is on your `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # add to your shell profile
```

### Build without installing

```sh
cargo build --release
./target/release/medusa
```

## Authentication

By default Medusa authenticates with the **Codex OAuth token** — the same
credentials the [Codex CLI](https://github.com/openai/codex) writes. If you
already use Codex, you're done. Otherwise:

```sh
# install and log in to the Codex CLI once
codex login
```

Medusa reads the token from `~/.codex/auth.json` (override the directory with
`CODEX_HOME`). Inside the TUI, `/auth` shows your current auth status.

### Using another provider

Set environment variables before launching:

```sh
# OpenAI-compatible endpoint
export MEDUSA_PROVIDER=openai-compatible
export MEDUSA_OPENAI_API_KEY=sk-...
export MEDUSA_MODEL=gpt-4o                # optional; defaults to gpt-5.5
# export MEDUSA_OPENAI_BASE_URL=https://your-endpoint/v1   # optional

# DeepSeek
export MEDUSA_PROVIDER=deepseek
export DEEPSEEK_API_KEY=...
```

## Usage

Launch the TUI in any project directory:

```sh
cd ~/code/myproject
medusa
```

Type a task and press **Enter**. **Shift+Enter** (or **Alt+Enter**) inserts a
newline for multi-line prompts. **Esc Esc** interrupts a running turn.

### Slash commands

| Command | What it does |
|---|---|
| `/help` | List all commands |
| `/plan` | Toggle plan mode (explore & propose before editing) |
| `/model` | Switch the model |
| `/permissions` | Change permission mode (open / guarded / readonly) |
| `/theme` | Cycle color themes (`medusa`, `opencode`, `tokyonight`, `catppuccin`, …) |
| `/workflow <script> [args]` | Run a `.medusa/workflows/*.js` workflow |
| `/tools` · `/skills` | Show available tools / skills |
| `/sessions` · `/resume` · `/fork` · `/tree` | Manage and branch conversation sessions |
| `/jobs` · `/kill` · `/tail` | Manage background terminal jobs |
| `/settings` · `/auth` · `/clear` · `/restart` | Settings, auth status, clear, restart |

Resume your last session in a workspace:

```sh
medusa continue
```

### Headless mode

Run a single non-interactive turn — useful for scripts and CI:

```sh
# task as an argument
medusa run "summarize what this crate does"

# task from stdin
echo "list the public API of this module" | medusa run

# machine-readable output
medusa run --json --permission readonly "audit error handling"
```

Options: `--model <name>`, `--permission <open|guarded|readonly>`, `--json`,
`--no-stream`.

## Configuration

Medusa reads project instructions from an `AGENTS.md` file at the workspace root
(falling back to `AGENT.md`, `CLAUDE.md`, then `MEDUSA.md`).

Selected environment variables:

| Variable | Purpose |
|---|---|
| `MEDUSA_MODEL` | Override the model |
| `MEDUSA_PROVIDER` | `codex` (default), `openai-compatible`, or `deepseek` |
| `MEDUSA_REASONING_EFFORT` | `none` / `low` / `medium` / `high` |
| `MEDUSA_CONTEXT_MAX_TOKENS` | Context budget before compaction (default 60k) |
| `MEDUSA_THEME` | Startup theme |
| `CODEX_HOME` | Directory holding `auth.json` (default `~/.codex`) |

Per-workspace state (sessions, permission grants) lives in a `.medusa/`
directory, which you'll usually want in `.gitignore`.

## Development

```sh
cargo test --workspace      # run the test suite
cargo clippy --workspace    # lint
cargo build --release       # optimized build
```

The workspace has two crates: `medusa-core` (agent loop, tools, model backends,
workflow engine) and `medusa-tui` (the terminal interface and headless CLI).

## License

MIT © Jaya Surya Kommireddy. See [LICENSE](LICENSE).
