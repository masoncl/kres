//! `kres turn` — large-context one-shot.
//!
//! Replaces the Python `turn.py`. Stream responses straight to disk;
//! thinking blocks are wrapped in `<thinking>...</thinking>`; streaming
//! is required (non-streaming would time out on large prompts).
//!
//! Defaults fixed from bugs.md:
//! - R2: thinking budget default is `min(max_tokens/4, 32_000)` when the
//!   operator omits `--thinking-budget`.

use std::fs::File;
use std::io::{BufWriter, Write};

use anyhow::{bail, Context, Result};
use kres_agents::AgentConfig;
use kres_llm::{
    config::CallConfig, request::Message, stream::StreamEventKind, Model, ThinkingBudget,
};
use serde_json::Value;

use crate::TurnArgs;

pub async fn run_turn(args: TurnArgs) -> Result<()> {
    let agent_cfg = AgentConfig::load(&args.config)
        .with_context(|| format!("loading model config {}", args.config.display()))?;

    let model = match args.model.as_deref() {
        Some(id) => Model::from_id(id),
        None => agent_cfg
            .model
            .as_deref()
            .map(Model::from_id)
            .unwrap_or_else(Model::sonnet_4_6),
    };
    let max_tokens = args.max_tokens.unwrap_or(model.max_output_tokens);

    // bugs.md#R2: default thinking budget is model-aware so newer
    // Opus 4.7+ models get the adaptive budget while older models
    // keep the fixed min(max_tokens/4, 32_000).
    let thinking = match args.thinking_budget {
        None => ThinkingBudget::default_for_model(&model.id, max_tokens),
        Some(0) => ThinkingBudget::Disabled,
        Some(n) => ThinkingBudget::enabled_clamped(n, max_tokens),
    };

    let (context, system_from_input) = read_input(args.input.as_deref())?;

    let system = if let Some(s) = args.system.clone() {
        Some(s)
    } else if let Some(ref path) = args.system_file {
        Some(std::fs::read_to_string(path)?)
    } else {
        system_from_input
    };

    let mut cfg = CallConfig::defaults_for(model.clone())
        .with_max_tokens(max_tokens)
        .with_thinking(thinking);
    if let Some(s) = system {
        cfg = cfg.with_system(s);
    }
    if let Some(t) = args.temperature {
        if thinking.is_enabled() {
            eprintln!("warning: --temperature ignored because thinking is enabled");
        } else {
            cfg = cfg.with_temperature(t);
        }
    }

    eprintln!("model: {}", cfg.model.id);
    eprintln!("input size: {} chars", context.len());
    eprintln!("max_tokens: {}", cfg.max_tokens);
    if let Some(b) = cfg.thinking.as_budget_tokens() {
        eprintln!("thinking_budget: {b}");
    }
    eprintln!("sending request (streaming)...");

    let client = agent_cfg.client_builder()?.build()?;
    let messages = vec![Message {
        role: "user".into(),
        content: context,
        cache: false,
        cached_prefix: None,
    }];
    let mut stream = client.stream_messages(&cfg, &messages).await?;

    let file = File::create(&args.output)
        .with_context(|| format!("creating output {}", args.output.display()))?;
    let mut out = BufWriter::new(file);

    let mut current_block_thinking = false;
    let mut saw_stop_reason: Option<String> = None;

    while let Some(event) = stream.next().await {
        let event = event?;
        match event.kind {
            StreamEventKind::BlockStart { block_type, .. } => {
                if block_type == "thinking" {
                    current_block_thinking = true;
                    writeln!(out, "<thinking>")?;
                } else {
                    current_block_thinking = false;
                }
            }
            StreamEventKind::TextDelta { text, .. } => {
                out.write_all(text.as_bytes())?;
            }
            StreamEventKind::ThinkingDelta { text, .. } => {
                out.write_all(text.as_bytes())?;
            }
            StreamEventKind::BlockStop { .. } => {
                if current_block_thinking {
                    write!(out, "\n</thinking>\n\n")?;
                } else {
                    writeln!(out)?;
                }
                current_block_thinking = false;
            }
            StreamEventKind::MessageDelta { stop_reason, .. } => {
                if stop_reason.is_some() {
                    saw_stop_reason = stop_reason;
                }
            }
            StreamEventKind::MessageStop => {
                break;
            }
            StreamEventKind::Ping | StreamEventKind::Other => {}
            StreamEventKind::MessageStart { .. } => {}
        }
    }

    out.flush()?;
    drop(out);

    if let Some(sr) = saw_stop_reason {
        eprintln!("stop reason: {sr}");
    }
    eprintln!("response written to {}", args.output.display());
    Ok(())
}

/// Read the turn context from `-i path` or stdin. When `-i` is given
/// and the file parses as an object with a "system" field, that field
/// is lifted out as the system prompt and the rest is re-serialized as
/// the user content.
fn read_input(path: Option<&std::path::Path>) -> Result<(String, Option<String>)> {
    if let Some(p) = path {
        let raw =
            std::fs::read_to_string(p).with_context(|| format!("reading input {}", p.display()))?;
        let (ctx, sys) = split_system_from_json(&raw)?;
        return Ok((ctx, sys));
    }
    // stdin
    use std::io::Read;
    let mut s = String::new();
    if atty_ish() {
        bail!("no input: use -i <json> or pipe data via stdin");
    }
    std::io::stdin().read_to_string(&mut s)?;
    if s.trim().is_empty() {
        bail!("stdin was empty");
    }
    Ok((s, None))
}

fn atty_ish() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// When the input JSON is `{"system": "...", ...rest}`, lift `system`
/// out and re-serialize the rest as the user content. Non-object
/// input is returned verbatim.
pub fn split_system_from_json(raw: &str) -> Result<(String, Option<String>)> {
    // Accept arbitrary text too; only try JSON extraction.
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Ok((raw.to_string(), None));
    };
    let Value::Object(mut map) = value else {
        return Ok((raw.to_string(), None));
    };
    let sys = match map.remove("system") {
        Some(Value::String(s)) => Some(s),
        _ => None,
    };
    let rest = serde_json::to_string_pretty(&Value::Object(map))?;
    Ok((rest, sys))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_plain_text_keeps_input() {
        let (c, s) = split_system_from_json("just text").unwrap();
        assert_eq!(c, "just text");
        assert_eq!(s, None);
    }

    #[test]
    fn split_object_lifts_system_field() {
        let raw = r#"{"system": "you are helpful", "question": "hi"}"#;
        let (c, s) = split_system_from_json(raw).unwrap();
        assert_eq!(s.as_deref(), Some("you are helpful"));
        let parsed: Value = serde_json::from_str(&c).unwrap();
        assert!(parsed.get("system").is_none());
        assert_eq!(parsed.get("question"), Some(&Value::from("hi")));
    }

    #[test]
    fn split_object_without_system_passes_through() {
        let raw = r#"{"question": "hi"}"#;
        let (_, s) = split_system_from_json(raw).unwrap();
        assert_eq!(s, None);
    }

    #[test]
    fn split_system_non_string_is_ignored() {
        let raw = r#"{"system": 42, "question": "hi"}"#;
        let (_, s) = split_system_from_json(raw).unwrap();
        // Non-string system is dropped rather than panicking.
        assert_eq!(s, None);
    }

    #[test]
    fn split_array_passes_through() {
        let raw = "[1,2,3]";
        let (c, s) = split_system_from_json(raw).unwrap();
        assert_eq!(c, raw);
        assert_eq!(s, None);
    }
}
