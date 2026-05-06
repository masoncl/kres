//! Per-call configuration for a single Anthropic request.

use crate::model::{Model, ThinkingBudget};

/// Config for one Anthropic `messages` call.
#[derive(Debug, Clone)]
pub struct CallConfig {
    pub model: Model,
    pub max_tokens: u32,
    pub thinking: ThinkingBudget,
    /// Only honoured when `thinking` is `Disabled`.
    pub temperature: Option<f32>,
    /// Optional system prompt.
    pub system: Option<String>,
    /// Emit the system prompt as a `cache_control: {ephemeral}` block
    /// so the Anthropic prompt cache scores a hit across runs that
    /// reuse the same system. Matches for all four
    /// agents.
    pub system_cached: bool,
    /// Soft input-token ceiling. When a 429 arrives AND the exact
    /// payload size from `count_tokens` exceeds this, the retry path
    /// shrinks the last user turn. `None` means "never shrink; just
    /// wait and retry". Maps to config.
    pub max_input_tokens: Option<u32>,
    /// Display label for the active-streams registry (e.g. "fast
    /// round 2", "slow lens memory"). When Some, `messages_streaming`
    /// registers an entry visible to the REPL status line and
    /// updates its token counters from `message_start` /
    /// `message_delta` events. None = silent call.
    pub stream_label: Option<String>,
}

impl CallConfig {
    /// Config with model-aware defaults: max_tokens = model's output
    /// ceiling, thinking shape chosen by model family.
    pub fn defaults_for(model: Model) -> Self {
        let max_tokens = model.max_output_tokens;
        let thinking = ThinkingBudget::default_for_model(&model.id, max_tokens);
        Self {
            model,
            max_tokens,
            thinking,
            temperature: None,
            system: None,
            system_cached: true,
            max_input_tokens: None,
            stream_label: None,
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        // Re-derive explicit budget when max_tokens changes and the
        // caller hadn't overridden the default. Adaptive/Disabled
        // aren't sized against max_tokens, so they stay put.
        let prev_default = ThinkingBudget::default_explicit_for(self.max_tokens);
        if matches!(self.thinking, ThinkingBudget::ExplicitBudget(_))
            && self.thinking == prev_default
        {
            self.thinking = ThinkingBudget::default_explicit_for(max_tokens);
        }
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_thinking(mut self, thinking: ThinkingBudget) -> Self {
        self.thinking = thinking;
        self
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    pub fn with_max_input_tokens(mut self, n: u32) -> Self {
        self.max_input_tokens = Some(n);
        self
    }

    pub fn with_stream_label(mut self, label: impl Into<String>) -> Self {
        self.stream_label = Some(label.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_for_opus_47_are_sane() {
        let c = CallConfig::defaults_for(Model::opus_4_7());
        assert_eq!(c.max_tokens, 128_000);
        // Opus 4.7 uses adaptive thinking — no budget_tokens, but
        // thinking IS enabled.
        assert!(c.thinking.is_enabled());
        assert!(matches!(c.thinking, ThinkingBudget::Adaptive(_)));
    }

    #[test]
    fn defaults_for_sonnet_46_use_explicit_budget() {
        let c = CallConfig::defaults_for(Model::sonnet_4_6());
        let tb = c.thinking.as_budget_tokens().unwrap();
        // bugs.md#R2: quarter-reservation rule must still hold.
        assert!(tb <= 32_000);
        assert!(c.max_tokens - tb >= c.max_tokens / 4);
    }

    #[test]
    fn builder_methods_chain() {
        let c = CallConfig::defaults_for(Model::opus_4_7())
            .with_max_tokens(8_000)
            .with_system("you are a test agent")
            .with_temperature(0.3);
        assert_eq!(c.max_tokens, 8_000);
        assert_eq!(c.system.as_deref(), Some("you are a test agent"));
        assert_eq!(c.temperature, Some(0.3));
    }
}
