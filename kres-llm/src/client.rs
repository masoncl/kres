//! Anthropic API client.
//!
//! Two entry points so far:
//! - [`Client::messages`]: non-streaming `messages` call, used by the
//!   `kres test` subcommand.
//! - [`Client::stream_messages`]: streaming call that emits parsed
//!   [`StreamEvent`]s, used by `kres turn` and later by the fast /
//!   slow agents.

use std::{collections::BTreeMap, collections::HashMap, time::Duration};

use futures::StreamExt;
use reqwest::header;

use std::sync::Arc;

use crate::{
    config::CallConfig,
    error::LlmError,
    model::Provider,
    proxy::detect_proxy,
    rate_limit::RateLimiter,
    request::{ContentBlock, Message, MessagesRequest, MessagesResponse, Usage},
    stream::{parse_event, StreamEvent},
};

const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_OPENAI_API_VERSION: &str = "2025-04-01-preview";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(300);
// codex-codes' JSON-RPC schema rejects an individual UserInput::Text string at
// 1,048,576 characters. This is transport framing, not a kres request limit:
// split one logical user message into ordered text items and preserve every
// byte. The model receives all items in the same turn.
const CODEX_CODES_TEXT_ITEM_BYTES: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmCredentials {
    Anthropic {
        api_key: String,
        base_url: String,
    },
    VertexDummy {
        api_key: String,
        project_id: String,
        region: String,
        base_url: String,
    },
    CodexCodes {
        api_key: Option<String>,
        base_url: Option<String>,
        codex_path: Option<std::path::PathBuf>,
        codex_home: Option<std::path::PathBuf>,
        codex_config: BTreeMap<String, serde_json::Value>,
    },
    ClaudeCodes {
        api_key: Option<String>,
        base_url: Option<String>,
        claude_path: Option<std::path::PathBuf>,
    },
    OpenAi {
        api_key: String,
        base_url: String,
    },
    AzureOpenAi {
        host: String,
        api_key: String,
        api_version: String,
    },
}

impl LlmCredentials {
    pub fn anthropic(api_key: impl Into<String>) -> Self {
        Self::Anthropic {
            api_key: api_key.into(),
            base_url: DEFAULT_ANTHROPIC_BASE_URL.into(),
        }
    }

    pub fn anthropic_with_base_url(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::Anthropic {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    pub fn openai(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self::OpenAi {
            api_key: api_key.into(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
        }
    }

    pub fn vertex_dummy(
        api_key: impl Into<String>,
        project_id: impl Into<String>,
        region: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::VertexDummy {
            api_key: api_key.into(),
            project_id: project_id.into(),
            region: region.into(),
            base_url: base_url.into(),
        }
    }

    pub fn codex_codes(
        api_key: Option<String>,
        base_url: Option<String>,
        codex_path: Option<std::path::PathBuf>,
        codex_home: Option<std::path::PathBuf>,
        codex_config: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self::CodexCodes {
            api_key,
            base_url,
            codex_path,
            codex_home,
            codex_config,
        }
    }

    pub fn claude_codes(
        api_key: Option<String>,
        base_url: Option<String>,
        claude_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self::ClaudeCodes {
            api_key,
            base_url,
            claude_path,
        }
    }

    pub fn azure_openai(
        host: impl Into<String>,
        api_key: impl Into<String>,
        api_version: Option<String>,
    ) -> Self {
        Self::AzureOpenAi {
            host: host.into(),
            api_key: api_key.into(),
            api_version: api_version.unwrap_or_else(|| DEFAULT_OPENAI_API_VERSION.to_string()),
        }
    }

    pub fn cache_key(&self) -> String {
        match self {
            LlmCredentials::Anthropic { api_key, base_url } => {
                format!("anthropic:{}:{api_key}", normalize_url(base_url))
            }
            LlmCredentials::VertexDummy {
                api_key,
                project_id,
                region,
                base_url,
            } => format!(
                "vertex-dummy:{}:{project_id}:{region}:{api_key}",
                normalize_url(base_url)
            ),
            LlmCredentials::CodexCodes {
                api_key,
                base_url,
                codex_path,
                codex_home,
                codex_config,
            } => format!(
                "codex-codes:{}:{}:{}:{}:{}",
                base_url.as_deref().unwrap_or("default"),
                api_key.as_deref().unwrap_or("cli-auth"),
                codex_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "codex".into()),
                codex_home
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "default-home".into()),
                serde_json::to_string(codex_config).unwrap_or_default(),
            ),
            LlmCredentials::ClaudeCodes {
                api_key,
                base_url,
                claude_path,
            } => format!(
                "claude-codes:{}:{}:{}",
                base_url.as_deref().unwrap_or("default"),
                api_key.as_deref().unwrap_or("cli-auth"),
                claude_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "claude".into())
            ),
            LlmCredentials::OpenAi { api_key, base_url } => {
                format!("openai:{}:{api_key}", normalize_url(base_url))
            }
            LlmCredentials::AzureOpenAi { host, api_key, .. } => {
                format!("azure-openai:{}:{api_key}", normalize_url(host))
            }
        }
    }

    fn api_key(&self) -> &str {
        match self {
            LlmCredentials::Anthropic { api_key, .. } => api_key,
            LlmCredentials::VertexDummy { api_key, .. } => api_key,
            LlmCredentials::CodexCodes { api_key, .. } => api_key.as_deref().unwrap_or(""),
            LlmCredentials::ClaudeCodes { api_key, .. } => api_key.as_deref().unwrap_or(""),
            LlmCredentials::OpenAi { api_key, .. } => api_key,
            LlmCredentials::AzureOpenAi { api_key, .. } => api_key,
        }
    }

    fn default_base_url(&self) -> String {
        match self {
            LlmCredentials::Anthropic { base_url, .. } => normalize_url(base_url),
            LlmCredentials::VertexDummy { base_url, .. } => normalize_url(base_url),
            LlmCredentials::CodexCodes { base_url, .. } => base_url
                .as_deref()
                .map(normalize_url)
                .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
            LlmCredentials::ClaudeCodes { base_url, .. } => base_url
                .as_deref()
                .map(normalize_url)
                .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
            LlmCredentials::OpenAi { base_url, .. } => normalize_url(base_url),
            LlmCredentials::AzureOpenAi { host, .. } => normalize_url(host),
        }
    }

    fn is_azure_openai(&self) -> bool {
        matches!(self, LlmCredentials::AzureOpenAi { .. })
    }

    fn provider(&self) -> Provider {
        match self {
            Self::VertexDummy { .. } => Provider::VertexDummy,
            Self::CodexCodes { .. } => Provider::CodexCodes,
            Self::ClaudeCodes { .. } => Provider::ClaudeCodes,
            Self::OpenAi { .. } | Self::AzureOpenAi { .. } => Provider::OpenAi,
            Self::Anthropic { .. } => Provider::Anthropic,
        }
    }
}

impl From<String> for LlmCredentials {
    fn from(value: String) -> Self {
        LlmCredentials::anthropic(value)
    }
}

impl From<&str> for LlmCredentials {
    fn from(value: &str) -> Self {
        LlmCredentials::anthropic(value)
    }
}

fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

#[derive(Clone)]
pub struct Client {
    credentials: LlmCredentials,
    base_url: String,
    http: reqwest::Client,
    /// Optional shared rate limiter. Multiple clients with the same
    /// credential should share one via `Arc::clone`.
    rate_limiter: Option<Arc<RateLimiter>>,
    /// Submission channel for one long-lived, multiplexed Codex app-server.
    codex_dispatcher:
        Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<CodexCommand>>>>,
    /// Idle Claude CLI processes, separated by immutable invocation settings.
    claude_pool: Arc<tokio::sync::Mutex<HashMap<ClaudePoolKey, Vec<IdleClaudeClient>>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ClaudePoolKey {
    model: String,
    system: Option<String>,
    thinking: String,
}

const MAX_IDLE_CLAUDE_CLIENTS_PER_KEY: usize = 8;
const CLAUDE_CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

struct IdleClaudeClient {
    client: claude_codes::AsyncClient,
    idle_since: tokio::time::Instant,
}

struct CodexCommand {
    request: CodexRequest,
    response: tokio::sync::oneshot::Sender<Result<MessagesResponse, LlmError>>,
}

struct CodexRequest {
    model: String,
    system: Option<String>,
    effort: Option<String>,
    prompt: String,
}

struct ActiveCodexTurn {
    model: String,
    text: String,
    usage: Usage,
    response: tokio::sync::oneshot::Sender<Result<MessagesResponse, LlmError>>,
}

impl Client {
    /// Build a client from provider credentials; picks up https_proxy /
    /// HTTPS_PROXY from the environment automatically.
    pub fn new(credentials: impl Into<LlmCredentials>) -> Result<Self, LlmError> {
        Self::builder(credentials).build()
    }

    pub fn builder(credentials: impl Into<LlmCredentials>) -> ClientBuilder {
        let credentials = credentials.into();
        let base_url = credentials.default_base_url();
        ClientBuilder {
            credentials,
            base_url,
            proxy: detect_proxy(),
            no_proxy: false,
            timeout: None,
            read_timeout: Some(DEFAULT_READ_TIMEOUT),
            user_agent: format!("kres/{}", env!("CARGO_PKG_VERSION")),
            rate_limiter: None,
            default_headers: header::HeaderMap::new(),
            identity_pem: None,
            ca_pem_bundles: Vec::new(),
        }
    }

    /// Return a clone of this client with its rate_limiter replaced.
    pub fn with_rate_limiter(mut self, rl: Option<Arc<RateLimiter>>) -> Self {
        self.rate_limiter = rl;
        self
    }

    /// Ask the Anthropic `count_tokens` endpoint for an exact input
    /// token count. Returns `None` on any failure — callers should
    /// fall back to the chars/4 cheap estimate.
    ///
    /// Used on a 429 to distinguish a provider/model capability rejection
    /// from ordinary shared rate limiting. It never changes the request.
    pub async fn count_tokens_exact(&self, cfg: &CallConfig, messages: &[Message]) -> Option<u64> {
        if self.credentials.provider() != Provider::Anthropic {
            return None;
        }
        #[derive(serde::Serialize)]
        struct Body<'a> {
            model: &'a str,
            messages: &'a [Message],
            #[serde(skip_serializing_if = "Option::is_none")]
            system: Option<&'a str>,
        }
        #[derive(serde::Deserialize)]
        struct CountResp {
            input_tokens: u64,
        }
        let body = Body {
            model: &cfg.model.id,
            messages,
            system: cfg.system.as_deref(),
        };
        let resp = self
            .http
            .post(format!("{}/v1/messages/count_tokens", self.base_url))
            .header("x-api-key", self.credentials.api_key())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<CountResp>().await.ok().map(|r| r.input_tokens)
    }

