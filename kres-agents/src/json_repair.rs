//! One model-assisted retry for a structured response rejected by serde.
//!
//! This module does not parse, extract, normalize, or compare JSON. The caller
//! owns one serde contract, supplies its generated JSON Schema and validation
//! error, and must deserialize the replacement through that same contract.

use std::sync::Arc;
use std::{collections::HashSet, fmt};

use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::shutdown::Shutdown;
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

use crate::error::AgentError;

const REPAIR_SYSTEM: &str = "Re-emit the rejected response as exactly one JSON value matching the supplied schema. Return raw, unfenced JSON only, with no prose or Markdown backticks. Do not invent evidence or omit requested records.";

#[derive(Debug, Clone, Copy)]
pub enum RepairLogKind {
    Code,
    Main,
}

#[derive(Debug, Clone, Copy)]
pub struct JsonContract<'a> {
    pub name: &'a str,
    pub schema: &'a str,
    pub instructions: &'a str,
}

#[derive(Debug)]
pub struct JsonRepairResult {
    pub text: String,
    pub usage: kres_llm::request::Usage,
}

pub struct JsonRepairCall<'a> {
    pub client: Arc<Client>,
    pub model: Model,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub contract: JsonContract<'a>,
    pub rejected_response: &'a str,
    pub validation_errors: &'a [String],
    pub logger: Option<Arc<TurnLogger>>,
    pub log_kind: RepairLogKind,
    pub shutdown: Option<Shutdown>,
}

/// Strict whole-response serde contract used by goal, todo, and other control
/// agents. No brace scanning or transport-wrapper unwrapping is performed.
#[derive(Debug, Clone, Copy)]
pub struct JsonObjectContract<'a> {
    pub name: &'a str,
    pub fields: &'a [&'a str],
}

impl JsonObjectContract<'_> {
    pub fn parse<T: DeserializeOwned>(&self, text: &str) -> Result<T, Vec<String>> {
        // Deserialize the typed contract before materialising a Value.  Value's
        // map representation collapses duplicate keys, which would otherwise
        // turn `{ "met": false, "met": true }` into a valid last-wins
        // decision before serde gets a chance to reject it.
        let parsed: T = parse_strict_json(self.name, text)?;
        let value: serde_json::Value = parse_strict_json(self.name, text)?;
        let object = value
            .as_object()
            .ok_or_else(|| vec![format!("{} response must be one JSON object", self.name)])?;
        if !self.fields.iter().any(|field| object.contains_key(*field)) {
            return Err(vec![format!(
                "{} response must contain one of: {}",
                self.name,
                self.fields.join(", ")
            )]);
        }
        Ok(parsed)
    }

    pub fn accept_repair<T: DeserializeOwned>(&self, replacement: &str) -> Result<T, Vec<String>> {
        self.parse(replacement)
    }
}

pub fn parse_strict_json<T: DeserializeOwned>(
    contract_name: &str,
    text: &str,
) -> Result<T, Vec<String>> {
    match parse_strict_json_inner(contract_name, text) {
        Ok(value) => Ok(value),
        Err(original_errors) => match strip_whole_json_fence(text) {
            Some(unfenced) => parse_strict_json_inner(contract_name, unfenced),
            None => Err(original_errors),
        },
    }
}

fn parse_strict_json_inner<T: DeserializeOwned>(
    contract_name: &str,
    text: &str,
) -> Result<T, Vec<String>> {
    reject_duplicate_keys(contract_name, text)?;
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        vec![format!(
            "{} response is invalid at {}: {}",
            contract_name,
            error.path(),
            error.inner()
        )]
    })?;
    deserializer.end().map_err(|error| {
        vec![format!(
            "{contract_name} response contains trailing content: {error}"
        )]
    })?;
    Ok(value)
}

fn reject_duplicate_keys(contract_name: &str, text: &str) -> Result<(), Vec<String>> {
    struct CheckedJson;

    impl<'de> serde::Deserialize<'de> for CheckedJson {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(CheckedVisitor)
        }
    }

    struct CheckedVisitor;

    impl<'de> Visitor<'de> for CheckedVisitor {
        type Value = CheckedJson;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a JSON value without duplicate object keys")
        }

        fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(CheckedJson)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            CheckedJson::deserialize(deserializer)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element::<CheckedJson>()?.is_some() {}
            Ok(CheckedJson)
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut keys = HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key.clone()) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate object key `{key}`"
                    )));
                }
                map.next_value::<CheckedJson>()?;
            }
            Ok(CheckedJson)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(text);
    CheckedJson::deserialize(&mut deserializer)
        .map_err(|error| vec![format!("{contract_name} response is invalid: {error}")])?;
    deserializer.end().map_err(|error| {
        vec![format!(
            "{contract_name} response contains trailing content: {error}"
        )]
    })
}

