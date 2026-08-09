# Medusa Configuration

Medusa keeps project-specific configuration under `.medusa/` and reads project
instructions from `AGENTS.md` at the workspace root. It falls back to
`AGENT.md`, `CLAUDE.md`, then `MEDUSA.md`.

## Permission Modes And Sandboxing

Fresh workspaces default to `guarded`:

- `open`: trusted commands can run without the platform sandbox.
- `guarded`: mutating actions require approval and commands use the sandbox.
- `readonly`: workspace mutations are unavailable.

On macOS, guarded commands run under a Seatbelt profile. On Linux, Medusa uses
bubblewrap. Writes stay confined to the workspace and temporary directories;
network access is denied unless enabled. If the required sandbox is
unavailable, Medusa blocks the command instead of silently running it on the
host.

Set `MEDUSA_SANDBOX=on|off` to override the permission-mode default. A model
request to run unsandboxed still requires explicit approval. Sandboxed
children receive `MEDUSA_SANDBOX=seatbelt` or
`MEDUSA_SANDBOX=bubblewrap`, plus `MEDUSA_SANDBOX_NETWORK_DISABLED=1` when
network access is denied.

## Checkpoints And Rewind

Before changing a file through `file_edit` or `file_patch`, Medusa records its
pre-image under `.medusa/checkpoints/`. `/rewind` restores the workspace to
the state before a selected turn.

Shell-command side effects are not checkpointed. Retention defaults to 50
checkpoints and 200 MB; configure it with `MEDUSA_CHECKPOINT_MAX` and
`MEDUSA_CHECKPOINT_MAX_MB`.

## MCP Servers

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

Servers start lazily. Their tools appear as `mcp_<server>_<tool>`. Only
servers marked `"readOnly": true` are available in readonly mode. Use `/mcp`
to inspect servers and `/mcp restart <server>` to restart one.

### Browser Control

