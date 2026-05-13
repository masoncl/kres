//! LLM client layer for kres.
//!
//! Owns: model selection, proxy detection, thinking-budget defaults,
//! non-streaming test calls, and streaming turn calls.
//!
//! Scope note: this crate owns provider-specific wire adapters so the
//! agent crates can keep using one message/response abstraction.
//!
//! Invariants owned here (see ../../bugs.md):
//! - R1: default model is `claude-opus-4-7`, never `-4-6`.
//! - R2: default thinking budget is `min(max_tokens / 4, 32_000)` so the
//!   model is not starved of output tokens.

pub mod client;
pub mod config;
pub mod error;
pub mod model;
pub mod proxy;
pub mod rate_limit;
pub mod request;
pub mod stream;

pub use client::LlmCredentials;
pub use config::CallConfig;
pub use error::LlmError;
pub use model::{Effort, Model, Provider, ThinkingBudget};
pub use rate_limit::RateLimiter;
