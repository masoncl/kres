use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("agent response contains no parseable JSON")]
    NoJson,

    /// Provider reported that the input exceeds the model's per-request
    /// capability. The request remains intact so a semantic caller can
    /// partition naturally partitionable work without losing content.
    #[error("input over limit: actual={actual} limit={limit}")]
    OverInputLimit { actual: u64, limit: u64 },

    #[error("{0}")]
    Other(String),
}

impl From<kres_llm::LlmError> for AgentError {
    fn from(error: kres_llm::LlmError) -> Self {
        match error {
            kres_llm::LlmError::OverInputLimit { actual, limit } => {
                Self::OverInputLimit { actual, limit }
            }
            other => Self::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_input_capability_error_remains_typed() {
        let error = AgentError::from(kres_llm::LlmError::OverInputLimit {
            actual: 200,
            limit: 100,
        });
        assert!(matches!(
            error,
            AgentError::OverInputLimit {
                actual: 200,
                limit: 100
            }
        ));
    }
}
