//! Agent response parsing.
//!
//! The fast and slow agents return a JSON object with shape:
//! `{"analysis": "...", "followups": [...], "skill_reads": [...],
//!   "findings": [...], "ready_for_slow": bool}`.
//!
//! Acceptance prefers a complete JSON object but also recognizes exactly one
//! embedded object as deterministic transport normalization. Serde still
//! validates nested DTOs and unknown fields; callers may ask the model to
//! re-emit a rejected response, and the replacement passes through this
//! identical boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::followup::Followup;

use kres_core::findings::Finding;

#[derive(schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeResponseSchema {
    pub analysis: Option<String>,
    pub followups: Option<Vec<Followup>>,
    pub skill_reads: Option<Vec<String>>,
    pub findings: Option<Vec<Finding>>,
    pub ready_for_slow: Option<bool>,
    pub code_output: Option<Vec<kres_core::CodeFile>>,
    pub code_edits: Option<Vec<kres_core::CodeEdit>>,
    pub plan: Option<kres_core::PlanRewrite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvalidFinding {
    pub index: usize,
    pub raw: Value,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct CodeResponse {
    pub analysis: String,
    pub followups: Vec<Followup>,
    pub skill_reads: Vec<String>,
    pub findings: Vec<Finding>,
    /// Original array positions for valid findings, used to replace repaired
    /// malformed siblings without reordering the model's response.
    pub(crate) finding_positions: Vec<usize>,
    /// Raw finding entries rejected by serde, retained so callers can
    /// request one schema-only repair instead of silently losing them.
    pub invalid_findings: Vec<InvalidFinding>,
    pub ready_for_slow: bool,
    /// Source files emitted by a Coding-mode slow-agent turn. Empty
    /// for Audit-mode responses. The coding-mode system prompt
    /// instructs the slow agent to return
    /// `{"analysis": "...", "code_output": [{path, content, purpose}], "followups": [...]}`
    /// and this field is populated from that `code_output` array.
    pub code_output: Vec<kres_core::CodeFile>,
    /// Surgical string-replacement edits to existing files, the
    /// coding-mode equivalent of code_output but for FIXES rather
    /// than new artifacts. Shape mirrors Claude Code's Edit
    /// primitive: `{file_path, old_string, new_string, replace_all}`.
    /// The reaper applies each entry via `tools::edit_file`.
    pub code_edits: Vec<kres_core::CodeEdit>,
    /// Optional rewritten plan. The slow agent is permitted to
    /// emit a top-level `plan` field when (a) it's the first slow
    /// call for the operator's top-level prompt and (b) the code
    /// it just inspected shows the existing plan is materially
    /// wrong. The wire shape is `{steps: [...]}` (only the steps
    /// are mutable — prompt/goal/mode/created_at inherit from the
    /// existing plan via `PlanRewrite::apply_to` at the apply
    /// site); parsing just the steps means a forgotten metadata
    /// field in the LLM reply does NOT silently drop the rewrite.
    /// `None` means "keep the existing plan", which is the common
    /// case.
    pub plan: Option<kres_core::PlanRewrite>,
    /// Which parse strategy won — used for diagnostics.
    pub strategy: ParseStrategy,
    /// Exact non-whitespace prefix/suffix discarded around one uniquely
    /// embedded JSON object. Callers persist this under a searchable JSONL
    /// normalization label; it is never folded into `analysis`.
    pub discarded_surrounding: Option<String>,
    /// Number of illegal literal control characters escaped inside JSON
    /// strings before strict deserialization.
    pub escaped_control_characters: usize,
    /// Structural problems that the forgiving parser would
    /// otherwise hide (for example a non-array `followups` field or
    /// an invalid item inside that array). Gather callers reject and
    /// retry these before dispatching tools.
    pub validation_errors: Vec<String>,
    /// Top-level fields outside the shared response envelope. Callers with
    /// workflow-specific extensions may consume these explicitly; strict
    /// contracts reject them instead of silently dropping misspellings.
    pub unknown_fields: BTreeMap<String, Value>,
}

/// The complete acceptance boundary for the shared code-agent envelope.
///
/// Parsing remains deliberately tolerant so malformed model output can be
/// diagnosed and repaired, but consumers must use this contract before they
/// act on any parsed field.  Workflow-specific top-level fields are allowed
/// only when the workflow declared them explicitly.
#[derive(Debug, Clone, Default)]
pub struct CodeResponseContract {
    allowed_extensions: BTreeSet<String>,
    required_fields: BTreeSet<String>,
    allow_invalid_findings: bool,
}

impl CodeResponseContract {
    fn fields(&self) -> Vec<&str> {
        const SHARED_FIELDS: &[&str] = &[
            "analysis",
            "followups",
            "skill_reads",
            "findings",
            "ready_for_slow",
            "code_output",
            "code_edits",
            "plan",
        ];
        let mut fields = SHARED_FIELDS.to_vec();
        fields.extend(self.allowed_extensions.iter().map(String::as_str));
        fields
    }

    pub fn new(allowed_extensions: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed_extensions: allowed_extensions.into_iter().collect(),
            required_fields: BTreeSet::new(),
            allow_invalid_findings: false,
        }
    }

    pub fn requiring(mut self, fields: impl IntoIterator<Item = &'static str>) -> Self {
        self.required_fields = fields.into_iter().map(str::to_owned).collect();
        self
    }

    pub fn schema_json(&self) -> Value {
        let mut schema = serde_json::to_value(schemars::schema_for!(CodeResponseSchema))
            .expect("generated response schema is serializable");
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            for extension in &self.allowed_extensions {
                properties
                    .entry(extension.clone())
                    .or_insert_with(|| serde_json::json!(true));
            }
        }
        if let Some(object) = schema.as_object_mut() {
            object.insert("minProperties".into(), Value::Number(1.into()));
            object.insert(
                "anyOf".into(),
                Value::Array(
                    self.fields()
                        .into_iter()
                        .map(|field| {
                            let mut properties = serde_json::Map::new();
                            properties.insert(
                                field.to_string(),
                                serde_json::json!({"not": {"type": "null"}}),
                            );
                            serde_json::json!({
                                "required": [field],
                                "properties": properties
                            })
                        })
                        .collect(),
                ),
            );
            if !self.required_fields.is_empty() {
                object.insert(
                    "required".into(),
                    Value::Array(
                        self.required_fields
                            .iter()
                            .cloned()
                            .map(Value::String)
                            .collect(),
                    ),
                );
                object.insert(
                    "allOf".into(),
                    Value::Array(
                        self.required_fields
                            .iter()
                            .map(|field| {
                                serde_json::json!({
                                    "properties": {(field): {"not": {"type": "null"}}}
                                })
                            })
                            .collect(),
                    ),
                );
            }
        }
        schema
    }

    /// Project the generated envelope schema to the fields a particular
    /// inference stage may emit. The accepted response still goes through
    /// this contract; this only keeps unrelated code/plan schemas out of the
    /// repair prompt.
    pub fn schema_json_for(&self, fields: &[&str]) -> Value {
        let selected = fields.iter().copied().collect::<BTreeSet<_>>();
        let mut schema = self.schema_json();
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.retain(|name, _| selected.contains(name.as_str()));
        }
        if let Some(object) = schema.as_object_mut() {
            object.insert(
                "anyOf".into(),
                Value::Array(
                    fields
                        .iter()
                        .map(|field| {
                            let field = *field;
                            serde_json::json!({
                                "required": [field],
                                "properties": {(field): {"not": {"type": "null"}}}
                            })
                        })
                        .collect(),
                ),
            );
            if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|field| field.as_str().is_some_and(|f| selected.contains(f)));
            }
            if let Some(all_of) = object.get_mut("allOf").and_then(Value::as_array_mut) {
                all_of.retain(|clause| {
                    clause
                        .get("properties")
                        .and_then(Value::as_object)
                        .is_some_and(|properties| {
                            properties
                                .keys()
                                .any(|name| selected.contains(name.as_str()))
                        })
                });
            }
        }
        prune_unused_schema_defs(&mut schema);
        schema
    }

    pub fn allowing_invalid_findings(mut self) -> Self {
        self.allow_invalid_findings = true;
        self
    }

    pub fn validate(&self, text: &str) -> Result<CodeResponse, Vec<String>> {
        self.validate_with(text, |_| Vec::new())
    }

    pub fn validate_with(
        &self,
        text: &str,
        semantic: impl FnOnce(&CodeResponse) -> Vec<String>,
    ) -> Result<CodeResponse, Vec<String>> {
        // Parse the typed envelope first so serde observes duplicate known
        // fields. Parsing into Value first would silently collapse them.
        let normalized = normalize_code_response_json(text)?;
        let raw: RawResponse =
            crate::json_repair::parse_strict_json("code-agent", &normalized.json)?;
        let root: Value = crate::json_repair::parse_strict_json("code-agent", &normalized.json)?;
        let object = root
            .as_object()
            .ok_or_else(|| vec!["response must be one JSON object".to_string()])?;
        if object.is_empty() || object.values().all(Value::is_null) {
            return Err(vec![
                "response object must contain at least one non-null field".to_string(),
            ]);
        }
        let missing_required = self
            .required_fields
            .iter()
            .filter(|field| object.get(*field).map_or(true, Value::is_null))
            .map(|field| format!("missing or null required top-level field `{field}`"))
            .collect::<Vec<_>>();
        if !missing_required.is_empty() {
            return Err(missing_required);
        }
        let allowed: BTreeSet<&str> = self.fields().into_iter().collect();
        let unknown = object
            .keys()
            .filter(|field| !allowed.contains(field.as_str()))
            .map(|field| format!("unknown top-level field `{field}`"))
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(unknown);
        }
        let mut response = finalize_raw_response(raw, text, normalized.strategy);
        response.discarded_surrounding = normalized.discarded_surrounding;
        response.escaped_control_characters = normalized.escaped_control_characters;
        let mut errors = response.validation_errors.clone();
        if !self.allow_invalid_findings {
            errors.extend(response.invalid_findings.iter().map(|finding| {
                format!("findings[{}] is invalid: {}", finding.index, finding.error)
            }));
        }
        errors.extend(
            response
                .unknown_fields
                .keys()
                .filter(|field| !self.allowed_extensions.contains(*field))
                .map(|field| format!("unknown top-level field `{field}`")),
        );
        errors.extend(semantic(&response));
        errors.sort();
        errors.dedup();
        if errors.is_empty() {
            Ok(response)
        } else {
            Err(errors)
        }
    }

    pub fn accept_repair(&self, repaired: &str) -> Result<CodeResponse, Vec<String>> {
        self.accept_repair_with(repaired, |_| Vec::new())
    }

    pub fn accept_repair_with(
        &self,
        repaired: &str,
        semantic: impl FnOnce(&CodeResponse) -> Vec<String>,
    ) -> Result<CodeResponse, Vec<String>> {
        self.validate_with(repaired, semantic)
    }
}