/// Remove one Markdown fence only when it frames the entire trimmed response.
/// The returned body still goes through the original strict serde/schema
/// contract; this is transport normalization, not JSON recovery.
pub fn strip_whole_json_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let first_break = trimmed.find('\n')?;
    let opening = trimmed[..first_break].trim_end_matches('\r');
    if opening != "```" && !opening.eq_ignore_ascii_case("```json") {
        return None;
    }
    let last_break = trimmed.rfind('\n')?;
    if last_break <= first_break || trimmed[last_break + 1..].trim() != "```" {
        return None;
    }
    Some(&trimmed[first_break + 1..last_break])
}

#[derive(Serialize)]
struct RepairRequest<'a> {
    task: &'static str,
    contract: &'a str,
    schema: &'a str,
    validation_errors: &'a [String],
    rejected_response: &'a str,
    instructions: &'a str,
}

pub async fn repair_json_response(
    call: JsonRepairCall<'_>,
) -> Result<JsonRepairResult, AgentError> {
    let request = RepairRequest {
        task: "repair_json_response",
        contract: call.contract.name,
        schema: call.contract.schema,
        validation_errors: call.validation_errors,
        rejected_response: call.rejected_response,
        instructions: call.contract.instructions,
    };
    let body = serde_json::to_string(&request)?;
    let label = format!("json-repair contract={}", call.contract.name);
    log_turn(&call.logger, call.log_kind, "user", &label, &body, None);

    let mut cfg = CallConfig::defaults_for(call.model)
        .with_max_tokens(call.max_tokens.min(16_000))
        .with_system(REPAIR_SYSTEM.to_string())
        .with_stream_label(label.clone());
    if let Some(limit) = call.max_input_tokens {
        cfg = cfg.with_max_input_tokens(limit);
    }
    let messages = vec![Message {
        role: "user".into(),
        content: body,
        cache: false,
        cached_prefix: None,
    }];
    let response = if let Some(shutdown) = call.shutdown {
        tokio::select! {
            _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during JSON repair".into())),
            result = call.client.messages_streaming(&cfg, &messages) => result,
        }
    } else {
        call.client.messages_streaming(&cfg, &messages).await
    }
    .map_err(|error| AgentError::Other(error.to_string()))?;
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            kres_llm::request::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let usage = LoggedUsage {
        input: response.usage.input_tokens,
        output: response.usage.output_tokens,
        cache_creation: response.usage.cache_creation_input_tokens,
        cache_read: response.usage.cache_read_input_tokens,
    };
    log_turn(
        &call.logger,
        call.log_kind,
        "assistant",
        &label,
        &text,
        Some(usage),
    );
    Ok(JsonRepairResult {
        text,
        usage: response.usage,
    })
}

fn log_turn(
    logger: &Option<Arc<TurnLogger>>,
    kind: RepairLogKind,
    role: &str,
    label: &str,
    text: &str,
    usage: Option<LoggedUsage>,
) {
    let Some(logger) = logger else { return };
    match kind {
        RepairLogKind::Code => logger.log_code_labeled(role, Some(label), text, usage, None),
        RepairLogKind::Main => logger.log_main(role, text, usage, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Decision {
        met: bool,
    }

    #[test]
    fn contract_accepts_only_one_strict_json_value() {
        let contract = JsonObjectContract {
            name: "decision",
            fields: &["met"],
        };
        assert_eq!(
            contract.parse::<Decision>(r#"{"met":true}"#).unwrap(),
            Decision { met: true }
        );
        assert_eq!(
            contract
                .parse::<Decision>("```json\n{\"met\":true}\n```")
                .unwrap(),
            Decision { met: true }
        );
        assert!(contract.parse::<Decision>(r#"prose {"met":true}"#).is_err());
        assert!(contract
            .parse::<Decision>(r#"{"met":true} trailing"#)
            .is_err());
        assert!(contract
            .parse::<Decision>(r#"{"met":true} {"met":false}"#)
            .is_err());
        assert!(contract
            .parse::<Decision>("preamble\n```json\n{\"met\":true}\n```")
            .is_err());
        assert!(contract
            .parse::<Decision>(r#"{"met":true,"extra":1}"#)
            .is_err());
        assert!(contract
            .parse::<Decision>(r#"{"met":false,"met":true}"#)
            .is_err());
    }

    #[test]
    fn model_json_consumers_do_not_reintroduce_recovery_scanners() {
        let consumers = [
            include_str!("response.rs"),
            include_str!("goal.rs"),
            include_str!("todo_agent.rs"),
            include_str!("consolidate.rs"),
            include_str!("pipeline.rs"),
            include_str!("workflow_runner.rs"),
            include_str!("main_agent.rs"),
            include_str!("promote.rs"),
            include_str!("finding_repair.rs"),
            include_str!("../../kres-repl/src/session.rs"),
        ];
        for source in consumers {
            for forbidden in [
                "extract_brace_objects",
                "select_last_json_candidate",
                "json_candidates(",
                "rfind('}')",
                "rfind(\"}\")",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "model JSON consumer contains forbidden recovery helper `{forbidden}`"
                );
            }
        }
        assert!(!include_str!("response.rs").contains("pub fn diagnose_code_response"));
    }
}
