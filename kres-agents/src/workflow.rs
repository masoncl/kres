//! Workflow definitions: typed structs + JSON Schema validation
//! for files like `configs/workflows/fix.json`.
//!
//! A workflow is a static, multi-step plan: research → write →
//! compile → review → publish style pipelines, with typed
//! inputs/outputs and conditional + iterative control flow. The
//! file format is documented by `configs/workflows/schema.json`
//! (JSON Schema 2020-12); this module embeds that schema and uses
//! the `jsonschema` crate to validate workflow files at load time,
//! then deserialises into the strongly-typed structs below.
//!
//! Validation is two-layered:
//!
//! 1. **JSON Schema** rejects malformed shapes — bad enum values,
//!    missing required fields, conditional rules (a `reaper` step
//!    must have an `action`; a non-reaper step must have a
//!    `prompt`; an `on_fail.action == "branch_to"` requires a
//!    `branch_to` field).
//! 2. **Cross-field invariants** the schema cannot express: every
//!    `depends_on` id resolves to a real step, every step id is
//!    unique within the workflow, `branch_to` and
//!    `eval.on_fail.rerun` ids resolve to a real step.
//!
//! The runtime executor is not implemented yet; this module exists
//! so workflow files can be authored, schema-checked, and parsed
//! into typed structs ahead of the executor work. `kres validate
//! <path>` exercises the loader.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Embedded JSON Schema. Consumers don't need
/// `configs/workflows/schema.json` on disk — the schema is part of
/// the binary so a single `kres` install validates workflow files
/// against the version it was built with.
const SCHEMA_JSON: &str = include_str!("../../configs/workflows/schema.json");

/// Top-level workflow definition. Mirrors the schema 1:1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    #[serde(rename = "$schema_version")]
    pub schema_version: u32,

    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none", default)]
    pub schema_url: Option<String>,

    /// Self-documentation block. Carried verbatim — this module
    /// does not interpret its contents.
    #[serde(rename = "$format", skip_serializing_if = "Option::is_none", default)]
    pub format: Option<serde_json::Value>,

    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,

    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,

    #[serde(default)]
    pub skills: Vec<String>,

    #[serde(default)]
    pub globals: serde_json::Map<String, serde_json::Value>,

    #[serde(default)]
    pub defaults: Defaults,

    pub steps: Vec<Step>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completion: Option<Completion>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub persistence: Option<Persistence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<Agent>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub actions: Option<Vec<ActionType>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_eval_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub on_exhausted: Option<OnExhausted>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<Agent>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub actions: Option<Vec<ActionType>>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_if: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skip_if: Option<String>,
    #[serde(default)]
    pub preserve_outputs_on_skip: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "deserialize_optional_prompt"
    )]
    pub prompt: Option<String>,
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub outputs: serde_json::Map<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub eval: Option<Eval>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub action: Option<ReaperAction>,
    #[serde(default)]
    pub terminal_on_success: bool,
    /// Lens fan-out: when present, the step's prompt runs once per
    /// lens (concurrently). The lens-object's fields are bound as
    /// `{{lens.<field>}}` for that call. Per-lens outputs are
    /// aggregated per [`Self::aggregate`] before eval runs.
    #[serde(default)]
    pub lenses: Vec<Lens>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub aggregate: Option<Aggregate>,
    /// When `aggregate == Aggregate::Consolidate`, configures the
    /// N+1 LLM call that runs after the lens fan-out settles.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub consolidate: Option<ConsolidateConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidateConfig {
    /// Author-supplied dedup / merge / completeness rules. The
    /// runner appends the per-lens outputs and an OUTPUT SCHEMA
    /// tail before sending to the LLM.
    #[serde(deserialize_with = "deserialize_prompt")]
    pub prompt: String,
    /// Optional agent override for the consolidate call.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<Agent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lens {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_if: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skip_if: Option<String>,
    /// Anything else the workflow author wants to bind. Resolved
    /// via `{{lens.<key>}}` in the step prompt.
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