fn prune_unused_schema_defs(schema: &mut Value) {
    let Some(all_defs) = schema.get("$defs").and_then(Value::as_object).cloned() else {
        return;
    };
    let mut needed = BTreeSet::new();
    collect_schema_refs(schema.get("properties"), &mut needed);
    collect_schema_refs(schema.get("anyOf"), &mut needed);
    collect_schema_refs(schema.get("allOf"), &mut needed);
    let mut pending = needed.iter().cloned().collect::<Vec<_>>();
    while let Some(name) = pending.pop() {
        let Some(definition) = all_defs.get(&name) else {
            continue;
        };
        let mut nested = BTreeSet::new();
        collect_schema_refs(Some(definition), &mut nested);
        for dependency in nested {
            if needed.insert(dependency.clone()) {
                pending.push(dependency);
            }
        }
    }
    if let Some(defs) = schema.get_mut("$defs").and_then(Value::as_object_mut) {
        defs.retain(|name, _| needed.contains(name));
    }
}

fn collect_schema_refs(value: Option<&Value>, refs: &mut BTreeSet<String>) {
    let Some(value) = value else { return };
    match value {
        Value::Object(object) => {
            if let Some(name) = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                refs.insert(name.to_string());
            }
            for child in object.values() {
                collect_schema_refs(Some(child), refs);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_schema_refs(Some(child), refs);
            }
        }
        _ => {}
    }
}

