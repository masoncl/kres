//! Anthropic `messages` wire schema.

use serde::{Deserialize, Serialize};

use crate::{config::CallConfig, model::ThinkingBudget};

/// A user/assistant message.
///
/// `content` is the per-round / volatile text. `cache` tells the
/// serialiser to wrap the entire body in a single ephemeral cache
/// block. `cached_prefix` is an optional stable head of the content
/// (e.g. the `skills + question` portion of a CodePrompt that doesn't
/// change across gather rounds). When set, the wire form becomes
/// two text blocks:
///
/// ```json
/// [
///   {"type":"text","text":"<prefix>","cache_control":{"type":"ephemeral"}},
///   {"type":"text","text":"<volatile>","cache_control":{"type":"ephemeral"}?}
/// ]
/// ```
///
/// The split lets the prefix cache-hit independently of per-round
/// content. Anthropic caps requests at 4 `cache_control` blocks
/// (system + up to 3 messages), so callers should still use
/// `strip_cache_flags` + `mark_latest_cached` on older history.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub cache: bool,
    /// Static prefix emitted as a separately-cached content block
    /// before `content`. Set via `Message::with_cached_prefix` when
    /// the caller has isolated a stable head that should cache-hit
    /// across rounds even when `content` changes. Anthropic requires
    /// ≥1024 tokens per cached block; callers choose the split point
    /// with that in mind.
    pub cached_prefix: Option<String>,
}

impl Message {
    pub fn plain(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            cache: false,
            cached_prefix: None,
        }
    }

    pub fn cached(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            cache: true,
            cached_prefix: None,
        }
    }

    /// Attach a stable prefix that gets its own ephemeral cache
    /// block on the wire. `content` becomes the tail. Concatenated
    /// prefix + content is what the model sees.
    pub fn with_cached_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.cached_prefix = Some(prefix.into());
        self
    }
}

impl serde::Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut obj = s.serialize_struct("Message", 2)?;
        obj.serialize_field("role", &self.role)?;
        match (&self.cached_prefix, self.cache) {
            (Some(prefix), want_cache_tail) => {
                // Two-block form. The prefix is always cached (that's
                // the whole point); the tail is cached when the
                // caller asked for it (usual case: latest user turn
                // stays cached so the next round can extend the cache
                // boundary past it).
                let mut blocks = vec![serde_json::json!({
                    "type": "text",
                    "text": prefix,
                    "cache_control": {"type": "ephemeral"},
                })];
                if want_cache_tail {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": self.content,
                        "cache_control": {"type": "ephemeral"},
                    }));
                } else {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": self.content,
                    }));
                }
                obj.serialize_field("content", &blocks)?;
            }
            (None, true) => {
                let block = serde_json::json!([{
                    "type": "text",
                    "text": self.content,
                    "cache_control": {"type": "ephemeral"},
                }]);
                obj.serialize_field("content", &block)?;
            }
            (None, false) => {
                obj.serialize_field("content", &self.content)?;
            }
        }
        obj.end()
    }
}

/// Clear cache flags on every message, INCLUDING folding any
/// `cached_prefix` back into the `content` head so old cached
/// blocks don't count against the ≤4 per-request cap. Used before
/// appending a new cached user turn.
pub fn strip_cache_flags(msgs: &mut [Message]) {
    for m in msgs {
        m.cache = false;
        if let Some(prefix) = m.cached_prefix.take() {
            // Keep the TEXT the model sees identical — fold the
            // stripped prefix back into `content` as a plain head.
            m.content = format!("{prefix}{}", m.content);
        }
    }
}

/// Mark the last user message cached (no-op if the history is empty
/// or the final entry is an assistant turn).
pub fn mark_latest_cached(msgs: &mut [Message]) {
    if let Some(last) = msgs.last_mut() {
        if last.role == "user" {
            last.cache = true;
        }
    }
}

/// Mark the most recent `n` user turns cached, stripping markers on
/// everything older. Anthropic permits at most 4 `cache_control`
/// blocks per request (system + up to 3 messages), so `n ≤ 3` is
/// safe even when the system prompt is also cached.
///
/// Use this instead of `strip_cache_flags` + `mark_latest_cached`
/// when running a multi-turn loop. With only the latest user
/// marker, Anthropic has no check point at the PRIOR latest user
/// turn — it can't detect the cache entry that was written on the
/// prior round. Keeping both markers gives it two check points:
/// one for the older cached prefix (hit), one for the new tail
/// (miss → fresh cache write). Net: `cache_read` is non-zero on
/// round 2+ of a gather loop.
pub fn mark_last_n_user_cached(msgs: &mut [Message], n: usize) {
    if n == 0 {
        strip_cache_flags(msgs);
        return;
    }
    let mut kept = 0usize;
    // Walk end → start so the tail user turns are the ones we keep.
    for m in msgs.iter_mut().rev() {
        if m.role != "user" {
            continue;
        }
        if kept < n {
            m.cache = true;
            kept += 1;
        } else {
            m.cache = false;
            if let Some(prefix) = m.cached_prefix.take() {
                m.content = format!("{prefix}{}", m.content);
            }
        }
    }
}

