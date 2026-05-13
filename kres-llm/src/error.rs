use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API returned status {status}: {body}")]
    ApiStatus { status: u16, body: String },

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
