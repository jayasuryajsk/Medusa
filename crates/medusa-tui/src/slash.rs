use std::{collections::VecDeque, fs, path::Path};

use color_eyre::eyre::{Result, WrapErr};
use medusa_core::persistence::atomic_write;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::constants::{MENTION_SKIP_DIRS, QUICK_MEMORY_HEADER, QUICK_MEMORY_SECTION};
use crate::styles::{accent, muted, prompt_style, value_style};

pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) args: &'static str,
    pub(crate) category: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        args: "",
        category: "system",
        description: "Show available slash commands",
    },
    SlashCommand {
        name: "/settings",
        args: "",
        category: "system",
        description: "Show current Medusa settings",
    },
    SlashCommand {
        name: "/model",
        args: "<name>",
        category: "model",
        description: "Choose the model and execution mode for new turns",
    },
    SlashCommand {
        name: "/reasoning",
        args: "[effort]",
        category: "model",
        description: "Set thinking effort, or Ultra orchestration when supported",
    },
    SlashCommand {
        name: "/permissions",
        args: "<mode>",
        category: "system",
        description: "Change terminal and file mutation permissions",
    },
    SlashCommand {
        name: "/reload",
        args: "",
        category: "system",
        description: "Restart Medusa and continue the current session",
    },
    SlashCommand {
        name: "/plan",
        args: "",
        category: "agent",
        description: "Toggle plan mode: explore read-only and propose a plan before editing",
    },
    SlashCommand {
        name: "/workflows",
        args: "",
        category: "view",
        description: "Show workflow runs and subagent progress",
    },
    SlashCommand {
        name: "/workflow",
        args: "<script|task> [args]",
        category: "agent",
        description: "Run a saved or model-authored JavaScript workflow",
    },
    SlashCommand {
        name: "/sessions",
        args: "",
        category: "session",
        description: "Browse workspace session files",
    },
    SlashCommand {
        name: "/tree",
        args: "",
        category: "session",
        description: "Show workspace session branches",
    },
    SlashCommand {
        name: "/resume",
        args: "<session>",
        category: "session",
        description: "Switch to a saved workspace session",
    },
    SlashCommand {
        name: "/fork",
        args: "",
        category: "session",
        description: "Fork the current session before risky work",
    },
    SlashCommand {
        name: "/rewind",
        args: "",
        category: "session",
        description: "Restore files to the state before a previous turn",
    },
    SlashCommand {
        name: "/edit",
        args: "",
        category: "session",
        description: "Edit a previous message and resend from there (forks the timeline)",
    },
    SlashCommand {
        name: "/review",
        args: "",
        category: "tools",
        description: "Seed the composer with a code-review prompt for pending changes",
    },
    SlashCommand {
        name: "/clear",
        args: "",
        category: "session",
        description: "Clear the current transcript",
    },
    SlashCommand {
        name: "/cost",
        args: "",
        category: "context",
        description: "Show session and last-turn token usage",
    },
    SlashCommand {
        name: "/context",
        args: "",
        category: "context",
        description: "Show estimated context usage against the token budget",
    },
    SlashCommand {
        name: "/compact",
        args: "",
        category: "context",
        description: "Summarize older history now to free context",
    },
    SlashCommand {
        name: "/theme",
        args: "<name>",
        category: "theme",
        description: "Switch UI theme",
    },
    SlashCommand {
        name: "/tools",
        args: "",
        category: "tools",
        description: "List model-accessible local tools",
    },
    SlashCommand {
        name: "/skills",
        args: "",
        category: "context",
        description: "List workspace skills",
    },
    SlashCommand {
        name: "/agents",
        args: "",
        category: "context",
        description: "List named agents defined in .medusa/agents",
    },
    SlashCommand {
        name: "/mcp",
        args: "[restart <server>]",
        category: "tools",
        description: "List MCP servers and their tools, or restart one",
    },
    SlashCommand {
        name: "/auth",
        args: "",
        category: "system",
        description: "Check the active provider and auth status",
    },
    SlashCommand {
        name: "/jobs",
        args: "",
        category: "tools",
        description: "Show background shell jobs",
    },
    SlashCommand {
        name: "/kill",
        args: "<job-id>",
        category: "tools",
        description: "Stop a running background job",
    },
    SlashCommand {
        name: "/tail",
        args: "<job-id>",
        category: "tools",
        description: "Show captured output for a background job",
    },
    SlashCommand {
        name: "/restart",
        args: "<job-id>",
        category: "tools",
        description: "Restart a background job command",
    },
    SlashCommand {
        name: "/exec",
        args: "<command>",
        category: "tools",
        description: "Run a shell command in this workspace",
    },
    SlashCommand {
        name: "/patch",
        args: "<path>",
        category: "tools",
        description: "Apply a unified diff file with git apply",
    },
];