pub fn lens_to_spec(lens: &Lens) -> kres_core::LensSpec {
    kres_core::LensSpec {
        id: lens.id.clone(),
        kind: lens
            .fields
            .get("tag")
            .or_else(|| lens.fields.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("investigate")
            .to_string(),
        name: lens
            .fields
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&lens.id)
            .to_string(),
        reason: lens
            .fields
            .get("investigate")
            .or_else(|| lens.fields.get("reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// Default. Array-typed declared outputs concatenate across
    /// lenses; scalar-typed outputs collect as
    /// `[{lens, value}, ...]`.
    #[default]
    Concat,
    /// Every declared output becomes a `{lens_id: value}` object.
    ByLens,
    /// After the lens fan-out settles, run an N+1 LLM call that
    /// semantically merges duplicate findings via a consolidator
    /// prompt. Replaces structural concatenation with a deduped,
    /// merged result. Requires a `consolidate` config block.
    Consolidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Fast,
    Slow,
    Code,
    Classifier,
    Reaper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Audit,
    Coding,
    Review,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionType {
    Read,
    Source,
    Type,
    Git,
    Grep,
    Callers,
    Make,
    Meson,
    Edit,
    Bash,
    Lore,
    #[serde(rename = "publish-fix")]
    PublishFix,
    #[serde(rename = "commit-fix")]
    CommitFix,
    #[serde(rename = "set-finding-status")]
    SetFindingStatus,
    #[serde(rename = "set-finding-results")]
    SetFindingResults,
    #[serde(rename = "set-finding-bugs")]
    SetFindingBugs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Eval {
    #[serde(rename = "type")]
    pub kind: EvalKind,
    /// field_check expression. Required when kind=FieldCheck.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expr: Option<String>,
    /// builtin validator name. Required when kind=Builtin.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// judge_llm prompt. Required when kind=JudgeLlm.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "deserialize_optional_prompt"
    )]
    pub judge_prompt: Option<String>,
    /// judge_llm: optional agent override.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent: Option<Agent>,
    pub on_fail: OnFail,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PromptValue {
    String(String),
    Lines(Vec<String>),
}

impl PromptValue {
    fn into_string(self) -> String {
        match self {
            PromptValue::String(s) => s,
            PromptValue::Lines(lines) => lines.join("\n"),
        }
    }
}

fn deserialize_prompt<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    PromptValue::deserialize(deserializer).map(PromptValue::into_string)
}

fn deserialize_optional_prompt<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<PromptValue>::deserialize(deserializer).map(|v| v.map(PromptValue::into_string))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalKind {
    /// Comparison expression evaluated locally.
    FieldCheck,
    /// Named Rust-side validator. A false result is an eval failure
    /// and consumes the normal on_fail retry budget.
    Builtin,
    /// LLM-judged. Sends step outputs + judge_prompt to an agent;
    /// the judge replies `{"pass": bool, "reason": string}`.
    JudgeLlm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnFail {
    pub action: OnFailAction,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub on_exhausted: Option<OnExhausted>,
    #[serde(default)]
    pub rerun: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch_to_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch_to_doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailAction {
    Repeat,
    RerunChain,
    BranchTo,
    Continue,
    ExitFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    ExitFailure,
    Continue,
    BranchTo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReaperAction {
    #[serde(rename = "type")]
    pub kind: ActionType,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub args: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    #[serde(default)]
    pub success_when_any: Vec<String>,
    #[serde(default)]
    pub failure_when_any: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persistence {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resumable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub shape: Option<String>,
}

/// Parse + validate a workflow JSON. Returns the typed [`Workflow`]
/// or an error explaining the first schema or cross-field violation.
pub fn parse_workflow(body: &str) -> Result<Workflow> {
    parse_workflow_with_base(body, None)
}

/// Parse + validate a workflow JSON, resolving any relative
/// `prompt_file` references against `prompt_base` before
/// deserialising into typed structs.
fn parse_workflow_with_base(body: &str, prompt_base: Option<&Path>) -> Result<Workflow> {
    let mut value: Value = serde_json::from_str(body).context("workflow body is not valid JSON")?;
    validate_against_schema(&value)?;
    resolve_prompt_files(&mut value, prompt_base)?;
    validate_against_schema(&value)?;
    let wf: Workflow = serde_json::from_value(value).context(
        "workflow JSON validated against schema but failed to deserialise — schema/struct drift",
    )?;
    validate_cross_field(&wf)?;
    Ok(wf)
}

/// Convenience wrapper: read `path`, parse + validate.
pub fn load_workflow(path: &Path) -> Result<Workflow> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading workflow {}", path.display()))?;
    parse_workflow_with_base(&body, path.parent())
        .with_context(|| format!("validating workflow {}", path.display()))
}

/// Embedded workflow JSON, keyed by id. Carried in the binary via
/// `include_str!` so a fresh install with no `~/.kres/workflows/`
/// can still run `kres run-workflow fix --input target=...`.
const EMBEDDED_WORKFLOWS: &[(&str, &str)] = &[
    ("fix", include_str!("../../configs/workflows/fix.json")),
    (
        "review",
        include_str!("../../configs/workflows/review.json"),
    ),
    (
        "triage",
        include_str!("../../configs/workflows/triage.json"),
    ),
    (
        "validate",
        include_str!("../../configs/workflows/validate.json"),
    ),
];

/// Iterator over every embedded workflow id. Useful for `/help` and
/// for the user_commands dispatch that needs to know which slash
/// names map to a workflow.
pub fn embedded_workflow_ids() -> impl Iterator<Item = &'static str> {
    EMBEDDED_WORKFLOWS.iter().map(|(k, _)| *k)
}

/// Resolve a workflow by id with operator-override layering.
///
/// Order:
///   1. `<override_dir>/<id>.json` on disk (when override_dir is
///      `Some` — typically `~/.kres/workflows`).
///   2. Embedded copy bundled in the binary
///      (`configs/workflows/<id>.json`).
///   3. Error.
///
/// Names are validated with the same `[a-z0-9_-]+` rule as
/// user_commands so a stray slash doesn't escape the override
/// directory.
pub fn lookup_workflow(override_dir: Option<&Path>, id: &str) -> Result<Workflow> {
    if !is_valid_workflow_id(id) {
        return Err(anyhow!("invalid workflow id '{id}'"));
    }
    if let Some(dir) = override_dir {
        let p = dir.join(format!("{id}.json"));
        if let Ok(body) = std::fs::read_to_string(&p) {
            if !body.trim().is_empty() {
                return parse_workflow_with_base(&body, p.parent())
                    .with_context(|| format!("validating workflow {}", p.display()));
            }
        }
    }
    if let Some((_, body)) = EMBEDDED_WORKFLOWS.iter().find(|(k, _)| *k == id) {
        let base = embedded_workflow_base();
        return parse_workflow_with_base(body, Some(&base))
            .with_context(|| format!("validating embedded workflow {id}"));
    }
    Err(anyhow!("no workflow named '{id}' found"))
}

fn embedded_workflow_base() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("configs/workflows")
}

fn is_valid_workflow_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validate the on-disk JSON shape against the embedded JSON Schema.
/// Returns the first batch of errors as a single anyhow error.
fn validate_against_schema(value: &serde_json::Value) -> Result<()> {
    // The embedded schema is parsed once per call. The crate's
    // `validator_for` pre-compiles it. Caching the validator across
    // calls is an obvious optimisation; defer it until validation
    // shows up on a profile.
    let schema_value: serde_json::Value =
        serde_json::from_str(SCHEMA_JSON).context("embedded workflow schema is not valid JSON")?;
    let validator = jsonschema::validator_for(&schema_value)
        .context("embedded workflow schema failed to compile")?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .take(10)
        .map(|e| format!("at /{}: {}", e.instance_path, e))
        .collect();
    if !errors.is_empty() {
        return Err(anyhow!(
            "workflow schema validation failed:\n  {}",
            errors.join("\n  ")
        ));
    }
    Ok(())
}

/// Workflow authors may keep large prompts inline as editable JSON
/// arrays or move them to a sibling file. The executor only knows
/// about final prompt strings, so resolve file-backed prompts before
/// serde deserialisation.
fn resolve_prompt_files(value: &mut Value, base_dir: Option<&Path>) -> Result<()> {
    let Some(root) = value.as_object_mut() else {
        return Ok(());
    };
    let Some(steps) = root.get_mut("steps").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for step in steps {
        let Some(step) = step.as_object_mut() else {
            continue;
        };
        resolve_prompt_file_key(step, "prompt_file", "prompt", base_dir)?;
        if let Some(consolidate) = step.get_mut("consolidate").and_then(Value::as_object_mut) {
            resolve_prompt_file_key(consolidate, "prompt_file", "prompt", base_dir)?;
        }
        if let Some(eval) = step.get_mut("eval").and_then(Value::as_object_mut) {
            resolve_prompt_file_key(eval, "judge_prompt_file", "judge_prompt", base_dir)?;
        }
    }
    Ok(())
}

fn resolve_prompt_file_key(
    map: &mut Map<String, Value>,
    file_key: &str,
    prompt_key: &str,
    base_dir: Option<&Path>,
) -> Result<()> {
    let Some(file_value) = map.remove(file_key) else {
        return Ok(());
    };
    if map.contains_key(prompt_key) {
        return Err(anyhow!(
            "workflow cannot set both '{prompt_key}' and '{file_key}' in the same object"
        ));
    }
    let raw_path = file_value
        .as_str()
        .ok_or_else(|| anyhow!("workflow '{file_key}' must be a string"))?;
    let path = resolve_prompt_path(raw_path, base_dir);
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("reading workflow prompt file {}", path.display()))?;
    map.insert(prompt_key.to_owned(), Value::String(contents));
    Ok(())
}

fn resolve_prompt_path(raw_path: &str, base_dir: Option<&Path>) -> PathBuf {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base_dir) = base_dir {
        base_dir.join(path)
    } else {
        path.to_path_buf()
    }
}

// Output names the interpolation engines (`resolve_one` /
// `resolve`) read directly from `StepState` rather than from
// `outputs`. A step that declares an output by one of these names
// would be silently unreachable via `{{<step>.<name>}}` because the
// special-case branch fires first. Reject the workflow at parse
// time so the collision is loud instead.
const RESERVED_STEP_OUTPUT_NAMES: &[&str] = &["attempt", "eval_failures", "prior_attempts"];

/// Cross-field invariants the schema cannot express:
/// - every step id is unique
/// - every `depends_on` id resolves to a real step
/// - every literal `eval.on_fail.branch_to` and every entry of
///   `eval.on_fail.rerun` resolves to a real step
/// - no step declares an output whose name is in
///   [`RESERVED_STEP_OUTPUT_NAMES`]
fn validate_cross_field(wf: &Workflow) -> Result<()> {
    let ids: BTreeSet<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    if ids.len() != wf.steps.len() {
        let mut seen = BTreeSet::new();
        let dup = wf
            .steps
            .iter()
            .find(|s| !seen.insert(s.id.as_str()))
            .map(|s| s.id.clone())
            .unwrap_or_default();
        return Err(anyhow!("duplicate step id: {dup}"));
    }
    for step in &wf.steps {
        if matches!(step.agent, Some(Agent::Reaper)) && step.eval.is_some() {
            return Err(anyhow!(
                "reaper step '{}' cannot declare eval; its action is the acceptance boundary",
                step.id
            ));
        }
        for reserved in RESERVED_STEP_OUTPUT_NAMES {
            if step.outputs.contains_key(*reserved) {
                return Err(anyhow!(
                    "step '{}' declares output '{}', which is reserved for the interpolation \
                     engine (`{{{{<step>.{}}}}}` reads it from StepState, not from outputs)",
                    step.id,
                    reserved,
                    reserved
                ));
            }
        }
        for dep in &step.depends_on {
            if !ids.contains(dep.as_str()) {
                return Err(anyhow!(
                    "step '{}' depends_on unknown step '{}'",
                    step.id,
                    dep
                ));
            }
        }
        // Lens ids unique within the step.
        if !step.lenses.is_empty() {
            let mut seen = BTreeSet::new();
            for l in &step.lenses {
                if !seen.insert(l.id.as_str()) {
                    return Err(anyhow!(
                        "step '{}' has duplicate lens id '{}'",
                        step.id,
                        l.id
                    ));
                }
            }
        }
        if let Some(eval) = &step.eval {
            if let Some(target) = &eval.on_fail.branch_to {
                if !ids.contains(target.as_str()) {
                    return Err(anyhow!(
                        "step '{}' on_fail.branch_to references unknown step '{}'",
                        step.id,
                        target
                    ));
                }
            }
            if eval.on_fail.action == OnFailAction::BranchTo
                && eval.on_fail.branch_to.is_none()
                && eval.on_fail.branch_to_output.is_none()
            {
                return Err(anyhow!(
                    "step '{}' on_fail.action branch_to requires branch_to or branch_to_output",
                    step.id
                ));
            }
            if matches!(eval.on_fail.on_exhausted, Some(OnExhausted::BranchTo))
                && eval.on_fail.branch_to.is_none()
                && eval.on_fail.branch_to_output.is_none()
            {
                return Err(anyhow!(
                    "step '{}' on_fail.on_exhausted branch_to requires branch_to or branch_to_output",
                    step.id
                ));
            }
            for r in &eval.on_fail.rerun {
                if !ids.contains(r.as_str()) {
                    return Err(anyhow!(
                        "step '{}' on_fail.rerun references unknown step '{}'",
                        step.id,
                        r
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Substring match against a prompt that has been joined from a
/// JSON `[…]` array (workflow prompts use `PromptValue::into_string`
/// to `\n`-join the array). Both haystack and needle are
/// whitespace-flattened (every run of whitespace collapsed to a
/// single space) before the contains check, so a phrase that the
/// JSON file happens to wrap across two array elements still
/// matches. Use this instead of bare `str::contains` whenever the
/// assertion is checking semantic prompt content (rather than
/// exact tokenization). Test-only.
#[cfg(test)]
pub(crate) fn prompt_contains_phrase(prompt: &str, phrase: &str) -> bool {
    fn flatten(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    flatten(prompt).contains(&flatten(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_phrase_collapses_whitespace() {
        // Phrase wrapped across newlines and extra indent must still
        // match a single-line needle.
        let wrapped = "line one\n  line  two\n\t\tline three";
        assert!(prompt_contains_phrase(
            wrapped,
            "line one line two line three"
        ));
        // Tokenization that disagrees still misses.
        assert!(!prompt_contains_phrase(wrapped, "line four"));
    }

    /// The shipped fix workflow is the canonical example. It must
    /// validate against the embedded schema AND deserialise into
    /// the typed structs.
    #[test]
    fn lookup_workflow_finds_embedded_fix() {
        let wf = lookup_workflow(None, "fix").unwrap();
        assert_eq!(wf.id, "fix");
    }

    #[test]
    fn lookup_workflow_finds_embedded_review() {
        let wf = lookup_workflow(None, "review").unwrap();
        assert_eq!(wf.id, "review");
    }

    #[test]
    fn lookup_workflow_finds_embedded_validate() {
        let wf = lookup_workflow(None, "validate").unwrap();
        assert_eq!(wf.id, "validate");
    }

    #[test]
    fn review_workflow_loads_with_parallel_lenses() {
        let body = include_str!("../../configs/workflows/review.json");
        let wf = parse_workflow(body).expect("review.json must validate against schema");
        assert_eq!(wf.id, "review");
        assert_eq!(wf.steps.len(), 1);
        let step = &wf.steps[0];
        assert_eq!(step.id, "investigate");
        assert_eq!(step.lenses.len(), 5);
        assert_eq!(step.lenses[0].id, "memory-lifetime");
        let assertions = step
            .lenses
            .iter()
            .find(|l| l.id == "assertions")
            .expect("review workflow has commit assertion lens");
        assert_eq!(
            assertions.run_if.as_deref(),
            Some("workflow.target_is_commit == true")
        );
        assert!(assertions
            .fields
            .get("investigate")
            .and_then(|v| v.as_str())
            .is_some_and(|s| {
                s.contains("disprove every assertion")
                    && s.contains("existing declarations, kerneldoc, comments, or docs")
                    && s.contains("made stale by the patch")
            }));
        assert!(step.consolidate.is_some());
        assert!(step.outputs.contains_key("analysis"));
        assert!(step.outputs.contains_key("findings"));
        assert!(wf
            .globals
            .get("finding_schema")
            .and_then(|v| v.as_str())
            .is_some_and(|schema| schema.contains("survey|read|source")));
    }

    #[test]
    fn triage_workflow_preserves_golden_contract() {
        let body = include_str!("../../configs/workflows/triage.json");
        let wf = parse_workflow(body).expect("triage.json must validate against schema");
        assert_eq!(wf.id, "triage");
        assert_eq!(wf.steps.len(), 1);

        let step = &wf.steps[0];
        assert_eq!(step.id, "triage");
        let actions = step.actions.as_ref().expect("triage actions");
        for action in [
            ActionType::Read,
            ActionType::Source,
            ActionType::Type,
            ActionType::Grep,
            ActionType::Git,
            ActionType::Callers,
            ActionType::Edit,
        ] {
            assert!(
                actions.contains(&action),
                "triage step should allow {action:?}"
            );
        }
        assert!(step.include.iter().any(|i| i.contains("triage_rules")));
        assert!(step.outputs.contains_key("verdict"));
        assert!(step.outputs.contains_key("severity"));
        assert!(step.outputs.contains_key("summary_written"));
        assert!(step.outputs.contains_key("severity_written"));
        assert!(step.outputs.contains_key("followups"));
        assert!(step.outputs.contains_key("code_output"));
        assert!(step.outputs.contains_key("triage_coding"));
        assert_eq!(
            step.eval.as_ref().and_then(|e| e.expr.as_deref()),
            Some(
                "summary_written == true && severity_written == true && triage_coding.schema_version == 1 && triage_coding.severity == severity"
            )
        );
    }

    #[test]
    fn validate_workflow_preserves_validation_contract() {
        let body = include_str!("../../configs/workflows/validate.json");
        let wf = parse_workflow(body).expect("validate.json must validate against schema");
        let triage = parse_workflow(include_str!("../../configs/workflows/triage.json"))
            .expect("triage.json must validate against schema");
        assert_eq!(wf.id, "validate");
        assert_eq!(wf.skills, vec!["auto"]);
        assert_eq!(wf.steps.len(), 2);

        let fast = &wf.steps[0];
        assert_eq!(fast.id, "validate-claims");
        assert_eq!(fast.agent, Some(Agent::Fast));
        assert_eq!(fast.mode, Some(Mode::Coding));
        assert!(fast.lenses.is_empty(), "validate must not use lenses");
        assert!(fast.outputs.contains_key("claim_validation"));
        let claim_schema = fast
            .outputs
            .get("claim_validation")
            .and_then(|def| def.get("schema"))
            .expect("claim_validation schema");
        assert_eq!(
            claim_schema.pointer("/properties/supported/items/type"),
            Some(&serde_json::Value::String("object".to_string())),
            "supported claim entries must preserve structured evidence"
        );
        assert_eq!(
            claim_schema.pointer("/properties/contradicted/items/type"),
            Some(&serde_json::Value::String("object".to_string())),
            "contradicted claim entries must preserve structured evidence"
        );
        assert_eq!(
            claim_schema.pointer("/properties/unresolved/items/type"),
            Some(&serde_json::Value::String("object".to_string())),
            "unresolved claim entries must preserve structured evidence"
        );
        assert_eq!(
            fast.eval.as_ref().and_then(|e| e.expr.as_deref()),
            Some("claim_validation.schema_version == 1")
        );
        let fast_prompt = fast.prompt.as_deref().expect("validate fast prompt");
        assert!(prompt_contains_phrase(
            fast_prompt,
            "Use `git show -s --oneline <sha>` to check that a commit object exists"
        ));
        assert!(prompt_contains_phrase(
            fast_prompt,
            "`git merge-base --is-ancestor <sha> HEAD` to check whether that commit is present"
        ));

        let slow = &wf.steps[1];
        assert_eq!(slow.id, "validate-reachability");
        assert_eq!(slow.agent, Some(Agent::Slow));
        assert_eq!(slow.mode, Some(Mode::Coding));
        assert_eq!(slow.depends_on, vec!["validate-claims"]);
        assert!(slow.lenses.is_empty(), "validate must not use lenses");
        assert!(slow.include.iter().any(|i| i.contains("triage_rules")));
        assert!(slow.outputs.contains_key("verdict"));
        assert!(slow.outputs.contains_key("severity"));
        assert!(slow.outputs.contains_key("summary_written"));
        assert!(slow.outputs.contains_key("severity_written"));
        assert!(slow.outputs.contains_key("code_output"));
        assert!(slow.outputs.contains_key("triage_coding"));
        assert_eq!(
            slow.outputs
                .get("triage_coding")
                .and_then(|def| def.get("schema")),
            triage.steps[0]
                .outputs
                .get("triage_coding")
                .and_then(|def| def.get("schema")),
            "validate must keep the same triage_coding schema as triage"
        );
        let slow_prompt = slow.prompt.as_deref().expect("validate slow prompt");
        assert!(prompt_contains_phrase(
            slow_prompt,
            "if verdict is `Invalid`, summary_status must be `invalid`"
        ));
        assert!(prompt_contains_phrase(
            slow_prompt,
            "- summary_status: one of fixed, plausible, unconfirmed, unknown, invalid, confirmed_latent"
        ));
        assert!(prompt_contains_phrase(
            slow_prompt,
            "do not leave `metadata.yaml` or `FINDING.md` saying the old question remains open"
        ));
        assert!(prompt_contains_phrase(
            slow_prompt,
            "add or update this top-level marker exactly: `validation_run: true`"
        ));
        assert!(prompt_contains_phrase(
            slow_prompt,
            "Use `git show -s --oneline <sha>` to check that a commit object exists"
        ));
        assert!(prompt_contains_phrase(
            slow_prompt,
            "`git merge-base --is-ancestor <sha> HEAD` to check whether that commit is present"
        ));
        assert_eq!(
            slow.eval.as_ref().and_then(|e| e.expr.as_deref()),
            Some(
                "summary_written == true && severity_written == true && triage_coding.schema_version == 1 && triage_coding.severity == severity"
            )
        );
    }

    #[test]
    fn prompt_arrays_join_to_runtime_strings() {
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt": ["line one", "", "line three"],
                "consolidate": {"prompt": ["merge", "these"]},
                "eval": {
                    "type": "judge_llm",
                    "judge_prompt": ["judge", "this"],
                    "on_fail": {"action": "continue"}
                }
            }]
        }"#;
        let wf = parse_workflow(body).expect("prompt arrays should validate and parse");
        let step = &wf.steps[0];
        assert_eq!(step.prompt.as_deref(), Some("line one\n\nline three"));
        assert_eq!(step.consolidate.as_ref().unwrap().prompt, "merge\nthese");
        assert_eq!(
            step.eval.as_ref().unwrap().judge_prompt.as_deref(),
            Some("judge\nthis")
        );
    }

    #[test]
    fn prompt_files_load_relative_to_workflow_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("step.md"), "step prompt\n").unwrap();
        std::fs::write(tmp.path().join("consolidate.md"), "merge prompt\n").unwrap();
        std::fs::write(tmp.path().join("judge.md"), "judge prompt\n").unwrap();
        let workflow = serde_json::json!({
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt_file": "step.md",
                "consolidate": {"prompt_file": "consolidate.md"},
                "eval": {
                    "type": "judge_llm",
                    "judge_prompt_file": "judge.md",
                    "on_fail": {"action": "continue"}
                }
            }]
        });
        let workflow_path = tmp.path().join("x.json");
        std::fs::write(&workflow_path, workflow.to_string()).unwrap();

        let wf = load_workflow(&workflow_path).expect("prompt files should resolve");
        let step = &wf.steps[0];
        assert_eq!(step.prompt.as_deref(), Some("step prompt\n"));
        assert_eq!(step.consolidate.as_ref().unwrap().prompt, "merge prompt\n");
        assert_eq!(
            step.eval.as_ref().unwrap().judge_prompt.as_deref(),
            Some("judge prompt\n")
        );
    }

    #[test]
    fn prompt_file_rejects_inline_prompt_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("step.md"), "step prompt\n").unwrap();
        let workflow = serde_json::json!({
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt": "inline",
                "prompt_file": "step.md"
            }]
        });
        let workflow_path = tmp.path().join("x.json");
        std::fs::write(&workflow_path, workflow.to_string()).unwrap();

        let err = load_workflow(&workflow_path).unwrap_err();
        assert!(
            err.chain()
                .any(|cause| cause.to_string().contains("schema validation failed")),
            "got: {err:?}"
        );
    }

    #[test]
    fn prompt_file_resolution_does_not_rewrite_freeform_globals() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("step.md"), "step prompt\n").unwrap();
        let workflow = serde_json::json!({
            "$schema_version": 1,
            "id": "x",
            "globals": {
                "example": {"prompt_file": "not-a-real-prompt.md"}
            },
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt_file": "step.md"
            }]
        });
        let workflow_path = tmp.path().join("x.json");
        std::fs::write(&workflow_path, workflow.to_string()).unwrap();

        let wf = load_workflow(&workflow_path).expect("step prompt_file should resolve");
        assert_eq!(wf.steps[0].prompt.as_deref(), Some("step prompt\n"));
        assert_eq!(
            wf.globals["example"]["prompt_file"].as_str(),
            Some("not-a-real-prompt.md")
        );
    }

    #[test]
    fn lookup_workflow_unknown_id_errors() {
        let err = lookup_workflow(None, "no-such-thing")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no workflow named"), "got: {err}");
    }

    #[test]
    fn lookup_workflow_rejects_bad_id() {
        let err = lookup_workflow(None, "../etc/passwd")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid workflow id"), "got: {err}");
    }

    #[test]
    fn lookup_workflow_disk_override_wins() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a custom fix.json with a different title.
        let custom = serde_json::json!({
            "$schema_version": 1,
            "id": "fix",
            "title": "OPERATOR OVERRIDE",
            "steps": [{"id": "s", "agent": "fast", "prompt": "p"}]
        });
        std::fs::write(tmp.path().join("fix.json"), custom.to_string()).unwrap();
        let wf = lookup_workflow(Some(tmp.path()), "fix").unwrap();
        assert_eq!(wf.title.as_deref(), Some("OPERATOR OVERRIDE"));
    }

    #[test]
    fn lookup_workflow_falls_back_to_embedded_when_override_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty override dir — fallback to embedded.
        let wf = lookup_workflow(Some(tmp.path()), "fix").unwrap();
        assert_ne!(wf.title.as_deref(), Some("OPERATOR OVERRIDE"));
    }

    #[test]
    fn fix_workflow_loads() {
        let body = include_str!("../../configs/workflows/fix.json");
        let wf = parse_workflow(body).expect("fix.json must validate against schema");
        assert_eq!(wf.id, "fix");
        assert_eq!(wf.schema_version, 1);
        // Research plus deterministic status/commit/build/publish
        // steps around the LLM-authored patch/provenance/message/review
        // steps, plus the post-review orchestrator that routes the
        // next step when review is not clean.
        assert_eq!(wf.steps.len(), 16, "fix workflow has 16 steps");
        let research = wf.steps.iter().find(|s| s.id == "research").unwrap();
        let research_eval = research.eval.as_ref().expect("research eval");
        assert_eq!(research_eval.kind, EvalKind::Builtin);
        assert_eq!(
            research_eval.name.as_deref(),
            Some("fix_research_status"),
            "research eval must use the named Rust-side status validator"
        );
        assert_eq!(research_eval.on_fail.action, OnFailAction::Repeat);
        assert_eq!(research_eval.on_fail.max_attempts, Some(3));
        let write_patch = wf.steps.iter().find(|s| s.id == "write-patch").unwrap();
        assert!(
            write_patch.outputs.contains_key("review_dispute"),
            "write-patch must expose a typed review dispute path"
        );
        // write-patch keeps a minimal sanity-check eval — it must
        // either emit code changes or fill in review_dispute. Routing
        // decisions live in the post-review orchestrator; this eval
        // only catches a model that returned nothing useful so the
        // chain doesn't proceed to commit-fix with an empty diff.
        let write_patch_eval = write_patch
            .eval
            .as_ref()
            .expect("write-patch must have a sanity-check eval");
        assert_eq!(write_patch_eval.kind, EvalKind::FieldCheck);
        assert_eq!(
            write_patch_eval.expr.as_deref(),
            Some("code_changes_emitted == true || review_dispute != ''"),
            "write-patch sanity check must accept either real edits or a non-empty dispute"
        );
        assert_eq!(write_patch_eval.on_fail.action, OnFailAction::BranchTo);
        assert_eq!(
            write_patch_eval.on_fail.branch_to.as_deref(),
            Some("orchestrator"),
            "write-patch eval failure must route to the orchestrator so retry decisions are LLM-owned, not blind repeats"
        );
        assert_eq!(write_patch_eval.on_fail.max_attempts, Some(12));
        let write_commit = wf
            .steps
            .iter()
            .find(|s| s.id == "write-commit-message")
            .unwrap();
        let write_commit_eval = write_commit
            .eval
            .as_ref()
            .expect("write-commit-message must have a sanity-check eval");
        assert_eq!(write_commit_eval.on_fail.action, OnFailAction::BranchTo);
        assert_eq!(
            write_commit_eval.on_fail.branch_to.as_deref(),
            Some("orchestrator"),
            "write-commit-message eval failure must route to the orchestrator, not blind repeat"
        );
        assert_eq!(write_commit_eval.on_fail.max_attempts, Some(12));
        assert!(wf.steps.iter().any(|s| s.id == "unconfirm"));
        assert!(wf.steps.iter().any(|s| s.id == "fixes-tag-search"));
        assert!(wf.steps.iter().any(|s| s.id == "publish"));
        let commit_template = include_str!("../../configs/prompts/commit-kernel-template.md");
        assert!(
            commit_template.contains("Write a kernel changelog, not an audit report")
                && commit_template.contains("indented evidence")
                && commit_template.contains("Prefer call chains and call graphs over prose")
                && commit_template.contains("Simple ASCII art is allowed")
                && commit_template.contains("Race timeline")
                && commit_template.contains("Call chain with state transition")
                && commit_template.contains("Call graph")
                && commit_template.contains("Before/after state")
                && commit_template.contains("Dense proof-memo paragraphs"),
            "commit template must require readable kernel changelog prose with evidence blocks"
        );
        let fixes = wf
            .steps
            .iter()
            .find(|s| s.id == "fixes-tag-search")
            .unwrap();
        assert_eq!(fixes.depends_on, vec!["write-patch".to_string()]);
        assert_eq!(
            fixes.run_if.as_deref(),
            Some("research.research_status == 'confirmed' && write-patch.code_changes_emitted == true && fixes-tag-search.attempt == 0"),
            "fixes-tag-search must run only once, after the first emitted patch"
        );
        assert!(
            fixes.preserve_outputs_on_skip,
            "fixes-tag-search must preserve provenance across no-op review-dispute passes"
        );
        let fixes_prompt = fixes.prompt.as_deref().unwrap_or("");
        assert!(
            fixes_prompt.contains("git blame` is only a starting point")
                || (fixes_prompt.contains("git blame` is only")
                    && fixes_prompt.contains("starting point")),
            "fixes-tag-search prompt must reject blame-only Fixes research"
        );
        assert!(
            fixes_prompt.contains("prove the invariant changed across that commit"),
            "fixes-tag-search prompt must require candidate-diff proof for Fixes"
        );
        assert!(
            fixes_prompt.contains("git diff HEAD~1")
                && fixes_prompt.contains("Do not base provenance")
                && fixes_prompt.contains("incremental retry diff alone"),
            "fixes-tag-search prompt must handle retry attempts with an existing committed patch"
        );
        assert!(
            fixes.outputs.contains_key("unproven_fixes_candidates"),
            "fixes-tag-search must preserve plausible unproven candidates"
        );
        // lore-search runs between research and write-patch and must
        // allow the `lore` followup kind so the fast agent can call
        // the semcode lore_search MCP tool.
        let lore = wf
            .steps
            .iter()
            .find(|s| s.id == "lore-search")
            .expect("fix workflow must include lore-search step");
        assert_eq!(lore.agent, Some(Agent::Fast));
        assert_eq!(lore.mode, Some(Mode::Audit));
        let lore_actions = lore
            .actions
            .as_ref()
            .expect("lore-search must list actions");
        assert!(
            lore_actions.contains(&ActionType::Lore),
            "lore-search must allow the `lore` followup kind"
        );
        assert!(
            lore.outputs.contains_key("existing_patches")
                && lore.outputs.contains_key("duplicate_proven"),
            "lore-search must declare existing_patches and duplicate_proven outputs"
        );
        // write-patch must depend on lore-search so its prompt can
        // interpolate the upstream-patches block.
        let write_patch = wf
            .steps
            .iter()
            .find(|s| s.id == "write-patch")
            .expect("fix workflow must include write-patch step");
        assert!(
            write_patch.depends_on.iter().any(|d| d == "lore-search"),
            "write-patch must depend_on lore-search"
        );
        let write_patch_prompt = write_patch.prompt.as_deref().unwrap_or("");
        assert!(
            write_patch_prompt.contains("{{lore-search.existing_patches")
                && write_patch_prompt.contains("{{lore-search.duplicate_proven"),
            "write-patch prompt must interpolate the lore-search outputs"
        );

        // Invalidate now hands off to record-invalidation-results,
        // which is the terminal-on-success short-circuit.
        let inv = wf.steps.iter().find(|s| s.id == "invalidate").unwrap();
        assert!(!inv.terminal_on_success);
        let rec_inv = wf
            .steps
            .iter()
            .find(|s| s.id == "record-invalidation-results")
            .unwrap();
        assert!(rec_inv.terminal_on_success);
        let write_commit_message = wf
            .steps
            .iter()
            .find(|s| s.id == "write-commit-message")
            .unwrap();
        let commit_prompt = write_commit_message.prompt.as_deref().unwrap_or("");
        assert!(
            commit_prompt.contains("human-readable kernel changelog")
                && commit_prompt.contains("dense proof memo")
                && commit_prompt.contains("focused indented evidence blocks")
                && commit_prompt.contains("Prefer call chains and ASCII call graphs")
                && commit_prompt.contains("Do not inventory every caller"),
            "write-commit-message prompt must reject wall-of-text commit messages"
        );
        // Review defects branch through the consolidated correction
        // target; review itself does not edit or amend.
        let review = wf.steps.iter().find(|s| s.id == "review").unwrap();
        assert_eq!(
            review.actions.as_deref(),
            Some(
                &[
                    ActionType::Read,
                    ActionType::Source,
                    ActionType::Type,
                    ActionType::Git,
                    ActionType::Grep,
                    ActionType::Callers,
                ][..]
            )
        );
        assert_eq!(review.lenses.len(), 7);
        assert_eq!(review.aggregate, Some(Aggregate::Consolidate));
        assert!(review.consolidate.is_some());
        let review_prompt = review.prompt.as_deref().unwrap_or("");
        assert!(
            review_prompt.contains("{{lens.id}}") && review_prompt.contains("{{lens.investigate}}"),
            "fix review must bind real JSON lenses into the prompt"
        );
        assert!(
            !review_prompt.contains("Apply these lenses exhaustively"),
            "fix review must not reintroduce a prompt-level lens checklist"
        );
        assert!(
            review.lenses.iter().any(|l| l.id == "maintainer"
                && l.fields
                    .get("investigate")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| {
                        s.contains("Antagonistic kernel-maintainer review")
                            && s.contains("Expect at least one set of corrections")
                            && s.contains("stale or contradicted documentation")
                            && s.contains("human-readable kernel changelog")
                            && s.contains("wall-of-text proof memo")
                            && s.contains("focused indented evidence blocks")
                            && s.contains("Prefer call chains and ASCII call graphs")
                            && s.contains("set clean=false")
                    })),
            "fix review must include the antagonistic maintainer lens"
        );
        assert!(
            review.lenses.iter().any(|l| l.id == "assertions"
                && l.run_if.as_deref()
                    == Some("write-patch.review_dispute != '' || commit.commit_sha != ''")
                && l.fields
                    .get("investigate")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| {
                        s.contains("disprove every assertion")
                            && s.contains("existing declarations, kerneldoc, comments, or docs")
                            && s.contains("made stale by the patch")
                            && s.contains("set clean=false")
                    })),
            "fix review must include the commit assertion lens"
        );
        let consolidate = review
            .consolidate
            .as_ref()
            .map(|c| c.prompt.as_str())
            .unwrap_or("");
        assert!(
            consolidate.contains("stale/contradicted/incomplete/misleading declarations")
                && consolidate.contains("clean=false"),
            "fix review consolidator must preserve stale-doc contract defects"
        );
        assert!(
            consolidate.contains("unresolved_risks")
                && consolidate.contains("typed lens fields")
                && consolidate.contains("trust the typed buckets"),
            "fix review consolidator must derive routing from typed fields, not prose"
        );
        let review_prompt = review.prompt.as_deref().unwrap_or("");
        assert!(
            review_prompt.contains("unresolved_risks[]")
                && review_prompt.contains("inconsistent with a"),
            "fix review step prompt must require typed unresolved_risks for concerns that lack proof"
        );
        let review_outputs = &review.outputs;
        assert!(
            review_outputs.contains_key("unresolved_risks"),
            "fix review step must declare unresolved_risks in its output schema"
        );
        assert!(
            consolidate.contains("wall-of-text proof memo")
                && consolidate.contains("commit-message defects"),
            "fix review consolidator must preserve maintainer readability defects"
        );
        let on_fail = &review.eval.as_ref().unwrap().on_fail;
        assert_eq!(on_fail.action, OnFailAction::BranchTo);
        // Review now hands off to the orchestrator step; the
        // orchestrator owns the next-step decision and dispatches
        // based on its own next_step output.
        assert_eq!(on_fail.branch_to.as_deref(), Some("orchestrator"));
        assert_eq!(on_fail.branch_to_output.as_deref(), None);
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{"id": "s", "agent": "fast", "prompt": "do thing"}],
            "wat": "stray"
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(
            err.contains("schema validation failed"),
            "expected schema error, got: {err}"
        );
    }

    #[test]
    fn rejects_bad_agent_value() {
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{"id": "s", "agent": "wizard", "prompt": "do"}]
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(err.contains("schema validation failed"), "got: {err}");
    }

    #[test]
    fn rejects_reaper_step_without_action() {
        // A reaper step must have an `action` block — the schema's
        // conditional `if reaper then required: [action]` guards
        // this. The error message comes from JSON Schema; we only
        // check that validation refuses the file.
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{"id": "s", "agent": "reaper", "prompt": "no action here"}]
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(err.contains("schema validation failed"), "got: {err}");
    }

    #[test]
    fn rejects_reaper_step_with_eval() {
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "reaper",
                "action": {"type": "make", "args": {"target": "all"}},
                "eval": {"type": "field_check", "expr": "true", "on_fail": {"action": "repeat"}}
            }]
        }"#;
        let error = parse_workflow(body).unwrap_err().to_string();
        assert!(error.contains("cannot declare eval"), "got: {error}");
    }

    #[test]
    fn rejects_removed_post_actions_path() {
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "slow",
                "prompt": "p",
                "post_actions": [{"type": "git", "name": "status"}]
            }]
        }"#;
        let error = parse_workflow(body).unwrap_err().to_string();
        assert!(error.contains("schema validation failed"), "got: {error}");
    }

    #[test]
    fn rejects_reserved_step_output_name() {
        // A step declaring an output named `attempt`, `eval_failures`,
        // or `prior_attempts` collides with the interpolation engine,
        // which reads those names from StepState ahead of any
        // declared output. Parse-time refusal keeps the collision
        // loud.
        for reserved in ["attempt", "eval_failures", "prior_attempts"] {
            let body = format!(
                r#"{{
                    "$schema_version": 1,
                    "id": "x",
                    "steps": [{{
                        "id": "s",
                        "agent": "fast",
                        "prompt": "do",
                        "outputs": {{"{reserved}": {{"type": "string"}}}}
                    }}]
                }}"#
            );
            let err = parse_workflow(&body).unwrap_err().to_string();
            assert!(
                err.contains(&format!("declares output '{reserved}'")),
                "expected reserved-output error for {reserved}, got: {err}"
            );
            assert!(err.contains("reserved for the interpolation"), "got: {err}");
        }
    }

    #[test]
    fn rejects_branch_to_without_target() {
        // on_fail.action == "branch_to" requires the branch_to field.
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt": "p",
                "eval": {
                    "type": "field_check",
                    "expr": "true",
                    "on_fail": {"action": "branch_to"}
                }
            }]
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(err.contains("schema validation failed"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_depends_on_id() {
        // Schema-valid but cross-field invalid: depends_on points
        // at a step that doesn't exist.
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt": "p",
                "depends_on": ["ghost"]
            }]
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(
            err.contains("depends_on unknown step 'ghost'"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_step_id() {
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [
                {"id": "s", "agent": "fast", "prompt": "a"},
                {"id": "s", "agent": "fast", "prompt": "b"}
            ]
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(err.contains("duplicate step id: s"), "got: {err}");
    }

    #[test]
    fn rejects_branch_to_unknown_step() {
        // Schema-valid (branch_to has a value) but cross-field
        // invalid: the value doesn't name any step.
        let body = r#"{
            "$schema_version": 1,
            "id": "x",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt": "p",
                "eval": {
                    "type": "field_check",
                    "expr": "true",
                    "on_fail": {"action": "branch_to", "branch_to": "ghost"}
                }
            }]
        }"#;
        let err = parse_workflow(body).unwrap_err().to_string();
        assert!(
            err.contains("branch_to references unknown step 'ghost'"),
            "got: {err}"
        );
    }
}
