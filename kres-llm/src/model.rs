//! Model selection + thinking-budget defaults.
//!
//! Fixes bugs.md#R1: default model is `claude-opus-4-7`.
//! Fixes bugs.md#R2: thinking budget default leaves room for output
//! tokens instead of swallowing the entire max_tokens budget.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    VertexDummy,
    CodexCodes,
    ClaudeCodes,
    OpenAi,
}

/// A model id paired with its known output-token ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub max_output_tokens: u32,
}

impl Model {
    pub fn opus_4_7() -> Self {
        Self {
            id: "claude-opus-4-7".to_string(),
            max_output_tokens: 128_000,
        }
    }

    pub fn sonnet_4_6() -> Self {
        Self {
            id: "claude-sonnet-4-6".to_string(),
            max_output_tokens: 64_000,
        }
    }

    /// Wrap an explicit model id from config; unknown ids fall back to
    /// a conservative 64k output ceiling so we don't blow up on an
    /// unexpected string.
    pub fn from_id(id: impl Into<String>) -> Self {
        let id: String = id.into();
        let max_output_tokens = match id.as_str() {
            "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => 128_000,
            id if is_openai_model(id) => 128_000,
            _ => 64_000,
        };
        Self {
            id,
            max_output_tokens,
        }
    }

    pub fn provider(&self) -> Provider {
        if is_openai_model(&self.id) {
            Provider::OpenAi
        } else {
            Provider::Anthropic
        }
    }
}

fn is_openai_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("gpt-") || id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4")
}

/// How extended thinking is configured for a single call.
///
/// Two API shapes are in use:
/// - Explicit budget `{"thinking": {"type": "enabled", "budget_tokens": N}}`
///   for models that do not support adaptive thinking.
/// - Adaptive `{"thinking": {"type": "adaptive"},
///              "output_config": {"effort": "low|medium|high"}}` —
///   opus-4-7 and newer.
///
/// bugs.md#R2: set `thinking_budget = max_tokens - 1`
/// regardless, starving the model of output tokens. The builders below
/// always leave at least 25% of max_tokens for output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingBudget {
    /// No extended-thinking block.
    Disabled,
    /// Explicit-budget thinking. Clamped to leave ≥25% of
    /// max_tokens available for output.
    ExplicitBudget(u32),
    /// New adaptive thinking. The model chooses the budget; the
    /// operator picks an `effort` bias.
    Adaptive(Effort),
}

/// Effort bias passed to adaptive thinking via `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
        }
    }
}

impl ThinkingBudget {
    /// Best default for a given model id.
    ///
    /// - OpenAI reasoning models use the same adaptive effort enum,
    ///   mapped to the provider's reasoning request field.
    /// - Models with "opus-4-7" (or later) in the id use adaptive
    ///   (medium).
    /// - Everything else uses an explicit budget sized for the output cap.
    pub fn default_for_model(model_id: &str, max_tokens: u32) -> Self {
        if is_openai_model(model_id) {
            return ThinkingBudget::Adaptive(Effort::Medium);
        }
        // Model families that require adaptive schema. Keep this list
        // conservative — when in doubt, use explicit-budget thinking.
        let adaptive = model_id.contains("opus-4-7") || model_id.contains("opus-4-8");
        if adaptive {
            ThinkingBudget::Adaptive(Effort::Medium)
        } else {
            Self::default_explicit_for(max_tokens)
        }
    }

    /// Default sane explicit budget: `min(max_tokens / 4, 32_000)`.
    pub fn default_explicit_for(max_tokens: u32) -> Self {
        let quarter = max_tokens / 4;
        let budget = quarter.min(32_000);
        if budget == 0 {
            ThinkingBudget::Disabled
        } else {
            ThinkingBudget::ExplicitBudget(budget)
        }
    }

    /// Model-agnostic default for callers that do not know the model id.
    pub fn default_for(max_tokens: u32) -> Self {
        Self::default_explicit_for(max_tokens)
    }

    /// Construct an explicit budget, clamping to leave at least 25% of
    /// `max_tokens` for output. Returns `Disabled` if caller passes 0.
    pub fn enabled_clamped(requested: u32, max_tokens: u32) -> Self {
        if requested == 0 {
            return ThinkingBudget::Disabled;
        }
        let reserved = max_tokens.div_ceil(4); // ceil(max/4)
        let ceiling = max_tokens.saturating_sub(reserved);
        let clamped = requested.min(ceiling);
        if clamped == 0 {
            ThinkingBudget::Disabled
        } else {
            ThinkingBudget::ExplicitBudget(clamped)
        }
    }

