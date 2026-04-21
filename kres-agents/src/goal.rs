//! Goal system: define + check the task's completion criteria.
//!
//! Ports / `_check_goal` (/:
//! 3304-3343, 3346-3392). These run on the SAME model and client as
//! the main (data-dispatch) agent, and log to `main.jsonl` — but
//! they DO NOT share the main-agent's system prompt. The main-agent
//! prompt trains the model to reply `done` when no fetch actions
//! are needed; that trained reflex was firing on check_goal calls
//! and blowing past the JSON envelope the caller expects (observed
//! in session e84c7fac: reply=`done`, parse failed,
//! `assume_met()` fired). [`GOAL_INSTRUCTIONS`] is the dedicated
//! system prompt for this agent — it tells the model it's a judge,
//! not a fetcher, and to return JSON only.
//!
//! Ownership: the session calls `define_goal` after each top-level
//! prompt (or `--prompt FILE` initial run), stores the returned
//! string, then calls `check_goal` after every reaped task. When the
//! goal is met, the session moves all remaining pending todos to the
//! deferred list.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

/// Dedicated system prompt for define_goal / check_goal. Swapped in
/// for the main-agent's fetcher system prompt so the goal judge
/// doesn't reply `done` to JSON-shaped requests.
pub const GOAL_INSTRUCTIONS: &str = include_str!("prompts/goal.txt");

/// Config needed to run a goal call. Same shape as the main-agent
/// client — reuses the main-agent's client for both.
#[derive(Clone)]
pub struct GoalClient {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub logger: Option<Arc<TurnLogger>>,
}

/// Result of a `check_goal` call.
#[derive(Debug, Clone)]
pub struct GoalCheck {
    /// Whether the goal is considered met by the analysis so far.
    pub met: bool,
    pub reason: String,
    pub missing: Vec<String>,
}

/// Result of a `define_goal` call: the completion criterion + the
/// classified work mode ("analysis" for reading code / surfacing
/// bugs, "coding" for writing code / reproducers / PoCs).
#[derive(Debug, Clone)]
pub struct GoalDefinition {
    pub goal: String,
    pub mode: TaskMode,
}

pub use kres_core::TaskMode;

#[derive(Debug, Deserialize)]
struct DefineResponse {
    #[serde(default)]
    goal: String,
    #[serde(default)]
    mode: Option<TaskMode>,
}

#[derive(Debug, Deserialize)]
struct CheckResponse {
    #[serde(default)]
    met: bool,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    missing: Vec<String>,
}

/// Ask the main agent for a completion criterion. Returns None when
/// the agent fails to produce a well-shaped response — callers
/// should treat "no goal" as "run until --turns or the todo list
/// drains" and NOT invoke `check_goal` ( behaviour).
pub async fn define_goal(gc: &GoalClient, prompt: &str) -> Option<GoalDefinition> {
    let request = json!({
        "task": "define_goal",
        "query": prompt.chars().take(2000).collect::<String>(),
        "instructions": "Define a clear, specific goal for this query \
                         AND classify the work mode. What must be true \
                         for this query to be considered complete? Be \
                         concrete: name specific things that must be \
                         found, verified, written, or answered. The \
                         `mode` field selects the pipeline: \"analysis\" \
                         (read code, surface bugs/invariants/notes) or \
                         \"coding\" (write code — reproducer, PoC, \
                         selftest, trigger program, harness). Choose \
                         \"coding\" only when the operator's REQUESTED \
                         OUTPUT is source code they will run. Default \
                         to \"analysis\" when the prompt is ambiguous. \
                         Return JSON only:\n\
                         {\"goal\": \"specific completion criteria\", \
                          \"mode\": \"analysis\" | \"coding\"}"
    });
    let body = serde_json::to_string_pretty(&request).ok()?;
    let mut cfg = CallConfig::defaults_for(gc.model.clone())
        .with_max_tokens(gc.max_tokens)
        .with_stream_label("define_goal");
    if let Some(s) = &gc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = gc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    let messages = vec![Message {
        role: "user".into(),
        content: body.clone(),
        cache: true,
        cached_prefix: None,
    }];
    if let Some(lg) = &gc.logger {
        lg.log_main("user", &body, None, None);
    }
    let resp = match gc.client.messages_streaming(&cfg, &messages).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "define_goal failed: {e}");
            return None;
        }
    };
    let text = extract_text(&resp);
    if let Some(lg) = &gc.logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }
    let parsed = extract_json_with_key::<DefineResponse>(&text, "goal")?;
    if parsed.goal.is_empty() {
        return None;
    }
    Some(GoalDefinition {
        goal: parsed.goal,
        mode: parsed.mode.unwrap_or_default(),
    })
}

