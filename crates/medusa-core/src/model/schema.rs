use std::path::Path;

use crate::harness::HarnessPolicy;
use crate::model::types::*;
use serde_json::{Value, json};

pub(crate) fn medusa_instructions(
    workspace: &Path,
    state: &ToolLoopState,
    policy: HarnessPolicy,
    skill_context: Option<&str>,
) -> String {
    let mut instructions = format!(
        "You are Medusa, a terminal-native autonomous coding agent. \
You help the user inspect, edit, test, debug, and evolve the current workspace through Medusa's tool loop. \
Current workspace: {}. \
Use fs_list to discover the workspace tree, file_search to find text, and file_read to read exact files or line ranges. \
For nontrivial code tasks, prefer explore_batch first: fan out read-only list/search/read/safe terminal probes in parallel, then synthesize the evidence before editing. \
Use terminal_exec for tests, builds, formatters, git, project scripts, and uncommon shell work. \
Do not write Python/shell just to list, search, or read files when a native Medusa file tool can do it. \
Prefer targeted tool calls that produce compact output; avoid dumping entire large files unless necessary. \
When file_edit is available, prefer it for exact old/new string or block replacement in one file. \
When file_patch is available, use Codex-style *** Begin Patch envelopes for multi-file edits, add/delete/move operations, or changes that are clearer as hunks; unified git diffs are supported as compatibility. \
Use task_update sparingly for user-visible phase changes during longer work. \
Use plan_update for nontrivial multi-step work: publish a concise checklist before acting, then update it as steps move from pending to active, done, or blocked. \
Do not use plan_update for casual chat or one-step answers. \
Use decision_request during planning when one or more user decisions materially change the plan. Pair it with plan_update, include assumptions, then stop and wait for the user answer before executing the affected work. \
Use question only when you genuinely need user input that cannot be safely inferred or discovered from the workspace. \
Never claim that tools are unavailable or that the tool loop is not attached. \
Do not narrate raw tool JSON or full command output back to the user unless it is directly relevant. \
Summarize what changed, what passed/failed, and next steps. \
For normal chat, answer naturally and concisely. \
\
Harness contract: {} \
Harness law: explore in parallel; act serialized. Use parallel exploration for context gathering and verification, but keep edits/patches/mutating commands in a single coherent lane. \
Turn mode: {}. \
{}",
        workspace.display(),
        crate::harness::core_harness_contract(),
        policy.mode_label(),
        policy.instructions()
    );

    if state.patch_requires_context {
        instructions.push_str(
            " A previous edit/patch failed, so mutation tools are temporarily withheld until you inspect the current file/context with file_read, file_search, fs_list, or terminal_exec. Do not retry blind edits.",
        );
    }

    if let Some(skill_context) = skill_context {
        instructions.push_str("\n\n");
        instructions.push_str(skill_context);
    }

    instructions
}

