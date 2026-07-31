# Medusa

A terminal-native, autonomous coding agent. Medusa inspects, edits, tests, and
debugs the current workspace through a tight tool loop, rendered in a fast TUI
that follows the Claude Code / Codex CLI visual language — compact batched tool
activity, inline colored diffs, and a live plan strip above the composer.

Built in Rust and distributed as a single binary. Guarded execution on Linux
also requires `bubblewrap`.

```
 MEDUSA   ● ready   workspace ~/code/myproject                   perm guarded  git
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
- **Web tools** — `web_search` (DuckDuckGo) and `web_fetch` let the model look
  up docs, changelogs, and error messages; fetched HTML is reduced to readable
  text and private/local addresses are refused.
- **Checkpoints & `/rewind`** — before the first edit of each turn, Medusa
  snapshots the pre-image of every file it's about to change under
  `.medusa/checkpoints/`; `/rewind` restores the workspace to the state before
  any previous turn.
- **Sandboxed commands** — model-run shell commands use macOS Seatbelt or Linux
  bubblewrap: writes stay confined to the workspace and temp dirs, and network
  is denied unless allowed. A required sandbox fails closed when unavailable;
  an unsandboxed retry always pauses for your approval.
- **MCP servers** — declare stdio Model Context Protocol servers in
  `.medusa/mcp.json`; their tools appear to the model as `mcp_<server>_<tool>`.
- **Custom agents** — define named subagents in `.medusa/agents/*.md` with
  their own prompts and tool policies, usable from workflow scripts.
- **Live plan strip** — the model's plan docks above the composer and updates in
  place, instead of scrolling away in chat history.
- **Inline diffs & syntax highlighting** — edits render as colored diffs; code
  blocks are syntax-highlighted; terminal output keeps its ANSI colors.
- **Post-edit verification** — after the model's last edit in a turn, Medusa
  runs a cheap project check (`cargo check`, `go build`, `tsc --noEmit`,
  `py_compile`) and feeds the pass/fail result straight back to the model, so
  breakage gets fixed in the next turn instead of discovered later.
- **Interactive permissions** — approve, always-allow, or deny each mutating
  action, with three modes: `open`, `guarded`, `readonly`.
- **JS workflow engine** — `/workflow <task>` has the model author and run a
  task-specific orchestration; saved scripts use `agent()` / `parallel()` /
  `phase()` with control flow and intermediate results kept outside the parent
  conversation. Runs are resource-bounded and journaled under
  `.medusa/workflow-runs/` for post-failure inspection.
- **Context engineering** — token budgeting with automatic LLM compaction of
  older turns so long sessions stay within the model's window; `/context`,
  `/compact`, and `/cost` show and manage usage on demand.
- **Composer conveniences** — `@` opens a fuzzy file picker to mention
  workspace files, a leading `# note` appends the note to `AGENTS.md` as quick
  memory, `/edit` backtracks to a previous message and resends from there, and
  a single **Esc** cancels a running turn.
- **Headless mode** — `medusa run` for one-shot, non-interactive turns and
  scripting.
- **Ghostty-tuned** — synchronized output (no tearing), Kitty keyboard protocol,
  fast change-driven rendering.

## Requirements

- **Rust 1.90+** (2024 edition) — install via [rustup](https://rustup.rs).
- **Linux guarded/readonly mode:** `bubblewrap` (`bwrap`) must be installed.
- **A model backend.** By default Medusa reuses your **Codex CLI** OAuth login
  (see below). OpenAI-compatible and DeepSeek backends are also supported via
  environment variables.
- **Optional browser control:** the current stable versions of Chrome,
  Node.js LTS, and npm.

## Install

### With Cargo from GitHub

Install the latest tagged release directly:

```sh
cargo install --locked \
  --git https://github.com/jayasuryajsk/Medusa.git \
  --tag v0.2.0 \
  medusa-tui
```

Cargo places `medusa` in `~/.cargo/bin`. If the command is not found after
installation, add that directory to your shell profile and restart the shell:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

### From release binaries

Prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64) are
attached to each [GitHub release](https://github.com/jayasuryajsk/Medusa/releases):

```sh
# pick the tarball for your platform, e.g. Apple Silicon:
curl -LO https://github.com/jayasuryajsk/Medusa/releases/latest/download/medusa-v0.2.0-aarch64-apple-darwin.tar.gz
tar xzf medusa-v0.2.0-aarch64-apple-darwin.tar.gz
cd medusa-v0.2.0-aarch64-apple-darwin
chmod +x medusa
mkdir -p ~/.local/bin
mv medusa ~/.local/bin/          # or anywhere on your PATH
```

Other targets: `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`.
Each release includes a `.sha256` file and GitHub build-provenance
attestation. Verify a download with `sha256sum -c <archive>.sha256` (Linux) or
`shasum -a 256 -c <archive>.sha256` (macOS).

### From a source checkout

```sh
git clone https://github.com/jayasuryajsk/Medusa.git
cd Medusa
cargo install --locked --path crates/medusa-tui
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
newline for multi-line prompts. **Esc** interrupts a running turn; **Esc Esc**
on an idle composer quits.

In the composer:

- **`@`** opens a fuzzy file picker and inserts the chosen workspace path into
  your message (Tab accepts, Esc dismisses).
- **`# <note>`** is quick memory: the note is appended under `## Notes` in
  `AGENTS.md` instead of being sent as a turn, so it persists for future
  sessions.
- **`/edit`** opens a picker over your previous messages; choosing one
  truncates the transcript there and lets you edit and resend (the original
  timeline is preserved as a session fork).

### Slash commands

| Command | What it does |
|---|---|
| `/help` | List all commands |
| `/plan` | Toggle plan mode (explore & propose before editing) |
| `/model` | Choose the model and execution mode (model list populated live from the Codex backend) |
| `/reasoning` | Set thinking effort (`low`…`max`) or Ultra proactive orchestration when supported |
| `/permissions` | Change permission mode (open / guarded / readonly) |
| `/theme` | Cycle color themes (`medusa`, `opencode`, `tokyonight`, `catppuccin`, …) |
| `/workflow <task>` | Have the model author and run a task-specific JavaScript workflow |
| `/workflow <script> [args]` | Run a saved `.medusa/workflows/*.js` workflow |
| `/rewind` | Restore files to the state before a previous turn |
| `/edit` | Backtrack: edit a previous message and resend from there |
| `/review` | Seed the composer with a code-review prompt for pending changes |
| `/compact` · `/context` · `/cost` | Compact history now · show context usage · show token usage |
| `/tools` · `/skills` · `/agents` | Show available tools / skills / named agents |
| `/mcp [restart <server>]` | List MCP servers and their tools, or restart one |
| `/sessions` · `/resume` · `/fork` · `/tree` | Manage and branch conversation sessions |
| `/jobs` · `/kill` · `/tail` · `/restart <job>` | Manage background terminal jobs |
| `/exec <command>` · `/patch <path>` | Run a shell command · apply a unified diff |
| `/settings` · `/auth` · `/clear` · `/reload` | Settings (incl. the bell toggle), auth status, clear, reload |

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
(falling back to `AGENT.md`, `CLAUDE.md`, then `MEDUSA.md`). Quick-memory
notes (`# <note>` in the composer) are appended to the same file.

### Sandboxing

On macOS, model-initiated shell commands run under a `sandbox-exec` Seatbelt
profile: reads stay broad (toolchains need them), writes are confined to the
workspace and temp directories, and network access is denied unless enabled.
On Linux the equivalent policy uses bubblewrap with a read-only host root,
writable workspace/temp mounts, PID and user namespaces, and an isolated
network namespace.

Fresh workspaces default to `guarded`. `open` runs trusted commands
unsandboxed; `guarded` and `readonly` require the platform sandbox. If Seatbelt
or bubblewrap is unavailable, Medusa blocks the command instead of silently
running it on the host. Override the mode-derived sandbox preference with
`MEDUSA_SANDBOX=on|off`; an explicit unsandboxed model request still requires
approval.

When a sandboxed command fails for a sandbox-plausible reason, the model may
retry with the sandbox off — that escalation always renders an approval card
and waits for you. Sandboxed children see `MEDUSA_SANDBOX=seatbelt` or
`MEDUSA_SANDBOX=bubblewrap` (plus `MEDUSA_SANDBOX_NETWORK_DISABLED=1` when
network is denied). Platforms without a supported sandbox can use `open` mode
or an individually approved unsandboxed command.

### Checkpoints & rewind

Before the first mutation of each file in a turn, Medusa stores the file's
pre-image under `.medusa/checkpoints/`. `/rewind` lists previous turns and
restores the workspace to the state before the one you pick. Scope: only
changes made through the edit/patch tools are captured — shell-command side
effects are not. The store is pruned to `MEDUSA_CHECKPOINT_MAX` checkpoints
and `MEDUSA_CHECKPOINT_MAX_MB` total size (defaults 50 and 200).

### MCP servers

Declare stdio MCP servers in `.medusa/mcp.json`:

```json
{
  "servers": {
    "docs": {
      "command": "npx",
      "args": ["-y", "@example/docs-mcp"],
      "env": { "DOCS_TOKEN": "..." },
      "readOnly": true
    }
  }
}
```

Servers spawn lazily; their tools are advertised to the model as
`mcp_<server>_<tool>`. Only servers you mark `"readOnly": true` are reachable
in `readonly` permission mode. `/mcp` shows server status and tools;
`/mcp restart <server>` restarts a wedged one.

#### Browser control

Medusa can control and inspect Chrome through the official
[Chrome DevTools MCP server](https://github.com/ChromeDevTools/chrome-devtools-mcp).
Add this to the workspace's `.medusa/mcp.json`:

```json
{
  "servers": {
    "browser": {
      "command": "npx",
      "args": [
        "-y",
        "chrome-devtools-mcp@1.6.0",
        "--isolated=true",
        "--no-usage-statistics"
      ],
      "readOnly": false
    }
  }
}
```

Restart Medusa or run `/reload`, then use `/mcp` to confirm that `browser` is
available. The server starts Chrome lazily on the first browser request.
`--isolated=true` uses a temporary browser profile, keeping normal Chrome
cookies and logged-in sessions separate. Browser tools are intentionally
unavailable in Medusa's `readonly` permission mode.

### Custom agents

Drop one Markdown file per agent in `.medusa/agents/`:

```
name: reviewer
description: Reviews diffs for correctness issues
tools: read|shell

You are a meticulous code reviewer. Focus on…
```

Header lines (`name:`, `description:`, `tools:` — any of
`read|shell|edit|verify`), then a blank line, then the body used as the
agent's system prompt. Workflow scripts reference agents by name via the
`agentType` field; `/agents` lists what's loaded.

### Lifecycle hooks

Trusted project hooks live in `.medusa/hooks.json`. Hooks run at `turn_start`,
`pre_tool`, `post_tool`, and `turn_end`; object entries can block on failure
and set a per-hook timeout:

```json
{
  "default_timeout_secs": 30,
  "hooks": {
    "post_tool": [
      {
        "command": "cargo fmt --all -- --check",
        "cwd": ".",
        "fail_on_error": true,
        "timeout_secs": 60
      }
    ]
  }
}
```

Hooks are local trusted automation, not model-generated commands, and do not
run inside the model sandbox. Output is bounded and timed out; never put
untrusted shell text in hook commands.

### Environment variables

Selected environment variables:

| Variable | Purpose |
|---|---|
| `MEDUSA_MODEL` | Override the model |
| `MEDUSA_PROVIDER` | `codex` (default), `openai-compatible`, or `deepseek` |
| `MEDUSA_REASONING_EFFORT` | Startup thinking effort (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`) or `ultra` orchestration; overrides the saved `/reasoning` choice |
| `MEDUSA_CONTEXT_MAX_TOKENS` | Context budget before compaction (default 60k) |
| `MEDUSA_VERIFY` | `off` disables post-edit verification |
| `MEDUSA_VERIFY_TIMEOUT_SECS` | Verification command timeout (default 90) |
| `MEDUSA_SANDBOX` | `on`/`off` overrides the permission-mode sandbox default |
| `MEDUSA_CHECKPOINT_MAX` | Max retained checkpoints (default 50) |
| `MEDUSA_CHECKPOINT_MAX_MB` | Max total checkpoint size in MB (default 200) |
| `MEDUSA_MCP_CONNECT_TIMEOUT_SECS` | MCP server connect/handshake budget (default 10) |
| `MEDUSA_MCP_TOOL_TIMEOUT_SECS` | Per-call MCP tool timeout (default 60) |
| `MEDUSA_MCP_DEBUG` | `1` logs MCP traffic to `.medusa/logs/mcp-<server>.log` |
| `MEDUSA_WORKFLOW_MAX_SCRIPT_AGENTS` | Maximum agents spawned by one workflow script (default 200) |
| `MEDUSA_WORKFLOW_MAX_PARALLEL` | Maximum concurrently running workflow agents (default 8) |
| `MEDUSA_WORKFLOW_SCRIPT_TIMEOUT_SECS` | Workflow JS deadline in seconds (default 3600; `0` disables) |
| `MEDUSA_WORKFLOW_SCRIPT_MEMORY_MB` | Workflow JS heap cap in MB (default 128, max 1024) |
| `MEDUSA_HOOK_TIMEOUT_SECS` | Default lifecycle-hook timeout (default 30, max 600) |
| `MEDUSA_BELL` | `on`/`off` overrides the bell setting (rings after long turns and on approval prompts) |
| `MEDUSA_THEME` | Startup theme |
| `CODEX_HOME` | Directory holding `auth.json` (default `~/.codex`) |

Inside sandboxed commands, Medusa identifies the backend in `MEDUSA_SANDBOX`
and, when network is denied, sets `MEDUSA_SANDBOX_NETWORK_DISABLED=1`.

Per-workspace state (sessions, permission grants, attachments, workflow
journals) lives in a private `.medusa/` directory and is written atomically.
Keep it in `.gitignore`; it can contain source excerpts, prompts, and model
output.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --release
```

The workspace has two crates: `medusa-core` (agent loop, tools, model backends,
workflow engine) and `medusa-tui` (the terminal interface and headless CLI).

## License

MIT © Jaya Surya Kommireddy. See [LICENSE](LICENSE).

Security reports are handled through [GitHub private vulnerability
reporting](SECURITY.md). Please do not open a public issue for a suspected
credential leak or sandbox escape.
