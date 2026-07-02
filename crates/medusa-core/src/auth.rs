use std::{fs, path::PathBuf};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAuthProbe {
    pub codex_bin: Option<PathBuf>,
    pub auth_cache_present: bool,
    pub status: CodexAuthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAuthStatus {
    Ready(String),
    NotLoggedIn(String),
    Unknown(String),
}

#[derive(Clone)]
pub struct CodexOAuthCredentials {
    access_token: String,
    account_id: Option<String>,
}

impl CodexOAuthCredentials {
    pub fn bearer_token(&self) -> &str {
        &self.access_token
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }
}

impl CodexAuthProbe {
    pub fn summary_lines(&self) -> Vec<String> {
        let codex = self
            .codex_bin
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found".to_string());
        let cache = if self.auth_cache_present {
            "present"
        } else {
            "missing"
        };

        vec![
            format!("codex: {codex}"),
            format!("auth cache: {cache}"),
            format!("status: {}", self.status.label()),
        ]
    }
}

impl CodexAuthStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Ready(message) => compact(message),
            Self::NotLoggedIn(message) => compact(message),
            Self::Unknown(message) => compact(message),
        }
    }
}

pub fn probe_codex_auth() -> CodexAuthProbe {
    let codex_bin = find_codex_bin();
    let path = auth_file_path();
    let auth_cache_present = fs::metadata(&path).is_ok();
    let status = match load_codex_oauth_credentials() {
        Ok(credentials) => {
            let account = if credentials.account_id().is_some() {
                "account selected"
            } else {
                "no account id"
            };
            CodexAuthStatus::Ready(format!("ChatGPT OAuth cache ready ({account})"))
        }
        Err(error) if !auth_cache_present => CodexAuthStatus::NotLoggedIn(error.to_string()),
        Err(error) => CodexAuthStatus::Unknown(error.to_string()),
    };

    CodexAuthProbe {
        codex_bin,
        auth_cache_present,
        status,
    }
}

pub fn load_codex_oauth_credentials() -> Result<CodexOAuthCredentials> {
    let path = auth_file_path();
    let contents = fs::read_to_string(&path)
        .wrap_err_with(|| format!("failed to read Codex auth cache: {}", path.display()))?;
    let auth: CodexAuthFile = serde_json::from_str(&contents)
        .wrap_err_with(|| format!("failed to parse Codex auth cache: {}", path.display()))?;

    if auth.auth_mode.as_deref() != Some("chatgpt") {
        bail!("Codex auth cache is not in ChatGPT OAuth mode");
    }

    let tokens = auth
        .tokens
        .ok_or_else(|| color_eyre::eyre::eyre!("Codex auth cache does not contain OAuth tokens"))?;
    let access_token = tokens
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| color_eyre::eyre::eyre!("Codex OAuth access token is missing"))?;

    Ok(CodexOAuthCredentials {
        access_token,
        account_id: tokens.account_id.filter(|id| !id.trim().is_empty()),
    })
}

fn auth_file_path() -> PathBuf {
    codex_home().join("auth.json")
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn find_codex_bin() -> Option<PathBuf> {
    let output = std::process::Command::new("sh")
        .arg("-lc")
        .arg("command -v codex")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn compact(message: &str) -> String {
    let message = message.replace('\n', " ");
    let mut chars = message.chars();
    let value = chars.by_ref().take(180).collect::<String>();

    if chars.next().is_some() {
        format!("{value}...")
    } else if value.is_empty() {
        "<empty>".to_string()
    } else {
        value
    }
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    auth_mode: Option<String>,
    tokens: Option<CodexAuthTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_label_compacts_ready_message() {
        let status = CodexAuthStatus::Ready("ChatGPT OAuth cache ready".to_string());

        assert_eq!(status.label(), "ChatGPT OAuth cache ready");
    }

    #[test]
    fn compact_replaces_newlines() {
        assert_eq!(compact("one\ntwo"), "one two");
    }
}
