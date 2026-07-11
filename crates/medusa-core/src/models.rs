//! Model catalog sourced from Codex's own backend model list.
//!
//! The Codex CLI fetches the account's available models from its backend and
//! caches them in `$CODEX_HOME/models_cache.json` (refreshed periodically).
//! Rather than hardcode a model list that drifts every release, Medusa reads
//! that same cache — so the `/model` picker shows exactly what the Codex
//! backend currently offers this account, including newly launched models,
//! with no code change. When the cache is absent or unreadable (a non-Codex
//! provider, a fresh install, or a machine without the Codex CLI), callers
//! fall back to their built-in defaults.

use std::path::PathBuf;

use serde::Deserialize;

use crate::auth::codex_home;

/// One selectable model, projected from the Codex cache into just what the
/// picker needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// The wire slug sent to the backend, e.g. `gpt-5.6-sol`.
    pub slug: String,
    /// Human label, e.g. `GPT-5.6-Sol`. Falls back to the slug.
    pub display_name: String,
    /// One-line description, when the backend provides one.
    pub description: Option<String>,
    /// The backend's default reasoning effort for this model, when given.
    pub default_reasoning: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsCache {
    #[serde(default)]
    models: Vec<CachedModel>,
}

#[derive(Debug, Deserialize)]
struct CachedModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    /// `list` = show in pickers, `hide` = internal/background only.
    #[serde(default)]
    visibility: Option<String>,
    /// Some listed models are chat-only and cannot be driven through the
    /// responses API the harness uses.
    #[serde(default = "default_true")]
    supported_in_api: bool,
    /// Backend-provided sort order (lower = higher priority).
    #[serde(default)]
    priority: i64,
}

fn default_true() -> bool {
    true
}

fn models_cache_path() -> PathBuf {
    codex_home().join("models_cache.json")
}

/// Read the Codex backend's cached model list for this account. Returns the
/// user-selectable models (visible + API-drivable), ordered by the backend's
/// own priority. `None` when the cache is missing or unparseable — callers
/// should fall back to their defaults rather than treating this as an error.
///
/// Memoized for the process: the picker reads it every frame while open, and
/// the cache only changes when the Codex CLI refreshes it (a relaunch picks
/// that up, same as any other model change taking effect on new turns).
pub fn codex_backend_models() -> Option<Vec<ModelInfo>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<Vec<ModelInfo>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let raw = std::fs::read_to_string(models_cache_path()).ok()?;
            parse_codex_models(&raw)
        })
        .clone()
}

fn parse_codex_models(raw: &str) -> Option<Vec<ModelInfo>> {
    let cache: ModelsCache = serde_json::from_str(raw).ok()?;
    let mut models: Vec<(i64, ModelInfo)> = cache
        .models
        .into_iter()
        .filter(|model| {
            // Show only models the backend lists for humans and that the
            // responses API can actually run.
            model.visibility.as_deref() != Some("hide") && model.supported_in_api
        })
        .map(|model| {
            let display_name = model
                .display_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| model.slug.clone());
            (
                model.priority,
                ModelInfo {
                    slug: model.slug,
                    display_name,
                    description: model.description.filter(|text| !text.trim().is_empty()),
                    default_reasoning: model
                        .default_reasoning_level
                        .filter(|text| !text.trim().is_empty()),
                },
            )
        })
        .collect();

    // Stable sort by backend priority; equal priorities keep cache order.
    models.sort_by_key(|(priority, _)| *priority);
    let models: Vec<ModelInfo> = models.into_iter().map(|(_, model)| model).collect();
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "fetched_at": "2026-07-11T04:40:46Z",
        "models": [
            {"slug":"gpt-5.5","display_name":"GPT-5.5","description":"Frontier model.","default_reasoning_level":"medium","visibility":"list","supported_in_api":true,"priority":0},
            {"slug":"gpt-5.6-sol","display_name":"GPT-5.6-Sol","description":"Latest.","default_reasoning_level":"medium","visibility":"list","supported_in_api":true,"priority":1},
            {"slug":"gpt-5.3-codex-spark","display_name":"Spark","visibility":"list","supported_in_api":false,"priority":26},
            {"slug":"codex-auto-review","display_name":"Auto Review","visibility":"hide","supported_in_api":true,"priority":43}
        ]
    }"#;

    #[test]
    fn parses_and_filters_and_orders_by_priority() {
        let models = parse_codex_models(SAMPLE).unwrap();
        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        // hidden (codex-auto-review) and non-API (spark) dropped; priority order.
        assert_eq!(slugs, vec!["gpt-5.5", "gpt-5.6-sol"]);
        assert_eq!(models[1].display_name, "GPT-5.6-Sol");
        assert_eq!(models[0].description.as_deref(), Some("Frontier model."));
        assert_eq!(models[0].default_reasoning.as_deref(), Some("medium"));
    }

    #[test]
    fn display_name_falls_back_to_slug() {
        let raw = r#"{"models":[{"slug":"custom-model","visibility":"list","supported_in_api":true,"priority":0}]}"#;
        let models = parse_codex_models(raw).unwrap();
        assert_eq!(models[0].display_name, "custom-model");
        assert!(models[0].description.is_none());
    }

    #[test]
    fn missing_or_garbage_cache_yields_none() {
        assert!(parse_codex_models("not json").is_none());
        assert!(parse_codex_models(r#"{"models":[]}"#).is_none());
        // All models filtered out -> None (caller uses its defaults).
        let all_hidden = r#"{"models":[{"slug":"x","visibility":"hide","supported_in_api":true}]}"#;
        assert!(parse_codex_models(all_hidden).is_none());
    }

    #[test]
    fn priority_defaults_keep_cache_order_when_absent() {
        let raw = r#"{"models":[
            {"slug":"a","visibility":"list","supported_in_api":true},
            {"slug":"b","visibility":"list","supported_in_api":true}
        ]}"#;
        let models = parse_codex_models(raw).unwrap();
        assert_eq!(
            models.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }
}
