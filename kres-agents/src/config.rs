//! Agent config files.
//!
//! A provider JSON file contains shared credentials and transport fields plus
//! a `models` object. Each model entry owns its token limits and thinking
//! defaults; agent roles only select a model and supply their embedded prompt.
//!
//! The `api_key` field carries the literal API key string. Shipped
//! configs in the repo carry `@FAST_KEY@` / `@SLOW_KEY@` placeholders
//! that setup.sh rewrites at install time from literal
//! `--fast-key` / `--slow-key` values.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
    /// Optional path to the `codex` executable for the codex-codes provider.
    #[serde(default)]
    pub codex_path: Option<PathBuf>,
    /// Optional isolated Codex state/config directory, exported as CODEX_HOME.
    #[serde(default)]
    pub codex_home: Option<PathBuf>,
    /// Codex CLI `-c key=value` overrides applied to the app-server process.
    #[serde(default)]
    pub codex_config: BTreeMap<String, serde_json::Value>,
    /// Optional path to the `claude` executable for the claude-codes provider.
    #[serde(default)]
    pub claude_path: Option<PathBuf>,
    /// Vertex project and region for the Anthropic Vertex protocol.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// Static HTTP headers applied to every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Header whose value is generated once per client as a UUID.
    #[serde(default)]
    pub session_header: Option<String>,
    /// Optional client certificate candidates for mTLS, tried in order.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Explicit proxy URL. An empty string disables proxy auto-detection.
    #[serde(default)]
    pub proxy: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub identity_candidates: Vec<TlsIdentityCandidate>,
    /// Additional PEM CA bundles used to verify the server certificate.
    #[serde(default)]
    pub ca_certificates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsIdentityCandidate {
    pub cert: String,
    #[serde(default)]
    pub key: Option<String>,
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

    pub fn load_for_role_model(
        path: impl AsRef<Path>,
        role: AgentKind,
        model_id: &str,
    ) -> Result<Self, AgentError> {
        Self::load_with_role_name(
            path,
            role.model_section(),
            role.default_system_file(),
            Some(model_id),
        )
    }

    fn load_with_role(path: impl AsRef<Path>, role: Option<AgentKind>) -> Result<Self, AgentError> {
        let role_name = role.and_then(AgentKind::model_section);
        let default_system_file = role.and_then(AgentKind::default_system_file);
        Self::load_with_role_name(path, role_name, default_system_file, None)
    }

    fn load_with_role_name(
        path: impl AsRef<Path>,
        _role_name: Option<&str>,
        default_system_file: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<Self, AgentError> {
        let (cfg_path_buf, path_model) = split_config_selector(path.as_ref());
        let cfg_path = cfg_path_buf.as_path();
        let raw = std::fs::read_to_string(cfg_path)?;
        let cfg: AgentConfig = serde_json::from_value(select_model_config(
            serde_json::from_str(&raw)?,
            model_id.or(path_model.as_deref()),
        )?)?;
        let mut cfg = cfg;
        if let Some(home) = cfg.codex_home.take() {
            cfg.codex_home = Some(expand_tilde(&home));
        }
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

    /// Return the real JSON path for a possibly-qualified
    /// `/path/provider.json:model-id` selector.
    pub fn backing_path(path: &Path) -> PathBuf {
        split_config_selector(path).0
    }

    pub fn credentials(&self) -> Result<LlmCredentials, AgentError> {
        let provider = self.provider.as_deref().map(normalize_provider);
        let api_key = self.api_key.as_deref().unwrap_or("dummy");
        if matches!(provider.as_deref(), Some("codex_codes")) {
            return Ok(LlmCredentials::codex_codes(
                self.api_key.clone(),
                self.base_url.clone(),
                self.codex_path.clone(),
                self.codex_home.clone(),
                self.codex_config.clone(),
            ));
        }
        if matches!(provider.as_deref(), Some("claude_codes")) {
            return Ok(LlmCredentials::claude_codes(
                self.api_key.clone(),
                self.base_url.clone(),
                self.claude_path.clone(),
            ));
        }
        if matches!(provider.as_deref(), Some("vertex_dummy")) {
            let project_id = required_field(self.project_id.as_deref(), "project_id")?;
            let region = required_field(self.region.as_deref(), "region")?;
            let base_url = required_field(self.base_url.as_deref(), "base_url")?;
            return Ok(LlmCredentials::vertex_dummy(
                api_key, project_id, region, base_url,
            ));
        }
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
        match self.base_url.as_deref() {
            Some(base_url) => Ok(LlmCredentials::anthropic_with_base_url(api_key, base_url)),
            None => Ok(LlmCredentials::anthropic(api_key)),
        }
    }

    pub fn client_builder(&self) -> Result<kres_llm::client::ClientBuilder, AgentError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                AgentError::Other(format!("invalid HTTP header name {name:?}: {e}"))
            })?;
            let value = HeaderValue::from_str(&expand_env(value)).map_err(|e| {
                AgentError::Other(format!("invalid value for HTTP header {name}: {e}"))
            })?;
            headers.insert(name, value);
        }
        if let Some(name) = self.session_header.as_deref() {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                AgentError::Other(format!("invalid session header name {name:?}: {e}"))
            })?;
            let value = HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())
                .expect("UUID is a valid header value");
            headers.insert(name, value);
        }

        let mut builder =
            kres_llm::client::Client::builder(self.credentials()?).default_headers(headers);
        if let Some(proxy) = self.proxy.as_deref() {
            builder = if proxy.is_empty() {
                builder.no_proxy()
            } else {
                builder.proxy(Some(expand_env(proxy)))
            };
        }
        if let Some(tls) = &self.tls {
            builder = builder.identity_pem(resolve_identity(tls)?);
            for path in &tls.ca_certificates {
                let path = expand_env(path);
                let pem = std::fs::read(&path)
                    .map_err(|e| AgentError::Other(format!("reading TLS CA bundle {path}: {e}")))?;
                builder = builder.ca_pem_bundle(pem);
            }
        }
        Ok(builder)
    }

    pub fn credential_cache_key(&self) -> Result<String, AgentError> {
        Ok(self.credentials()?.cache_key())
    }

    fn validate_credentials(&self, cfg_path: &Path) -> Result<(), AgentError> {
        let provider = self.provider.as_deref().map(normalize_provider);
        if matches!(provider.as_deref(), Some("codex_codes")) {
            return Ok(());
        }
        if matches!(provider.as_deref(), Some("claude_codes")) {
            return Ok(());
        }
        if matches!(provider.as_deref(), Some("vertex_dummy")) {
            return match self.api_key.as_deref() {
                Some(k) if k.starts_with('@') && k.ends_with('@') => {
                    Err(AgentError::Other(format!(
                        "agent config {} still contains the placeholder key {:?}",
                        cfg_path.display(),
                        k
                    )))
                }
                Some(k) if !valid_secret(k) => Err(AgentError::Other(format!(
                    "agent config {} has an invalid `api_key`",
                    cfg_path.display()
                ))),
                _ => Ok(()),
            };
        }
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

fn split_config_selector(path: &Path) -> (PathBuf, Option<String>) {
    let rendered = path.to_string_lossy();
    match rendered.rsplit_once(".json:") {
        Some((base, model)) if !model.is_empty() => (
            PathBuf::from(format!("{base}.json")),
            Some(model.to_string()),
        ),
        _ => (path.to_path_buf(), None),
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

fn select_model_config(
    value: serde_json::Value,
    requested_model: Option<&str>,
) -> Result<serde_json::Value, AgentError> {
    let mut provider = value
        .as_object()
        .cloned()
        .ok_or_else(|| AgentError::Other("provider config must be a JSON object".into()))?;
    for forbidden in [
        "model",
        "max_tokens",
        "max_input_tokens",
        "rate_limit",
        "thinking",
        "defaults",
        "fast",
        "slow",
        "main",
        "todo",
        "classifier",
    ] {
        if provider.contains_key(forbidden) {
            return Err(AgentError::Other(format!(
                "provider config must put `{forbidden}` inside a `models.<model-id>` entry"
            )));
        }
    }
    let models = provider.remove("models").ok_or_else(|| {
        AgentError::Other("provider config missing required `models` object".into())
    })?;
    let models = models.as_object().ok_or_else(|| {
        AgentError::Other("provider config `models` must be a JSON object".into())
    })?;
    let model_id = match requested_model {
        Some(id) => id,
        None if models.len() == 1 => models.keys().next().expect("one model"),
        None => {
            return Err(AgentError::Other(format!(
                "provider config contains {} models; select one explicitly",
                models.len()
            )))
        }
    };
    let model = models.get(model_id).ok_or_else(|| {
        AgentError::Other(format!(
            "provider config does not provide model `{model_id}`"
        ))
    })?;
    let model = model.as_object().ok_or_else(|| {
        AgentError::Other(format!("model `{model_id}` config must be a JSON object"))
    })?;
    for (key, value) in model {
        if is_credential_key(key) || matches!(key.as_str(), "model" | "system" | "system_file") {
            return Err(AgentError::Other(format!(
                "model `{model_id}` must not set shared field `{key}`"
            )));
        }
        provider.insert(key.clone(), value.clone());
    }
    provider.insert("model".into(), serde_json::Value::String(model_id.into()));
    Ok(serde_json::Value::Object(provider))
}

fn is_credential_key(key: &str) -> bool {
    matches!(
        key,
        "api_key"
            | "provider"
            | "base_url"
            | "host"
            | "api_version"
            | "codex_path"
            | "codex_home"
            | "codex_config"
            | "claude_path"
            | "project_id"
            | "region"
            | "headers"
            | "session_header"
            | "tls"
            | "proxy"
    )
}

fn required_field<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, AgentError> {
    value
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| AgentError::Other(format!("agent config missing `{name}`")))
}

fn expand_env(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        out.push_str(&std::env::var(name).unwrap_or_default());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn resolve_identity(tls: &TlsConfig) -> Result<Vec<u8>, AgentError> {
    let mut failures = Vec::new();
    for candidate in &tls.identity_candidates {
        let cert_path = expand_env(&candidate.cert);
        if cert_path.is_empty() {
            failures.push("certificate path expanded to empty".to_string());
            continue;
        }
        let mut pem = match std::fs::read(&cert_path) {
            Ok(pem) => pem,
            Err(e) => {
                failures.push(format!("{cert_path}: {e}"));
                continue;
            }
        };
        if let Some(key) = candidate.key.as_deref() {
            let key_path = expand_env(key);
            if !key_path.is_empty() && key_path != cert_path {
                match std::fs::read(&key_path) {
                    Ok(key) => {
                        pem.push(b'\n');
                        pem.extend(key);
                    }
                    Err(e) => {
                        failures.push(format!("{key_path}: {e}"));
                        continue;
                    }
                }
            }
        }
        return Ok(pem);
    }
    Err(AgentError::Other(format!(
        "no usable mTLS identity: {}",
        failures.join("; ")
    )))
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
        let mut value: serde_json::Value = serde_json::from_str(contents).unwrap();
        if value.get("models").is_none() {
            let object = value.as_object_mut().unwrap();
            let model_id = object
                .remove("model")
                .and_then(|v| v.as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| "test-model".into());
            let mut model = object
                .remove("defaults")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            for key in ["max_tokens", "max_input_tokens", "rate_limit", "thinking"] {
                if let Some(value) = object.remove(key) {
                    model.insert(key.into(), value);
                }
            }
            object.insert(
                "models".into(),
                serde_json::json!({model_id: serde_json::Value::Object(model)}),
            );
        }
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(serde_json::to_string_pretty(&value).unwrap().as_bytes())
            .unwrap();
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
        assert_eq!(c.model.as_deref(), Some("test-model"));
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
    fn model_limits_are_independent_of_agent_role() {
        let p = write_tmp(
            r#"{
                "provider": "openai",
                "api_key": "sk-openai",
                "models": {"gpt-5.5": {
                    "max_tokens": 64000,
                    "rate_limit": 900000,
                    "thinking": {"type": "adaptive", "effort": "medium"}
                }}
            }"#,
        );
        let fast = AgentConfig::load_for_role(&p, AgentKind::Fast).unwrap();
        let slow = AgentConfig::load_for_role(&p, AgentKind::Slow).unwrap();
        assert_eq!(fast.max_tokens, Some(64000));
        assert_eq!(fast.rate_limit, Some(900000));
        assert_eq!(
            fast.thinking.as_ref().map(|t| t.to_budget(64000)),
            Some(ThinkingBudget::Adaptive(Effort::Medium))
        );
        assert_eq!(slow.max_tokens, Some(64000));
        assert_eq!(
            slow.thinking.as_ref().map(|t| t.to_budget(64000)),
            Some(ThinkingBudget::Adaptive(Effort::Medium))
        );
        assert!(matches!(
            slow.credentials().unwrap(),
            LlmCredentials::OpenAi { .. }
        ));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn model_entries_reject_shared_connection_fields() {
        let p = write_tmp(
            r#"{
                "api_key": "sk-top-level",
                "models": {"claude-sonnet-4-6": {
                    "api_key": "sk-role-level",
                    "max_tokens": 64000
                }}
            }"#,
        );
        let msg = format!(
            "{}",
            AgentConfig::load_for_role(&p, AgentKind::Slow).unwrap_err()
        );
        assert!(
            msg.contains("must not set shared field `api_key`"),
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
            r#"{"api_key":"sk-x","system_file":"prompt.md","models":{"test-model":{}}}"#,
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
            r#"{{"api_key":"sk-x","system_file":"{}","models":{{"test-model":{{}}}}}}"#,
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
            r#"{"api_key":"sk-x","system":"inline-should-lose","system_file":"prompt.md","models":{"test-model":{}}}"#,
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
            r#"{"api_key":"sk-x","system_file":"system-prompts/main-agent.system.md","models":{"test-model":{}}}"#,
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
            r#"{"api_key":"sk-x","system_file":"system-prompts/main-agent.system.md","models":{"test-model":{}}}"#,
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
            r#"{"api_key":"sk-x","models":{"claude-sonnet-4-6":{}}}"#,
        )
        .unwrap();

        let c = AgentConfig::load_for_role(&cfg_path, AgentKind::Fast).unwrap();
        assert_eq!(c.system.as_deref(), Some("MODEL ROOT OVERRIDE"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vertex_config_does_not_require_api_key() {
        let p = write_tmp(
            r#"{
                "provider": "vertex-dummy",
                "model": "claude-sonnet-4-6",
                "base_url": "https://gateway.example/v1",
                "project_id": "project-a",
                "region": "global",
                "session_header": "X-Session"
            }"#,
        );
        let cfg = AgentConfig::load(&p).unwrap();
        assert!(matches!(
            cfg.credentials().unwrap(),
            LlmCredentials::VertexDummy { .. }
        ));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn codex_codes_config_does_not_require_api_key() {
        let p = write_tmp(
            r#"{
                "provider": "codex-codes",
                "codex_path": "/opt/bin/codex",
                "codex_home": "/tmp/kres-codex-home",
                "codex_config": {
                    "mcp_servers.meta_core.enabled": false,
                    "project_skill_configurable_directories": [],
                    "project_doc_max_bytes": 0
                },
                "models": {"gpt-5.6-sol": {
                    "max_tokens": 128000,
                    "thinking": {"type": "adaptive", "effort": "high"}
                }}
            }"#,
        );
        let cfg = AgentConfig::load(&p).unwrap();
        let credentials = cfg.credentials().unwrap();
        let LlmCredentials::CodexCodes {
            codex_home,
            codex_config,
            ..
        } = credentials
        else {
            panic!("expected codex-codes credentials");
        };
        assert_eq!(codex_home, Some(PathBuf::from("/tmp/kres-codex-home")));
        assert_eq!(
            codex_config.get("mcp_servers.meta_core.enabled"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            codex_config.get("project_skill_configurable_directories"),
            Some(&serde_json::json!([]))
        );
        assert_eq!(cfg.max_tokens, Some(128000));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn claude_codes_config_does_not_require_api_key() {
        let p = write_tmp(
            r#"{
                "provider": "claude-codes",
                "claude_path": "/opt/bin/claude",
                "models": {"claude-sonnet-4-6": {
                    "max_tokens": 64000,
                    "thinking": {"type": "adaptive", "effort": "high"}
                }}
            }"#,
        );
        let cfg = AgentConfig::load(&p).unwrap();
        assert!(matches!(
            cfg.credentials().unwrap(),
            LlmCredentials::ClaudeCodes { .. }
        ));
        assert_eq!(cfg.max_tokens, Some(64000));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn mtls_identity_uses_first_readable_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("identity.pem");
        std::fs::write(&good, b"certificate-and-key").unwrap();
        let tls = TlsConfig {
            identity_candidates: vec![
                TlsIdentityCandidate {
                    cert: dir.path().join("missing.pem").display().to_string(),
                    key: None,
                },
                TlsIdentityCandidate {
                    cert: good.display().to_string(),
                    key: None,
                },
            ],
            ca_certificates: Vec::new(),
        };
        assert_eq!(resolve_identity(&tls).unwrap(), b"certificate-and-key");
    }
}