    /// Return `Some(n)` only for explicit-budget thinking.
    pub fn as_budget_tokens(&self) -> Option<u32> {
        match self {
            ThinkingBudget::ExplicitBudget(n) => Some(*n),
            _ => None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, ThinkingBudget::Disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thinking_budget_leaves_room_for_output() {
        // bugs.md#R2: with max_tokens=128000, the old code set
        // budget=127999, leaving 1 token for the answer. The new
        // explicit default MUST leave at least 25% of max_tokens for
        // output.
        let b = ThinkingBudget::default_explicit_for(128_000);
        let tokens = b.as_budget_tokens().unwrap();
        assert!(tokens <= 32_000, "default capped at 32000, got {tokens}");
        assert!(
            128_000 - tokens >= 128_000 / 4,
            "at least a quarter of max_tokens reserved for output"
        );
    }

    #[test]
    fn default_thinking_budget_small_max() {
        let b = ThinkingBudget::default_explicit_for(4_000);
        assert_eq!(b.as_budget_tokens(), Some(1_000));
    }

    #[test]
    fn default_for_model_picks_adaptive_for_opus_47() {
        let b = ThinkingBudget::default_for_model("claude-opus-4-7", 128_000);
        assert!(matches!(b, ThinkingBudget::Adaptive(Effort::Medium)));
    }

    #[test]
    fn default_for_model_picks_explicit_for_opus_46() {
        let b = ThinkingBudget::default_for_model("claude-opus-4-6", 128_000);
        match b {
            ThinkingBudget::ExplicitBudget(n) => {
                assert!(n <= 32_000);
                assert!(128_000 - n >= 128_000 / 4);
            }
            other => panic!("expected ExplicitBudget, got {:?}", other),
        }
    }

    #[test]
    fn default_for_model_unknown_defaults_to_explicit() {
        let b = ThinkingBudget::default_for_model("claude-sonnet-4-6", 64_000);
        assert!(matches!(b, ThinkingBudget::ExplicitBudget(_)));
    }

    #[test]
    fn gpt_models_use_openai_provider_and_reasoning_effort() {
        let m = Model::from_id("gpt-5.5");
        assert_eq!(m.provider(), Provider::OpenAi);
        assert_eq!(m.max_output_tokens, 128_000);
        assert_eq!(
            ThinkingBudget::default_for_model(&m.id, m.max_output_tokens),
            ThinkingBudget::Adaptive(Effort::Medium)
        );
    }

    #[test]
    fn effort_strings() {
        assert_eq!(Effort::Low.as_str(), "low");
        assert_eq!(Effort::Medium.as_str(), "medium");
        assert_eq!(Effort::High.as_str(), "high");
    }

    #[test]
    fn default_thinking_budget_zero_max_is_disabled() {
        let b = ThinkingBudget::default_for(0);
        assert_eq!(b, ThinkingBudget::Disabled);
    }

    #[test]
    fn clamped_requested_budget_respects_quarter_reservation() {
        // User asks for 127999 (what the old default did); we clamp
        // to leave a quarter for output.
        let b = ThinkingBudget::enabled_clamped(127_999, 128_000);
        let tokens = b.as_budget_tokens().unwrap();
        let reserved = 128_000_u32.div_ceil(4);
        assert!(tokens <= 128_000 - reserved);
        // Explicit budget form, not adaptive.
        assert!(matches!(b, ThinkingBudget::ExplicitBudget(_)));
    }

    #[test]
    fn clamped_zero_is_disabled() {
        assert_eq!(
            ThinkingBudget::enabled_clamped(0, 128_000),
            ThinkingBudget::Disabled
        );
    }

    #[test]
    fn from_id_known_values() {
        assert_eq!(Model::from_id("claude-opus-4-8").max_output_tokens, 128_000);
        assert_eq!(Model::from_id("claude-opus-4-7").max_output_tokens, 128_000);
        assert_eq!(Model::from_id("claude-opus-4-6").max_output_tokens, 128_000);
    }

    #[test]
    fn from_id_unknown_falls_back_safely() {
        // Unknown ids get a conservative default rather than panicking.
        let m = Model::from_id("claude-future-model-x");
        assert_eq!(m.max_output_tokens, 64_000);
    }
}
