//! Anthropic `messages` wire schema.

use serde::{Deserialize, Serialize};

use crate::{config::CallConfig, model::ThinkingBudget};

/// A user/assistant message.
///
/// `content` is the per-round / volatile text. `cache` tells the
/// serialiser to wrap the entire body in a single ephemeral cache
/// block. `cached_prefixes` are stable heads of the content, emitted
/// in order before it, each as its own cached block. One prefix is
/// the common case (e.g. the `skills + question` portion of a
/// CodePrompt that doesn't change across gather rounds). Two is used
/// by the lens fan-out, which separates a session-scoped head from a
/// task-scoped one so the session head survives across tasks. With
/// one prefix the wire form is two text blocks:
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
    /// Static prefixes emitted as separately-cached content blocks,
    /// in order, before `content`. Set via
    /// `Message::with_cached_prefix` / `with_cached_prefixes` when the
    /// caller has isolated a stable head that should cache-hit across
    /// rounds even when `content` changes.
    ///
    /// Anthropic requires >=1024 tokens per cached block and permits
    /// at most 4 `cache_control` blocks per request, counting the
    /// system prompt. So a single-message request can afford at most
    /// three prefixes, and a multi-turn one far fewer — see
    /// `mark_last_n_user_cached`, which budgets one marker per
    /// retained user turn. Callers choose split points with both
    /// limits in mind.
    pub cached_prefixes: Vec<CachedPrefix>,
}

/// How long a cached block should survive without a read.
///
/// The provider charges more to WRITE a longer-lived block, so this is
/// a real choice per block, not a free win. It pays only when the same
/// bytes are re-read after the short window has already closed.
/// Measured on the 2026-08-22 arch/x86/kvm/mmu/mmu.c review by
/// replaying every request's block hashes against both windows:
///
/// * the lens session head (skills + previous findings) is stable for
///   a whole task and re-read for 24-59 minutes, so five-minute
///   expiry forced 3.5x the writes an hour would have. `Long` is
///   29.6% cheaper on it.
/// * the per-task head (question, symbols, plan) took 390 distinct
///   values over 1499 uses -- nearly one per task -- so a longer
///   window removes almost no writes and just pays more for each.
///   `Long` is 26.4% MORE expensive on it.
///
/// Blocks cache as a chain: an entry covers everything before it too.
/// So only the FIRST block gains from `Long` independently; giving a
/// later block a longer window while an earlier one keeps `Short`
/// buys nothing, because the prefix it depends on expires first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// The provider default (five minutes).
    #[default]
    Short,
    /// Extended (one hour).
    Long,
}

/// A cached content block and the window it should live for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPrefix {
    pub text: String,
    pub ttl: CacheTtl,
}

impl From<&str> for CachedPrefix {
    fn from(text: &str) -> Self {
        Self::short(text)
    }
}

impl From<String> for CachedPrefix {
    fn from(text: String) -> Self {
        Self::short(text)
    }
}

impl CachedPrefix {
    pub fn short(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ttl: CacheTtl::Short,
        }
    }

    pub fn long(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ttl: CacheTtl::Long,
        }
    }
}

impl Message {
    pub fn plain(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            cache: false,
            cached_prefixes: Vec::new(),
        }
    }

    pub fn cached(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            cache: true,
            cached_prefixes: Vec::new(),
        }
    }

    /// Attach a stable prefix that gets its own ephemeral cache
    /// block on the wire. `content` becomes the tail. Concatenated
    /// prefix + content is what the model sees.
    pub fn with_cached_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.cached_prefixes = vec![CachedPrefix::short(prefix)];
        self
    }

    /// Ordered cached heads. Empty entries are dropped: an empty
    /// block is not cacheable and would only spend one of the four
    /// `cache_control` slots.
    pub fn with_cached_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cached_prefixes = prefixes
            .into_iter()
            .map(CachedPrefix::short)
            .filter(|p: &CachedPrefix| !p.text.is_empty())
            .collect();
        self
    }
}

