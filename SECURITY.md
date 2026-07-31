# Security Policy

Medusa executes model-selected commands and stores local conversation state.
Treat it as a powerful developer tool, not as a security boundary by itself.

## Reporting a Vulnerability

Please report suspected vulnerabilities privately through
[GitHub Security Advisories](https://github.com/jayasuryajsk/Medusa/security/advisories/new).
Do not open a public issue for credential exposure, sandbox escape, approval
bypass, arbitrary file access, or command-execution vulnerabilities.

Include the affected version and operating system, permission mode, a minimal
reproduction, expected versus observed behavior, and any logs with credentials
removed. We will acknowledge the report, investigate it, and coordinate a fix
and disclosure with the reporter.

## Supported Versions

Security fixes target the latest released version. Pre-release builds and old
minor versions may require upgrading before a fix is provided.

## Security Model

- Fresh workspaces start in `guarded` permission mode.
- Guarded and readonly commands use macOS Seatbelt or Linux bubblewrap and
  fail closed if the requested sandbox is unavailable.
- Unsandboxed execution requires open mode or an explicit approval.
- File tools reject parent traversal and symlink escapes.
- Pre-edit checkpoints support workspace rewind, but cannot undo side effects
  made by arbitrary shell commands.
- Session state, approval grants, attachments, and workflow journals under
  `.medusa/` may contain sensitive source code and prompts. They are restricted
  to the current user on Unix and should never be committed.
- `.medusa/hooks.json`, `.medusa/mcp.json`, custom agents, and workflow scripts
  are trusted project configuration. Review them before running Medusa in an
  unfamiliar repository.
- Medusa reads an existing Codex OAuth cache when the Codex provider is used.
  It does not print the token, but any process running as the same user may be
  able to read that cache.

No sandbox eliminates all risk. Use `readonly` for unfamiliar repositories,
review approval prompts carefully, and run sensitive work in a disposable VM
or container.
