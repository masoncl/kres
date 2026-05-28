//! Agent config files.
//!
//! Shape of each per-agent JSON file: credentials (`api_key` for all
//! providers, plus `host` + optional `api_version` for Azure GPT),
//! `model`, `max_tokens`, `max_input_tokens`, `rate_limit`, `thinking`,
//! and `system` (or `system_file`).
//!
//! The `api_key` field carries the literal API key string. Shipped
//! configs in the repo carry `@FAST_KEY@` / `@SLOW_KEY@` placeholders
//! that setup.sh rewrites at install time from literal
//! `--fast-key` / `--slow-key` values.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AgentError;
use kres_llm::{
    model::{Effort, ThinkingBudget},
    LlmCredentials, Model, Provider,
};

/// Which agent role this config describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Fast,
    Slow,
    Main,
    Todo,
    Classifier,
    Consolidator,
    Merger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Literal API key string. setup.sh substitutes @FAST_KEY@ /
    /// @SLOW_KEY@ placeholders in the shipped configs at install
    /// time; operators can also edit the file directly.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    /// Model id override. Required in practice — when omitted, kres
    /// falls back to Model::sonnet_4_6(). All shipped configs set this.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Soft payload ceiling for input tokens; caller is responsible
    /// for shrinking when exceeded.
    #[serde(default)]
    pub max_input_tokens: Option<u32>,
    /// Rate-limit bucket in tokens-per-minute.
    #[serde(default)]
    pub rate_limit: Option<u32>,
    /// Optional request-level thinking override.
    ///
    /// Shape:
    ///   {"type":"adaptive","effort":"medium"}
    ///   {"type":"enabled","budget_tokens":32000}
    ///   {"type":"disabled"}
    ///
    /// When omitted, kres uses model-aware defaults.
    #[serde(default)]
    pub thinking: Option<AgentThinkingConfig>,
    /// Inline system prompt (passed to Anthropic as `system`). If
    /// `system_file` is also set, `system_file` wins.
    #[serde(default)]
    pub system: Option<String>,
    /// Path to a file whose contents become the system prompt.
    ///
    /// Resolution order:
    ///   1. `~/...` → `$HOME/...`
    ///   2. Absolute path → used as-is
    ///   3. Relative path → resolved against the CONFIG FILE's
    ///      directory. For model configs under `~/.kres/models/`, a
    ///      `system-prompts/<name>.system.md` path also checks
    ///      `~/.kres/system-prompts/<name>.system.md`.
    ///
    /// Intended so long prompts can live in versioned `.md` files
    /// rather than as escaped JSON strings.
    #[serde(default)]
    pub system_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentThinkingConfig {
    Disabled,
    Enabled {
        #[serde(default)]
        budget_tokens: Option<u32>,
    },
    Adaptive {
        #[serde(default)]
        effort: Option<AgentThinkingEffort>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentThinkingEffort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl AgentThinkingConfig {
    pub fn to_budget(&self, max_tokens: u32) -> ThinkingBudget {
        match self {
            AgentThinkingConfig::Disabled => ThinkingBudget::Disabled,
            AgentThinkingConfig::Enabled { budget_tokens } => budget_tokens
                .map(|n| ThinkingBudget::enabled_clamped(n, max_tokens))
                .unwrap_or_else(|| ThinkingBudget::default_explicit_for(max_tokens)),
            AgentThinkingConfig::Adaptive { effort } => ThinkingBudget::Adaptive(
                effort
                    .map(Into::into)
                    .unwrap_or(kres_llm::model::Effort::Medium),
            ),
        }
    }
}

impl From<AgentThinkingEffort> for Effort {
    fn from(value: AgentThinkingEffort) -> Self {
        match value {
            AgentThinkingEffort::Low => Effort::Low,
            AgentThinkingEffort::Medium => Effort::Medium,
            AgentThinkingEffort::High => Effort::High,
            AgentThinkingEffort::XHigh => Effort::XHigh,
        }
    }
}

impl AgentConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        Self::load_with_role(path, None)
    }

    pub fn load_for_role(path: impl AsRef<Path>, role: AgentKind) -> Result<Self, AgentError> {
        Self::load_with_role(path, Some(role))
    }

    fn load_with_role(path: impl AsRef<Path>, role: Option<AgentKind>) -> Result<Self, AgentError> {
        let role_name = role.and_then(AgentKind::model_section);
        let default_system_file = role.and_then(AgentKind::default_system_file);
        Self::load_with_role_name(path, role_name, default_system_file)
    }

    fn load_with_role_name(
        path: impl AsRef<Path>,
        role_name: Option<&str>,
        default_system_file: Option<&str>,
    ) -> Result<Self, AgentError> {
        let cfg_path = path.as_ref();
        let raw = std::fs::read_to_string(cfg_path)?;
        let cfg: AgentConfig = serde_json::from_value(expand_model_config_sections(
            serde_json::from_str(&raw)?,
            role_name,
        )?)?;
        let mut cfg = cfg;
        if cfg.system.is_none() && cfg.system_file.is_none() {
            if let Some(default) = default_system_file {
                cfg.system_file = Some(PathBuf::from(default));
            }
        }
        cfg.validate_credentials(cfg_path)?;
        // Resolve and read `system_file` if present. It supersedes
        // any inline `system` — callers that want to override
        // should just drop the `system_file` field.
        //
        // Resolution order, in descending priority:
        //   1. Disk file at the resolved path. An operator who
        //      wants to customize a prompt drops a file at the
        //      referenced path (typically
        //      `~/.kres/system-prompts/X.md`)
        //      and kres reads it.
        //   2. Embedded prompt keyed by the file's basename. This
        //      is the normal path for stock installs — the
        //      `.system.md` files are compiled into the binary
        //      via `include_str!` (see `embedded_prompts` module),
        //      so a fresh install with no `~/.kres/system-prompts/`
        //      copy
        //      still runs. This replaces the previous "setup.sh
        //      must copy every prompt" workflow — operators no
        //      longer need `setup.sh --overwrite` when the repo's
        //      prompts change; rebuilding kres refreshes them.
        //   3. Both missing → error, same as before.
        if let Some(ref sf) = cfg.system_file {
            let candidates = system_file_candidates(cfg_path, sf);
            let mut last_err: Option<std::io::Error> = None;
            for resolved in &candidates {
                match std::fs::read_to_string(resolved) {
                    Ok(body) => {
                        cfg.system = Some(body);
                        break;
                    }
                    Err(err) => last_err = Some(err),
                }
            }
            if cfg.system.is_none() {
                let basename = candidates
                    .first()
                    .and_then(|p| p.file_name())
                    .and_then(|o| o.to_str())
                    .unwrap_or("");
                if let Some(embedded) = crate::embedded_prompts::lookup(basename) {
                    cfg.system = Some(embedded.to_string());
                } else {
                    let attempted = candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let disk_err = last_err
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "not found".to_string());
                    return Err(AgentError::Other(format!(
                        "system_file {attempted}: {disk_err} (no embedded fallback for basename '{basename}')"
                    )));
                }
            }
        }
        Ok(cfg)
    }

    pub fn credentials(&self) -> Result<LlmCredentials, AgentError> {
        let provider = self.provider.as_deref().map(normalize_provider);
        let api_key = self.api_key.as_deref().ok_or_else(|| {
            AgentError::Other("agent config missing credentials: set `api_key`".into())
        })?;
        if let Some(host) = self.host.as_deref() {
            return Ok(LlmCredentials::azure_openai(
                host,
                api_key,
                self.api_version.clone(),
            ));
        }
        if matches!(provider.as_deref(), Some("openai" | "open_ai")) || self.model_is_openai() {
            return Ok(LlmCredentials::openai(api_key, self.base_url.clone()));
        }
        Ok(LlmCredentials::anthropic(api_key))
    }

    pub fn credential_cache_key(&self) -> Result<String, AgentError> {
        Ok(self.credentials()?.cache_key())
    }

    fn validate_credentials(&self, cfg_path: &Path) -> Result<(), AgentError> {
        match self.api_key.as_deref() {
            Some(k) if valid_secret(k) => Ok(()),
            Some(k) if k.starts_with('@') && k.ends_with('@') => {
                Err(AgentError::Other(format!(
                    "agent config {} still contains the placeholder key {:?}; run setup.sh --fast-key/--slow-key to fill it in",
                    cfg_path.display(),
                    k
                )))
            }
            _ => Err(AgentError::Other(format!(
                "agent config {} missing credentials: set `api_key`",
                cfg_path.display()
            ))),
        }
    }

    fn model_is_openai(&self) -> bool {
        self.model
            .as_deref()
            .map(|id| Model::from_id(id).provider() == Provider::OpenAi)
            .unwrap_or(false)
    }
}