impl serde::Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut obj = s.serialize_struct("Message", 2)?;
        obj.serialize_field("role", &self.role)?;
        match (self.cached_prefixes.is_empty(), self.cache) {
            (false, want_cache_tail) => {
                // Multi-block form. Every prefix is cached (that's
                // the whole point); the tail is cached when the
                // caller asked for it (usual case: latest user turn
                // stays cached so the next round can extend the cache
                // boundary past it).
                let mut blocks: Vec<serde_json::Value> = self
                    .cached_prefixes
                    .iter()
                    .map(|prefix| {
                        serde_json::json!({
                            "type": "text",
                            "text": prefix.text,
                            "cache_control": CacheControl::ephemeral(prefix.ttl),
                        })
                    })
                    .collect();
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
            (true, true) => {
                let block = serde_json::json!([{
                    "type": "text",
                    "text": self.content,
                    "cache_control": {"type": "ephemeral"},
                }]);
                obj.serialize_field("content", &block)?;
            }
            (true, false) => {
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
        if !m.cached_prefixes.is_empty() {
            // Keep the TEXT the model sees identical — fold the
            // stripped prefixes back into `content` as a plain head,
            // in the order they would have been emitted.
            let head: String = std::mem::take(&mut m.cached_prefixes)
                .into_iter()
                .map(|p| p.text)
                .collect();
            m.content = format!("{head}{}", m.content);
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
            if !m.cached_prefixes.is_empty() {
                let head: String = std::mem::take(&mut m.cached_prefixes)
                    .into_iter()
                    .map(|p| p.text)
                    .collect();
                m.content = format!("{head}{}", m.content);
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
                cached_prefixes: vec!["head-".into()],
            },
            a("a1"),
            u("latest"),
        ];
        mark_last_n_user_cached(&mut h, 1); // keep only `latest`
        assert!(!h[0].cache);
        assert!(h[0].cached_prefixes.is_empty());
        assert_eq!(h[0].content, "head-tail", "prefix folded");
    }

    /// Two cached heads render as two `cache_control` blocks ahead of
    /// the tail, in order. Order is the whole mechanism: Anthropic
    /// caches by prefix, so a session head that is not first can never
    /// be the shared part.
    #[test]
    fn two_cached_prefixes_serialize_as_two_ordered_cached_blocks() {
        let m = Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: false,
            cached_prefixes: vec!["SESSION".into(), "TASK".into()],
        };
        let v = serde_json::to_value(&m).expect("serializes");
        let blocks = v["content"].as_array().expect("block array");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["text"], "SESSION");
        assert_eq!(blocks[1]["text"], "TASK");
        assert_eq!(blocks[2]["text"], "DELTA");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(
            blocks[2].get("cache_control").is_none(),
            "an uncached tail must not spend a cache_control slot"
        );
    }

    /// The window is per block, and only a block that asked for the
    /// long one carries `ttl`. Chained prefix caching is why this is
    /// per block rather than per request: the first block's entry
    /// stands alone, so it is the only one that can profit from
    /// outliving the default window while a later block keeps it.
    #[test]
    fn only_a_long_lived_prefix_carries_an_extended_ttl() {
        let m = Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: false,
            cached_prefixes: vec![CachedPrefix::long("SESSION"), CachedPrefix::short("TASK")],
        };
        let v = serde_json::to_value(&m).expect("serializes");
        let blocks = v["content"].as_array().expect("block array");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[0]["cache_control"]["ttl"], "1h");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(
            blocks[1]["cache_control"].get("ttl").is_none(),
            "the default window must not be spelled out; absent means five minutes"
        );
    }

    #[test]
    fn a_bare_string_prefix_defaults_to_the_short_window() {
        // Every caller that has not thought about it gets the cheap
        // write, not the expensive one.
        let m = Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: false,
            cached_prefixes: vec!["HEAD".into()],
        };
        assert_eq!(m.cached_prefixes[0].ttl, CacheTtl::Short);
        let v = serde_json::to_value(&m).expect("serializes");
        assert!(v["content"][0]["cache_control"].get("ttl").is_none());
    }

    /// System + two heads + a cached tail is exactly Anthropic's cap of
    /// four. The lens path leaves the tail uncached, so it sits at
    /// three; this asserts the ceiling is understood, not exceeded.
    #[test]
    fn a_cached_tail_alongside_two_heads_uses_three_message_slots() {
        let m = Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: true,
            cached_prefixes: vec!["SESSION".into(), "TASK".into()],
        };
        let v = serde_json::to_value(&m).expect("serializes");
        let blocks = v["content"].as_array().expect("block array");
        let cached = blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
        assert_eq!(cached, 3, "system prompt would make four — the cap");
    }

    /// Folding must restore the exact text, in order, or a stripped
    /// history turn shows the model something different from what it
    /// saw when the turn was live.
    #[test]
    fn folding_two_prefixes_preserves_order_and_text() {
        let mut h = vec![
            Message {
                role: "user".into(),
                content: "TAIL".into(),
                cache: true,
                cached_prefixes: vec!["SESSION-".into(), "TASK-".into()],
            },
            u("latest"),
        ];
        mark_last_n_user_cached(&mut h, 1);
        assert!(h[0].cached_prefixes.is_empty());
        assert_eq!(h[0].content, "SESSION-TASK-TAIL");
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
    /// `None` is the provider default window. Present only for the
    /// longer one, since the provider rejects a request whose blocks
    /// do not run longest-lived first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

impl CacheControl {
    pub fn ephemeral(ttl: CacheTtl) -> Self {
        Self {
            kind: "ephemeral",
            ttl: match ttl {
                CacheTtl::Short => None,
                CacheTtl::Long => Some("1h"),
            },
        }
    }
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
        // Cache blocks are processed `tools`, then `system`, then
        // `messages`, and the provider refuses a longer-lived block
        // that follows a shorter-lived one. The system prompt sits
        // ahead of every message block, so its window has to be at
        // least the longest any message asks for. It is also the most
        // stable text in the request — one role prompt, byte-identical
        // on every call — so widening it costs one extra write and
        // buys the reads back immediately.
        let system_ttl = if messages
            .iter()
            .any(|m| m.cached_prefixes.iter().any(|p| p.ttl == CacheTtl::Long))
        {
            CacheTtl::Long
        } else {
            CacheTtl::Short
        };
        let system = cfg.system.as_deref().map(|s| {
            if cfg.system_cached {
                SystemField::Cached([SystemBlock {
                    kind: "text",
                    text: s,
                    cache_control: CacheControl::ephemeral(system_ttl),
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
            cached_prefixes: Vec::new(),
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
            cached_prefixes: Vec::new(),
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
            cached_prefixes: Vec::new(),
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
            cached_prefixes: Vec::new(),
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
            cached_prefixes: Vec::new(),
        }];
        let req = MessagesRequest::from_config(&cfg, &msgs, false);
        let v: Value = serde_json::to_value(&req).unwrap();
        // f32 → JSON number widens through f64 — compare with an epsilon
        // rather than bit-exact equality.
        let t = v.get("temperature").and_then(|x| x.as_f64()).unwrap();
        assert!((t - 0.3_f64).abs() < 1e-6, "got {t}");
        assert!(v.get("thinking").is_none());
    }

    /// Reproduces a live 400: "a ttl='1h' cache_control block must
    /// not come after a ttl='5m' cache_control block. Note that blocks
    /// are processed in the following order: `tools`, `system`,
    /// `messages`." The system block precedes every message block, so
    /// its window has to cover the longest one any message asks for.
    #[test]
    fn a_long_lived_message_block_widens_the_system_block() {
        let cfg = CallConfig::defaults_for(Model::opus_4_7()).with_system("role prompt");
        assert!(cfg.system_cached, "this test needs a cached system block");
        let msgs = vec![Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: false,
            cached_prefixes: vec![CachedPrefix::long("SESSION"), CachedPrefix::short("TASK")],
        }];
        let v: Value = serde_json::to_value(MessagesRequest::from_config(&cfg, &msgs, false))
            .expect("serializes");

        assert_eq!(
            v["system"][0]["cache_control"]["ttl"], "1h",
            "system must not be shorter-lived than a message block that follows it"
        );
        let blocks = v["messages"][0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["cache_control"]["ttl"], "1h");
        assert!(
            blocks[1]["cache_control"].get("ttl").is_none(),
            "the task block keeps the default window"
        );
    }

    /// The common case must not pay for a wider window it never asked
    /// for: no long-lived message block means no `ttl` anywhere.
    #[test]
    fn an_all_default_request_names_no_ttl() {
        let cfg = CallConfig::defaults_for(Model::opus_4_7()).with_system("role prompt");
        let msgs = vec![Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: false,
            cached_prefixes: vec![CachedPrefix::short("HEAD")],
        }];
        let v: Value = serde_json::to_value(MessagesRequest::from_config(&cfg, &msgs, false))
            .expect("serializes");
        assert!(v["system"][0]["cache_control"].get("ttl").is_none());
        assert!(v["messages"][0]["content"][0]["cache_control"]
            .get("ttl")
            .is_none());
    }

    #[test]
    fn request_omits_system_when_absent() {
        let cfg = CallConfig::defaults_for(Model::opus_4_7());
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefixes: Vec::new(),
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
