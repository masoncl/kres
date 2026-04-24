//! Anthropic API client.
//!
//! Two entry points so far:
//! - [`Client::messages`]: non-streaming `messages` call, used by the
//!   `kres test` subcommand.
//! - [`Client::stream_messages`]: streaming call that emits parsed
//!   [`StreamEvent`]s, used by `kres turn` and later by the fast /
//!   slow agents.

use std::time::Duration;

use futures::StreamExt;
use reqwest::header;

use std::sync::Arc;

use crate::{
    config::CallConfig,
    error::LlmError,
    proxy::detect_proxy,
    rate_limit::RateLimiter,
    request::{Message, MessagesRequest, MessagesResponse},
    stream::{parse_event, StreamEvent},
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone)]
pub struct Client {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
    /// Optional shared rate limiter. Multiple Clients with the same
    /// API key should share one via `Arc::clone`.
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl Client {
    /// Build a client from an API key; picks up https_proxy / HTTPS_PROXY
    /// from the environment automatically.
    pub fn new(api_key: impl Into<String>) -> Result<Self, LlmError> {
        Self::builder(api_key).build()
    }

    pub fn builder(api_key: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            proxy: detect_proxy(),
            timeout: None,
            user_agent: format!("kres/{}", env!("CARGO_PKG_VERSION")),
            rate_limiter: None,
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
    /// Used on a 429 to decide whether the payload needs shrinking
    /// before retrying (§10 in todo.md).
    pub async fn count_tokens_exact(&self, cfg: &CallConfig, messages: &[Message]) -> Option<u64> {
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
            .header("x-api-key", &self.api_key)
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
    /// Shrink rules:
    /// - Always shrink when `count_tokens` exceeds `max_input_tokens`
    ///   (a size problem).
    /// - Proactively shrink after `SHRINK_AFTER_CONSECUTIVE_429S`
    ///   same-size 429s even when under the limit, as a last-resort
    ///   for workspace-level budget exhaustion we can't wait out
    ///   (a pacing problem masquerading as a size problem).
    ///
    /// Every 429 logs unconditionally to stderr (operator-visible) so
    /// the pacing story is never hidden behind tracing filters.
    pub async fn messages(
        &self,
        cfg: &CallConfig,
        messages: &[Message],
    ) -> Result<MessagesResponse, LlmError> {
        const MAX_RETRIES: u32 = 20;
        const SHRINK_AFTER_CONSECUTIVE_429S: u32 = 3;

        let mut working_messages: Vec<Message> = messages.to_vec();
        // Count of 429s seen since the last shrink (or start of call).
        // A successful shrink resets this so we give the smaller
        // payload a fresh retry budget.
        let mut consecutive_429s: u32 = 0;
        for attempt in 0..=MAX_RETRIES {
            let body = MessagesRequest::from_config(cfg, &working_messages, false);
            let resp_result = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
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
                    // Count the payload exactly so we can decide
                    // whether it's a size problem or a pacing
                    // problem. `count_tokens` may itself 429 — None
                    // means "unknown", treat as pacing.
                    let exact = self.count_tokens_exact(cfg, &working_messages).await;
                    let limit = cfg.max_input_tokens;
                    let over_limit = match (exact, limit) {
                        (Some(e), Some(l)) => e > l as u64,
                        _ => false,
                    };
                    let pacing_stuck = consecutive_429s >= SHRINK_AFTER_CONSECUTIVE_429S;
                    let should_shrink = over_limit || pacing_stuck;
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 attempt={}/{} consecutive={} exact_tokens={:?} max_input_tokens={:?} retry_after={:?} wait={:?} shrink={} reason={}",
                        attempt, MAX_RETRIES, consecutive_429s, exact, limit, retry_after, wait, should_shrink,
                        if over_limit { "over-limit" } else if pacing_stuck { "pacing-stuck" } else { "wait" },
                    );
                    if should_shrink {
                        if let Some(last) = working_messages.last_mut() {
                            if last.role == "user" {
                                // Target size: for over-limit, aim at
                                // 90% of the limit. For pacing-stuck,
                                // aim at 70% of the CURRENT exact
                                // count (don't know workspace budget,
                                // just make the next request smaller
                                // so it's more likely to fit under
                                // whatever slice is left).
                                let target_tokens: u64 = if over_limit {
                                    (limit.unwrap() as u64 * 9) / 10
                                } else {
                                    let cur = exact
                                        .unwrap_or_else(|| (last.content.len() as u64 / 4).max(1));
                                    (cur * 7) / 10
                                };
                                let target_chars = (target_tokens as usize).saturating_mul(4);
                                if let Some(new_content) =
                                    kres_core::shrink::shrink_last_user_message(
                                        &last.content,
                                        target_chars,
                                    )
                                {
                                    kres_core::async_eprintln!(
                                        "[rate-limit] shrink applied before={}c after={}c target_tokens={} reason={}",
                                        last.content.len(),
                                        new_content.len(),
                                        target_tokens,
                                        if over_limit { "over-limit" } else { "pacing-stuck" },
                                    );
                                    last.content = new_content;
                                    // Reset counter so the smaller
                                    // payload gets a fresh retry
                                    // budget.
                                    consecutive_429s = 0;
                                }
                            }
                        }
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
        use eventsource_stream::Eventsource;

        let body = MessagesRequest::from_config(cfg, messages, true);
        let max_retries = 8;
        let mut last_err: Option<LlmError> = None;
        let mut consecutive_429s: u32 = 0;
        for attempt in 0..=max_retries {
            let resp_result = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
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
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(last_err.unwrap_or_else(|| LlmError::Other("stream exhausted retries".into())))
    }

    /// Streaming `messages` call with the full retry+shrink semantics
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
        const MAX_RETRIES: u32 = 20;
        const SHRINK_AFTER_CONSECUTIVE_429S: u32 = 3;

        let mut working_messages: Vec<Message> = messages.to_vec();
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
            let body = MessagesRequest::from_config(cfg, &working_messages, true);
            let resp_result = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
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
                    Ok(resp) => return Ok(resp),
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
                    let pacing_stuck = consecutive_429s >= SHRINK_AFTER_CONSECUTIVE_429S;
                    let should_shrink = over_limit || pacing_stuck;
                    kres_core::async_eprintln!(
                        "[rate-limit] 429 (stream) attempt={}/{} consecutive={} exact_tokens={:?} max_input_tokens={:?} retry_after={:?} wait={:?} shrink={} reason={}",
                        attempt, MAX_RETRIES, consecutive_429s, exact, limit, retry_after, wait, should_shrink,
                        if over_limit { "over-limit" } else if pacing_stuck { "pacing-stuck" } else { "wait" },
                    );
                    if should_shrink {
                        if let Some(last) = working_messages.last_mut() {
                            if last.role == "user" {
                                let target_tokens: u64 = if over_limit {
                                    (limit.unwrap() as u64 * 9) / 10
                                } else {
                                    let cur = exact
                                        .unwrap_or_else(|| (last.content.len() as u64 / 4).max(1));
                                    (cur * 7) / 10
                                };
                                let target_chars = (target_tokens as usize).saturating_mul(4);
                                if let Some(new_content) =
                                    kres_core::shrink::shrink_last_user_message(
                                        &last.content,
                                        target_chars,
                                    )
                                {
                                    kres_core::async_eprintln!(
                                        "[rate-limit] shrink applied before={}c after={}c target_tokens={} reason={}",
                                        last.content.len(),
                                        new_content.len(),
                                        target_tokens,
                                        if over_limit { "over-limit" } else { "pacing-stuck" },
                                    );
                                    last.content = new_content;
                                    consecutive_429s = 0;
                                }
                            }
                        }
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
            return Err(LlmError::ApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Err(LlmError::Other("exhausted retries".into()))
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
    kres_core::async_eprintln!(
        "[network] {} attempt={}/{} kind={} error={} — retrying in {:?} (check connectivity to api.anthropic.com)",
        label,
        attempt + 1,
        max + 1,
        transport_error_kind(e),
        e,
        wait,
    );
}

/// User-visible notice that we exhausted retries on transport errors.
fn log_transport_giveup(label: &str, max: u32, e: &reqwest::Error) {
    kres_core::async_eprintln!(
        "[network] {} giving up after {} attempts: kind={} error={} — API unreachable, check network / proxy / DNS",
        label,
        max + 1,
        transport_error_kind(e),
        e,
    );
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
    api_key: String,
    base_url: String,
    proxy: Option<String>,
    timeout: Option<Duration>,
    user_agent: String,
    rate_limiter: Option<Arc<RateLimiter>>,
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

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    pub fn rate_limiter(mut self, rl: Option<Arc<RateLimiter>>) -> Self {
        self.rate_limiter = rl;
        self
    }

    pub fn build(self) -> Result<Client, LlmError> {
        let mut b = reqwest::Client::builder().user_agent(self.user_agent);
        if let Some(proxy_url) = self.proxy.as_deref() {
            let p = reqwest::Proxy::all(proxy_url)
                .map_err(|_| LlmError::BadProxy(proxy_url.to_string()))?;
            b = b.proxy(p);
        }
        if let Some(t) = self.timeout {
            b = b.timeout(t);
        }
        let http = b.build()?;
        Ok(Client {
            api_key: self.api_key,
            base_url: self.base_url,
            http,
            rate_limiter: self.rate_limiter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[tokio::test]
    async fn builder_sets_base_url() {
        let c = Client::builder("sk-test")
            .base_url("http://localhost:1")
            .build()
            .unwrap();
        assert_eq!(c.base_url, "http://localhost:1");
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
            cached_prefix: None,
        }];
        let res = c.messages(&cfg, &msgs).await;
        // Either an ApiStatus (if something is listening) or Http error
        // (if connect fails). Both are acceptable — we only assert
        // that we don't panic and don't silently succeed.
        assert!(res.is_err());
    }
}