impl AgentKind {
    fn model_section(self) -> Option<&'static str> {
        match self {
            AgentKind::Fast => Some("fast"),
            AgentKind::Slow => Some("slow"),
            AgentKind::Main => Some("main"),
            AgentKind::Todo => Some("todo"),
            AgentKind::Classifier => Some("classifier"),
            AgentKind::Consolidator | AgentKind::Merger => None,
        }
    }

    fn default_system_file(self) -> Option<&'static str> {
        match self {
            AgentKind::Fast => Some("system-prompts/fast-code-agent.system.md"),
            AgentKind::Slow => Some("system-prompts/slow-code-agent-audit.system.md"),
            AgentKind::Main => Some("system-prompts/main-agent.system.md"),
            AgentKind::Todo => Some("system-prompts/todo-agent.system.md"),
            AgentKind::Classifier => Some("system-prompts/classifier-agent.system.md"),
            AgentKind::Consolidator | AgentKind::Merger => None,
        }
    }
}

fn role_default_system_file(role: &str) -> Option<&'static str> {
    match role {
        "fast" => AgentKind::Fast.default_system_file(),
        "slow" => AgentKind::Slow.default_system_file(),
        "main" => AgentKind::Main.default_system_file(),
        "todo" => AgentKind::Todo.default_system_file(),
        "classifier" => AgentKind::Classifier.default_system_file(),
        _ => None,
    }
}