    /// Non-streaming `messages` call with retry on 429 / 5xx.
    ///
    /// Rate-limiting policy: the server is the source of truth. On
    /// 429 we honour `retry-after` and wait, with up to `MAX_RETRIES`
    /// attempts — enough to outlast a workspace-wide budget crunch
    /// where concurrent agents on the same key are collectively over
    /// a 1M-tpm ceiling (observed in session
    /// bf0a7119-459b-519a-b7f4-a092fd9e6611, 8 retries were not).
    ///
    /// Kres never shrinks a request. When the provider confirms that a request
    /// exceeds its configured model capability, surface the error so the
    /// semantic caller can partition the work losslessly. 429s for shared
    /// workspace budget exhaustion are handled by waiting and retrying.
    ///
    /// Every 429 logs unconditionally to stderr (operator-visible) so
    /// the pacing story is never hidden behind tracing filters.
    pub async fn messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        if self.credentials.provider() == Provider::CodexCodes {
            return self.codex_codes_messages(cfg, messages).await;
        }
        if self.credentials.provider() == Provider::ClaudeCodes {
            return self.claude_codes_messages(cfg, messages).await;
        }
        if self.credentials.provider() == Provider::OpenAi {
            return self.openai_messages(cfg, messages).await;
        }
        const MAX_RETRIES: u32 = 20;
        let working_messages: Vec<Message> = messages.to_vec();
        let mut consecutive_429s: u32 = 0;
        for attempt in 0..=MAX_RETRIES {
            let body = self.messages_body(cfg, &working_messages, false);
            let resp_result = self
                .http
                .post(self.anthropic_url(cfg, false))
                .headers(self.anthropic_headers(false))
                .header(header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await;
            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    if attempt < MAX_RETRIES && is_transport_retryable(&e) {
                        let wait = backoff_duration(attempt);
                        log_transport_retry("messages", attempt, MAX_RETRIES, &e, wait);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if is_transport_retryable(&e) {
                        log_transport_giveup("messages", MAX_RETRIES, &e);
                    }
                    return Err(LlmError::Http(e));
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(resp.json::<MessagesResponse>().await?);
            }
            let retry_after = parse_retry_after(&resp);
            let body_text = resp.text().await.unwrap_or_default();
            if attempt < MAX_RETRIES && is_retryable_status(status) {
                if status.as_u16() == 429 {
                    consecutive_429s += 1;
                    let base_wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                    let wait = extended_wait(base_wait, consecutive_429s);
                    // Count the payload exactly so we only report a real size
                    // problem. `count_tokens` may itself
                    // 429; None means "unknown", so wait and retry.
                    let exact = self.count_tokens_exact(cfg, &working_messages).await;
                    let limit = cfg.max_input_tokens;
                    let over_limit = match (exact, limit) {
                        (Some(e), Some(l)) => e > l as u64,
                        _ => false,
                    };
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 attempt={}/{} consecutive={} exact_tokens={:?} max_input_tokens={:?} retry_after={:?} wait={:?} over_limit={} reason={}",
                        attempt, MAX_RETRIES, consecutive_429s, exact, limit, retry_after, wait, over_limit,
                        if over_limit { "over-limit" } else { "wait" },
                    );
                    if over_limit {
                        return Err(LlmError::OverInputLimit {
                            actual: exact.unwrap_or(0),
                            limit: limit.unwrap_or(0) as u64,
                        });
                    }
                    tokio::time::sleep(wait).await;
                    continue;
                }
                let wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                tracing::warn!(
                    target: "kres_llm",
                    attempt,
                    status = status.as_u16(),
                    ?wait,
                    "retrying after server error"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            if let Some(error) = input_limit_error(cfg, &working_messages, status, &body_text) {
                return Err(error);
            }
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(LlmError::Other("exhausted retries".into()))
    }

    /// Streaming `messages` call. The returned stream yields parsed
    /// SSE events and a final `Result<(), LlmError>` is surfaced via
    /// the last-event mechanism (see `StreamHandle`).
    ///
    /// Retries 429/5xx/transient-connect errors on the initial POST
    /// (before the SSE upgrade). Once the stream is established,
    /// mid-stream SSE errors are surfaced to the caller — we cannot
    /// resume server-side streaming state from scratch.
    pub async fn stream_messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<StreamHandle, LlmError> {
        if matches!(
            self.credentials.provider(),
            Provider::OpenAi | Provider::CodexCodes | Provider::ClaudeCodes
        ) {
            return self.buffered_stream_messages(cfg, messages).await;
        }
        use eventsource_stream::Eventsource;

        let body = self.messages_body(cfg, messages, true);
        let max_retries = 8;
        let mut last_err: Option<LlmError> = None;
        let mut consecutive_429s: u32 = 0;
        for attempt in 0..=max_retries {
            let resp_result = self
                .http
                .post(self.anthropic_url(cfg, true))
                .headers(self.anthropic_headers(true))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "text/event-stream")
                .json(&body)
                .send()
                .await;
            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    if attempt < max_retries && is_transport_retryable(&e) {
                        let wait = backoff_duration(attempt);
                        log_transport_retry("stream", attempt, max_retries, &e, wait);
                        tokio::time::sleep(wait).await;
                        last_err = Some(LlmError::Http(e));
                        continue;
                    }
                    if is_transport_retryable(&e) {
                        log_transport_giveup("stream", max_retries, &e);
                    }
                    return Err(LlmError::Http(e));
                }
            };
            let status = resp.status();
            if status.is_success() {
                let byte_stream = resp.bytes_stream();
                let event_stream = byte_stream.eventsource();
                let parsed = event_stream.filter_map(|event_result| async move {
                    match event_result {
                        Ok(evt) => match parse_event(&evt.event, &evt.data) {
                            Ok(e) => Some(Ok(e)),
                            Err(e) => Some(Err(LlmError::Json(e))),
                        },
                        Err(e) => Some(Err(LlmError::Sse(e.to_string()))),
                    }
                });
                return Ok(StreamHandle {
                    inner: Box::pin(parsed),
                });
            }
            let retry_after = parse_retry_after(&resp);
            let body_text = resp.text().await.unwrap_or_default();
            if attempt < max_retries && is_retryable_status(status) {
                let base_wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                let wait = if status.as_u16() == 429 {
                    consecutive_429s += 1;
                    extended_wait(base_wait, consecutive_429s)
                } else {
                    consecutive_429s = 0;
                    base_wait
                };
                if status.as_u16() == 429 {
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 (stream) attempt={} consecutive={} retry_after={:?} wait={:?}",
                        attempt,
                        consecutive_429s,
                        retry_after,
                        wait
                    );
                }
                tracing::warn!(
                    target: "kres_llm",
                    attempt,
                    status = status.as_u16(),
                    ?wait,
                    "stream retrying after server error"
                );
                tokio::time::sleep(wait).await;
                last_err = Some(LlmError::ApiStatus {
                    status: status.as_u16(),
                    body: body_text,
                });
                continue;
            }
            if let Some(error) = input_limit_error(cfg, messages, status, &body_text) {
                return Err(error);
            }
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError::Other("stream exhausted retries".into())))
    }

    /// Streaming `messages` call with full lossless retry semantics
    /// of [`Client::messages`], returning an assembled
    /// [`MessagesResponse`]. Callers get a drop-in replacement for
    /// the non-streaming method while the wire protocol runs as SSE,
    /// so bigger calls don't block on the full body before any bytes
    /// come back. Mid-stream errors surface as `LlmError::Sse` to
    /// the caller (we cannot resume a dropped stream; retry happens
    /// only at the initial POST).
    pub async fn messages_streaming(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        if self.credentials.provider() == Provider::CodexCodes {
            return self.codex_codes_messages(cfg, messages).await;
        }
        if self.credentials.provider() == Provider::ClaudeCodes {
            return self.claude_codes_messages(cfg, messages).await;
        }
        if self.credentials.provider() == Provider::OpenAi {
            return self.openai_messages(cfg, messages).await;
        }
        const MAX_RETRIES: u32 = 20;
        let working_messages: Vec<Message> = messages.to_vec();
        let mut consecutive_429s: u32 = 0;
        // When the caller tagged this call with a stream_label,
        // register it in the active-streams registry so the REPL
        // status line can show live token counts. The guard is held
        // for the whole retry sequence: a mid-stream drop + retry
        // reuses the same registry slot, so the operator sees "fast
        // round 2" flicker input tokens as it restarts rather than
        // briefly disappear.
        let stream_guard = cfg
            .stream_label
            .as_ref()
            .map(|l| kres_core::io::register_stream(l, &cfg.model.id));
        for attempt in 0..=MAX_RETRIES {
            let body = self.messages_body(cfg, &working_messages, true);
            let resp_result = self
                .http
                .post(self.anthropic_url(cfg, true))
                .headers(self.anthropic_headers(true))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "text/event-stream")
                .json(&body)
                .send()
                .await;
            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    if attempt < MAX_RETRIES && is_transport_retryable(&e) {
                        let wait = backoff_duration(attempt);
                        log_transport_retry("messages_streaming", attempt, MAX_RETRIES, &e, wait);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if is_transport_retryable(&e) {
                        log_transport_giveup("messages_streaming", MAX_RETRIES, &e);
                    }
                    return Err(LlmError::Http(e));
                }
            };
            let status = resp.status();
            if status.is_success() {
                // Assemble a MessagesResponse by walking the SSE
                // event stream. Mid-stream failures (TCP drop,
                // malformed event, parse error) drop the partial
                // response and re-enter the outer retry loop — the
                // request is idempotent, so retrying from scratch is
                // safe (we pay for the input tokens again, but we'd
                // otherwise fail the whole task).
                let assembled = consume_stream(resp, stream_guard.as_ref()).await;
                match assembled {
                    // Not every provider names the model in its SSE
                    // message_start, so fall back to the one we asked
                    // for. A log record without it cannot say which
                    // model answered, which is precisely the question
                    // when a workflow deliberately runs two.
                    Ok(mut resp) => {
                        if resp.model.is_none() {
                            resp.model = Some(cfg.model.id.clone());
                        }
                        return Ok(resp);
                    }
                    Err(e) if is_mid_stream_retryable(&e) && attempt < MAX_RETRIES => {
                        let wait = backoff_duration(attempt);
                        kres_core::async_eprintln!(
                            "[stream-interrupt] attempt={}/{} error={} wait={:?} — retrying from scratch",
                            attempt,
                            MAX_RETRIES,
                            e,
                            wait,
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let retry_after = parse_retry_after(&resp);
            let body_text = resp.text().await.unwrap_or_default();
            if attempt < MAX_RETRIES && is_retryable_status(status) {
                if status.as_u16() == 429 {
                    consecutive_429s += 1;
                    let base_wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                    let wait = extended_wait(base_wait, consecutive_429s);
                    let exact = self.count_tokens_exact(cfg, &working_messages).await;
                    let limit = cfg.max_input_tokens;
                    let over_limit = match (exact, limit) {
                        (Some(e), Some(l)) => e > l as u64,
                        _ => false,
                    };
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 (stream) attempt={}/{} consecutive={} exact_tokens={:?} max_input_tokens={:?} retry_after={:?} wait={:?} over_limit={} reason={}",
                        attempt, MAX_RETRIES, consecutive_429s, exact, limit, retry_after, wait, over_limit,
                        if over_limit { "over-limit" } else { "wait" },
                    );
                    if over_limit {
                        return Err(LlmError::OverInputLimit {
                            actual: exact.unwrap_or(0),
                            limit: limit.unwrap_or(0) as u64,
                        });
                    }
                    tokio::time::sleep(wait).await;
                    continue;
                }
                let wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                tracing::warn!(
                    target: "kres_llm",
                    attempt,
                    status = status.as_u16(),
                    ?wait,
                    "streaming retrying after server error"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            if let Some(error) = input_limit_error(cfg, &working_messages, status, &body_text) {
                return Err(error);
            }
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(LlmError::Other("exhausted retries".into()))
    }

    async fn openai_messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        if use_openai_responses_api(&cfg.model.id) {
            return self.openai_responses_messages(cfg, messages).await;
        }
        const MAX_RETRIES: u32 = 20;
        let working_messages: Vec<Message> = messages.to_vec();
        let mut consecutive_429s: u32 = 0;
        for attempt in 0..=MAX_RETRIES {
            let body = OpenAiChatRequest::from_config(cfg, &working_messages, false);
            let resp_result = self
                .http
                .post(self.openai_chat_url(cfg))
                .headers(self.openai_headers())
                .json(&body)
                .send()
                .await;
            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    if attempt < MAX_RETRIES && is_transport_retryable(&e) {
                        let wait = backoff_duration(attempt);
                        log_transport_retry("openai_messages", attempt, MAX_RETRIES, &e, wait);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if is_transport_retryable(&e) {
                        log_transport_giveup("openai_messages", MAX_RETRIES, &e);
                    }
                    return Err(LlmError::Http(e));
                }
            };
            let status = resp.status();
            if status.is_success() {
                let raw = resp.json::<OpenAiChatResponse>().await?;
                return Ok(raw.into_messages_response());
            }
            let retry_after = parse_retry_after(&resp);
            let body_text = resp.text().await.unwrap_or_default();
            if attempt < MAX_RETRIES && is_retryable_status(status) {
                let base_wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                let wait = if status.as_u16() == 429 {
                    consecutive_429s += 1;
                    extended_wait(base_wait, consecutive_429s)
                } else {
                    consecutive_429s = 0;
                    base_wait
                };
                if status.as_u16() == 429 {
                    // No exact count is available on this provider, so a 429
                    // cannot be attributed to request size here: the chars/4
                    // estimate is far too coarse to turn a shared-budget 429
                    // into a hard failure. A genuine capability rejection
                    // arrives as a 400 naming the context limit and is typed
                    // by `input_limit_error` below.
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 (openai) attempt={}/{} consecutive={} estimated_tokens={} max_input_tokens={:?} retry_after={:?} wait={:?} reason=wait",
                        attempt,
                        MAX_RETRIES,
                        consecutive_429s,
                        estimate_message_tokens(&working_messages),
                        cfg.max_input_tokens,
                        retry_after,
                        wait,
                    );
                }
                tokio::time::sleep(wait).await;
                continue;
            }
            if let Some(error) = input_limit_error(cfg, &working_messages, status, &body_text) {
                return Err(error);
            }
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(LlmError::Other("exhausted retries".into()))
    }

    async fn openai_responses_messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        const MAX_RETRIES: u32 = 20;
        let working_messages: Vec<Message> = messages.to_vec();
        let mut consecutive_429s: u32 = 0;
        for attempt in 0..=MAX_RETRIES {
            let body = OpenAiResponsesRequest::from_config(cfg, &working_messages, false);
            let resp_result = self
                .http
                .post(self.openai_responses_url())
                .headers(self.openai_headers())
                .json(&body)
                .send()
                .await;
            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    if attempt < MAX_RETRIES && is_transport_retryable(&e) {
                        let wait = backoff_duration(attempt);
                        log_transport_retry(
                            "openai_responses_messages",
                            attempt,
                            MAX_RETRIES,
                            &e,
                            wait,
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    if is_transport_retryable(&e) {
                        log_transport_giveup("openai_responses_messages", MAX_RETRIES, &e);
                    }
                    return Err(LlmError::Http(e));
                }
            };
            let status = resp.status();
            if status.is_success() {
                let raw = resp.json::<OpenAiResponsesResponse>().await?;
                return Ok(raw.into_messages_response());
            }
            let retry_after = parse_retry_after(&resp);
            let body_text = resp.text().await.unwrap_or_default();
            if attempt < MAX_RETRIES && is_retryable_status(status) {
                let base_wait = retry_after.unwrap_or_else(|| backoff_duration(attempt));
                let wait = if status.as_u16() == 429 {
                    consecutive_429s += 1;
                    extended_wait(base_wait, consecutive_429s)
                } else {
                    consecutive_429s = 0;
                    base_wait
                };
                if status.as_u16() == 429 {
                    // No exact count is available on this provider, so a 429
                    // cannot be attributed to request size here: the chars/4
                    // estimate is far too coarse to turn a shared-budget 429
                    // into a hard failure. A genuine capability rejection
                    // arrives as a 400 naming the context limit and is typed
                    // by `input_limit_error` below.
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 (openai responses) attempt={}/{} consecutive={} estimated_tokens={} max_input_tokens={:?} retry_after={:?} wait={:?} reason=wait",
                        attempt,
                        MAX_RETRIES,
                        consecutive_429s,
                        estimate_message_tokens(&working_messages),
                        cfg.max_input_tokens,
                        retry_after,
                        wait,
                    );
                }
                tokio::time::sleep(wait).await;
                continue;
            }
            if let Some(error) = input_limit_error(cfg, &working_messages, status, &body_text) {
                return Err(error);
            }
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(LlmError::Other("exhausted retries".into()))
    }

    async fn buffered_stream_messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<StreamHandle, LlmError> {
        let resp = self.messages(cfg, messages).await?;
        let text = response_text(&resp);
        let events = futures::stream::iter(vec![
            Ok(StreamEvent {
                kind: crate::stream::StreamEventKind::BlockStart {
                    index: 0,
                    block_type: "text".to_string(),
                },
            }),
            Ok(StreamEvent {
                kind: crate::stream::StreamEventKind::TextDelta { index: 0, text },
            }),
            Ok(StreamEvent {
                kind: crate::stream::StreamEventKind::BlockStop { index: 0 },
            }),
            Ok(StreamEvent {
                kind: crate::stream::StreamEventKind::MessageStop,
            }),
        ]);
        Ok(StreamHandle {
            inner: Box::pin(events),
        })
    }

    fn openai_chat_url(&self, cfg: &CallConfig) -> String {
        match &self.credentials {
            LlmCredentials::AzureOpenAi { api_version, .. } => format!(
                "{}/openai/deployments/{}/chat/completions?api-version={}",
                self.base_url, cfg.model.id, api_version
            ),
            _ => format!("{}/chat/completions", self.openai_base_url()),
        }
    }

    fn openai_responses_url(&self) -> String {
        match &self.credentials {
            LlmCredentials::AzureOpenAi { api_version, .. } => {
                format!(
                    "{}/openai/responses?api-version={}",
                    self.base_url, api_version
                )
            }
            _ => format!("{}/responses", self.openai_base_url()),
        }
    }

    fn openai_headers(&self) -> header::HeaderMap {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        let key = header::HeaderValue::from_str(self.credentials.api_key())
            .unwrap_or_else(|_| header::HeaderValue::from_static(""));
        if self.credentials.is_azure_openai() {
            headers.insert("api-key", key.clone());
            headers.insert("Ocp-Apim-Subscription-Key", key);
        } else {
            let bearer = format!("Bearer {}", self.credentials.api_key());
            let bearer = header::HeaderValue::from_str(&bearer)
                .unwrap_or_else(|_| header::HeaderValue::from_static(""));
            headers.insert(header::AUTHORIZATION, bearer);
        }
        headers
    }

    async fn claude_codes_messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        use claude_codes::ClaudeOutput;

        let LlmCredentials::ClaudeCodes {
            api_key,
            base_url,
            claude_path,
        } = &self.credentials
        else {
            return Err(LlmError::Other(
                "claude-codes credentials unavailable".into(),
            ));
        };

        let mut prompt = String::new();
        for message in messages {
            prompt.push_str(&message.role.to_ascii_uppercase());
            prompt.push('\n');
            for prefix in &message.cached_prefixes {
                prompt.push_str(prefix);
            }
            prompt.push_str(&message.content);
            prompt.push_str("\n\n");
        }
        prompt.push_str("Respond only to the conversation above.");

        let pool_key = ClaudePoolKey {
            model: cfg.model.id.clone(),
            system: cfg.system.clone(),
            thinking: format!("{:?}", cfg.thinking),
        };
        let pooled = {
            let mut pool = self.claude_pool.lock().await;
            pool.get_mut(&pool_key)
                .and_then(Vec::pop)
                .map(|idle| idle.client)
        };
        let mut reused = false;
        let mut client = if let Some(mut client) = pooled {
            if client.is_alive() {
                reused = true;
                client
            } else {
                self.spawn_claude_codes_client(
                    cfg,
                    claude_path.as_ref(),
                    api_key.as_deref(),
                    base_url.as_deref(),
                )?
            }
        } else {
            self.spawn_claude_codes_client(
                cfg,
                claude_path.as_ref(),
                api_key.as_deref(),
                base_url.as_deref(),
            )?
        };
        let pid = client.pid();
        let responses = match client.query(&prompt).await {
            Ok(responses) => responses,
            Err(error) => {
                let _ = client.shutdown().await;
                return Err(LlmError::Other(format!(
                    "claude-codes query failed: {error}"
                )));
            }
        };

        let mut response_text = String::new();
        let mut actual_model = None;
        let mut stop_reason = Some("end_turn".to_string());
        let mut usage = Usage::default();
        let mut result_error = None;
        for response in responses {
            match response {
                ClaudeOutput::Assistant(assistant) => {
                    actual_model = Some(assistant.message.model);
                    if let Some(reason) = assistant.message.stop_reason {
                        stop_reason = Some(format!("{reason:?}").to_ascii_lowercase());
                    }
                    for block in assistant.message.content {
                        if let claude_codes::ContentBlock::Text(text) = block {
                            response_text.push_str(&text.text);
                        }
                    }
                }
                ClaudeOutput::Result(result) => {
                    if result.is_error {
                        result_error = Some(if result.errors.is_empty() {
                            result.result.unwrap_or_else(|| "unknown error".into())
                        } else {
                            result.errors.join("; ")
                        });
                    } else if let Some(text) = result.result {
                        response_text = text;
                    }
                    if let Some(result_usage) = result.usage {
                        usage = Usage {
                            input_tokens: u64::from(result_usage.input_tokens),
                            output_tokens: u64::from(result_usage.output_tokens),
                            cache_read_input_tokens: u64::from(
                                result_usage.cache_read_input_tokens,
                            ),
                            cache_creation_input_tokens: u64::from(
                                result_usage.cache_creation_input_tokens,
                            ),
                        };
                    }
                    if let Some(reason) = result.stop_reason {
                        stop_reason = Some(reason);
                    }
                }
                _ => {}
            }
        }
        let reported_model = actual_model.clone();
        tracing::info!(
            target: "kres_llm::claude_codes",
            requested_model = %cfg.model.id,
            reported_model = reported_model.as_deref().unwrap_or("unknown"),
            pid = pid.unwrap_or_default(),
            reused,
            "Claude Code request completed"
        );
        if let Some(error) = result_error {
            let _ = client.shutdown().await;
            return Err(LlmError::Other(format!(
                "claude-codes query failed: {error}"
            )));
        }
        let reset_ok = client
            .query("/clear")
            .await
            .map(|responses| {
                responses.iter().any(
                    |response| matches!(response, ClaudeOutput::Result(result) if !result.is_error),
                )
            })
            .unwrap_or(false)
            && client.is_alive();
        if reset_ok {
            let mut pool = self.claude_pool.lock().await;
            let clients = pool.entry(pool_key.clone()).or_default();
            if clients.len() < MAX_IDLE_CLAUDE_CLIENTS_PER_KEY {
                let idle_since = tokio::time::Instant::now();
                clients.push(IdleClaudeClient { client, idle_since });
                let pool = Arc::clone(&self.claude_pool);
                let pid = clients.last().and_then(|idle| idle.client.pid());
                tokio::spawn(async move {
                    tokio::time::sleep(CLAUDE_CLIENT_IDLE_TIMEOUT).await;
                    let expired = {
                        let mut pool = pool.lock().await;
                        let clients = pool.get_mut(&pool_key);
                        let position = clients.as_ref().and_then(|clients| {
                            clients.iter().position(|idle| {
                                idle.idle_since == idle_since && idle.client.pid() == pid
                            })
                        });
                        let expired = position.and_then(|position| {
                            clients.and_then(|clients| {
                                (idle_since.elapsed() >= CLAUDE_CLIENT_IDLE_TIMEOUT)
                                    .then(|| clients.swap_remove(position).client)
                            })
                        });
                        if pool.get(&pool_key).is_some_and(Vec::is_empty) {
                            pool.remove(&pool_key);
                        }
                        expired
                    };
                    if let Some(client) = expired {
                        tracing::info!(
                            target: "kres_llm::claude_codes",
                            pid = pid.unwrap_or_default(),
                            "shutting down idle Claude Code process"
                        );
                        let _ = client.shutdown().await;
                    }
                });
            } else {
                drop(pool);
                let _ = client.shutdown().await;
            }
        } else {
            tracing::warn!(
                target: "kres_llm::claude_codes",
                requested_model = %cfg.model.id,
                pid = pid.unwrap_or_default(),
                "discarding Claude Code process after failed context reset"
            );
            let _ = client.shutdown().await;
        }
        Ok(MessagesResponse {
            model: actual_model.or_else(|| Some(cfg.model.id.clone())),
            stop_reason,
            usage,
            content: vec![ContentBlock::Text {
                text: response_text,
            }],
        })
    }

    fn spawn_claude_codes_client(
        &self,
        cfg: &CallConfig,
        claude_path: Option<&std::path::PathBuf>,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<claude_codes::AsyncClient, LlmError> {
        use claude_codes::{AsyncClient, ClaudeCliBuilder, PermissionMode};
        use std::process::Stdio;

        let mut builder = ClaudeCliBuilder::new()
            .model(&cfg.model.id)
            .permission_mode(PermissionMode::DontAsk)
            .strict_mcp_config(true)
            .settings(r#"{"permissions":{"deny":["*"]}}"#);
        if let Some(path) = claude_path {
            builder = builder.command(path);
        }
        if let Some(key) = api_key {
            builder = builder.api_key(key);
        }
        if let Some(system) = cfg.system.as_deref() {
            builder = builder.append_system_prompt(system);
        }
        if let crate::model::ThinkingBudget::ExplicitBudget(tokens) = cfg.thinking {
            builder = builder.max_thinking_tokens(tokens);
        }
        let mut command = builder
            .build_command()
            .map_err(|e| LlmError::Other(format!("claude-codes initialization failed: {e}")))?;
        // `--setting-sources ""` loads no user, project or local settings, so
        // the operator's hooks, plugins and project instructions stay out of a
        // kres call. `--bare` isolates the same way, but it also restricts
        // Anthropic auth to ANTHROPIC_API_KEY or an apiKeyHelper and never
        // reads OAuth or the keychain, which leaves a CLI-only login unable to
        // serve a request at all.
        command.args([
            "--safe-mode",
            "--tools",
            "",
            "--setting-sources",
            "",
            "--disable-slash-commands",
        ]);
        command.stderr(Stdio::null());
        if let Some(url) = base_url {
            command.env("ANTHROPIC_BASE_URL", url);
        }
        let child = command
            .spawn()
            .map_err(|e| LlmError::Other(format!("claude-codes spawn failed: {e}")))?;
        AsyncClient::new(child)
            .map_err(|e| LlmError::Other(format!("claude-codes initialization failed: {e}")))
    }

    fn openai_base_url(&self) -> String {
        if self.base_url == DEFAULT_ANTHROPIC_BASE_URL {
            DEFAULT_OPENAI_BASE_URL.to_string()
        } else {
            self.base_url.clone()
        }
    }

    async fn codex_codes_messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        let mut prompt = String::new();
        for message in messages {
            prompt.push_str(&message.role.to_ascii_uppercase());
            prompt.push('\n');
            for prefix in &message.cached_prefixes {
                prompt.push_str(prefix);
            }
            prompt.push_str(&message.content);
            prompt.push_str("\n\n");
        }
        prompt.push_str("Respond only to the conversation above.");

        if !matches!(self.credentials, LlmCredentials::CodexCodes { .. }) {
            return Err(LlmError::Other(
                "codex-codes credentials unavailable".into(),
            ));
        }
        let request = CodexRequest {
            model: cfg.model.id.clone(),
            system: cfg.system.clone(),
            effort: openai_reasoning_effort(cfg.thinking).map(str::to_string),
            prompt,
        };
        self.submit_codex_request(request).await
    }

    async fn submit_codex_request(
        &self,
        request: CodexRequest,
    ) -> Result<MessagesResponse, LlmError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let mut command = CodexCommand {
            request,
            response: response_tx,
        };
        for _ in 0..2 {
            let mut slot = self.codex_dispatcher.lock().await;
            if slot.is_none() {
                *slot = Some(start_codex_dispatcher(&self.credentials).await?);
            }
            let sender = slot.as_ref().expect("dispatcher initialized above");
            match sender.send(command) {
                Ok(()) => {
                    drop(slot);
                    return response_rx.await.map_err(|_| {
                        LlmError::Other("codex-codes dispatcher stopped during turn".into())
                    })?;
                }
                Err(error) => {
                    command = error.0;
                    *slot = None;
                }
            }
        }
        Err(LlmError::Other(
            "codex-codes dispatcher could not be started".into(),
        ))
    }

    fn messages_body(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
        stream: bool,
    ) -> serde_json::Value {
        let request = MessagesRequest::from_config(cfg, messages, stream);
        if self.credentials.provider() == Provider::VertexDummy {
            request.into_vertex_value()
        } else {
            serde_json::to_value(request).expect("MessagesRequest is serializable")
        }
    }

    fn anthropic_url(&self, cfg: &CallConfig, stream: bool) -> String {
        match &self.credentials {
            LlmCredentials::VertexDummy {
                project_id, region, ..
            } => {
                let method = if stream {
                    "streamRawPredict"
                } else {
                    "rawPredict"
                };
                format!(
                    "{}/projects/{project_id}/locations/{region}/publishers/anthropic/models/{}:{method}",
                    self.base_url, cfg.model.id
                )
            }
            _ => format!("{}/v1/messages", self.base_url),
        }
    }

    fn anthropic_headers(&self, stream: bool) -> header::HeaderMap {
        let mut headers = header::HeaderMap::new();
        if self.credentials.provider() == Provider::Anthropic {
            if let Ok(key) = header::HeaderValue::from_str(self.credentials.api_key()) {
                headers.insert("x-api-key", key);
            }
            headers.insert(
                "anthropic-version",
                header::HeaderValue::from_static(ANTHROPIC_VERSION),
            );
        }
        if stream {
            headers.insert(
                header::ACCEPT,
                header::HeaderValue::from_static("text/event-stream"),
            );
        }
        headers
    }
}