pub(crate) const THEME_SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/theme next",
        args: "",
        category: "theme",
        description: "Switch to the next UI theme",
    },
    SlashCommand {
        name: "/theme prev",
        args: "",
        category: "theme",
        description: "Switch to the previous UI theme",
    },
    SlashCommand {
        name: "/theme medusa",
        args: "",
        category: "theme",
        description: "Sharp black, acid green, warm prompt accents",
    },
    SlashCommand {
        name: "/theme opencode",
        args: "",
        category: "theme",
        description: "Quiet blue command surface with crisp contrast",
    },
    SlashCommand {
        name: "/theme tokyonight",
        args: "",
        category: "theme",
        description: "Deep navy with cyan highlights",
    },
    SlashCommand {
        name: "/theme catppuccin",
        args: "",
        category: "theme",
        description: "Soft mocha surface with rosewater accents",
    },
    SlashCommand {
        name: "/theme dracula",
        args: "",
        category: "theme",
        description: "Inky violet with neon pink and green highlights",
    },
    SlashCommand {
        name: "/theme nord",
        args: "",
        category: "theme",
        description: "Arctic blue-gray calm with frosty cyan accents",
    },
    SlashCommand {
        name: "/theme gruvbox",
        args: "",
        category: "theme",
        description: "Retro warm earth tones with punchy orange prompts",
    },
    SlashCommand {
        name: "/theme solarized-dark",
        args: "",
        category: "theme",
        description: "Low-glare teal base with balanced amber accents",
    },
    SlashCommand {
        name: "/theme material-dark",
        args: "",
        category: "theme",
        description: "Blue-grey Material base with balanced teal and amber",
    },
    SlashCommand {
        name: "/theme material-teal",
        args: "",
        category: "theme",
        description: "Material teal command surface with cyan tool accents",
    },
    SlashCommand {
        name: "/theme material-amber",
        args: "",
        category: "theme",
        description: "Material amber selection with teal prompts",
    },
    SlashCommand {
        name: "/theme material-indigo",
        args: "",
        category: "theme",
        description: "Material indigo focus with light-blue tooling",
    },
    SlashCommand {
        name: "/theme material-rose",
        args: "",
        category: "theme",
        description: "Material rose accents with teal supporting signals",
    },
    SlashCommand {
        name: "/theme rose-pine",
        args: "",
        category: "theme",
        description: "Muted rose and gold over a soho-night violet base",
    },
    SlashCommand {
        name: "/theme ayu-mirage",
        args: "",
        category: "theme",
        description: "Dusky slate with warm orange and sky-blue accents",
    },
    SlashCommand {
        name: "/theme everforest",
        args: "",
        category: "theme",
        description: "Soft forest greens with warm bark and sage tones",
    },
    SlashCommand {
        name: "/theme vesper",
        args: "",
        category: "theme",
        description: "Near-black minimalism with a single peach accent",
    },
];