Medusa can control Chrome through the official
[Chrome DevTools MCP server](https://github.com/ChromeDevTools/chrome-devtools-mcp):

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

Restart Medusa or run `/reload`, then check `/mcp`. The isolated browser uses
a temporary profile, separate from normal Chrome cookies and sessions.
Browser tools are unavailable in readonly mode.

## Custom Agents

Create one Markdown file per named agent under `.medusa/agents/`:

```text
name: reviewer
description: Reviews diffs for correctness issues
tools: read

You are a meticulous code reviewer. Lead with concrete findings.
```

Valid tool policies are `read`, `shell`, `edit`, and `verify`. Workflow
scripts reference named agents through `agentType`; `/agents` lists loaded
definitions.

Parallel workflow agents are runtime-enforced read-only. Agents that use
`tools: edit` must run sequentially with `agent()`.

## Lifecycle Hooks

Trusted project hooks live in `.medusa/hooks.json` and can run at
`turn_start`, `pre_tool`, `post_tool`, and `turn_end`:

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

Hooks are trusted local automation, not model-generated commands. They run
outside the model sandbox, so never interpolate untrusted text into hook
commands.

## Model Providers

Medusa's agent loop talks to a provider-neutral model gateway. A model is
identified as `provider/model`; the provider selects an auth strategy and wire
protocol while the model ID is sent unchanged to that endpoint.

Built-ins:

| Provider | Protocol | Credentials |
|---|---|---|
| `codex` | Codex Responses | Codex CLI OAuth cache |
| `openai` | OpenAI Responses | `medusa auth set openai` or `OPENAI_API_KEY` |
| `deepseek` | OpenAI Chat | `medusa auth set deepseek` or `DEEPSEEK_API_KEY` |
| `openrouter` | OpenAI Chat | `medusa auth set openrouter` or `OPENROUTER_API_KEY` |
| `ollama` | OpenAI Chat | none; defaults to `http://localhost:11434/v1` |
| `lmstudio` | OpenAI Chat | none; defaults to `http://localhost:1234/v1` |

Credential commands write `~/.local/share/medusa/auth.json` atomically with
owner-only permissions:

```sh
medusa auth list
medusa auth set deepseek
medusa auth remove deepseek
```

Stored keys take precedence over environment variables. Codex OAuth remains in
`~/.codex/auth.json`; Medusa never copies that token.

### Custom Providers

Provider overlays are loaded in this order: built-ins, global
`~/.config/medusa/providers.json`, then workspace `.medusa/providers.json`.
Later definitions override earlier ones. Set `MEDUSA_CONFIG_HOME` to move the
global Medusa config directory.

```json
{
  "providers": {
    "company": {
      "name": "Company Gateway",
      "protocol": "openai-chat",
      "auth": "bearer",
      "thinking": "openai",
      "baseUrl": "https://models.company.example/v1",
      "apiKeyEnv": ["COMPANY_MODEL_KEY"],
      "headers": { "X-Client": "medusa" },
      "defaultModel": "coder-v2",
      "capabilities": {
        "tools": true,
        "reasoning": true,
        "images": false,
        "parallelTools": true,
        "promptCache": true
      },
      "models": {
        "coder-v2": {
          "name": "Coder V2",
          "description": "Internal coding model",
          "defaultReasoning": "medium",
          "reasoning": ["none", "low", "medium", "high"]
        }
      }
    }
  }
}
```

Supported protocols are `codex-responses`, `openai-responses`, and
`openai-chat`. Supported auth modes are `codex-oauth`, `bearer`, and `none`.
Chat reasoning dialects are `openai` (`reasoning_effort`), `openrouter`
(`reasoning.effort`), `deepseek`, and `none`.
Per-model capabilities override provider defaults and control tool schemas,
parallel tool requests, image payloads, and reasoning choices.

## Environment Variables

| Variable | Purpose |
|---|---|
| `MEDUSA_MODEL` | Override the `provider/model` selection (bare legacy IDs still resolve) |
| `MEDUSA_PROVIDER` | Legacy/provider hint for a bare `MEDUSA_MODEL` |
| `MEDUSA_REASONING_EFFORT` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or `ultra` |
| `MEDUSA_OPENAI_API_KEY` | API key for OpenAI-compatible providers |
| `MEDUSA_OPENAI_BASE_URL` | Base URL for an OpenAI-compatible endpoint |
| `DEEPSEEK_API_KEY` | DeepSeek API key |
| `OPENROUTER_API_KEY` | OpenRouter API key |
| `MEDUSA_CONFIG_HOME` | Global configuration directory; default `~/.config/medusa` |
| `MEDUSA_DATA_HOME` | Private state directory; default `~/.local/share/medusa` |
| `MEDUSA_CONTEXT_MAX_TOKENS` | Context budget before compaction; default 60k |
| `MEDUSA_VERIFY` | Set to `off` to disable post-edit verification |
| `MEDUSA_VERIFY_TIMEOUT_SECS` | Verification timeout; default 90 |
| `MEDUSA_SANDBOX` | `on` or `off` |
| `MEDUSA_CHECKPOINT_MAX` | Retained checkpoints; default 50 |
| `MEDUSA_CHECKPOINT_MAX_MB` | Checkpoint storage budget; default 200 MB |
| `MEDUSA_MCP_CONNECT_TIMEOUT_SECS` | MCP connection timeout; default 10 |
| `MEDUSA_MCP_TOOL_TIMEOUT_SECS` | MCP call timeout; default 60 |
| `MEDUSA_MCP_DEBUG` | Set to `1` to log MCP traffic under `.medusa/logs/` |
| `MEDUSA_WORKFLOW_MAX_SCRIPT_AGENTS` | Agents allowed in one workflow; default 200 |
| `MEDUSA_WORKFLOW_MAX_PARALLEL` | Concurrent workflow agents; default 8 |
| `MEDUSA_WORKFLOW_SCRIPT_TIMEOUT_SECS` | JavaScript deadline; default 3600, `0` disables |
| `MEDUSA_WORKFLOW_SCRIPT_MEMORY_MB` | JavaScript heap cap; default 128 MB |
| `MEDUSA_NO_PROGRESS_REPETITIONS` | Equivalent outcomes before stopping; default 3 |
| `MEDUSA_HOOK_TIMEOUT_SECS` | Default hook timeout; default 30 |
| `MEDUSA_BELL` | `on` or `off` |
| `MEDUSA_THEME` | Startup theme |
| `CODEX_HOME` | Codex credentials directory; default `~/.codex` |

## Workspace State

Sessions, checkpoints, permission grants, attachments, and workflow journals
live under `.medusa/`. Files are written atomically with private permissions.
Keep `.medusa/` in `.gitignore`; it can contain prompts, source excerpts, and
model output.