impl CodeResponse {
    pub(crate) fn merge_repaired_findings(
        &mut self,
        repaired: Vec<crate::finding_repair::RepairedFinding>,
    ) {
        let mut positioned = self
            .finding_positions
            .drain(..)
            .zip(self.findings.drain(..))
            .collect::<Vec<_>>();
        positioned.extend(repaired.into_iter().map(|item| (item.index, item.finding)));
        positioned.sort_by_key(|(index, _)| *index);
        for (index, finding) in positioned {
            self.finding_positions.push(index);
            self.findings.push(finding);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseStrategy {
    #[default]
    WholeBody,
    FencedBlock,
    BraceMatch,
    /// The outer object encoded the actual response object as a JSON string
    /// in its `analysis` field.
    NestedJson,
    /// Body had no JSON at all — the caller's analysis field is just
    /// the raw text.
    RawText,
}

#[derive(Debug, Deserialize, Default)]
struct RawResponse {
    #[serde(default)]
    analysis: Value,
    #[serde(default)]
    followups: Value,
    #[serde(default)]
    skill_reads: Value,
    #[serde(default)]
    findings: Value,
    #[serde(default)]
    ready_for_slow: Value,
    #[serde(default)]
    code_output: Value,
    #[serde(default)]
    code_edits: Value,
    #[serde(default)]
    plan: Value,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// Preserve a rejected response for logs, repair prompts, and diagnostics.
/// This is not an acceptance API; use [`CodeResponseContract::validate`]
/// before consuming any field.
pub(crate) fn diagnose_code_response(text: &str) -> CodeResponse {
    parse_code_response_with_extensions(text, &BTreeSet::new())
}

fn parse_code_response_with_extensions(
    text: &str,
    _allowed_extensions: &BTreeSet<String>,
) -> CodeResponse {
    if let Ok(normalized) = normalize_code_response_json(text) {
        if let Ok(response) = serde_json::from_str::<RawResponse>(&normalized.json) {
            let mut parsed = into_code_response(response, text, normalized.strategy);
            parsed.discarded_surrounding = normalized.discarded_surrounding;
            parsed.escaped_control_characters = normalized.escaped_control_characters;
            return parsed;
        }
    }
    raw_text_response(text, "response must be exactly one JSON object")
}

pub fn log_json_normalization(
    logger: Option<&kres_core::log::TurnLogger>,
    response: &CodeResponse,
    context: &str,
) {
    if response.discarded_surrounding.is_none() && response.escaped_control_characters == 0 {
        return;
    }
    let Some(logger) = logger else { return };
    let mut content = format!(
        "context: {context}\nescaped_control_characters: {}",
        response.escaped_control_characters
    );
    if let Some(discarded) = &response.discarded_surrounding {
        content.push('\n');
        content.push_str(discarded);
    }
    let label = if response.discarded_surrounding.is_some() {
        "json-normalization discarded-surrounding-prose"
    } else {
        "json-normalization escaped-control-characters"
    };
    logger.log_code_labeled("normalization", Some(label), &content, None, None);
}

fn raw_text_response(text: &str, error: &str) -> CodeResponse {
    CodeResponse {
        analysis: text.trim().to_string(),
        followups: vec![],
        skill_reads: vec![],
        findings: vec![],
        finding_positions: vec![],
        invalid_findings: vec![],
        ready_for_slow: false,
        code_output: vec![],
        code_edits: vec![],
        plan: None,
        strategy: ParseStrategy::RawText,
        discarded_surrounding: None,
        escaped_control_characters: 0,
        validation_errors: vec![error.to_string()],
        unknown_fields: BTreeMap::new(),
    }
}

fn finalize_raw_response(
    response: RawResponse,
    original: &str,
    strategy: ParseStrategy,
) -> CodeResponse {
    into_code_response(response, original, strategy)
}

fn into_code_response(r: RawResponse, _original: &str, strategy: ParseStrategy) -> CodeResponse {
    let (followups, mut validation_errors) = value_to_followups(r.followups);
    let (analysis, analysis_error) = value_to_analysis(r.analysis);
    if let Some(error) = analysis_error {
        validation_errors.push(error);
    }
    let (findings, finding_positions, invalid_findings, findings_error) =
        value_to_findings(r.findings);
    if let Some(error) = findings_error {
        validation_errors.push(error);
    }
    let (skill_reads, skill_read_errors) = value_to_string_list(r.skill_reads, "skill_reads");
    validation_errors.extend(skill_read_errors);
    let (ready_for_slow, ready_error) = value_to_bool(r.ready_for_slow, "ready_for_slow");
    if let Some(error) = ready_error {
        validation_errors.push(error);
    }
    let (code_output, code_output_errors) = value_to_code_output(r.code_output);
    validation_errors.extend(code_output_errors);
    let (code_edits, code_edit_errors) = value_to_code_edits(r.code_edits);
    validation_errors.extend(code_edit_errors);
    let (plan, plan_error) = value_to_plan(r.plan);
    if let Some(error) = plan_error {
        validation_errors.push(error);
    }
    CodeResponse {
        analysis,
        followups,
        skill_reads,
        findings,
        finding_positions,
        invalid_findings,
        ready_for_slow,
        code_output,
        code_edits,
        plan,
        strategy,
        discarded_surrounding: None,
        escaped_control_characters: 0,
        validation_errors,
        unknown_fields: r.extra,
    }
}

struct NormalizedCodeJson {
    json: String,
    strategy: ParseStrategy,
    discarded_surrounding: Option<String>,
    escaped_control_characters: usize,
}

fn normalize_code_response_json(text: &str) -> Result<NormalizedCodeJson, Vec<String>> {
    if crate::json_repair::parse_strict_json::<Value>("code-agent", text).is_ok() {
        let strategy = if crate::json_repair::strip_whole_json_fence(text).is_some() {
            ParseStrategy::FencedBlock
        } else {
            ParseStrategy::WholeBody
        };
        let json = crate::json_repair::strip_whole_json_fence(text)
            .unwrap_or(text)
            .to_string();
        return Ok(NormalizedCodeJson {
            json,
            strategy,
            discarded_surrounding: None,
            escaped_control_characters: 0,
        });
    }

    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let (escaped, count) = escape_json_string_controls(trimmed);
        if count > 0
            && crate::json_repair::parse_strict_json::<Value>("code-agent", &escaped).is_ok()
        {
            return Ok(NormalizedCodeJson {
                json: escaped,
                strategy: ParseStrategy::WholeBody,
                discarded_surrounding: None,
                escaped_control_characters: count,
            });
        }
    }

    let candidates = outermost_json_object_candidates(text);
    if candidates.len() != 1 {
        return Err(vec![format!(
            "code-agent response must contain exactly one JSON object; found {} valid embedded candidates",
            candidates.len()
        )]);
    }
    let candidate = &candidates[0];
    let prefix = &text[..candidate.start];
    let suffix = &text[candidate.end..];
    let discarded = format!(
        "JSON_NORMALIZATION_DISCARDED_SURROUNDING\n--- prefix ---\n{}\n--- suffix ---\n{}",
        prefix, suffix
    );
    Ok(NormalizedCodeJson {
        json: candidate.json.clone(),
        strategy: ParseStrategy::BraceMatch,
        discarded_surrounding: Some(discarded),
        escaped_control_characters: candidate.escaped_control_characters,
    })
}

pub(crate) fn normalized_code_response_json(text: &str) -> Result<String, Vec<String>> {
    normalize_code_response_json(text).map(|normalized| normalized.json)
}

struct JsonObjectCandidate {
    start: usize,
    end: usize,
    json: String,
    escaped_control_characters: usize,
}

fn outermost_json_object_candidates(text: &str) -> Vec<JsonObjectCandidate> {
    let mut candidates = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text.char_indices() {
        if start.is_none() {
            if ch == '{' {
                start = Some(offset);
                depth = 1;
                in_string = false;
                escaped = false;
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let candidate_start = start.take().expect("candidate start exists");
                    let end = offset + ch.len_utf8();
                    let raw = &text[candidate_start..end];
                    let (json, escaped_control_characters) = escape_json_string_controls(raw);
                    if crate::json_repair::parse_strict_json::<Value>("code-agent", &json)
                        .ok()
                        .is_some_and(|value| value.is_object())
                    {
                        candidates.push(JsonObjectCandidate {
                            start: candidate_start,
                            end,
                            json,
                            escaped_control_characters,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    candidates
}

fn escape_json_string_controls(text: &str) -> (String, usize) {
    let mut output = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut count = 0usize;
    for ch in text.chars() {
        if in_string {
            if escaped {
                output.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => {
                    output.push(ch);
                    escaped = true;
                }
                '"' => {
                    output.push(ch);
                    in_string = false;
                }
                '\n' => {
                    output.push_str("\\n");
                    count += 1;
                }
                '\r' => {
                    output.push_str("\\r");
                    count += 1;
                }
                '\t' => {
                    output.push_str("\\t");
                    count += 1;
                }
                control if control <= '\u{001f}' => {
                    use std::fmt::Write;
                    let _ = write!(output, "\\u{:04x}", control as u32);
                    count += 1;
                }
                _ => output.push(ch),
            }
        } else {
            output.push(ch);
            if ch == '"' {
                in_string = true;
            }
        }
    }
    (output, count)
}

fn value_to_plan(v: Value) -> (Option<kres_core::PlanRewrite>, Option<String>) {
    match v {
        Value::Null => (None, None),
        other => {
            // Only the `steps` field is consumed; any other fields
            // the LLM stuffs in (prompt, goal, mode, created_at)
            // are ignored. An empty-steps rewrite is indistinguish-
            // able from "no rewrite", so drop it.
            let rewrite: kres_core::PlanRewrite = match serde_json::from_value(other) {
                Ok(rewrite) => rewrite,
                Err(error) => return (None, Some(format!("`plan` is invalid: {error}"))),
            };
            if rewrite.steps.is_empty() {
                (None, None)
            } else {
                (Some(rewrite), None)
            }
        }
    }
}

fn value_to_code_edits(v: Value) -> (Vec<kres_core::CodeEdit>, Vec<String>) {
    if v.is_null() {
        return (vec![], vec![]);
    }
    let Value::Array(items) = v else {
        return (vec![], vec!["`code_edits` must be an array".into()]);
    };
    parse_array_items(items, "code_edits", |item| {
        let edit: kres_core::CodeEdit = serde_json::from_value(item).map_err(|e| e.to_string())?;
        if edit.file_path.is_empty() {
            return Err("file_path must be non-empty".into());
        }
        Ok(edit)
    })
}

fn value_to_code_output(v: Value) -> (Vec<kres_core::CodeFile>, Vec<String>) {
    if v.is_null() {
        return (vec![], vec![]);
    }
    let Value::Array(items) = v else {
        return (vec![], vec!["`code_output` must be an array".into()]);
    };
    parse_array_items(items, "code_output", |item| {
        let file: kres_core::CodeFile = serde_json::from_value(item).map_err(|e| e.to_string())?;
        if file.path.is_empty() || file.content.is_empty() {
            return Err("path and content must be non-empty".into());
        }
        Ok(file)
    })
}

fn value_to_analysis(v: Value) -> (String, Option<String>) {
    match v {
        Value::String(s) => (s, None),
        Value::Null => (String::new(), None),
        other => (
            other.to_string(),
            Some("`analysis` must be a string".to_string()),
        ),
    }
}

fn value_to_string_list(v: Value, field: &str) -> (Vec<String>, Vec<String>) {
    if v.is_null() {
        return (vec![], vec![]);
    }
    let Value::Array(items) = v else {
        return (vec![], vec![format!("`{field}` must be an array")]);
    };
    parse_array_items(items, field, |item| match item {
        Value::String(value) => Ok(value),
        _ => Err("must be a string".into()),
    })
}

fn value_to_bool(v: Value, field: &str) -> (bool, Option<String>) {
    match v {
        Value::Null => (false, None),
        Value::Bool(value) => (value, None),
        _ => (false, Some(format!("`{field}` must be a boolean"))),
    }
}

fn parse_array_items<T, F>(items: Vec<Value>, field: &str, mut parse: F) -> (Vec<T>, Vec<String>)
where
    F: FnMut(Value) -> Result<T, String>,
{
    let mut parsed = Vec::with_capacity(items.len());
    let mut errors = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        match parse(item) {
            Ok(value) => parsed.push(value),
            Err(error) => errors.push(format!("{field}[{index}] is invalid: {error}")),
        }
    }
    (parsed, errors)
}

fn value_to_followups(v: Value) -> (Vec<Followup>, Vec<String>) {
    if v.is_null() {
        return (vec![], vec![]);
    }
    let Value::Array(items) = v else {
        return (vec![], vec!["`followups` must be an array".to_string()]);
    };
    let mut parsed = Vec::with_capacity(items.len());
    let mut errors = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        match serde_json::from_value(item) {
            Ok(followup) => parsed.push(followup),
            Err(error) => errors.push(format!("followups[{index}] is invalid: {error}")),
        }
    }
    (parsed, errors)
}

fn value_to_findings(
    v: Value,
) -> (
    Vec<Finding>,
    Vec<usize>,
    Vec<InvalidFinding>,
    Option<String>,
) {
    if v.is_null() {
        return (vec![], vec![], vec![], None);
    }
    let Value::Array(items) = v else {
        return (
            vec![],
            vec![],
            vec![],
            Some("`findings` must be an array".to_string()),
        );
    };
    let mut findings = Vec::new();
    let mut positions = Vec::new();
    let mut invalid = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        match serde_json::from_value::<Finding>(item.clone()) {
            Ok(finding) => {
                findings.push(finding.redacted_for_agent());
                positions.push(index);
            }
            Err(error) => invalid.push(InvalidFinding {
                index,
                raw: item,
                error: error.to_string(),
            }),
        }
    }
    (findings, positions, invalid, None)
}

#[cfg(test)]
mod strict_contract_tests {
    use super::*;

    #[test]
    fn accepts_exact_object_and_rejects_wrappers_or_prose() {
        let contract = CodeResponseContract::default();
        assert!(contract
            .validate(r#"{"analysis":"ok","followups":[]}"#)
            .is_ok());
        let embedded = contract
            .validate(
                r#"## Analysis
Here is the result:
{"analysis":"ok"}
Thanks."#,
            )
            .unwrap();
        assert_eq!(embedded.strategy, ParseStrategy::BraceMatch);
        assert!(embedded
            .discarded_surrounding
            .as_deref()
            .is_some_and(
                |discarded| discarded.contains("## Analysis") && discarded.contains("Thanks.")
            ));
        assert!(contract.validate(r#"{"analysis":"ok"} trailing"#).is_ok());
        assert!(contract
            .validate(r#"{"analysis":"ok"} {"analysis":"other"}"#)
            .is_err());
        assert!(contract
            .validate("```json\n{\"analysis\":\"ok\",\"followups\":[]}\n```")
            .is_ok());
        assert!(contract
            .validate("prose\n```json\n{\"analysis\":\"ok\"}\n```")
            .is_ok());
        assert!(contract
            .validate(r#"{"result":{"analysis":"ok"}}"#)
            .is_err());
    }

    #[test]
    fn embedded_normalization_rejects_multiple_json_objects() {
        let error = CodeResponseContract::default()
            .validate(r#"first {"analysis":"one"} second {"analysis":"two"}"#)
            .unwrap_err();
        assert!(error
            .iter()
            .any(|message| message.contains("2 valid embedded candidates")));
    }

    #[test]
    fn embedded_normalization_skips_many_brace_heavy_non_json_blocks() {
        let mut response = "C examples:\n".to_string();
        for _ in 0..2_000 {
            response.push_str("if (condition) { do_work(); }\n");
        }
        response.push_str("{\"analysis\":\"ok\",\"followups\":[]}");

        let parsed = CodeResponseContract::default().validate(&response).unwrap();
        assert_eq!(parsed.analysis, "ok");
        assert_eq!(parsed.strategy, ParseStrategy::BraceMatch);
    }

    #[test]
    fn embedded_normalization_escapes_controls_only_inside_json_strings() {
        let response = CodeResponseContract::default()
            .validate("heading\n{\"analysis\":\"first\nsecond\",\"followups\":[]}\nfooter")
            .unwrap();
        assert_eq!(response.analysis, "first\nsecond");
        assert_eq!(response.escaped_control_characters, 1);
        assert_eq!(response.strategy, ParseStrategy::BraceMatch);
    }

    #[test]
    fn discarded_surrounding_is_logged_with_searchable_label() {
        let response = CodeResponseContract::default()
            .validate("important preamble\n{\"analysis\":\"ok\"}\nimportant suffix")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let logger = kres_core::log::TurnLogger::new(temp.path()).unwrap();
        let code_log = logger.session_dir().join("code.jsonl");
        log_json_normalization(Some(&logger), &response, "test-context");
        let logged = std::fs::read_to_string(code_log).unwrap();

        assert!(logged.contains("json-normalization discarded-surrounding-prose"));
        assert!(logged.contains("important preamble"));
        assert!(logged.contains("important suffix"));
        assert!(logged.contains("test-context"));
    }

    #[test]
    fn control_only_normalization_uses_distinct_log_label() {
        let response = CodeResponseContract::default()
            .validate("{\"analysis\":\"first\nsecond\"}")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let logger = kres_core::log::TurnLogger::new(temp.path()).unwrap();
        let code_log = logger.session_dir().join("code.jsonl");
        log_json_normalization(Some(&logger), &response, "test-context");
        let logged = std::fs::read_to_string(code_log).unwrap();

        assert!(logged.contains("json-normalization escaped-control-characters"));
        assert!(!logged.contains("discarded-surrounding-prose"));
    }

    #[test]
    fn projected_schema_omits_unrelated_fields_and_definitions() {
        let schema =
            CodeResponseContract::default().schema_json_for(&["analysis", "findings", "followups"]);
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 3);
        assert!(properties.contains_key("analysis"));
        assert!(properties.contains_key("findings"));
        assert!(properties.contains_key("followups"));
        assert!(!properties.contains_key("code_edits"));

        let defs = schema["$defs"].as_object().unwrap();
        assert!(defs.contains_key("Finding"));
        assert!(defs.contains_key("Followup"));
        assert!(!defs.contains_key("CodeEdit"));
        assert!(!defs.contains_key("PlanRewrite"));
    }

    #[test]
    fn rejects_unknown_nested_fields() {
        let error = CodeResponseContract::default()
            .validate(r#"{"code_output":[{"path":"a","content":"x","purpsoe":"typo"}]}"#)
            .unwrap_err();
        assert!(error.iter().any(|item| item.contains("purpsoe")));
    }

    #[test]
    fn generated_schema_contains_nested_definitions() {
        let schema = CodeResponseContract::default().schema_json();
        assert!(schema.get("properties").is_some());
        assert!(schema.get("$defs").is_some());
        assert_eq!(schema.get("minProperties"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn rejects_empty_response_object() {
        assert!(CodeResponseContract::default().validate("{}").is_err());
        assert!(CodeResponseContract::default()
            .validate(r#"{"analysis":null}"#)
            .is_err());
    }

    #[test]
    fn rejects_duplicate_envelope_fields_before_value_conversion() {
        assert!(CodeResponseContract::default()
            .validate(r#"{"ready_for_slow":false,"ready_for_slow":true}"#)
            .is_err());
        assert!(CodeResponseContract::default()
            .validate(
                r#"{"findings":[{"id":"f","title":"one","title":"two","severity":"high","summary":"x"}]}"#,
            )
            .is_err());
    }

    #[test]
    fn required_fields_are_enforced_and_reflected_in_schema() {
        let contract = CodeResponseContract::default().requiring(["findings"]);
        assert!(contract.validate(r#"{"analysis":"bug"}"#).is_err());
        assert!(contract
            .validate(r#"{"analysis":"bug","findings":null}"#)
            .is_err());
        assert!(contract.validate(r#"{"findings":[]}"#).is_ok());
        assert_eq!(
            contract.schema_json().get("required"),
            Some(&serde_json::json!(["findings"]))
        );
        assert_eq!(
            contract.schema_json()["allOf"][0]["properties"]["findings"]["not"]["type"],
            "null"
        );
    }

    #[test]
    fn repaired_findings_return_to_their_original_positions() {
        let mut response = CodeResponseContract::default()
            .allowing_invalid_findings()
            .validate(
                r#"{"findings":[{"id":"bad","severity":"high"},{"id":"good","title":"good","severity":"high","summary":"s"}]}"#,
            )
            .unwrap();
        let repaired: Finding = serde_json::from_value(serde_json::json!({
            "id": "bad", "title": "fixed", "severity": "high", "summary": "s"
        }))
        .unwrap();
        response.merge_repaired_findings(vec![crate::finding_repair::RepairedFinding {
            index: 0,
            finding: repaired,
        }]);
        assert_eq!(response.findings[0].id, "bad");
        assert_eq!(response.findings[1].id, "good");
    }

    #[test]
    fn strips_store_owned_provenance_from_model_findings() {
        let parsed = CodeResponseContract::default()
            .validate(
                r#"{"findings":[{"id":"f","title":"bug","severity":"high","summary":"s","first_seen_task":"forged","last_updated_task":"forged","first_seen_at":"2020-01-02T03:04:05Z","details":[{"task":"forged","analysis":"x"}]}]}"#,
            )
            .unwrap();
        let finding = &parsed.findings[0];
        assert!(finding.first_seen_task.is_none());
        assert!(finding.last_updated_task.is_none());
        assert!(finding.first_seen_at.is_none());
        assert!(finding.details.is_empty());
    }
}