#[cfg(test)]
mod cache_helpers_tests {
    use super::*;

    fn u(s: &str) -> Message {
        Message::plain("user", s)
    }
    fn a(s: &str) -> Message {
        Message::plain("assistant", s)
    }

    #[test]
    fn mark_last_n_keeps_n_most_recent_user_turns() {
        let mut h = vec![u("u1"), a("a1"), u("u2"), a("a2"), u("u3")];
        mark_last_n_user_cached(&mut h, 2);
        assert!(!h[0].cache, "u1 should be stripped");
        assert!(h[2].cache, "u2 kept");
        assert!(h[4].cache, "u3 kept");
    }

    #[test]
    fn mark_last_n_zero_strips_all() {
        let mut h = vec![u("u1"), u("u2")];
        h[0].cache = true;
        h[1].cache = true;
        mark_last_n_user_cached(&mut h, 0);
        assert!(!h[0].cache);
        assert!(!h[1].cache);
    }

    #[test]
    fn mark_last_n_skips_assistant_turns() {
        // Assistant turns aren't eligible for cache markers; count
        // only user messages.
        let mut h = vec![u("u1"), a("a1"), a("a2"), u("u2")];
        mark_last_n_user_cached(&mut h, 2);
        assert!(h[0].cache, "u1 kept (2nd-most-recent user)");
        assert!(h[3].cache, "u2 kept (most-recent user)");
    }

    #[test]
    fn mark_last_n_folds_old_prefix_back_into_content() {
        // A previously-cached-prefix message being demoted should
        // keep its text integrity — the prefix folds back into
        // content so the model sees the same bytes.
        let mut h = vec![
            Message {
                role: "user".into(),
                content: "tail".into(),
                cache: true,
                cached_prefix: Some("head-".into()),
            },
            a("a1"),
            u("latest"),
        ];
        mark_last_n_user_cached(&mut h, 1); // keep only `latest`
        assert!(!h[0].cache);
        assert!(h[0].cached_prefix.is_none());
        assert_eq!(h[0].content, "head-tail", "prefix folded");
    }
}

/// Serialised thinking block. Two shapes:
/// - `{"type": "enabled", "budget_tokens": N}` — explicit budget.
/// - `{"type": "adaptive"}` — adaptive (effort rides separately in
///   `output_config.effort`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ThinkingRequest {
    Explicit {
        #[serde(rename = "type")]
        kind: &'static str, // "enabled"
        budget_tokens: u32,
    },
    Adaptive {
        #[serde(rename = "type")]
        kind: &'static str, // "adaptive"
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputConfig {
    pub effort: &'static str,
}

/// System prompt wire representation. Either a plain string
/// (non-cached) or an array with a single ephemeral-cache block.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SystemField<'a> {
    Plain(&'a str),
    Cached([SystemBlock<'a>; 1]),
}

#[derive(Debug, Serialize)]
pub struct SystemBlock<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: &'a str,
    pub cache_control: CacheControl,
}

#[derive(Debug, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: &'static str, // "ephemeral"
}

#[derive(Debug, Serialize)]
pub struct MessagesRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemField<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub stream: bool,
}

impl<'a> MessagesRequest<'a> {
    pub fn from_config(cfg: &'a CallConfig, messages: &'a [Message], stream: bool) -> Self {
        let (thinking, output_config) = match cfg.thinking {
            ThinkingBudget::Disabled => (None, None),
            ThinkingBudget::ExplicitBudget(n) => (
                Some(ThinkingRequest::Explicit {
                    kind: "enabled",
                    budget_tokens: n,
                }),
                None,
            ),
            ThinkingBudget::Adaptive(effort) => (
                Some(ThinkingRequest::Adaptive { kind: "adaptive" }),
                Some(OutputConfig {
                    effort: effort.as_str(),
                }),
            ),
        };
        // Temperature is only valid when thinking is disabled.
        let temperature = if thinking.is_some() {
            None
        } else {
            cfg.temperature
        };
        let system = cfg.system.as_deref().map(|s| {
            if cfg.system_cached {
                SystemField::Cached([SystemBlock {
                    kind: "text",
                    text: s,
                    cache_control: CacheControl { kind: "ephemeral" },
                }])
            } else {
                SystemField::Plain(s)
            }
        });
        Self {
            model: &cfg.model.id,
            max_tokens: cfg.max_tokens,
            messages,
            system,
            thinking,
            output_config,
            temperature,
            stream,
        }
    }