/// Score a command against the palette query. Lower scores rank higher. The
/// returned positions are byte offsets of matched characters inside the
/// command name without its leading slash, used for match highlighting.
pub(crate) fn slash_match(command: &SlashCommand, query: &str) -> Option<(u8, Vec<usize>)> {
    if query.is_empty() {
        return Some((10, Vec::new()));
    }

    let name = command.name.trim_start_matches('/').to_ascii_lowercase();
    if name == query {
        return Some((0, (0..name.len()).collect()));
    }
    if let Some(start) = name.find(query) {
        let score = if start == 0 { 1 } else { 2 };
        return Some((score, (start..start + query.len()).collect()));
    }
    if let Some(positions) = subsequence_positions(&name, query) {
        return Some((3, positions));
    }

    let category = command.category.to_ascii_lowercase();
    let description = command.description.to_ascii_lowercase();
    let args = command.args.to_ascii_lowercase();
    if category.starts_with(query) {
        Some((4, Vec::new()))
    } else if category.contains(query) {
        Some((5, Vec::new()))
    } else if description.contains(query) {
        Some((6, Vec::new()))
    } else if args.contains(query) {
        Some((7, Vec::new()))
    } else {
        None
    }
}

/// Score a workspace path against an @mention query (already lowercased).
/// Lower scores rank first: 0 file-name prefix, 1 any path-segment prefix,
/// 2 substring, 3 subsequence. Matched byte positions index the full path.
pub(crate) fn mention_match(path: &str, query: &str) -> Option<(u8, Vec<usize>)> {
    if query.is_empty() {
        return Some((4, Vec::new()));
    }

    let lower = path.to_ascii_lowercase();
    let name_start = lower.rfind('/').map(|index| index + 1).unwrap_or(0);
    if lower[name_start..].starts_with(query) {
        return Some((0, (name_start..name_start + query.len()).collect()));
    }
    for (index, _) in lower.match_indices('/') {
        let segment_start = index + 1;
        if lower[segment_start..].starts_with(query) {
            return Some((1, (segment_start..segment_start + query.len()).collect()));
        }
    }
    if lower.starts_with(query) {
        return Some((1, (0..query.len()).collect()));
    }
    if let Some(start) = lower.find(query) {
        return Some((2, (start..start + query.len()).collect()));
    }
    subsequence_positions(&lower, query).map(|positions| (3, positions))
}

/// Breadth-first workspace file walk for the mention picker: relative paths
/// with '/' separators, junk directories skipped, capped at `max` entries.
/// Breadth-first order means shallow files survive the cap in big trees.
pub(crate) fn collect_workspace_files(root: &Path, max: usize) -> Vec<String> {
    let mut files = Vec::new();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if files.len() >= max {
                return files;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                if !MENTION_SKIP_DIRS.contains(&name.as_str()) {
                    queue.push_back(entry.path());
                }
            } else if file_type.is_file()
                && name != ".DS_Store"
                && let Ok(relative) = entry.path().strip_prefix(root)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files
}