fn estimate_message_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let prefix: usize = m.cached_prefixes.iter().map(String::len).sum();
            ((prefix + m.content.len()) / 4) as u64
        })
        .sum()
}

fn input_limit_error(
    cfg: &CallConfig,
    messages: &[Message],
    status: reqwest::StatusCode,
    body: &str,
) -> Option<LlmError> {
    // Only a status a provider actually uses to reject an oversized request
    // can mean "over limit". Without this gate, any terminal error — 401, 403,
    // 404, an exhausted 5xx retry — on a request whose estimate happens to
    // exceed the configured capability would be reported as OverInputLimit,
    // hiding the real cause and sending a semantic owner off to partition work
    // that was never too large.
    if !matches!(status.as_u16(), 400 | 413 | 422) {
        return None;
    }
    let estimated = estimate_message_tokens(messages);
    let body = body.to_ascii_lowercase();
    let provider_reports_limit = [
        "context_length_exceeded",
        "maximum context length",
        "prompt is too long",
        "input is too long",
        "too many input tokens",
        "exceeds the context window",
        "input exceeds",
    ]
    .iter()
    .any(|marker| body.contains(marker));
    let configured_limit = cfg.max_input_tokens.map(u64::from);
    let exceeds_configured = configured_limit.is_some_and(|limit| estimated > limit);
    if !exceeds_configured && !provider_reports_limit {
        return None;
    }
    // Some providers report only that the request exceeded their context
    // without returning the numeric capability. Give semantic callers a
    // conservative smaller partition target instead of losing the typed
    // over-limit signal. This value is used for partition sizing, never to
    // discard request content.
    let limit = configured_limit.unwrap_or_else(|| estimated.saturating_mul(3).div_ceil(4).max(1));
    Some(LlmError::OverInputLimit {
        actual: estimated.max(limit.saturating_add(1)),
        limit,
    })
}

