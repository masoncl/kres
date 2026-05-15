use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API returned status {status}: {body}")]
    ApiStatus { status: u16, body: String },

    /// Provider reported (or kres preemptively detected) that the
    /// input exceeds the model's per-request token limit. Returned
    /// only when the caller set `CallConfig::surface_over_input_limit`;
    /// otherwise the client internally shrinks the last user
    /// message and retries.
    #[error("input over limit: actual={actual} limit={limit}")]
    OverInputLimit { actual: u64, limit: u64 },

    #[error("SSE stream error: {0}")]
    Sse(String),

    #[error("JSON decode error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported scheme in proxy URL: {0}")]
    BadProxy(String),

    #[error("{0}")]
    Other(String),
}
