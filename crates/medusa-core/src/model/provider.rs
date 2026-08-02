//! Provider registry, model catalog, and provider-neutral model selection.
//!
//! Medusa keeps its agent loop independent from model vendors. Providers
//! describe how to reach a model endpoint, how to authenticate, and which
//! protocol/capabilities it supports. Built-ins are merged with optional
//! global and workspace JSON overlays, so adding an OpenAI-compatible service
//! does not require changing the harness.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

use crate::models::{DEFAULT_REASONING_EFFORTS, ModelInfo, ReasoningLevel, codex_backend_models};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelReference {
    provider: String,
    model: String,
}

impl ModelReference {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let provider = normalize_id(&provider.into());
        let model = model.into().trim().to_string();
        if provider.is_empty() {
            bail!("model provider cannot be empty");
        }
        if model.is_empty() {
            bail!("model id cannot be empty");
        }
        Ok(Self { provider, model })
    }

    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let Some((provider, model)) = value.split_once('/') else {
            bail!("model reference `{value}` must use provider/model syntax");
        };
        Self::new(provider, model)
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn as_string(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

impl std::fmt::Display for ModelReference {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderProtocol {
    #[serde(rename = "codex-responses")]
    CodexResponses,
    #[serde(rename = "openai-responses", alias = "open-ai-responses")]
    OpenAiResponses,
    #[serde(
        rename = "openai-chat",
        alias = "open-ai-chat",
        alias = "chat-completions"
    )]
    OpenAiChat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderAuth {
    CodexOauth,
    Bearer,
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingDialect {
    #[default]
    None,
    Openai,
    Openrouter,
    Deepseek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelCapabilities {
    pub tools: bool,
    pub reasoning: bool,
    pub images: bool,
    pub parallel_tools: bool,
    pub prompt_cache: bool,
}

impl ModelCapabilities {
    pub const fn coding_default() -> Self {
        Self {
            tools: true,
            reasoning: true,
            images: false,
            parallel_tools: true,
            prompt_cache: true,
        }
    }
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self::coding_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModel {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub default_reasoning: Option<String>,
    pub reasoning_levels: Vec<ReasoningLevel>,
    pub capabilities: ModelCapabilities,
    pub context_window: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDefinition {
    pub id: String,
    pub display_name: String,
    pub protocol: ProviderProtocol,
    pub auth: ProviderAuth,
    pub base_url: String,
    pub api_key_env: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub default_model: String,
    pub thinking: ThinkingDialect,
    pub capabilities: ModelCapabilities,
    pub models: BTreeMap<String, ProviderModel>,
}

impl ProviderDefinition {
    pub fn endpoint(&self, suffix: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }

    pub fn model_capabilities(&self, model: &str) -> ModelCapabilities {
        self.models
            .get(model)
            .map(|entry| entry.capabilities)
            .unwrap_or(self.capabilities)
    }

    pub fn auth_hint(&self) -> String {
        match self.auth {
            ProviderAuth::CodexOauth => "Codex OAuth (`codex login`)".to_string(),
            ProviderAuth::None => "no credentials".to_string(),
            ProviderAuth::Bearer if self.api_key_env.is_empty() => {
                format!("a stored API key for `{}`", self.id)
            }
            ProviderAuth::Bearer => format!(
                "a stored API key or {}",
                self.api_key_env
                    .iter()
                    .map(|key| format!("`{key}`"))
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderDefinition>,
}

impl ProviderRegistry {
    pub fn load(workspace: &Path) -> Result<Self> {
        let mut registry = Self::builtins();
        for path in provider_config_paths(workspace) {
            if path.exists() {
                registry.merge_file(&path)?;
            }
        }
        Ok(registry)
    }

    pub fn builtins() -> Self {
        let mut providers = BTreeMap::new();
        for provider in [
            codex_provider(),
            openai_provider(),
            deepseek_provider(),
            openrouter_provider(),
            ollama_provider(),
            lm_studio_provider(),
        ] {
            providers.insert(provider.id.clone(), provider);
        }
        Self { providers }
    }

    pub fn providers(&self) -> impl Iterator<Item = &ProviderDefinition> {
        self.providers.values()
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderDefinition> {
        self.providers.get(&canonical_provider_id(id))
    }

    pub fn resolve(&self, reference: &ModelReference) -> Result<&ProviderDefinition> {
        self.provider(reference.provider()).ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "unknown model provider `{}`; configure it in .medusa/providers.json",
                reference.provider()
            )
        })
    }

    pub fn default_reference_for(&self, provider: &str) -> Result<ModelReference> {
        let provider = self
            .provider(provider)
            .ok_or_else(|| color_eyre::eyre::eyre!("unknown model provider `{provider}`"))?;
        ModelReference::new(&provider.id, &provider.default_model)
    }

    /// Parse a full provider/model reference or upgrade a legacy bare model
    /// slug. Explicit providers win; otherwise known model prefixes preserve
    /// Medusa's previous Codex/DeepSeek behavior.
    pub fn select(&self, value: &str, provider_hint: Option<&str>) -> Result<ModelReference> {
        let value = value.trim();
        if value.contains('/') {
            let parsed = ModelReference::parse(value)?;
            let provider = canonical_provider_id(parsed.provider());
            self.resolve(&ModelReference::new(&provider, parsed.model())?)?;
            return ModelReference::new(provider, parsed.model());
        }

        if let Some(provider) = provider_hint.filter(|value| !value.trim().is_empty()) {
            let provider = canonical_provider_id(provider);
            self.provider(&provider)
                .ok_or_else(|| color_eyre::eyre::eyre!("unknown model provider `{provider}`"))?;
            return ModelReference::new(provider, value);
        }

        let provider = if value.to_ascii_lowercase().starts_with("deepseek") {
            "deepseek"
        } else {
            "codex"
        };
        ModelReference::new(provider, value)
    }

    pub fn model_catalog(&self) -> Vec<ModelInfo> {
        let mut catalog = Vec::new();
        let mut seen = BTreeSet::new();

        for provider in self.providers.values() {
            if provider.models.is_empty() {
                let reference = format!("{}/{}", provider.id, provider.default_model);
                if seen.insert(reference.clone()) {
                    catalog.push(ModelInfo {
                        slug: reference,
                        display_name: provider.default_model.clone(),
                        description: Some(format!(
                            "Configured default for {}",
                            provider.display_name
                        )),
                        default_reasoning: provider
                            .capabilities
                            .reasoning
                            .then(|| "medium".to_string()),
                        reasoning_levels: if provider.capabilities.reasoning {
                            reasoning_levels(
                                DEFAULT_REASONING_EFFORTS
                                    .iter()
                                    .map(|value| (*value).to_string())
                                    .collect(),
                            )
                        } else {
                            Vec::new()
                        },
                        context_window: None,
                    });
                }
            }
            for model in provider.models.values() {
                let reference = format!("{}/{}", provider.id, model.id);
                if seen.insert(reference.clone()) {
                    catalog.push(ModelInfo {
                        slug: reference,
                        display_name: model.display_name.clone(),
                        description: model.description.clone(),
                        default_reasoning: model.default_reasoning.clone(),
                        reasoning_levels: model.reasoning_levels.clone(),
                        context_window: model.context_window,
                    });
                }
            }
        }
        catalog
    }

    pub fn capabilities_for(&self, reference: &str) -> Option<ModelCapabilities> {
        let reference = if reference.contains('/') {
            ModelReference::parse(reference).ok()?
        } else {
            self.select(reference, None).ok()?
        };
        let provider = self.resolve(&reference).ok()?;
        Some(provider.model_capabilities(reference.model()))
    }

    fn merge_file(&mut self, path: &Path) -> Result<()> {
        let raw = fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read provider config {}", path.display()))?;
        let config: ProviderConfigFile = serde_json::from_str(&raw)
            .wrap_err_with(|| format!("failed to parse provider config {}", path.display()))?;
        for (id, overlay) in config.providers {
            self.merge_provider(&id, overlay)?;
        }
        Ok(())
    }

    fn merge_provider(&mut self, raw_id: &str, overlay: ProviderConfig) -> Result<()> {
        let id = canonical_provider_id(raw_id);
        if id.is_empty() {
            bail!("provider id cannot be empty");
        }

        let existing = self.providers.remove(&id);
        let protocol = overlay
            .protocol
            .or_else(|| existing.as_ref().map(|provider| provider.protocol))
            .ok_or_else(|| color_eyre::eyre::eyre!("custom provider `{id}` requires `protocol`"))?;
        let auth = overlay
            .auth
            .or_else(|| existing.as_ref().map(|provider| provider.auth))
            .unwrap_or(ProviderAuth::Bearer);
        let base_url = overlay
            .base_url
            .or_else(|| existing.as_ref().map(|provider| provider.base_url.clone()))
            .ok_or_else(|| color_eyre::eyre::eyre!("custom provider `{id}` requires `baseUrl`"))?;
        let base_url = validate_base_url(&id, &base_url)?;
        let default_model = overlay
            .default_model
            .or_else(|| {
                existing
                    .as_ref()
                    .map(|provider| provider.default_model.clone())
            })
            .or_else(|| overlay.models.keys().next().cloned())
            .ok_or_else(|| {
                color_eyre::eyre::eyre!("custom provider `{id}` requires `defaultModel`")
            })?;
        let default_model = default_model.trim().to_string();
        if default_model.is_empty() {
            bail!("provider `{id}` has an empty `defaultModel`");
        }

        let api_key_env = overlay
            .api_key_env
            .unwrap_or_else(|| {
                existing
                    .as_ref()
                    .map_or_else(Vec::new, |provider| provider.api_key_env.clone())
            })
            .into_iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let thinking = overlay
            .thinking
            .or_else(|| existing.as_ref().map(|provider| provider.thinking))
            .unwrap_or_default();
        let capabilities = overlay
            .capabilities
            .or_else(|| existing.as_ref().map(|provider| provider.capabilities))
            .unwrap_or_else(|| default_capabilities(protocol, thinking));

        let mut provider = ProviderDefinition {
            id: id.clone(),
            display_name: overlay
                .name
                .or_else(|| {
                    existing
                        .as_ref()
                        .map(|provider| provider.display_name.clone())
                })
                .unwrap_or_else(|| title_case_id(&id)),
            protocol,
            auth,
            base_url,
            api_key_env,
            headers: existing
                .as_ref()
                .map_or_else(BTreeMap::new, |provider| provider.headers.clone()),
            default_model,
            thinking,
            capabilities,
            models: existing.map_or_else(BTreeMap::new, |provider| provider.models),
        };
        provider.headers.extend(overlay.headers);

        for (model_id, model) in overlay.models {
            let model_id = model_id.trim().to_string();
            if model_id.is_empty() {
                bail!("provider `{id}` contains an empty model id");
            }
            let existing_model = provider.models.remove(&model_id);
            provider.models.insert(
                model_id.clone(),
                ProviderModel {
                    id: model_id.clone(),
                    display_name: model
                        .name
                        .or_else(|| {
                            existing_model
                                .as_ref()
                                .map(|entry| entry.display_name.clone())
                        })
                        .unwrap_or_else(|| model_id.clone()),
                    description: model.description.or_else(|| {
                        existing_model
                            .as_ref()
                            .and_then(|entry| entry.description.clone())
                    }),
                    default_reasoning: model.default_reasoning.or_else(|| {
                        existing_model
                            .as_ref()
                            .and_then(|entry| entry.default_reasoning.clone())
                    }),
                    reasoning_levels: model
                        .reasoning
                        .map(reasoning_levels)
                        .or_else(|| {
                            existing_model
                                .as_ref()
                                .map(|entry| entry.reasoning_levels.clone())
                        })
                        .unwrap_or_default(),
                    capabilities: model
                        .capabilities
                        .or_else(|| existing_model.as_ref().map(|entry| entry.capabilities))
                        .unwrap_or(provider.capabilities),
                    context_window: model.context_window.or_else(|| {
                        existing_model
                            .as_ref()
                            .and_then(|entry| entry.context_window)
                    }),
                },
            );
        }

        self.providers.insert(id, provider);
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
struct ProviderConfigFile {
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ProviderConfig {
    name: Option<String>,
    protocol: Option<ProviderProtocol>,
    auth: Option<ProviderAuth>,
    base_url: Option<String>,
    api_key_env: Option<Vec<String>>,
    headers: BTreeMap<String, String>,
    default_model: Option<String>,
    thinking: Option<ThinkingDialect>,
    capabilities: Option<ModelCapabilities>,
    models: BTreeMap<String, ProviderModelConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ProviderModelConfig {
    name: Option<String>,
    description: Option<String>,
    default_reasoning: Option<String>,
    reasoning: Option<Vec<String>>,
    capabilities: Option<ModelCapabilities>,
    context_window: Option<usize>,
}

fn provider_config_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(global) = global_config_dir() {
        paths.push(global.join("providers.json"));
    }
    paths.push(workspace.join(".medusa").join("providers.json"));
    paths
}

pub fn global_config_dir() -> Option<PathBuf> {
    env::var_os("MEDUSA_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("medusa")))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/medusa")))
}

pub fn canonical_provider_id(value: &str) -> String {
    let normalized = normalize_id(value);
    match normalized.as_str() {
        "openai-compatible" | "chat-completions" | "compatible" => "openai".to_string(),
        "openai-codex" => "codex".to_string(),
        "lm-studio" => "lmstudio".to_string(),
        _ => normalized,
    }
}

fn normalize_id(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}

fn title_case_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_base_url(provider: &str, value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&value)
        .wrap_err_with(|| format!("provider `{provider}` has an invalid `baseUrl`"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("provider `{provider}` baseUrl must use http or https");
    }
    Ok(value)
}

fn default_capabilities(
    protocol: ProviderProtocol,
    thinking: ThinkingDialect,
) -> ModelCapabilities {
    let mut capabilities = ModelCapabilities::coding_default();
    if protocol == ProviderProtocol::OpenAiChat && thinking == ThinkingDialect::None {
        capabilities.reasoning = false;
    }
    capabilities
}

fn reasoning_levels(values: Vec<String>) -> Vec<ReasoningLevel> {
    values
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .map(|effort| ReasoningLevel {
            effort,
            description: None,
        })
        .collect()
}

fn configured_model(
    id: &str,
    display_name: &str,
    description: &str,
    reasoning: &[&str],
    capabilities: ModelCapabilities,
) -> ProviderModel {
    ProviderModel {
        id: id.to_string(),
        display_name: display_name.to_string(),
        description: Some(description.to_string()),
        default_reasoning: reasoning.contains(&"medium").then(|| "medium".to_string()),
        reasoning_levels: reasoning_levels(
            reasoning.iter().map(|value| (*value).to_string()).collect(),
        ),
        capabilities,
        context_window: None,
    }
}

fn codex_provider() -> ProviderDefinition {
    let capabilities = ModelCapabilities {
        images: true,
        ..ModelCapabilities::coding_default()
    };
    let mut models = BTreeMap::new();
    if let Some(codex_models) = codex_backend_models() {
        for model in codex_models {
            models.insert(
                model.slug.clone(),
                ProviderModel {
                    id: model.slug,
                    display_name: model.display_name,
                    description: model.description,
                    default_reasoning: model.default_reasoning,
                    reasoning_levels: model.reasoning_levels,
                    capabilities,
                    context_window: model.context_window,
                },
            );
        }
    }
    if models.is_empty() {
        models.insert(
            "gpt-5.5".to_string(),
            configured_model(
                "gpt-5.5",
                "GPT-5.5",
                "Codex OAuth model",
                &["none", "low", "medium", "high", "xhigh", "max", "ultra"],
                capabilities,
            ),
        );
    }
    ProviderDefinition {
        id: "codex".to_string(),
        display_name: "Codex".to_string(),
        protocol: ProviderProtocol::CodexResponses,
        auth: ProviderAuth::CodexOauth,
        base_url: env::var("MEDUSA_CODEX_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string()),
        api_key_env: Vec::new(),
        headers: BTreeMap::new(),
        default_model: "gpt-5.5".to_string(),
        thinking: ThinkingDialect::None,
        capabilities,
        models,
    }
}

fn openai_provider() -> ProviderDefinition {
    let capabilities = ModelCapabilities {
        images: true,
        ..ModelCapabilities::coding_default()
    };
    let mut models = BTreeMap::new();
    models.insert(
        "gpt-5.5".to_string(),
        configured_model(
            "gpt-5.5",
            "GPT-5.5 (API)",
            "OpenAI Responses API",
            &["none", "low", "medium", "high", "xhigh"],
            capabilities,
        ),
    );
    ProviderDefinition {
        id: "openai".to_string(),
        display_name: "OpenAI".to_string(),
        protocol: ProviderProtocol::OpenAiResponses,
        auth: ProviderAuth::Bearer,
        base_url: env::var("MEDUSA_OPENAI_BASE_URL")
            .or_else(|_| env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        api_key_env: vec![
            "MEDUSA_OPENAI_API_KEY".to_string(),
            "MEDUSA_API_KEY".to_string(),
            "OPENAI_API_KEY".to_string(),
        ],
        headers: BTreeMap::new(),
        default_model: "gpt-5.5".to_string(),
        thinking: ThinkingDialect::None,
        capabilities,
        models,
    }
}

fn deepseek_provider() -> ProviderDefinition {
    let capabilities = ModelCapabilities::coding_default();
    let mut models = BTreeMap::new();
    let mut flash = configured_model(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        "DeepSeek native Responses API model",
        &["none", "low", "high", "max"],
        capabilities,
    );
    flash.default_reasoning = Some("high".to_string());
    flash.context_window = Some(1_000_000);
    models.insert("deepseek-v4-flash".to_string(), flash);
    ProviderDefinition {
        id: "deepseek".to_string(),
        display_name: "DeepSeek".to_string(),
        protocol: ProviderProtocol::OpenAiResponses,
        auth: ProviderAuth::Bearer,
        base_url: env::var("MEDUSA_DEEPSEEK_BASE_URL")
            .or_else(|_| env::var("DEEPSEEK_BASE_URL"))
            .unwrap_or_else(|_| "https://api.deepseek.com".to_string()),
        api_key_env: vec![
            "MEDUSA_DEEPSEEK_API_KEY".to_string(),
            "DEEPSEEK_API_KEY".to_string(),
        ],
        headers: BTreeMap::new(),
        default_model: "deepseek-v4-flash".to_string(),
        thinking: ThinkingDialect::Deepseek,
        capabilities,
        models,
    }
}

fn openrouter_provider() -> ProviderDefinition {
    ProviderDefinition {
        id: "openrouter".to_string(),
        display_name: "OpenRouter".to_string(),
        protocol: ProviderProtocol::OpenAiChat,
        auth: ProviderAuth::Bearer,
        base_url: "https://openrouter.ai/api/v1".to_string(),
        api_key_env: vec!["OPENROUTER_API_KEY".to_string()],
        headers: BTreeMap::new(),
        default_model: "openai/gpt-5.5".to_string(),
        thinking: ThinkingDialect::Openrouter,
        capabilities: ModelCapabilities::coding_default(),
        models: BTreeMap::new(),
    }
}

fn ollama_provider() -> ProviderDefinition {
    let capabilities = ModelCapabilities {
        reasoning: false,
        ..ModelCapabilities::coding_default()
    };
    ProviderDefinition {
        id: "ollama".to_string(),
        display_name: "Ollama".to_string(),
        protocol: ProviderProtocol::OpenAiChat,
        auth: ProviderAuth::None,
        base_url: env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".to_string()),
        api_key_env: Vec::new(),
        headers: BTreeMap::new(),
        default_model: "qwen3-coder".to_string(),
        thinking: ThinkingDialect::None,
        capabilities,
        models: BTreeMap::new(),
    }
}

fn lm_studio_provider() -> ProviderDefinition {
    let capabilities = ModelCapabilities {
        reasoning: false,
        ..ModelCapabilities::coding_default()
    };
    ProviderDefinition {
        id: "lmstudio".to_string(),
        display_name: "LM Studio".to_string(),
        protocol: ProviderProtocol::OpenAiChat,
        auth: ProviderAuth::None,
        base_url: env::var("LMSTUDIO_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:1234/v1".to_string()),
        api_key_env: Vec::new(),
        headers: BTreeMap::new(),
        default_model: "local-model".to_string(),
        thinking: ThinkingDialect::None,
        capabilities,
        models: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn workspace(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "medusa-provider-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn parses_provider_model_and_keeps_nested_model_ids() {
        let reference = ModelReference::parse("openrouter/anthropic/claude-sonnet").unwrap();
        assert_eq!(reference.provider(), "openrouter");
        assert_eq!(reference.model(), "anthropic/claude-sonnet");
    }

    #[test]
    fn upgrades_legacy_model_names_without_guessing_unknown_vendors() {
        let registry = ProviderRegistry::builtins();
        assert_eq!(
            registry
                .select("deepseek-v4-flash", None)
                .unwrap()
                .as_string(),
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(
            registry.select("gpt-5.5", None).unwrap().as_string(),
            "codex/gpt-5.5"
        );
        assert_eq!(
            registry
                .select("my-model", Some("ollama"))
                .unwrap()
                .as_string(),
            "ollama/my-model"
        );
    }

    #[test]
    fn deepseek_builtin_uses_native_responses_api() {
        let registry = ProviderRegistry::builtins();
        let provider = registry.provider("deepseek").unwrap();

        assert_eq!(provider.protocol, ProviderProtocol::OpenAiResponses);
        assert_eq!(
            provider.endpoint("responses"),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            provider.models["deepseek-v4-flash"].description.as_deref(),
            Some("DeepSeek native Responses API model")
        );
        assert_eq!(
            provider.models["deepseek-v4-flash"]
                .default_reasoning
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            provider.models["deepseek-v4-flash"]
                .reasoning_levels
                .iter()
                .map(|level| level.effort.as_str())
                .collect::<Vec<_>>(),
            vec!["none", "low", "high", "max"]
        );
        assert_eq!(
            provider.models["deepseek-v4-flash"].context_window,
            Some(1_000_000)
        );
    }

    #[test]
    fn workspace_config_adds_custom_provider_and_overlays_builtins() {
        let workspace = workspace("merge");
        let medusa = workspace.join(".medusa");
        fs::create_dir_all(&medusa).unwrap();
        fs::write(
            medusa.join("providers.json"),
            r#"{
              "providers": {
                "acme": {
                  "name": "Acme AI",
                  "protocol": "open-ai-chat",
                  "baseUrl": "https://models.acme.test/v1",
                  "apiKeyEnv": ["ACME_API_KEY"],
                  "defaultModel": "coder",
                  "thinking": "openai",
                  "models": {
                    "coder": {
                      "name": "Acme Coder",
                      "reasoning": ["none", "high"]
                    }
                  }
                },
                "deepseek": { "baseUrl": "https://proxy.test/v1" }
              }
            }"#,
        )
        .unwrap();

        let registry = ProviderRegistry::load(&workspace).unwrap();
        let acme = registry.provider("acme").unwrap();
        assert_eq!(acme.display_name, "Acme AI");
        assert_eq!(acme.default_model, "coder");
        assert_eq!(acme.thinking, ThinkingDialect::Openai);
        assert!(acme.capabilities.reasoning);
        assert_eq!(acme.models["coder"].reasoning_levels.len(), 2);
        assert_eq!(
            registry.provider("deepseek").unwrap().base_url,
            "https://proxy.test/v1"
        );
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn chat_providers_without_a_reasoning_dialect_hide_the_control() {
        let registry = ProviderRegistry::builtins();
        assert!(!registry.provider("ollama").unwrap().capabilities.reasoning);
        assert!(
            !registry
                .provider("lmstudio")
                .unwrap()
                .capabilities
                .reasoning
        );
        assert!(
            registry
                .provider("openrouter")
                .unwrap()
                .capabilities
                .reasoning
        );
    }

    #[test]
    fn rejects_invalid_custom_provider_urls() {
        let workspace = workspace("invalid-url");
        let medusa = workspace.join(".medusa");
        fs::create_dir_all(&medusa).unwrap();
        fs::write(
            medusa.join("providers.json"),
            r#"{
              "providers": {
                "acme": {
                  "protocol": "openai-chat",
                  "baseUrl": "file:///tmp/models",
                  "defaultModel": "coder"
                }
              }
            }"#,
        )
        .unwrap();

        let error = ProviderRegistry::load(&workspace).unwrap_err().to_string();
        assert!(error.contains("must use http or https"));
        fs::remove_dir_all(workspace).ok();
    }
}