pub(crate) fn medusa_tools(allow_patch: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "name": "file_read",
            "description": "Read one or more workspace files with optional line bounds. Prefer this over shell/Python for viewing files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Workspace-relative file paths to read."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Optional 1-based starting line."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Optional 1-based ending line."
                    }
                },
                "required": ["paths"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "file_search",
            "description": "Search text in workspace files. Prefer this over grep/rg when you only need matches.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional workspace-relative file or directory to search."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Optional recursive directory depth."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Optional maximum number of matching lines."
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Whether the search is case-sensitive. Defaults to true."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "fs_list",
            "description": "List workspace files and directories. Prefer this over shell/Python for directory discovery.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional workspace-relative directory or file path."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Optional recursive depth."
                    },
                    "max_entries": {
                        "type": "integer",
                        "description": "Optional maximum number of returned entries."
                    }
                },
                "required": [],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "explore_batch",
            "description": "Run multiple read-only exploration probes in parallel and return one compact evidence board. Use before nontrivial edits to gather context fast. Probes may list files, search text, read file ranges, or run conservative read-only terminal checks.",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Short reason for the exploration batch."
                    },
                    "probes": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 12,
                        "description": "Read-only probes to run concurrently. Keep them focused and compact.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "description": "Probe kind: list, search, read, or terminal."
                                },
                                "query": {
                                    "type": "string",
                                    "description": "Search query for kind=search."
                                },
                                "path": {
                                    "type": "string",
                                    "description": "Workspace-relative path for list/search."
                                },
                                "paths": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Workspace-relative files for kind=read."
                                },
                                "command": {
                                    "type": "string",
                                    "description": "Read-only shell command for kind=terminal. Mutating commands are rejected."
                                },
                                "cwd": {
                                    "type": "string",
                                    "description": "Optional workspace-relative cwd for terminal probes."
                                },
                                "start_line": {
                                    "type": "integer",
                                    "description": "Optional 1-based start line for read probes."
                                },
                                "end_line": {
                                    "type": "integer",
                                    "description": "Optional 1-based end line for read probes."
                                },
                                "depth": {
                                    "type": "integer",
                                    "description": "Optional depth for list/search probes."
                                },
                                "max_results": {
                                    "type": "integer",
                                    "description": "Optional match cap for search probes."
                                },
                                "max_entries": {
                                    "type": "integer",
                                    "description": "Optional entry cap for list probes."
                                },
                                "case_sensitive": {
                                    "type": "boolean",
                                    "description": "Whether search is case-sensitive."
                                }
                            },
                            "required": ["kind"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["probes"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "terminal_exec",
            "description": "Run a shell command inside the Medusa workspace. Use for tests, builds, formatters, git, project scripts, and uncommon shell work.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to run."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional workspace-relative directory for the command."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run the command as a background shell job and return immediately. Use only for long-running servers/watchers or tasks the user explicitly wants in the background."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "task_update",
            "description": "Update Medusa's visible status line.",
            "parameters": {
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Short status text."
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "plan_update",
            "description": "Replace Medusa's visible task plan with a concise checklist for nontrivial work. Use before acting on multi-step tasks and update statuses as work progresses.",
            "parameters": {
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Optional short title or current objective for this plan."
                    },
                    "items": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 12,
                        "description": "Ordered task steps. Keep each step short and user-visible.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {
                                    "type": "string",
                                    "description": "Concise step text."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "active", "done", "blocked"],
                                    "description": "Current step status. At most one item should be active."
                                },
                                "evidence": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Optional short evidence notes, file paths, or test results backing this step."
                                }
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "question",
            "description": "Ask the user a concise blocking question when progress would be risky without your answer.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "One concise question for the user."
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }
        }),
        json!({
            "type": "function",
            "name": "decision_request",
            "description": "Create a visible planning decision queue when user choices materially affect the plan. Use after or alongside plan_update, then wait for the user to answer.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short heading for the decision group."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why these decisions matter to the plan."
                    },
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 8,
                        "description": "Planning questions the user should answer before execution.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Stable short identifier, such as storage or ui_mode."
                                },
                                "prompt": {
                                    "type": "string",
                                    "description": "One concise decision question."
                                },
                                "kind": {
                                    "type": "string",
                                    "enum": ["choice", "text"],
                                    "description": "Use choice when options are known; use text only when free-form input is required."
                                },
                                "options": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Choice labels. Required for kind=choice."
                                },
                                "recommended": {
                                    "type": "string",
                                    "description": "Recommended option or answer, if any."
                                },
                                "required": {
                                    "type": "boolean",
                                    "description": "Whether execution should wait for this answer. Defaults to true."
                                }
                            },
                            "required": ["id", "prompt", "kind"],
                            "additionalProperties": false
                        }
                    },
                    "assumptions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Assumptions Medusa will use if a non-required decision is unanswered."
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }
        }),
    ];

    if allow_patch {
        tools.insert(
            3,
            json!({
                "type": "function",
                "name": "file_edit",
                "description": "Edit one file by replacing an exact oldString with newString. Existing files require non-empty oldString. For new files, pass an empty oldString and the new file content in newString.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path to edit or create."
                        },
                        "oldString": {
                            "type": "string",
                            "description": "Exact existing text to replace. Use an empty string only when creating a new file."
                        },
                        "newString": {
                            "type": "string",
                            "description": "Replacement text or new file content."
                        },
                        "replaceAll": {
                            "type": "boolean",
                            "description": "Replace every occurrence. Defaults to false; without this, multiple matches fail."
                        }
                    },
                    "required": ["path", "oldString", "newString"],
                    "additionalProperties": false
                }
            }),
        );
        tools.insert(
            4,
            json!({
                "type": "function",
                "name": "file_patch",
                "description": "Apply a Codex-style *** Begin Patch envelope or unified git diff in the Medusa workspace. Prefer Codex-style patches for multi-file edits, add/delete/move operations, and hunk-based edits. If patching fails, inspect the live file with file_read/file_search/fs_list or terminal_exec before retrying.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "diff": {
                            "type": "string",
                            "description": "Codex-style patch envelope or unified git diff."
                        },
                        "description": {
                            "type": "string",
                            "description": "Short human-readable reason for the patch."
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Optional workspace-relative directory where the patch should apply."
                        }
                    },
                    "required": ["diff"],
                    "additionalProperties": false
                }
            }),
        );
    }

    tools
}

pub(crate) fn chat_completion_tools(allow_patch: bool) -> Vec<Value> {
    medusa_tools(allow_patch)
        .into_iter()
        .filter_map(|tool| {
            Some(json!({
                "type": "function",
                "function": {
                    "name": tool.get("name")?.clone(),
                    "description": tool.get("description")?.clone(),
                    "parameters": tool.get("parameters")?.clone(),
                }
            }))
        })
        .collect()
}

pub(crate) fn deepseek_reasoning_effort(effort: &str) -> &'static str {
    match effort
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], "")
        .as_str()
    {
        "xhigh" | "max" => "max",
        _ => "high",
    }
}
