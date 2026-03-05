use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub zoxide: ZoxideConfig,
    #[serde(default)]
    pub keys: KeyConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyConfig {
    #[serde(default = "default_open_key_string")]
    pub open: String,
    #[serde(default = "default_detach_key_string")]
    pub detach: String,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self {
            open: default_open_key_string(),
            detach: default_detach_key_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoxideConfig {
    #[serde(default = "default_zoxide_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_open: bool,
    #[serde(default = "default_zoxide_limit")]
    pub limit: usize,
}

impl Default for ZoxideConfig {
    fn default() -> Self {
        Self {
            enabled: default_zoxide_enabled(),
            auto_open: false,
            limit: default_zoxide_limit(),
        }
    }
}

fn default_zoxide_enabled() -> bool {
    true
}

fn default_zoxide_limit() -> usize {
    30
}

fn default_open_key() -> &'static str {
    "ctrl-g"
}

fn default_detach_key() -> &'static str {
    "ctrl-]"
}

fn default_open_key_string() -> String {
    default_open_key().to_string()
}

fn default_detach_key_string() -> String {
    default_detach_key().to_string()
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };

        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: AppConfig = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        Ok(config)
    }

    pub fn open_key_binding(&self) -> &str {
        normalized_key_binding(&self.keys.open, default_open_key())
    }

    pub fn detach_key_binding(&self) -> &str {
        normalized_key_binding(&self.keys.detach, default_detach_key())
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        return Some(
            PathBuf::from(config_home)
                .join("fish-session")
                .join("config.json"),
        );
    }

    env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("fish-session")
            .join("config.json")
    })
}

fn normalized_key_binding<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("ctrl-]") {
        return "ctrl-]";
    }

    let bytes = trimmed.as_bytes();
    if bytes.len() == 6
        && bytes[..5].eq_ignore_ascii_case(b"ctrl-")
        && bytes[5].is_ascii_alphabetic()
    {
        return trimmed;
    }

    fallback
}