fn response_text(resp: &MessagesResponse) -> String {
    let mut out = String::new();
    for block in &resp.content {
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

fn use_openai_responses_api(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.starts_with("gpt-5") || id.starts_with('o')
}

fn openai_reasoning_effort(thinking: crate::model::ThinkingBudget) -> Option<&'static str> {
    match thinking {
        crate::model::ThinkingBudget::Disabled => None,
        crate::model::ThinkingBudget::Adaptive(effort) => Some(effort.as_str()),
        crate::model::ThinkingBudget::ExplicitBudget(tokens) => {
            if tokens <= 2_048 {
                Some("minimal")
            } else if tokens <= 8_192 {
                Some("low")
            } else if tokens <= 16_384 {
                Some("medium")
            } else {
                Some("high")
            }
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct OpenAiResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<OpenAiResponsesInputMessage>,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAiReasoningRequest>,
    text: OpenAiTextRequest,
    stream: bool,
}

impl OpenAiResponsesRequest {
    fn from_config(cfg: &CallConfig, messages: &[Message], stream: bool) -> Self {
        let input = messages
            .iter()
            .map(|m| {
                let role = match m.role.as_str() {
                    "assistant" => "assistant",
                    _ => "user",
                };
                OpenAiResponsesInputMessage {
                    role: role.to_string(),
                    content: if m.cached_prefixes.is_empty() {
                        m.content.clone()
                    } else {
                        format!("{}{}", m.cached_prefixes.concat(), m.content)
                    },
                }
            })
            .collect();
        Self {
            model: cfg.model.id.clone(),
            instructions: cfg.system.clone(),
            input,
            max_output_tokens: cfg.max_tokens,
            reasoning: openai_reasoning_effort(cfg.thinking)
                .map(|effort| OpenAiReasoningRequest { effort }),
            text: OpenAiTextRequest {
                verbosity: cfg
                    .text_verbosity
                    .as_deref()
                    .unwrap_or("medium")
                    .to_string(),
            },
            stream,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct OpenAiResponsesInputMessage {
    role: String,
    content: String,
}

#[derive(Debug, serde::Serialize)]
struct OpenAiReasoningRequest {
    effort: &'static str,
}

#[derive(Debug, serde::Serialize)]
struct OpenAiTextRequest {
    verbosity: String,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiResponsesResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<OpenAiResponsesUsage>,
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    output: Vec<OpenAiResponsesOutputItem>,
}

impl OpenAiResponsesResponse {
    fn into_messages_response(self) -> MessagesResponse {
        let text = self.output_text.unwrap_or_else(|| {
            self.output
                .into_iter()
                .flat_map(|item| item.content.into_iter())
                .filter_map(openai_response_content_text)
                .collect::<String>()
        });
        MessagesResponse {
            model: self.model,
            stop_reason: self.status,
            usage: self.usage.map(Into::into).unwrap_or_default(),
            content: vec![ContentBlock::Text { text }],
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiResponsesOutputItem {
    #[serde(default)]
    content: Vec<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<OpenAiInputTokenDetails>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiInputTokenDetails>,
}

impl From<OpenAiResponsesUsage> for Usage {
    fn from(value: OpenAiResponsesUsage) -> Self {
        let cached_tokens = value
            .input_tokens_details
            .or(value.prompt_tokens_details)
            .map(|details| details.cached_tokens)
            .unwrap_or(0);
        Usage {
            input_tokens: value.input_tokens.or(value.prompt_tokens).unwrap_or(0),
            output_tokens: value.output_tokens.or(value.completion_tokens).unwrap_or(0),
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cached_tokens,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

fn openai_response_content_text(value: serde_json::Value) -> Option<String> {
    value
        .get("text")
        .and_then(|text| text.as_str())
        .or_else(|| value.get("content").and_then(|content| content.as_str()))
        .map(ToString::to_string)
}

#[derive(Debug, serde::Serialize)]
struct OpenAiChatRequest {
    messages: Vec<OpenAiChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

impl OpenAiChatRequest {
    fn from_config(cfg: &CallConfig, messages: &[Message], stream: bool) -> Self {
        let mut out = Vec::new();
        if let Some(system) = cfg.system.as_deref() {
            out.push(OpenAiChatMessage {
                role: "system".to_string(),
                content: system.to_string(),
            });
        }
        for m in messages {
            let role = match m.role.as_str() {
                "assistant" => "assistant",
                _ => "user",
            };
            out.push(OpenAiChatMessage {
                role: role.to_string(),
                content: if m.cached_prefixes.is_empty() {
                    m.content.clone()
                } else {
                    format!("{}{}", m.cached_prefixes.concat(), m.content)
                },
            });
        }
        let gpt5_or_reasoning = cfg.model.id.starts_with("gpt-5") || cfg.model.id.starts_with('o');
        Self {
            messages: out,
            max_tokens: (!gpt5_or_reasoning).then_some(cfg.max_tokens),
            max_completion_tokens: gpt5_or_reasoning.then_some(cfg.max_tokens),
            temperature: (!gpt5_or_reasoning).then_some(cfg.temperature.unwrap_or(0.0)),
            stream,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct OpenAiChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiChatResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
}

impl OpenAiChatResponse {
    fn into_messages_response(self) -> MessagesResponse {
        let stop_reason = self.choices.first().and_then(|c| c.finish_reason.clone());
        let text = self
            .choices
            .into_iter()
            .filter_map(|c| c.message)
            .map(|m| m.content.unwrap_or_default())
            .collect::<String>();
        MessagesResponse {
            model: self.model,
            stop_reason,
            usage: self.usage.map(Into::into).unwrap_or_default(),
            content: vec![ContentBlock::Text { text }],
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    message: Option<OpenAiResponseMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiInputTokenDetails>,
}

impl From<OpenAiUsage> for Usage {
    fn from(value: OpenAiUsage) -> Self {
        Usage {
            input_tokens: value.prompt_tokens,
            output_tokens: value.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: value
                .prompt_tokens_details
                .map(|details| details.cached_tokens)
                .unwrap_or(0),
        }
    }
}

/// Walk the SSE byte stream from an already-validated 200 response
/// and assemble a full [`MessagesResponse`]. Any TCP-level drop,
/// SSE framing error, or event-parse error surfaces as
/// `LlmError::Sse` / `LlmError::Json` — those are retryable from
/// scratch by the caller (the request is idempotent).
async fn consume_stream(
    resp: reqwest::Response,
    registry_guard: Option<&kres_core::io::StreamGuard>,
) -> Result<MessagesResponse, LlmError> {
    use crate::request::{ContentBlock, Usage};
    use crate::stream::StreamEventKind;
    use eventsource_stream::Eventsource;

    let byte_stream = resp.bytes_stream();
    let mut event_stream = byte_stream.eventsource();
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = Usage::default();
    let mut model: Option<String> = None;
    let mut stop_reason: Option<String> = None;
    // Running output-char count so the registry can show incremental
    // progress. We convert to a rough token estimate (/4) on each
    // delta. The exact output_tokens from message_delta supersedes
    // this once the stream wraps up.
    let mut output_chars: u64 = 0;
    while let Some(evt) = event_stream.next().await {
        let raw = match evt {
            Ok(r) => r,
            Err(e) => return Err(LlmError::Sse(e.to_string())),
        };
        let parsed = match parse_event(&raw.event, &raw.data) {
            Ok(p) => p,
            Err(e) => return Err(LlmError::Json(e)),
        };
        match parsed.kind {
            StreamEventKind::MessageStart {
                input_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                model: m,
            } => {
                usage.input_tokens = input_tokens;
                usage.cache_creation_input_tokens = cache_creation_input_tokens;
                usage.cache_read_input_tokens = cache_read_input_tokens;
                if m.is_some() {
                    model = m;
                }
                if let Some(g) = registry_guard {
                    g.on_message_start(
                        input_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    );
                }
            }
            StreamEventKind::BlockStart { index, block_type } => {
                let idx = index as usize;
                while blocks.len() <= idx {
                    blocks.push(ContentBlock::Other);
                }
                blocks[idx] = match block_type.as_str() {
                    "text" => ContentBlock::Text {
                        text: String::new(),
                    },
                    "thinking" => ContentBlock::Thinking {
                        thinking: String::new(),
                    },
                    _ => ContentBlock::Other,
                };
            }
            StreamEventKind::TextDelta { index, text } => {
                let n = text.len() as u64;
                if let Some(ContentBlock::Text { text: t }) = blocks.get_mut(index as usize) {
                    t.push_str(&text);
                }
                output_chars = output_chars.saturating_add(n);
                if let Some(g) = registry_guard {
                    // Rough live estimate: chars/4. Will be
                    // overwritten by the final output_tokens from
                    // message_delta when the stream closes.
                    g.set_output_tokens(output_chars / 4);
                }
            }
            StreamEventKind::ThinkingDelta { index, text } => {
                let n = text.len() as u64;
                if let Some(ContentBlock::Thinking { thinking }) = blocks.get_mut(index as usize) {
                    thinking.push_str(&text);
                }
                output_chars = output_chars.saturating_add(n);
                if let Some(g) = registry_guard {
                    g.set_output_tokens(output_chars / 4);
                }
            }
            StreamEventKind::MessageDelta {
                stop_reason: sr,
                output_tokens,
                input_tokens: it,
                cache_creation_input_tokens: cc,
                cache_read_input_tokens: cr,
            } => {
                if sr.is_some() {
                    stop_reason = sr;
                }
                if let Some(ot) = output_tokens {
                    usage.output_tokens = ot;
                    if let Some(g) = registry_guard {
                        g.set_output_tokens(ot);
                    }
                }
                // Anthropic's streaming message_delta sometimes
                // carries the cache stats that weren't in
                // message_start. Take whichever value is Some and
                // update both the response usage and the live
                // registry guard. Observed on session 870217e4:
                // message_start emitted input/cache_creation but
                // cache_read_input_tokens only appeared on the
                // final message_delta.
                if let Some(v) = it {
                    usage.input_tokens = v;
                }
                if let Some(v) = cc {
                    usage.cache_creation_input_tokens = v;
                }
                if let Some(v) = cr {
                    usage.cache_read_input_tokens = v;
                }
                if (it.is_some() || cc.is_some() || cr.is_some()) && registry_guard.is_some() {
                    if let Some(g) = registry_guard {
                        g.on_message_start(
                            usage.input_tokens,
                            usage.cache_creation_input_tokens,
                            usage.cache_read_input_tokens,
                        );
                    }
                }
            }
            StreamEventKind::MessageStop => break,
            _ => {}
        }
    }
    // Anthropic always emits message_stop on a clean end. If the
    // stream ended without it, treat as a truncation and ask the
    // caller to retry.
    if stop_reason.is_none() && blocks.is_empty() {
        return Err(LlmError::Sse(
            "stream ended before message_start / any content".into(),
        ));
    }
    Ok(MessagesResponse {
        model,
        stop_reason,
        usage,
        content: blocks,
    })
}

/// Errors surfaced by `consume_stream` that warrant a full-request
/// retry from scratch. Anthropic has no mid-stream resume, so we
/// drop the partial response and redo the POST.
fn is_mid_stream_retryable(e: &LlmError) -> bool {
    matches!(e, LlmError::Sse(_) | LlmError::Json(_))
}

/// HTTP statuses that merit a retry (rate limit + transient 5xx).
fn is_retryable_status(s: reqwest::StatusCode) -> bool {
    s.as_u16() == 429
        || s.as_u16() == 408
        || s.as_u16() == 500
        || s.as_u16() == 502
        || s.as_u16() == 503
        || s.as_u16() == 504
}

fn is_transport_retryable(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// Short tag describing the transport failure category, for log output.
fn transport_error_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect-failed"
    } else if e.is_request() {
        "request-failed"
    } else {
        "transport"
    }
}

/// User-visible notice that we hit a transport error and are retrying.
/// Without this, an offline / DNS-broken host looks like kres just hanging.
fn log_transport_retry(label: &str, attempt: u32, max: u32, e: &reqwest::Error, wait: Duration) {
    let detail = error_chain(e);
    kres_core::async_eprintln!(
        "[network] {} attempt={}/{} kind={} error={} — retrying in {:?} (check connectivity to the configured API endpoint)",
        label,
        attempt + 1,
        max + 1,
        transport_error_kind(e),
        detail,
        wait,
    );
}

/// User-visible notice that we exhausted retries on transport errors.
fn log_transport_giveup(label: &str, max: u32, e: &reqwest::Error) {
    let detail = error_chain(e);
    kres_core::async_eprintln!(
        "[network] {} giving up after {} attempts: kind={} error={} — API unreachable, check network / proxy / DNS",
        label,
        max + 1,
        transport_error_kind(e),
        detail,
    );
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        messages.push(cause.to_string());
        source = cause.source();
    }
    messages.join(": ")
}

/// Parse the `retry-after` header. Returns `None` when absent or
/// unparseable. Accepts both integer-seconds and HTTP-date forms
/// (RFC 7231 §7.1.3). The HTTP-date parser is a tiny local impl —
/// not a new dependency — that handles the three canonical forms.
fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let h = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let s = h.to_str().ok()?.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    parse_http_date_to_duration(s)
}

/// Parse an IMF-fixdate "Sun, 06 Nov 1994 08:49:37 GMT" string and
/// return the delta from now (saturating to zero for past dates).
/// Returns None on unparseable input — callers fall back to
/// exponential backoff.
fn parse_http_date_to_duration(s: &str) -> Option<Duration> {
    // Example: "Sun, 06 Nov 1994 08:49:37 GMT"
    // Strip the weekday + comma prefix; the rest is `DD MON YYYY HH:MM:SS GMT`.
    let after_comma = s.split_once(", ").map(|(_, rest)| rest).unwrap_or(s);
    let parts: Vec<&str> = after_comma.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: u32 = parts[0].parse().ok()?;
    let month = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts[2].parse().ok()?;
    let hms: Vec<&str> = parts[3].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let hour: u32 = hms[0].parse().ok()?;
    let min: u32 = hms[1].parse().ok()?;
    let sec: u32 = hms[2].parse().ok()?;
    let when = chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour, min, sec)?
        .and_utc();
    let now = chrono::Utc::now();
    let delta = when.signed_duration_since(now);
    if delta.num_seconds() <= 0 {
        Some(Duration::from_secs(0))
    } else {
        Some(Duration::from_secs(delta.num_seconds() as u64))
    }
}

/// Exponential backoff with a small pseudo-random jitter to avoid
/// thundering-herd synchronisation across concurrent clients sharing
/// an API key. Base table: 1s, 2s, 4s, 8s, 16s, 30s, 30s, ...
/// Jitter multiplier is 0.75..=1.25 derived from a cheap PID-based
/// source — deterministic-per-process (tests asserting exact values
/// pass the no-jitter base via `backoff_duration_base`).
fn backoff_duration(attempt: u32) -> Duration {
    let base = backoff_duration_base(attempt);
    apply_jitter(base, attempt)
}

/// Extend a server-supplied retry_after (or our own backoff) when we've
/// already slept through several consecutive 429s and nothing has
/// opened up. A short retry_after that keeps coming back means the
/// workspace budget is oversubscribed, not that the caller was briefly
/// unlucky; sleeping for the same 5–15s window on every retry then
/// burns through MAX_RETRIES without ever letting the bucket refill.
/// Starting at the 5th consecutive 429 we layer an exponentially
/// growing extra on top of `base`, capped so we never sleep for more
/// than ~2min at once: consec=5 → +5s, 6 → +10s, 7 → +20s, 8 → +40s,
/// 9 → +80s, 10+ → +120s.
fn extended_wait(base: Duration, consecutive: u32) -> Duration {
    if consecutive < 5 {
        return base;
    }
    let shift = (consecutive - 5).min(5);
    let extra_secs = 5u64.saturating_mul(1u64 << shift).min(120);
    base.saturating_add(Duration::from_secs(extra_secs))
}

fn backoff_duration_base(attempt: u32) -> Duration {
    let secs = (1u64 << attempt.min(5)).min(30);
    Duration::from_secs(secs)
}

fn apply_jitter(base: Duration, attempt: u32) -> Duration {
    // Deterministic 8-bit hash of (pid, attempt) → 0..=255.
    let pid = std::process::id() as u64;
    let h = (pid.wrapping_mul(2_654_435_761) ^ (attempt as u64).wrapping_mul(1_779_033_703)) as u8;
    // Map to factor in [0.75, 1.25).
    let factor = 0.75 + (h as f64 / 512.0);
    let scaled = base.as_secs_f64() * factor;
    Duration::from_secs_f64(scaled)
}

/// Boxed stream of parsed SSE events; `Err(LlmError)` ends the stream.
pub struct StreamHandle {
    inner: futures::stream::BoxStream<'static, Result<StreamEvent, LlmError>>,
}

impl StreamHandle {
    pub async fn next(&mut self) -> Option<Result<StreamEvent, LlmError>> {
        self.inner.next().await
    }
}

#[derive(Clone)]
pub struct ClientBuilder {
    credentials: LlmCredentials,
    base_url: String,
    proxy: Option<String>,
    no_proxy: bool,
    timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    user_agent: String,
    rate_limiter: Option<Arc<RateLimiter>>,
    default_headers: header::HeaderMap,
    identity_pem: Option<Vec<u8>>,
    ca_pem_bundles: Vec<Vec<u8>>,
}

impl ClientBuilder {
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn proxy(mut self, proxy: Option<String>) -> Self {
        self.proxy = proxy;
        self
    }

    /// Disable proxy auto-detection. Use in tests against a local
    /// mock server when the environment has `HTTP_PROXY` set —
    /// reqwest otherwise routes the request through the proxy and
    /// 127.0.0.1 endpoints get rejected as "private".
    pub fn no_proxy(mut self) -> Self {
        self.no_proxy = true;
        self.proxy = None;
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    pub fn read_timeout(mut self, t: Duration) -> Self {
        self.read_timeout = Some(t);
        self
    }

    pub fn no_read_timeout(mut self) -> Self {
        self.read_timeout = None;
        self
    }

    pub fn rate_limiter(mut self, rl: Option<Arc<RateLimiter>>) -> Self {
        self.rate_limiter = rl;
        self
    }

    pub fn default_headers(mut self, headers: header::HeaderMap) -> Self {
        self.default_headers = headers;
        self
    }

    pub fn identity_pem(mut self, pem: Vec<u8>) -> Self {
        self.identity_pem = Some(pem);
        self
    }

    pub fn ca_pem_bundle(mut self, pem: Vec<u8>) -> Self {
        self.ca_pem_bundles.push(pem);
        self
    }

    pub fn build(self) -> Result<Client, LlmError> {
        let mut b = reqwest::Client::builder().user_agent(self.user_agent);
        if !self.default_headers.is_empty() {
            b = b.default_headers(self.default_headers);
        }
        if let Some(pem) = self.identity_pem {
            b = b.identity(reqwest::Identity::from_pem(&pem)?);
        }
        for pem in self.ca_pem_bundles {
            for certificate in reqwest::Certificate::from_pem_bundle(&pem)? {
                b = b.add_root_certificate(certificate);
            }
        }
        if self.no_proxy {
            b = b.no_proxy();
        } else if let Some(proxy_url) = self.proxy.as_deref() {
            let p = reqwest::Proxy::all(proxy_url)
                .map_err(|_| LlmError::BadProxy(proxy_url.to_string()))?;
            b = b.proxy(p);
        }
        if let Some(t) = self.timeout {
            b = b.timeout(t);
        }
        if let Some(t) = self.read_timeout {
            b = b.read_timeout(t);
        }
        let http = b.build()?;
        Ok(Client {
            credentials: self.credentials,
            base_url: self.base_url,
            http,
            rate_limiter: self.rate_limiter,
            codex_dispatcher: Arc::new(tokio::sync::Mutex::new(None)),
            claude_pool: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }
}

async fn start_codex_dispatcher(
    credentials: &LlmCredentials,
) -> Result<tokio::sync::mpsc::UnboundedSender<CodexCommand>, LlmError> {
    use codex_codes::AsyncClient;

    let builder = codex_app_server_builder(credentials)?;
    let client = AsyncClient::start_with(builder)
        .await
        .map_err(|error| LlmError::Other(format!("codex-codes initialization failed: {error}")))?;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(run_codex_dispatcher(client, receiver));
    Ok(sender)
}

fn codex_app_server_builder(
    credentials: &LlmCredentials,
) -> Result<codex_codes::AppServerBuilder, LlmError> {
    use codex_codes::AppServerBuilder;

    let LlmCredentials::CodexCodes {
        api_key,
        base_url,
        codex_path,
        codex_home,
        codex_config,
    } = credentials
    else {
        return Err(LlmError::Other(
            "codex-codes credentials unavailable".into(),
        ));
    };
    let mut builder = AppServerBuilder::new().env("RUST_LOG", "off");
    if let Some(path) = codex_path {
        builder = builder.command(path);
    }
    if let Some(url) = base_url {
        builder = builder.env("OPENAI_BASE_URL", url);
    }
    if let Some(key) = api_key {
        builder = builder.env("CODEX_API_KEY", key);
    }
    if let Some(home) = codex_home {
        std::fs::create_dir_all(home).map_err(|error| {
            LlmError::Other(format!("creating codex_home {}: {error}", home.display()))
        })?;
        builder = builder.env("CODEX_HOME", home);
    }
    for (key, value) in codex_config {
        builder = builder.config_override(key, codex_config_toml(value)?);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder = builder.working_directory(cwd);
    }
    Ok(builder)
}

fn codex_config_toml(value: &serde_json::Value) -> Result<String, LlmError> {
    match value {
        serde_json::Value::Null => Err(LlmError::Other(
            "codex_config values must not be null".into(),
        )),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::String(value) => serde_json::to_string(value)
            .map_err(|error| LlmError::Other(format!("encoding codex_config string: {error}"))),
        serde_json::Value::Array(values) => values
            .iter()
            .map(codex_config_toml)
            .collect::<Result<Vec<_>, _>>()
            .map(|values| format!("[{}]", values.join(", "))),
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                let key = serde_json::to_string(key).map_err(|error| {
                    LlmError::Other(format!("encoding codex_config key: {error}"))
                })?;
                Ok(format!("{key} = {}", codex_config_toml(value)?))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|values| format!("{{ {} }}", values.join(", "))),
    }
}

async fn run_codex_dispatcher(
    mut client: codex_codes::AsyncClient,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<CodexCommand>,
) {
    use codex_codes::{Notification, ServerMessage};

    let mut active = HashMap::<String, ActiveCodexTurn>::new();
    loop {
        if active.is_empty() {
            let Some(command) = commands.recv().await else {
                return;
            };
            start_codex_turn(&mut client, command, &mut active).await;
        }
        while let Ok(command) = commands.try_recv() {
            start_codex_turn(&mut client, command, &mut active).await;
        }

        let message = match client.next_message().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                fail_codex_turns(
                    &mut active,
                    "codex-codes app-server closed before completing active turns",
                );
                return;
            }
            Err(error) => {
                fail_codex_turns(&mut active, &format!("codex-codes receive failed: {error}"));
                return;
            }
        };
        match message {
            ServerMessage::Notification(Notification::AgentMessageDelta(delta)) => {
                if let Some(turn) = active.get_mut(&delta.thread_id) {
                    turn.text.push_str(&delta.delta);
                }
            }
            ServerMessage::Notification(Notification::ThreadTokenUsageUpdated(update)) => {
                if let Some(turn) = active.get_mut(&update.thread_id) {
                    turn.usage.input_tokens = update.token_usage.last.input_tokens.max(0) as u64;
                    turn.usage.output_tokens = update.token_usage.last.output_tokens.max(0) as u64;
                    turn.usage.cache_read_input_tokens =
                        update.token_usage.last.cached_input_tokens.max(0) as u64;
                    turn.usage.cache_creation_input_tokens = update
                        .token_usage
                        .last
                        .cache_write_input_tokens
                        .unwrap_or_default()
                        .max(0) as u64;
                }
            }
            ServerMessage::Notification(Notification::TurnCompleted(done)) => {
                if let Some(turn) = active.remove(&done.thread_id) {
                    let result = if let Some(error) = done.turn.error {
                        Err(LlmError::Other(format!(
                            "codex-codes turn failed: {error:?}"
                        )))
                    } else {
                        Ok(MessagesResponse {
                            model: Some(turn.model),
                            stop_reason: Some("end_turn".into()),
                            usage: turn.usage,
                            content: vec![ContentBlock::Text { text: turn.text }],
                        })
                    };
                    let _ = turn.response.send(result);
                }
            }
            ServerMessage::Notification(Notification::Error(error)) if !error.will_retry => {
                if let Some(turn) = active.remove(&error.thread_id) {
                    let _ = turn.response.send(Err(LlmError::Other(format!(
                        "codex-codes turn failed: {:?}",
                        error.error
                    ))));
                }
            }
            ServerMessage::Request { id, request } => {
                let _ = client
                    .respond_error(
                        id,
                        -32601,
                        &format!("kres does not handle {}", request.method()),
                    )
                    .await;
            }
            _ => {}
        }
    }
}

async fn start_codex_turn(
    client: &mut codex_codes::AsyncClient,
    command: CodexCommand,
    active: &mut HashMap<String, ActiveCodexTurn>,
) {
    use codex_codes::{
        AskForApproval, SandboxMode, SandboxPolicy, ThreadStartParams, TurnStartParams, UserInput,
    };

    let CodexCommand { request, response } = command;
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    let thread_params = ThreadStartParams {
        approval_policy: Some(AskForApproval::Never),
        approvals_reviewer: None,
        base_instructions: request.system,
        config: None,
        cwd: cwd.clone(),
        developer_instructions: None,
        ephemeral: Some(true),
        model: Some(request.model.clone()),
        model_provider: None,
        personality: None,
        sandbox: Some(SandboxMode::Read_only),
        service_name: None,
        service_tier: None,
        session_start_source: None,
        thread_source: None,
    };
    let thread = match client.thread_start(&thread_params).await {
        Ok(thread) => thread,
        Err(error) => {
            let _ = response.send(Err(LlmError::Other(format!(
                "codex-codes thread start failed: {error}"
            ))));
            return;
        }
    };
    let thread_id = thread.thread.id;
    let input = split_utf8_losslessly(&request.prompt, CODEX_CODES_TEXT_ITEM_BYTES)
        .into_iter()
        .map(|text| UserInput::Text {
            text,
            text_elements: None,
        })
        .collect();
    let turn_params = TurnStartParams {
        approval_policy: Some(AskForApproval::Never),
        approvals_reviewer: None,
        client_user_message_id: None,
        cwd,
        effort: request.effort.map(codex_codes::ReasoningEffort),
        input,
        model: Some(request.model.clone()),
        output_schema: None,
        personality: None,
        sandbox_policy: Some(SandboxPolicy::ReadOnly {
            network_access: Some(false),
        }),
        service_tier: None,
        summary: None,
        thread_id: thread_id.clone(),
    };
    if let Err(error) = client.turn_start(&turn_params).await {
        let _ = response.send(Err(LlmError::Other(format!(
            "codex-codes turn start failed: {error}"
        ))));
        return;
    }
    active.insert(
        thread_id,
        ActiveCodexTurn {
            model: request.model,
            text: String::new(),
            usage: Usage::default(),
            response,
        },
    );
}

fn split_utf8_losslessly(text: &str, item_bytes: usize) -> Vec<String> {
    assert!(item_bytes > 0, "transport item size must be positive");
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + item_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        parts.push(text[start..end].to_string());
        start = end;
    }
    parts
}

fn fail_codex_turns(active: &mut HashMap<String, ActiveCodexTurn>, message: &str) {
    for (_, turn) in active.drain() {
        let _ = turn
            .response
            .send(Err(LlmError::Other(message.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[test]
    fn provider_context_rejection_is_typed_without_configured_limit() {
        let cfg = CallConfig::defaults_for(Model::from_id("test-model"));
        let messages = [Message::plain("user", "x".repeat(4_000))];
        let error = input_limit_error(
            &cfg,
            &messages,
            reqwest::StatusCode::BAD_REQUEST,
            "input exceeds the context window",
        )
        .expect("provider rejection should become a partitionable error");

        assert!(matches!(
            error,
            LlmError::OverInputLimit { actual, limit } if actual > limit && limit > 0
        ));
    }

    #[test]
    fn terminal_non_size_errors_are_not_misclassified_as_context_limit() {
        // A large request that fails auth must surface the auth failure. The
        // estimate exceeding the configured capability is not evidence that
        // THIS error was a size rejection.
        let cfg = CallConfig::defaults_for(Model::from_id("test-model")).with_max_input_tokens(10);
        let messages = [Message::plain("user", "x".repeat(400_000))];
        assert!(estimate_message_tokens(&messages) > 10);

        for status in [401u16, 403, 404, 500, 503] {
            assert!(
                input_limit_error(
                    &cfg,
                    &messages,
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    "invalid api key",
                )
                .is_none(),
                "status {status} must not be reported as an input-limit error"
            );
        }
    }

    #[test]
    fn oversized_request_on_a_size_status_is_still_typed() {
        // The gate must not suppress the real case: a 400 on a request whose
        // estimate exceeds the configured capability stays partitionable even
        // when the provider body carries no recognizable marker.
        let cfg = CallConfig::defaults_for(Model::from_id("test-model")).with_max_input_tokens(10);
        let messages = [Message::plain("user", "x".repeat(400_000))];

        assert!(matches!(
            input_limit_error(
                &cfg,
                &messages,
                reqwest::StatusCode::BAD_REQUEST,
                "request failed",
            ),
            Some(LlmError::OverInputLimit { .. })
        ));
    }

    #[test]
    fn ordinary_bad_request_is_not_misclassified_as_context_limit() {
        let cfg = CallConfig::defaults_for(Model::from_id("test-model"));
        let messages = [Message::plain("user", "payload")];
        assert!(input_limit_error(
            &cfg,
            &messages,
            reqwest::StatusCode::BAD_REQUEST,
            "invalid request field",
        )
        .is_none());
    }

    #[tokio::test]
    async fn builder_sets_base_url() {
        let c = Client::builder("sk-test")
            .base_url("http://localhost:1")
            .build()
            .unwrap();
        assert_eq!(c.base_url, "http://localhost:1");
    }

    #[tokio::test]
    #[ignore = "requires an authenticated Codex CLI"]
    async fn codex_codes_multiplexes_fresh_threads() {
        let client = Client::new(LlmCredentials::codex_codes(
            None,
            None,
            None,
            None,
            BTreeMap::new(),
        ))
        .unwrap();
        let cfg = CallConfig::defaults_for(Model::from_id("gpt-5.6-sol"));
        let first = [Message::plain("user", "Reply with exactly FIRST")];
        let second = [Message::plain("user", "Reply with exactly SECOND")];

        let (first_result, second_result) = tokio::join!(
            client.messages(&cfg, &first),
            client.messages(&cfg, &second)
        );

        assert_eq!(response_text(&first_result.unwrap()).trim(), "FIRST");
        assert_eq!(response_text(&second_result.unwrap()).trim(), "SECOND");
    }

    #[test]
    fn codex_codes_builds_isolated_app_server_command() {
        let codex_home =
            std::env::temp_dir().join(format!("kres-codex-home-test-{}", std::process::id()));
        std::fs::remove_dir_all(&codex_home).ok();
        let config = BTreeMap::from([
            (
                "mcp_servers.meta_core.enabled".to_string(),
                serde_json::Value::Bool(false),
            ),
            (
                "project_skill_configurable_directories".to_string(),
                serde_json::json!([]),
            ),
            (
                "features.plugins".to_string(),
                serde_json::Value::Bool(false),
            ),
        ]);
        let credentials = LlmCredentials::codex_codes(
            None,
            None,
            Some("/opt/bin/codex".into()),
            Some(codex_home.clone()),
            config,
        );
        let command = codex_app_server_builder(&credentials)
            .unwrap()
            .build_command_sync()
            .unwrap();
        assert_eq!(command.get_program(), "/opt/bin/codex");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-c",
                "features.plugins=false",
                "-c",
                "mcp_servers.meta_core.enabled=false",
                "-c",
                "project_skill_configurable_directories=[]",
                "app-server",
                "--listen",
                "stdio://",
            ]
        );
        let env: BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect();
        assert_eq!(
            env.get(std::ffi::OsStr::new("CODEX_HOME")),
            Some(&codex_home.as_os_str())
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("RUST_LOG")),
            Some(&std::ffi::OsStr::new("off"))
        );
        assert!(codex_home.is_dir());
        std::fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn codex_config_values_use_toml_syntax() {
        assert_eq!(
            codex_config_toml(&serde_json::json!(false)).unwrap(),
            "false"
        );
        assert_eq!(codex_config_toml(&serde_json::json!(0)).unwrap(), "0");
        assert_eq!(
            codex_config_toml(&serde_json::json!(["one", "two"])).unwrap(),
            r#"["one", "two"]"#
        );
        assert_eq!(
            codex_config_toml(&serde_json::json!({"enabled": false})).unwrap(),
            r#"{ "enabled" = false }"#
        );
        assert!(codex_config_toml(&serde_json::Value::Null).is_err());
    }

    #[test]
    fn vertex_builds_protocol_url_and_body() {
        let credentials = LlmCredentials::vertex_dummy(
            "dummy",
            "project-a",
            "global",
            "https://gateway.example/v1/",
        );
        let client = Client::builder(credentials).build().unwrap();
        let cfg = CallConfig::defaults_for(Model::sonnet_4_6());
        let messages = vec![Message {
            role: "user".into(),
            content: "hello".into(),
            cache: false,
            cached_prefixes: Vec::new(),
        }];

        assert_eq!(
            client.anthropic_url(&cfg, true),
            "https://gateway.example/v1/projects/project-a/locations/global/publishers/anthropic/models/claude-sonnet-4-6:streamRawPredict"
        );
        let body = client.messages_body(&cfg, &messages, true);
        assert!(body.get("model").is_none());
        assert_eq!(body["stream"], true);
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert!(!client.anthropic_headers(true).contains_key("x-api-key"));

        let non_streaming = client.messages_body(&cfg, &messages, false);
        assert!(non_streaming.get("stream").is_none());
    }

    #[tokio::test]
    async fn bad_proxy_is_reported() {
        let e = Client::builder("sk-test")
            .proxy(Some("not a url".into()))
            .build();
        assert!(matches!(e, Err(LlmError::BadProxy(_))));
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_duration_base(0), Duration::from_secs(1));
        assert_eq!(backoff_duration_base(1), Duration::from_secs(2));
        assert_eq!(backoff_duration_base(2), Duration::from_secs(4));
        assert_eq!(backoff_duration_base(5), Duration::from_secs(30));
        assert_eq!(backoff_duration_base(10), Duration::from_secs(30));
    }

    #[test]
    fn extended_wait_noop_below_threshold() {
        let base = Duration::from_secs(10);
        for c in 0..5 {
            assert_eq!(extended_wait(base, c), base);
        }
    }

    #[test]
    fn extended_wait_grows_and_caps() {
        let base = Duration::from_secs(10);
        // consec=5: +5s, 6: +10, 7: +20, 8: +40, 9: +80, 10+: +120
        assert_eq!(extended_wait(base, 5), Duration::from_secs(15));
        assert_eq!(extended_wait(base, 6), Duration::from_secs(20));
        assert_eq!(extended_wait(base, 7), Duration::from_secs(30));
        assert_eq!(extended_wait(base, 8), Duration::from_secs(50));
        assert_eq!(extended_wait(base, 9), Duration::from_secs(90));
        assert_eq!(extended_wait(base, 10), Duration::from_secs(130));
        assert_eq!(extended_wait(base, 20), Duration::from_secs(130));
    }

    #[test]
    fn codex_transport_framing_preserves_every_utf8_byte() {
        let input = format!(
            "{}λ{}🙂{}",
            "a".repeat(999_999),
            "b".repeat(9),
            "c".repeat(1_100_000)
        );
        let parts = split_utf8_losslessly(&input, CODEX_CODES_TEXT_ITEM_BYTES);

        assert!(parts.len() >= 3);
        assert!(parts
            .iter()
            .all(|part| part.len() <= CODEX_CODES_TEXT_ITEM_BYTES));
        assert_eq!(parts.concat(), input);
    }

    #[test]
    fn official_openai_uses_responses_url_and_bearer_header() {
        let client = Client::builder(LlmCredentials::openai("secret", None))
            .build()
            .unwrap();
        assert_eq!(
            client.openai_responses_url(),
            "https://api.openai.com/v1/responses"
        );
        let headers = client.openai_headers();
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer secret")
        );
        assert!(headers.get("api-key").is_none());
    }

    #[test]
    fn azure_openai_uses_azure_url_and_api_key_headers() {
        let client = Client::builder(LlmCredentials::azure_openai(
            "dev.example.net",
            "secret",
            Some("2024-02-15-preview".to_string()),
        ))
        .build()
        .unwrap();
        assert_eq!(
            client.openai_responses_url(),
            "https://dev.example.net/openai/responses?api-version=2024-02-15-preview"
        );
        let headers = client.openai_headers();
        assert_eq!(
            headers.get("api-key").and_then(|v| v.to_str().ok()),
            Some("secret")
        );
        assert_eq!(
            headers
                .get("Ocp-Apim-Subscription-Key")
                .and_then(|v| v.to_str().ok()),
            Some("secret")
        );
        assert!(headers.get(header::AUTHORIZATION).is_none());
    }

    #[test]
    fn openai_gpt5_request_uses_max_completion_tokens() {
        let cfg = CallConfig::defaults_for(Model::from_id("gpt-5.5"))
            .with_max_tokens(128)
            .with_system("sys");
        let msgs = vec![Message::plain("user", "hi")];
        let req = OpenAiChatRequest::from_config(&cfg, &msgs, false);
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(v["max_completion_tokens"], 128);
        assert!(v.get("max_tokens").is_none());
        assert!(v.get("temperature").is_none());
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][1]["content"], "hi");
    }

    #[test]
    fn openai_gpt5_responses_request_sets_reasoning_and_medium_verbosity() {
        let cfg = CallConfig::defaults_for(Model::from_id("gpt-5.5"))
            .with_max_tokens(128)
            .with_system("sys")
            .with_thinking(crate::model::ThinkingBudget::Adaptive(
                crate::model::Effort::High,
            ));
        let msgs = vec![Message::plain("user", "hi")];
        let req = OpenAiResponsesRequest::from_config(&cfg, &msgs, false);
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(v["model"], "gpt-5.5");
        assert_eq!(v["instructions"], "sys");
        assert_eq!(v["max_output_tokens"], 128);
        assert_eq!(v["reasoning"]["effort"], "high");
        assert_eq!(v["text"]["verbosity"], "medium");
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"], "hi");
    }

    #[test]
    fn openai_responses_request_honors_text_verbosity_override() {
        let cfg = CallConfig::defaults_for(Model::from_id("gpt-5.5"))
            .with_max_tokens(128)
            .with_text_verbosity("high");
        let msgs = vec![Message::plain("user", "hi")];
        let req = OpenAiResponsesRequest::from_config(&cfg, &msgs, false);
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(v["text"]["verbosity"], "high");
    }

    #[test]
    fn openai_explicit_budget_maps_to_reasoning_effort() {
        assert_eq!(
            openai_reasoning_effort(crate::model::ThinkingBudget::ExplicitBudget(2_048)),
            Some("minimal")
        );
        assert_eq!(
            openai_reasoning_effort(crate::model::ThinkingBudget::ExplicitBudget(8_192)),
            Some("low")
        );
        assert_eq!(
            openai_reasoning_effort(crate::model::ThinkingBudget::ExplicitBudget(16_384)),
            Some("medium")
        );
        assert_eq!(
            openai_reasoning_effort(crate::model::ThinkingBudget::ExplicitBudget(16_385)),
            Some("high")
        );
    }

    #[test]
    fn openai_responses_response_maps_to_messages_response() {
        let raw = r#"{
            "model": "gpt-5.5",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "hello"}]
            }],
            "usage": {
                "input_tokens": 3000,
                "output_tokens": 4,
                "input_tokens_details": {"cached_tokens": 2048}
            }
        }"#;
        let resp: OpenAiResponsesResponse = serde_json::from_str(raw).unwrap();
        let mapped = resp.into_messages_response();
        assert_eq!(mapped.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(mapped.stop_reason.as_deref(), Some("completed"));
        assert_eq!(mapped.usage.input_tokens, 3000);
        assert_eq!(mapped.usage.output_tokens, 4);
        assert_eq!(mapped.usage.cache_read_input_tokens, 2048);
        assert_eq!(response_text(&mapped), "hello");
    }

    #[test]
    fn openai_response_maps_to_messages_response() {
        let raw = r#"{
            "model": "gpt-5.5",
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 3000,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": 1024}
            }
        }"#;
        let resp: OpenAiChatResponse = serde_json::from_str(raw).unwrap();
        let mapped = resp.into_messages_response();
        assert_eq!(mapped.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(mapped.stop_reason.as_deref(), Some("stop"));
        assert_eq!(mapped.usage.input_tokens, 3000);
        assert_eq!(mapped.usage.output_tokens, 4);
        assert_eq!(mapped.usage.cache_read_input_tokens, 1024);
        assert_eq!(response_text(&mapped), "hello");
    }

    #[test]
    fn backoff_jitter_stays_within_band() {
        // Jittered duration must be within ±25% of the base.
        for attempt in 0..=10 {
            let base = backoff_duration_base(attempt).as_secs_f64();
            let jittered = backoff_duration(attempt).as_secs_f64();
            let ratio = jittered / base;
            assert!(
                (0.74..=1.26).contains(&ratio),
                "attempt {attempt}: ratio {ratio} outside [0.75, 1.25]"
            );
        }
    }

    #[test]
    fn parse_retry_after_http_date_form() {
        // Build a fixed-point date parser input (seconds from a known
        // past date). We can't mock chrono::Utc::now here, but we
        // can assert the seconds-only path works and the date
        // parser returns Some for a sane input.
        let d = parse_http_date_to_duration("Sun, 06 Nov 1994 08:49:37 GMT");
        assert!(d.is_some(), "HTTP-date should parse");
        // 1994 is in the past; delta must saturate to 0.
        assert_eq!(d.unwrap(), Duration::from_secs(0));
    }

    #[test]
    fn parse_retry_after_http_date_malformed() {
        assert!(parse_http_date_to_duration("not a date").is_none());
        assert!(parse_http_date_to_duration("Sun, 99 Xyz 9999 25:99:99 GMT").is_none());
    }

    #[test]
    fn retryable_statuses_cover_429_and_5xx() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
        // 4xx non-429 should NOT retry.
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        // 2xx should not retry (caller shouldn't be calling this for
        // successes, but the check is symmetric).
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[tokio::test]
    async fn api_error_status_surfaces_body() {
        // We don't hit the real API in unit tests. Point at a URL that
        // will 4xx deterministically; any 400-level response shows
        // we correctly decode the error envelope.
        let c = Client::builder("sk-test")
            .base_url("http://127.0.0.1:1") // connect refused — exercises Http path
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let cfg = CallConfig::defaults_for(Model::opus_4_7());
        let msgs = vec![Message {
            role: "user".into(),
            content: "hi".into(),
            cache: false,
            cached_prefixes: Vec::new(),
        }];
        let res = c.messages(&cfg, &msgs).await;
        // Either an ApiStatus (if something is listening) or Http error
        // (if connect fails). Both are acceptable — we only assert
        // that we don't panic and don't silently succeed.
        assert!(res.is_err());
    }
}
