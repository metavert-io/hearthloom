//! Configuration: YAML at `config/{env}.yaml` with `${VAR:default}` env
//! expansion; `.env` in the working directory is auto-loaded (Orrery's
//! pattern). The system must run degraded-but-useful with no LLM provider
//! configured, so every provider field defaults to "none"/empty.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_YAML: &str = r#"# Hearthloom configuration.
# ${VAR:default} expands from the environment; .env is auto-loaded.
daemon:
  host: "${HEARTHLOOM_HOST:127.0.0.1}"
  port: "${HEARTHLOOM_PORT:43278}"
provider:
  # LLM provider: none | anthropic. Tokens are spent at ingest and idle
  # time only — the hot query path never calls a provider.
  llm: "${HEARTHLOOM_LLM:none}"
  # Legacy override for the HIGH tier (blank = the provider's own default).
  model: "${HEARTHLOOM_MODEL:}"
  api_key: "${ANTHROPIC_API_KEY:}"
  # Per-tier models for the active provider (blank = provider defaults).
  # low = cheap/basic, medium = real entity extraction, high = complex.
  model_low: "${HEARTHLOOM_MODEL_LOW:}"
  model_medium: "${HEARTHLOOM_MODEL_MEDIUM:}"
  model_high: "${HEARTHLOOM_MODEL_HIGH:}"
  # Embeddings: auto (use ~/.hearthloom/models if present) | local-onnx |
  # none. Fully local either way; no network.
  embeddings: "${HEARTHLOOM_EMBEDDINGS:auto}"
"#;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    /// Optional per-model price overrides (USD per 1M tokens), matched
    /// by substring BEFORE the built-in table in provider::pricing_for —
    /// the reconciliation escape hatch when provider prices drift.
    /// Omitted cache rates derive from input (write 1.25x, read 0.1x).
    #[serde(default)]
    pub llm_pricing: std::collections::BTreeMap<String, PricingEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingEntry {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
}

impl PricingEntry {
    pub fn to_pricing(&self) -> crate::provider::ModelPricing {
        crate::provider::ModelPricing {
            input: self.input,
            output: self.output,
            cache_write: self.cache_write.unwrap_or(self.input * 1.25),
            cache_read: self.cache_read.unwrap_or(self.input * 0.1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port", deserialize_with = "de_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_none")]
    pub llm: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// Per-tier model ids for the active provider; blank falls back to the
    /// provider's seed defaults (and legacy `model` seeds `high`).
    #[serde(default)]
    pub model_low: String,
    #[serde(default)]
    pub model_medium: String,
    #[serde(default)]
    pub model_high: String,
    /// auto | local-onnx | none. `auto` loads the local model if the
    /// files exist and silently degrades to none if not; `local-onnx`
    /// errors loudly when the model is missing.
    #[serde(default = "default_auto")]
    pub embeddings: String,
}

impl ProviderConfig {
    /// Resolve the tier→model map: provider seed defaults, with legacy
    /// `model` seeding `high`, then any explicit per-tier overrides.
    pub fn tier_models(&self) -> crate::provider::TierModels {
        let mut m = crate::provider::TierModels::defaults_for(&self.llm);
        if !self.model.trim().is_empty() {
            m.high = self.model.clone();
        }
        if !self.model_low.trim().is_empty() {
            m.low = self.model_low.clone();
        }
        if !self.model_medium.trim().is_empty() {
            m.medium = self.model_medium.clone();
        }
        if !self.model_high.trim().is_empty() {
            m.high = self.model_high.clone();
        }
        m
    }
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    43278
}
fn default_none() -> String {
    "none".into()
}
fn default_auto() -> String {
    "auto".into()
}
fn default_model() -> String {
    // blank: each provider's tier table decides (a Claude id here would
    // also override an OpenAI or custom provider's high tier)
    String::new()
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            host: default_host(),
            port: default_port(),
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            llm: default_none(),
            model: default_model(),
            api_key: String::new(),
            model_low: String::new(),
            model_medium: String::new(),
            model_high: String::new(),
            embeddings: default_auto(),
        }
    }
}

/// Ports arrive as strings after `${VAR:default}` expansion; accept both.
fn de_port<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PortLike {
        Num(u16),
        Str(String),
    }
    match PortLike::deserialize(d)? {
        PortLike::Num(n) => Ok(n),
        PortLike::Str(s) => s.trim().parse().map_err(serde::de::Error::custom),
    }
}

impl Config {
    /// Load config for the current env (`HEARTHLOOM_ENV`, default
    /// `development`) from the home's config dir. Missing file → defaults.
    pub fn load(config_dir: &Path) -> Result<Self> {
        load_dotenv(Path::new(".env"));
        // Secrets for the daemon live under ~/.hearthloom (the launchd
        // daemon has no shell environment to inherit from) — sealed as
        // .env.sealed when a master key is available (env var or macOS
        // keychain; plaintext .env migrates automatically and is
        // shredded), plaintext fallback otherwise (dev mode, warned on
        // Monitoring). Decryption is in-memory only.
        if let Some(home_root) = config_dir.parent() {
            if let Some(content) = crate::secrets::load_sealed_env(home_root) {
                apply_dotenv(&content);
            }
        }
        let env = std::env::var("HEARTHLOOM_ENV").unwrap_or_else(|_| "development".into());
        let path = config_dir.join(format!("{env}.yaml"));
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let expanded = expand_env(&raw);
        let config: Config = serde_yaml::from_str(&expanded)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }
}

/// Expand `${VAR}` and `${VAR:default}` from the environment.
pub fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let expr = &after[..end];
                let (var, default) = match expr.split_once(':') {
                    Some((v, d)) => (v, d),
                    None => (expr, ""),
                };
                match std::env::var(var) {
                    Ok(val) if !val.is_empty() => out.push_str(&val),
                    _ => out.push_str(default),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Load a `.env` file into the process environment. Existing variables
/// are never overwritten. Minimal parser: `KEY=value` lines, `#`
/// comments, optional surrounding quotes.
fn load_dotenv(path: &Path) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    apply_dotenv(&content);
}

/// Apply already-loaded .env CONTENT (the sealed-secrets path decrypts
/// in memory and never touches disk with plaintext).
fn apply_dotenv(content: &str) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !key.is_empty() && std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env() {
        std::env::set_var("HEARTHLOOM_TEST_VAR", "hello");
        assert_eq!(expand_env("${HEARTHLOOM_TEST_VAR:x}"), "hello");
        assert_eq!(expand_env("${HEARTHLOOM_TEST_UNSET:fallback}"), "fallback");
        assert_eq!(expand_env("${HEARTHLOOM_TEST_UNSET}"), "");
        assert_eq!(expand_env("a ${HEARTHLOOM_TEST_VAR:x} b"), "a hello b");
        assert_eq!(expand_env("no vars"), "no vars");
    }

    #[test]
    fn test_default_config_parses() {
        let expanded = expand_env(DEFAULT_CONFIG_YAML);
        let config: Config = serde_yaml::from_str(&expanded).unwrap();
        assert_eq!(config.daemon.host, "127.0.0.1");
        assert_eq!(config.daemon.port, 43278);
        assert_eq!(config.provider.llm, "none");
    }

    #[test]
    fn test_missing_file_gives_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.daemon.port, 43278);
    }
}
