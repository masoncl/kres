use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("agent response contains no parseable JSON")]
    NoJson,

    /// Provider reported (or kres preemptively detected) that the
    /// input exceeds the model's per-request token limit. Surfaced
    /// when the caller set `CallConfig::surface_over_input_limit`,
    /// so an upstream retry loop (e.g. the workflow runner's
    /// prior_attempts prune-and-retry) can shrink and reissue.
    #[error("input over limit: actual={actual} limit={limit}")]
    OverInputLimit { actual: u64, limit: u64 },

    #[error("{0}")]
    Other(String),
}