fn expand_model_config_sections(
    value: serde_json::Value,
    role: Option<&str>,
) -> Result<serde_json::Value, AgentError> {
    let Some(obj) = value.as_object() else {
        return Ok(value);
    };
    if !obj.contains_key("defaults")
        && !obj.contains_key("fast")
        && !obj.contains_key("slow")
        && !obj.contains_key("main")
        && !obj.contains_key("todo")
        && !obj.contains_key("classifier")
    {
        return Ok(value);
    }

    let mut merged = serde_json::Map::new();
    for (k, v) in obj {
        if !matches!(
            k.as_str(),
            "defaults" | "fast" | "slow" | "main" | "todo" | "classifier"
        ) {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(defaults) = obj.get("defaults") {
        merge_object_section(&mut merged, defaults, "defaults")?;
    }
    if let Some(role) = role {
        if let Some(section) = obj.get(role) {
            merge_object_section(&mut merged, section, role)?;
        }
        if !merged.contains_key("system") && !merged.contains_key("system_file") {
            if let Some(default) = role_default_system_file(role) {
                merged.insert(
                    "system_file".to_string(),
                    serde_json::Value::String(default.to_string()),
                );
            }
        }
    }
    Ok(serde_json::Value::Object(merged))
}

fn merge_object_section(
    dst: &mut serde_json::Map<String, serde_json::Value>,
    section: &serde_json::Value,
    name: &str,
) -> Result<(), AgentError> {
    let Some(obj) = section.as_object() else {
        return Err(AgentError::Other(format!(
            "agent config section `{name}` must be a JSON object"
        )));
    };
    for (k, v) in obj {
        if is_credential_key(k) {
            return Err(AgentError::Other(format!(
                "agent config section `{name}` must not set credential field `{k}`; set credentials once at the model-file top level"
            )));
        }
        dst.insert(k.clone(), v.clone());
    }
    Ok(())
}

fn is_credential_key(key: &str) -> bool {
    matches!(
        key,
        "api_key" | "provider" | "base_url" | "host" | "api_version"
    )
}

fn system_file_candidates(cfg_path: &Path, system_file: &Path) -> Vec<PathBuf> {
    let expanded = expand_tilde(system_file);
    if expanded.is_absolute() {
        return vec![expanded];
    }

    let config_dir = cfg_path.parent().unwrap_or_else(|| Path::new("."));
    let mut candidates = Vec::new();
    if config_dir.file_name().and_then(|n| n.to_str()) == Some("models")
        && expanded.starts_with("system-prompts")
    {
        if let Some(root) = config_dir.parent() {
            candidates.push(root.join(&expanded));
        }
    }
    candidates.push(config_dir.join(expanded));
    candidates
}

fn normalize_provider(provider: &str) -> String {
    provider.trim().replace('-', "_").to_ascii_lowercase()
}

fn valid_secret(secret: &str) -> bool {
    !(secret.trim().is_empty() || secret.starts_with('@') && secret.ends_with('@'))
}

fn expand_tilde(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut out = PathBuf::from(home);
            out.push(rest);
            return out;
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "kres-agent-cfg-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    #[test]
    fn loads_full_shape() {
        let p = write_tmp(
            r#"{
                "api_key": "sk-live-key-value",
                "model": "claude-opus-4-7",
                "max_tokens": 128000,
                "max_input_tokens": 900000,
                "rate_limit": 800000,
                "thinking": {"type": "adaptive", "effort": "high"},
                "system": "you are a fast agent"
            }"#,
        );
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(c.api_key.as_deref(), Some("sk-live-key-value"));
        assert_eq!(c.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(c.max_tokens, Some(128000));
        assert_eq!(
            c.thinking.as_ref().map(|t| t.to_budget(128000)),
            Some(ThinkingBudget::Adaptive(Effort::High))
        );
        assert!(c.system.as_deref().unwrap().contains("fast agent"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn loads_adaptive_xhigh_effort() {
        let p = write_tmp(
            r#"{
                "api_key": "sk-x",
                "thinking": {"type": "adaptive", "effort": "xhigh"}
            }"#,
        );
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(
            c.thinking.as_ref().map(|t| t.to_budget(128000)),
            Some(ThinkingBudget::Adaptive(Effort::XHigh))
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn minimal_shape() {
        let p = write_tmp(r#"{"api_key": "sk-abc"}"#);
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(c.api_key.as_deref(), Some("sk-abc"));
        assert!(matches!(
            c.credentials().unwrap(),
            LlmCredentials::Anthropic { .. }
        ));
        assert_eq!(c.model, None);
        assert_eq!(c.max_tokens, None);
        assert_eq!(c.thinking, None);
        assert_eq!(c.system, None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn gpt_credentials_use_individual_fields() {
        let p = write_tmp(
            r#"{
                "host": "example.azure.net",
                "api_key": "sk-gpt",
                "api_version": "2024-02-15-preview",
                "model": "gpt-5.5"
            }"#,
        );
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(c.api_key.as_deref(), Some("sk-gpt"));
        assert!(matches!(
            c.credentials().unwrap(),
            LlmCredentials::AzureOpenAi { .. }
        ));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn official_openai_credentials_use_api_key_fields() {
        let p = write_tmp(
            r#"{
                "provider": "openai",
                "api_key": "sk-openai",
                "base_url": "https://api.openai.com/v1",
                "model": "gpt-5.5"
            }"#,
        );
        let c = AgentConfig::load(&p).unwrap();
        assert!(matches!(
            c.credentials().unwrap(),
            LlmCredentials::OpenAi { .. }
        ));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn official_openai_rejects_legacy_key_field() {
        let p = write_tmp(
            r#"{
                "provider": "openai",
                "key": "sk-openai",
                "model": "gpt-5.5"
            }"#,
        );
        let msg = format!("{}", AgentConfig::load(&p).unwrap_err());
        assert!(msg.contains("unknown field `key`"), "got: {msg}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn gpt_model_rejects_legacy_key_field_without_provider() {
        let p = write_tmp(
            r#"{
                "key": "sk-openai",
                "model": "gpt-5.5"
            }"#,
        );
        let msg = format!("{}", AgentConfig::load(&p).unwrap_err());
        assert!(msg.contains("unknown field `key`"), "got: {msg}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn model_file_defaults_and_role_sections_are_merged() {
        let p = write_tmp(
            r#"{
                "provider": "openai",
                "api_key": "sk-openai",
                "model": "gpt-5.5",
                "defaults": {
                    "max_tokens": 64000,
                    "rate_limit": 900000,
                    "thinking": {"type": "adaptive", "effort": "medium"}
                },
                "fast": {
                    "max_tokens": 16000,
                    "thinking": {"type": "adaptive", "effort": "low"}
                },
                "slow": {
                    "thinking": {"type": "adaptive", "effort": "high"}
                }
            }"#,
        );
        let fast = AgentConfig::load_for_role(&p, AgentKind::Fast).unwrap();
        let slow = AgentConfig::load_for_role(&p, AgentKind::Slow).unwrap();
        assert_eq!(fast.max_tokens, Some(16000));
        assert_eq!(fast.rate_limit, Some(900000));
        assert_eq!(
            fast.thinking.as_ref().map(|t| t.to_budget(16000)),
            Some(ThinkingBudget::Adaptive(Effort::Low))
        );
        assert_eq!(slow.max_tokens, Some(64000));
        assert_eq!(
            slow.thinking.as_ref().map(|t| t.to_budget(64000)),
            Some(ThinkingBudget::Adaptive(Effort::High))
        );
        assert!(matches!(
            slow.credentials().unwrap(),
            LlmCredentials::OpenAi { .. }
        ));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn model_role_sections_reject_credentials() {
        let p = write_tmp(
            r#"{
                "api_key": "sk-top-level",
                "model": "claude-sonnet-4-6",
                "slow": {
                    "api_key": "sk-role-level",
                    "max_tokens": 64000
                }
            }"#,
        );
        let msg = format!(
            "{}",
            AgentConfig::load_for_role(&p, AgentKind::Slow).unwrap_err()
        );
        assert!(
            msg.contains("must not set credential field `api_key`"),
            "got: {msg}"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn thinking_enabled_clamps_budget() {
        let p = write_tmp(
            r#"{
                "api_key": "sk-abc",
                "thinking": {"type": "enabled", "budget_tokens": 99000}
            }"#,
        );
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(
            c.thinking.as_ref().map(|t| t.to_budget(1000)),
            Some(ThinkingBudget::ExplicitBudget(750))
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn placeholder_key_errors() {
        // An unsubstituted setup.sh placeholder must surface as a
        // clear config error rather than silently hitting the API
        // with a string like "@FAST_KEY@".
        let p = write_tmp(r#"{"api_key": "@FAST_KEY@"}"#);
        let msg = format!("{}", AgentConfig::load(&p).unwrap_err());
        assert!(
            msg.contains("placeholder") && msg.contains("@FAST_KEY@"),
            "got: {msg}"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_key_errors() {
        let p = write_tmp(r#"{"api_key": ""}"#);
        let msg = format!("{}", AgentConfig::load(&p).unwrap_err());
        assert!(msg.contains("set `api_key`"), "got: {msg}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn system_file_relative_to_config_dir() {
        // Config at /tmp/foo/agent.json → system_file "x.md" must
        // resolve to /tmp/foo/x.md, not ./x.md.
        let dir = std::env::temp_dir().join(format!("kres-sysfile-rel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md_path = dir.join("prompt.md");
        std::fs::write(&md_path, "body from the md file").unwrap();
        let cfg_path = dir.join("agent.json");
        std::fs::write(
            &cfg_path,
            r#"{"api_key": "sk-x", "system_file": "prompt.md"}"#,
        )
        .unwrap();
        let c = AgentConfig::load(&cfg_path).unwrap();
        assert_eq!(c.system.as_deref(), Some("body from the md file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn system_file_absolute_path() {
        let dir = std::env::temp_dir().join(format!("kres-sysfile-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md_path = dir.join("prompt.md");
        std::fs::write(&md_path, "absolute-path body").unwrap();
        let cfg_path = dir.join("agent.json");
        let cfg_body = format!(
            r#"{{"api_key": "sk-x", "system_file": "{}"}}"#,
            md_path.display()
        );
        std::fs::write(&cfg_path, cfg_body).unwrap();
        let c = AgentConfig::load(&cfg_path).unwrap();
        assert_eq!(c.system.as_deref(), Some("absolute-path body"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn system_file_overrides_inline_system() {
        let dir = std::env::temp_dir().join(format!("kres-sysfile-over-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md_path = dir.join("prompt.md");
        std::fs::write(&md_path, "from-file").unwrap();
        let cfg_path = dir.join("agent.json");
        std::fs::write(
            &cfg_path,
            r#"{"api_key": "sk-x", "system": "inline-should-lose", "system_file": "prompt.md"}"#,
        )
        .unwrap();
        let c = AgentConfig::load(&cfg_path).unwrap();
        assert_eq!(c.system.as_deref(), Some("from-file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_system_file_without_embedded_match_errors() {
        // The basename doesn't correspond to any embedded prompt
        // (the `.system.md` table is agent-role specific) and the
        // disk path is absent → both fallbacks fail and the caller
        // gets a clear error.
        let p =
            write_tmp(r#"{"api_key": "sk-x", "system_file": "/tmp/does-not-exist-kres-test.md"}"#);
        let e = AgentConfig::load(&p).unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("system_file"), "got: {msg}");
        assert!(
            msg.contains("no embedded fallback"),
            "error should mention the embedded-fallback attempt, got: {msg}"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_system_file_falls_back_to_embedded_prompt() {
        // When the disk path is absent but the basename matches a
        // known embedded prompt (the typical "stock install, no
        // ~/.kres/system-prompts/" case), kres uses the compiled-in copy
        // instead of erroring. This test targets `main-agent.system.md`
        // because that name is guaranteed present in the embedded
        // table.
        let dir =
            std::env::temp_dir().join(format!("kres-sysfile-embedded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Pointing at a nonexistent sibling file whose basename
        // matches an embedded key.
        let cfg_path = dir.join("agent.json");
        std::fs::write(
            &cfg_path,
            r#"{"api_key": "sk-x", "system_file": "system-prompts/main-agent.system.md"}"#,
        )
        .unwrap();
        let c = AgentConfig::load(&cfg_path).unwrap();
        let body = c.system.expect("embedded fallback should populate system");
        assert!(!body.trim().is_empty(), "embedded prompt came back empty");
        // Sanity check — the main-agent system prompt mentions
        // the action-type vocabulary.
        assert!(
            body.contains("action") || body.contains("grep"),
            "body doesn't look like the main-agent prompt: {}",
            &body[..body.len().min(200)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn existing_disk_file_wins_over_embedded() {
        // An operator's custom copy at the referenced path must
        // take precedence over the embedded one — this is the
        // override path.
        let dir =
            std::env::temp_dir().join(format!("kres-sysfile-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Shadow the embedded main-agent prompt with a tiny
        // operator-supplied one. Same basename, different body.
        let prompts = dir.join("system-prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(
            prompts.join("main-agent.system.md"),
            "OPERATOR-OVERRIDE BODY",
        )
        .unwrap();
        let cfg_path = dir.join("agent.json");
        std::fs::write(
            &cfg_path,
            r#"{"api_key": "sk-x", "system_file": "system-prompts/main-agent.system.md"}"#,
        )
        .unwrap();
        let c = AgentConfig::load(&cfg_path).unwrap();
        assert_eq!(c.system.as_deref(), Some("OPERATOR-OVERRIDE BODY"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_config_default_system_file_uses_config_root_override() {
        let root =
            std::env::temp_dir().join(format!("kres-model-sysfile-root-{}", std::process::id()));
        let models = root.join("models");
        let prompts = root.join("system-prompts");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(
            prompts.join("fast-code-agent.system.md"),
            "MODEL ROOT OVERRIDE",
        )
        .unwrap();
        let cfg_path = models.join("claude-sonnet-4-6.json");
        std::fs::write(
            &cfg_path,
            r#"{"api_key": "sk-x", "model": "claude-sonnet-4-6"}"#,
        )
        .unwrap();

        let c = AgentConfig::load_for_role(&cfg_path, AgentKind::Fast).unwrap();
        assert_eq!(c.system.as_deref(), Some("MODEL ROOT OVERRIDE"));
        std::fs::remove_dir_all(&root).ok();
    }
}
