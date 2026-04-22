//! Agent config files.
//!
//! Shape of each per-agent JSON file: `key`, `model`, `max_tokens`,
//! `max_input_tokens`, `rate_limit`, `system` (or `system_file`), plus
//! agent-specific fields like `concurrency` (main).
//!
//! The `key` field carries the literal API key string. Shipped
//! configs in the repo carry `@FAST_KEY@` / `@SLOW_KEY@` placeholders
//! that setup.sh rewrites at install time (setup.sh --fast-key /
//! --slow-key accepts either a literal string or a path whose
//! contents get substituted in).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AgentError;

/// Which agent role this config describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Fast,
    Slow,
    Main,
    Todo,
    Consolidator,
    Merger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Literal API key string. setup.sh substitutes @FAST_KEY@ /
    /// @SLOW_KEY@ placeholders in the shipped configs at install
    /// time; operators can also edit the file directly.
    pub key: String,
    /// Model id override. Required in practice — when omitted, kres
    /// falls back to Model::sonnet_4_6() since there is no key file
    /// to sniff. All shipped configs set this.
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
    /// Max concurrent service workers, only meaningful for the main
    /// agent.
    #[serde(default)]
    pub concurrency: Option<u32>,
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
    ///      directory (so `~/.kres/fast-code-agent.json` can
    ///      reference a sibling `fast-code-agent.system.md`).
    ///
    /// Intended so long prompts can live in versioned `.md` files
    /// rather than as escaped JSON strings.
    #[serde(default)]
    pub system_file: Option<PathBuf>,
}

impl AgentConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        let cfg_path = path.as_ref();
        let raw = std::fs::read_to_string(cfg_path)?;
        let cfg: AgentConfig = serde_json::from_str(&raw)?;
        if cfg.key.trim().is_empty() {
            return Err(AgentError::Other(format!(
                "agent config {} has an empty `key` field — did setup.sh run?",
                cfg_path.display()
            )));
        }
        if cfg.key.starts_with('@') && cfg.key.ends_with('@') {
            return Err(AgentError::Other(format!(
                "agent config {} still contains the placeholder key {:?}; run setup.sh --fast-key/--slow-key to fill it in",
                cfg_path.display(),
                cfg.key
            )));
        }
        let mut cfg = cfg;
        // Resolve and read `system_file` if present. It supersedes
        // any inline `system` — callers that want to override
        // should just drop the `system_file` field.
        //
        // Resolution order, in descending priority:
        //   1. Disk file at the resolved path. An operator who
        //      wants to customize a prompt drops a file at the
        //      referenced path (typically `~/.kres/prompts/X.md`)
        //      and kres reads it.
        //   2. Embedded prompt keyed by the file's basename. This
        //      is the normal path for stock installs — the
        //      `.system.md` files are compiled into the binary
        //      via `include_str!` (see `embedded_prompts` module),
        //      so a fresh install with no `~/.kres/prompts/` copy
        //      still runs. This replaces the previous "setup.sh
        //      must copy every prompt" workflow — operators no
        //      longer need `setup.sh --overwrite` when the repo's
        //      prompts change; rebuilding kres refreshes them.
        //   3. Both missing → error, same as before.
        if let Some(ref sf) = cfg.system_file {
            let expanded = expand_tilde(sf);
            let resolved = if expanded.is_absolute() {
                expanded
            } else {
                // Relative to the config file's parent directory.
                cfg_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(expanded)
            };
            let disk_read = std::fs::read_to_string(&resolved);
            match disk_read {
                Ok(body) => {
                    cfg.system = Some(body);
                }
                Err(disk_err) => {
                    let basename = resolved.file_name().and_then(|o| o.to_str()).unwrap_or("");
                    if let Some(embedded) = crate::embedded_prompts::lookup(basename) {
                        cfg.system = Some(embedded.to_string());
                    } else {
                        return Err(AgentError::Other(format!(
                            "system_file {}: {disk_err} (no embedded fallback for basename '{basename}')",
                            resolved.display()
                        )));
                    }
                }
            }
        }
        Ok(cfg)
    }
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
                "key": "sk-live-key-value",
                "model": "claude-opus-4-7",
                "max_tokens": 128000,
                "max_input_tokens": 900000,
                "rate_limit": 800000,
                "concurrency": 3,
                "system": "you are a fast agent"
            }"#,
        );
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(c.key, "sk-live-key-value");
        assert_eq!(c.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(c.max_tokens, Some(128000));
        assert_eq!(c.concurrency, Some(3));
        assert!(c.system.as_deref().unwrap().contains("fast agent"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn minimal_shape() {
        let p = write_tmp(r#"{"key": "sk-abc"}"#);
        let c = AgentConfig::load(&p).unwrap();
        assert_eq!(c.key, "sk-abc");
        assert_eq!(c.model, None);
        assert_eq!(c.max_tokens, None);
        assert_eq!(c.system, None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn placeholder_key_errors() {
        // An unsubstituted setup.sh placeholder must surface as a
        // clear config error rather than silently hitting the API
        // with a string like "@FAST_KEY@".
        let p = write_tmp(r#"{"key": "@FAST_KEY@"}"#);
        let msg = format!("{}", AgentConfig::load(&p).unwrap_err());
        assert!(
            msg.contains("placeholder") && msg.contains("@FAST_KEY@"),
            "got: {msg}"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_key_errors() {
        let p = write_tmp(r#"{"key": ""}"#);
        let msg = format!("{}", AgentConfig::load(&p).unwrap_err());
        assert!(msg.contains("empty"), "got: {msg}");
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
        std::fs::write(&cfg_path, r#"{"key": "sk-x", "system_file": "prompt.md"}"#).unwrap();
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
            r#"{{"key": "sk-x", "system_file": "{}"}}"#,
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
            r#"{"key": "sk-x", "system": "inline-should-lose", "system_file": "prompt.md"}"#,
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
        let p = write_tmp(r#"{"key": "sk-x", "system_file": "/tmp/does-not-exist-kres-test.md"}"#);
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
        // ~/.kres/prompts/" case), kres uses the compiled-in copy
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
            r#"{"key": "sk-x", "system_file": "prompts/main-agent.system.md"}"#,
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
        let prompts = dir.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(
            prompts.join("main-agent.system.md"),
            "OPERATOR-OVERRIDE BODY",
        )
        .unwrap();
        let cfg_path = dir.join("agent.json");
        std::fs::write(
            &cfg_path,
            r#"{"key": "sk-x", "system_file": "prompts/main-agent.system.md"}"#,
        )
        .unwrap();
        let c = AgentConfig::load(&cfg_path).unwrap();
        assert_eq!(c.system.as_deref(), Some("OPERATOR-OVERRIDE BODY"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
