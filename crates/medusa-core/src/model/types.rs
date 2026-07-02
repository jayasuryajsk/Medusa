use std::path::PathBuf;

const CODEX_BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "gpt-5.5";
const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-flash";
pub(crate) const DEFAULT_CONTEXT_MAX_CHARS: usize = 120_000;

#[derive(Debug, Clone)]
pub struct DirectCodexBackend {
    pub(crate) workspace: PathBuf,
    pub(crate) provider: ModelProvider,
    pub(crate) provider_locked: bool,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) chat_base_url: String,
    pub(crate) chat_api_key: Option<String>,
    pub(crate) client: reqwest::blocking::Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelProvider {
    Codex,
    DeepSeek,
    OpenAiCompatible,
}

impl ModelProvider {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "codex" | "openai-codex" => Some(Self::Codex),
            "deepseek" => Some(Self::DeepSeek),
            "openai" | "openai-compatible" | "chat-completions" | "compatible" => {
                Some(Self::OpenAiCompatible)
            }
            _ => None,
        }
    }

    pub(crate) fn infer_from_model(model: &str) -> Option<Self> {
        let normalized = model.trim().to_ascii_lowercase();
        if normalized.starts_with("deepseek") {
            Some(Self::DeepSeek)
        } else if normalized.starts_with("gpt-") {
            Some(Self::Codex)
        } else {
            None
        }
    }

    pub(crate) fn default_model(self) -> &'static str {
        match self {
            Self::Codex => DEFAULT_MODEL,
            Self::DeepSeek => DEFAULT_DEEPSEEK_MODEL,
            Self::OpenAiCompatible => DEFAULT_MODEL,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::DeepSeek => "deepseek",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }

    pub(crate) fn base_url(self) -> String {
        match self {
            Self::Codex => CODEX_BACKEND_URL.to_string(),
            Self::DeepSeek => std::env::var("MEDUSA_DEEPSEEK_BASE_URL")
                .or_else(|_| std::env::var("DEEPSEEK_BASE_URL"))
                .unwrap_or_else(|_| DEEPSEEK_BASE_URL.to_string()),
            Self::OpenAiCompatible => std::env::var("MEDUSA_OPENAI_BASE_URL")
                .or_else(|_| std::env::var("OPENAI_BASE_URL"))
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        }
    }

    pub(crate) fn api_key(self) -> Option<String> {
        match self {
            Self::Codex => None,
            Self::DeepSeek => {
                api_key_from_env_or_launchctl(&["MEDUSA_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"])
            }
            Self::OpenAiCompatible => api_key_from_env_or_launchctl(&[
                "MEDUSA_OPENAI_API_KEY",
                "MEDUSA_API_KEY",
                "OPENAI_API_KEY",
            ]),
        }
    }

    pub(crate) fn auth_hint(self) -> &'static str {
        match self {
            Self::Codex => "Codex OAuth",
            Self::DeepSeek => "env var `DEEPSEEK_API_KEY` or `MEDUSA_DEEPSEEK_API_KEY`",
            Self::OpenAiCompatible => {
                "env var `MEDUSA_OPENAI_API_KEY`, `MEDUSA_API_KEY`, or `OPENAI_API_KEY`"
            }
        }
    }
}

pub(crate) fn api_key_from_env_or_launchctl(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }

    for key in keys {
        if let Some(value) = launchctl_getenv(key) {
            return Some(value);
        }
    }

    None
}

fn launchctl_getenv(key: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("launchctl")
            .arg("getenv")
            .arg(key)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?;
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        None
    }
}

pub(crate) fn is_mutation_tool(name: &str) -> bool {
    matches!(name, "file_edit" | "file_patch")
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChatResult {
    pub response: String,
    pub event_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStreamEvent {
    Delta(String),
    ReasoningDelta(String),
    ToolStart { name: String, summary: String },
    ToolResult { name: String, output: String },
    Done { event_count: usize },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub attachments: Vec<ConversationAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationAttachment {
    pub mime: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
    pub(crate) name: String,
    pub(crate) call_id: String,
    pub(crate) arguments: String,
    pub(crate) reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolLoopState {
    pub(crate) patch_requires_context: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ToolLoopPolicy {
    MutationAllowed,
    ReadOnly,
}

impl ToolLoopPolicy {
    pub(crate) fn mutation_allowed() -> Self {
        Self::MutationAllowed
    }

    pub(crate) fn read_only() -> Self {
        Self::ReadOnly
    }

    pub(crate) fn allow_mutation(self) -> bool {
        matches!(self, Self::MutationAllowed)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ToolExecution {
    pub(crate) output: String,
    pub(crate) failed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnOutcome {
    pub(crate) event_count: usize,
    pub(crate) tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PartialChatToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}