/// Append `- <note>` under the `## Notes` section of AGENTS.md at the
/// workspace root, creating the file or the section when missing. The model
/// sees the note automatically on the next turn: project instructions are
/// reloaded from AGENTS.md at the start of every turn.
pub(crate) fn append_quick_memory(workspace: &Path, note: &str) -> Result<()> {
    let path = workspace.join("AGENTS.md");
    let existing = if path.exists() {
        fs::read_to_string(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };
    let updated = quick_memory_content(&existing, note);
    atomic_write(&path, updated).wrap_err_with(|| format!("failed to write {}", path.display()))
}

/// Collapse a quick-memory note to a single safe line so it can never forge a
/// new AGENTS.md section. Embedded newlines (and the whitespace around them)
/// fold into single spaces, and a leading `#` run is stripped — a multi-line
/// paste like `deploy\n## Deploy\nrun` becomes one bullet, and the insert-point
/// scan (which breaks on any line starting with `#`) always steps over it.
pub(crate) fn sanitize_quick_memory_note(note: &str) -> String {
    let collapsed = note
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.trim_start_matches('#').trim_start().to_string()
}

/// Pure content transform behind [`append_quick_memory`]: the note lands at
/// the end of the `## Notes` section (before any following heading),
/// creating the file skeleton or the section when absent.
pub(crate) fn quick_memory_content(existing: &str, note: &str) -> String {
    let note = sanitize_quick_memory_note(note);
    let bullet = format!("- {note}");
    if existing.trim().is_empty() {
        return format!("{QUICK_MEMORY_HEADER}\n\n{QUICK_MEMORY_SECTION}\n\n{bullet}\n");
    }

    let mut lines = existing.lines().map(str::to_string).collect::<Vec<_>>();
    let Some(section) = lines
        .iter()
        .position(|line| line.trim() == QUICK_MEMORY_SECTION)
    else {
        let mut result = existing.trim_end().to_string();
        result.push_str(&format!("\n\n{QUICK_MEMORY_SECTION}\n\n{bullet}\n"));
        return result;
    };

    let mut insert_at = lines.len();
    for (index, line) in lines.iter().enumerate().skip(section + 1) {
        if line.trim_start().starts_with('#') {
            insert_at = index;
            break;
        }
    }
    while insert_at > section + 1 && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }
    lines.insert(insert_at, bullet);
    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Fuzzy subsequence match: every query character appears in order in the
/// name (so "wf" matches "workflow"). Returns matched byte positions.
pub(crate) fn subsequence_positions(name: &str, query: &str) -> Option<Vec<usize>> {
    if query.chars().count() < 2 {
        return None;
    }
    let mut positions = Vec::new();
    let mut name_chars = name.char_indices();
    for query_char in query.chars() {
        loop {
            let (index, name_char) = name_chars.next()?;
            if name_char == query_char {
                positions.push(index);
                break;
            }
        }
    }
    Some(positions)
}

/// Render a workspace path with fuzzy-matched bytes highlighted.
pub(crate) fn mention_path_spans(path: &str, positions: &[usize]) -> Vec<Span<'static>> {
    path.char_indices()
        .map(|(index, ch)| {
            let style = if positions.contains(&index) {
                accent().add_modifier(Modifier::BOLD)
            } else {
                value_style()
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

/// Render a command name with matched characters highlighted, padded to
/// `width` display columns.
pub(crate) fn highlighted_command_name_spans(
    name: &'static str,
    positions: &[usize],
    width: usize,
) -> Vec<Span<'static>> {
    let body = name.trim_start_matches('/');
    let mut spans = vec![Span::styled("/", prompt_style())];
    for (index, ch) in body.char_indices() {
        let style = if positions.contains(&index) {
            accent().add_modifier(Modifier::BOLD)
        } else {
            prompt_style()
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    let used = 1 + body.chars().count();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    spans
}

pub(crate) fn command_palette_detail_lines(command: &SlashCommand) -> Vec<Line<'static>> {
    let signature = if command.args.is_empty() {
        command.name.to_string()
    } else {
        format!("{} {}", command.name, command.args)
    };
    let action = if command.args.is_empty() {
        "Enter runs immediately."
    } else {
        "Enter places the command in the composer."
    };

    vec![
        Line::from(vec![
            Span::styled(command.category, muted().add_modifier(Modifier::BOLD)),
            Span::styled("  ", muted()),
            Span::styled(signature, prompt_style()),
        ]),
        Line::from(""),
        Line::from(Span::styled(command.description, value_style())),
        Line::from(""),
        Line::from(vec![
            Span::styled("action  ", muted()),
            Span::styled(action, value_style()),
        ]),
        Line::from(vec![
            Span::styled("scope   ", muted()),
            Span::styled(command.category, muted()),
        ]),
    ]
}

pub(crate) fn tools_text() -> String {
    [
        "tools",
        "explore.batch  Run parallel read-only probes and return evidence",
        "file.read      Read files by path/range",
        "file.search    Search file contents by regex",
        "semantic.search Find code by meaning (local index)",
        "file.glob      Find files by name pattern",
        "fs.list        List workspace paths",
        "file.edit      Replace exact old/new strings",
        "file.patch     Apply Codex patches or git diffs",
        "terminal.exec  Run shell commands/tests/builds",
        "task.update    Update current status",
        "plan.update    Replace visible task checklist",
        "decision.request Queue planning questions for the user",
    ]
    .join("\n")
}