    /// Vertex's Anthropic publisher accepts the Messages payload, but
    /// routes the model in the URL and uses a Vertex-specific API version.
    pub fn into_vertex_value(self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("MessagesRequest is serializable");
        if let Some(object) = value.as_object_mut() {
            object.remove("model");
            // Vertex still requires `stream: true` in the body even though
            // streaming is also selected by the :streamRawPredict endpoint.
            // Omit only the false value used with :rawPredict.
            if object.get("stream") != Some(&serde_json::Value::Bool(true)) {
                object.remove("stream");
            }
            object.insert(
                "anthropic_version".into(),
                serde_json::Value::String("vertex-2023-10-16".into()),
            );
        }
        value
    }
}

/// Non-streaming response envelope — only the fields we use.
#[derive(Debug, Deserialize)]
pub struct MessagesResponse {
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Effort, Model, ThinkingBudget};
    use serde_json::Value;

    #[test]
    fn request_omits_temperature_when_thinking_enabled() {
        let mut cfg = CallConfig::defaults_for(Model::opus_4_7());
        cfg.temperature = Some(0.7);
        assert!(cfg.thinking.is_enabled());
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefix: None,
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, false);
        let v: Value = serde_json::to_value(&req).unwrap();
        assert!(v.get("temperature").is_none());
        assert!(v.get("thinking").is_some());
    }

    #[test]
    fn adaptive_request_serialises_correctly() {
        let cfg = CallConfig::defaults_for(Model::opus_4_7())
            .with_thinking(ThinkingBudget::Adaptive(Effort::High));
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefix: None,
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, false);
        let v: Value = serde_json::to_value(&req).unwrap();
        // `thinking: {"type": "adaptive"}` with no budget_tokens
        assert_eq!(
            v.get("thinking").and_then(|t| t.get("type")),
            Some(&Value::from("adaptive"))
        );
        assert!(v
            .get("thinking")
            .and_then(|t| t.get("budget_tokens"))
            .is_none());
        // `output_config: {"effort": "high"}`
        assert_eq!(
            v.get("output_config").and_then(|o| o.get("effort")),
            Some(&Value::from("high"))
        );
    }

    #[test]
    fn explicit_budget_request_serialises_with_budget_tokens() {
        let cfg = CallConfig::defaults_for(Model::sonnet_4_6())
            .with_thinking(ThinkingBudget::ExplicitBudget(5_000));
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefix: None,
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, false);
        let v: Value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v.get("thinking").and_then(|t| t.get("type")),
            Some(&Value::from("enabled"))
        );
        assert_eq!(
            v.get("thinking").and_then(|t| t.get("budget_tokens")),
            Some(&Value::from(5_000))
        );
        assert!(v.get("output_config").is_none());
    }

    #[test]
    fn disabled_request_has_no_thinking() {
        let cfg =
            CallConfig::defaults_for(Model::opus_4_7()).with_thinking(ThinkingBudget::Disabled);
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefix: None,
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, false);
        let v: Value = serde_json::to_value(&req).unwrap();
        assert!(v.get("thinking").is_none());
        assert!(v.get("output_config").is_none());
    }

    #[test]
    fn request_includes_temperature_when_thinking_disabled() {
        let cfg = CallConfig::defaults_for(Model::opus_4_7())
            .with_thinking(ThinkingBudget::Disabled)
            .with_temperature(0.3);
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefix: None,
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, false);
        let v: Value = serde_json::to_value(&req).unwrap();
        // f32 → JSON number widens through f64 — compare with an epsilon
        // rather than bit-exact equality.
        let t = v.get("temperature").and_then(|x| x.as_f64()).unwrap();
        assert!((t - 0.3_f64).abs() < 1e-6, "got {t}");
        assert!(v.get("thinking").is_none());
    }

    #[test]
    fn request_omits_system_when_absent() {
        let cfg = CallConfig::defaults_for(Model::opus_4_7());
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefix: None,
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, true);
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("system").is_none());
        assert_eq!(v.get("stream"), Some(&Value::Bool(true)));
    }

    #[test]
    fn response_deserializes_content_blocks() {
        let raw = r#"{
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 20},
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "hello"},
                {"type": "tool_use"}
            ]
        }"#;
        let r: MessagesResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.output_tokens, 20);
        assert_eq!(r.content.len(), 3);
        match &r.content[0] {
            ContentBlock::Thinking { thinking } => assert_eq!(thinking, "hmm"),
            _ => panic!("expected thinking"),
        }
        match &r.content[1] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("expected text"),
        }
        matches!(r.content[2], ContentBlock::Other);
    }
}