/// Ask the main agent whether `goal` has been met by `analysis`.
/// Returns `(met, reason, missing)`. On any failure returns
/// `(true, "check failed, assuming met", [])` — matches 's
/// policy of not stranding a task because of a flaky check call.
///
/// `original_prompt` is the operator's raw query. Including it as a
/// separate field lets the judge weigh the literal intent ("check
/// every file one by one") against the derived `goal` string that
/// may have compressed or generalised that intent during
/// define_goal.
pub async fn check_goal(
    gc: &GoalClient,
    original_prompt: &str,
    goal: &str,
    analysis: &str,
) -> GoalCheck {
    let request = json!({
        "task": "check_goal",
        "original_prompt": original_prompt,
        "goal": goal,
        "analysis": analysis,
        "instructions": "Has the analysis satisfied BOTH the operator's \
                         original_prompt AND the derived goal? The goal is \
                         a summary the main agent produced from the prompt; \
                         treat the prompt as the ground-truth intent. If \
                         the prompt asks for a sweep (e.g. 'check every \
                         file', 'analyse each function') and the analysis \
                         only covers the first item, that is NOT met — \
                         list the remaining items in `missing`. Return \
                         JSON only:\n\
                         {\"met\": true/false, \"reason\": \"why or why not\", \
                         \"missing\": [\"what still needs to be done\"]}"
    });
    let body = match serde_json::to_string_pretty(&request) {
        Ok(s) => s,
        Err(_) => return assume_met(),
    };
    let mut cfg = CallConfig::defaults_for(gc.model.clone())
        .with_max_tokens(gc.max_tokens)
        .with_stream_label("check_goal");
    if let Some(s) = &gc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = gc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    let messages = vec![Message {
        role: "user".into(),
        content: body.clone(),
        cache: true,
        cached_prefix: None,
    }];
    if let Some(lg) = &gc.logger {
        lg.log_main("user", &body, None, None);
    }
    let resp = match gc.client.messages_streaming(&cfg, &messages).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "check_goal failed: {e}");
            return assume_met();
        }
    };
    let text = extract_text(&resp);
    if let Some(lg) = &gc.logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }
    match extract_json_with_key::<CheckResponse>(&text, "met") {
        Some(r) => GoalCheck {
            met: r.met,
            reason: r.reason,
            missing: r.missing,
        },
        None => assume_met(),
    }
}

fn assume_met() -> GoalCheck {
    GoalCheck {
        met: true,
        reason: "check failed, assuming met".into(),
        missing: Vec::new(),
    }
}

/// Find the first `{...}` block containing the requested key and
/// deserialise it into `T`. Matches (text, key)`
/// for the narrow "expect a JSON object with this field" case.
fn extract_json_with_key<T: for<'de> Deserialize<'de>>(text: &str, key: &str) -> Option<T> {
    // Try strict parse first.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if v.get(key).is_some() {
            return serde_json::from_value(v).ok();
        }
    }
    // Brace-match for the first `{...}` containing "<key>":
    let bytes = text.as_bytes();
    let marker = format!("\"{key}\"");
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let slice = &text[s..=i];
                        if slice.contains(&marker) {
                            if let Ok(t) = serde_json::from_str(slice) {
                                return Some(t);
                            }
                        }
                        start = None;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn extract_text(resp: &kres_llm::request::MessagesResponse) -> String {
    let mut out = String::new();
    for block in &resp.content {
        if let kres_llm::request::ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_strict() {
        let r: DefineResponse =
            extract_json_with_key(r#"{"goal": "find the bug"}"#, "goal").unwrap();
        assert_eq!(r.goal, "find the bug");
    }

    #[test]
    fn extract_json_embedded() {
        let r: CheckResponse = extract_json_with_key(
            r#"prefix {"met": true, "reason": "ok", "missing": []} suffix"#,
            "met",
        )
        .unwrap();
        assert!(r.met);
        assert_eq!(r.reason, "ok");
    }

    #[test]
    fn extract_json_missing_key() {
        let r: Option<DefineResponse> = extract_json_with_key(r#"{"other": "x"}"#, "goal");
        assert!(r.is_none());
    }

    #[test]
    fn assume_met_default_is_truthy() {
        let c = assume_met();
        assert!(c.met);
        assert!(c.missing.is_empty());
    }
}
