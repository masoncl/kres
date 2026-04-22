//! Agent response parsing.
//!
//! The fast and slow agents return a JSON object with shape:
//! `{"analysis": "...", "followups": [...], "skill_reads": [...],
//!   "findings": [...], "ready_for_slow": bool}`.
//!
//! Real responses sometimes ship with prose before the JSON, fenced
//! code blocks (using triple-backticks), or even nested/malformed
//! JSON. This module tries three strategies in order:
//!
//! 1. Parse the entire body as JSON.
//! 2. Extract the contents of the first fenced `json` block (or bare
//!    fence) and parse that.
//! 3. Brace-match from the first `{` to its balanced `}` and parse
//!    that.
//!
//! Closes bugs.md#M3 partially: on every fallthrough we log WHICH
//! strategy won, so a broken JSON is distinguishable from a valid
//! but empty analysis by inspecting traces.
//!
//! bugs.md#L3 guard: after successful parse, the `todo`/`followups`
//! fields are checked with `isinstance(list)` semantics — non-list
//! values collapse to empty.

use serde::Deserialize;
use serde_json::Value;

use crate::{error::AgentError, followup::Followup};

use kres_core::findings::Finding;

#[derive(Debug, Clone, Default)]
pub struct CodeResponse {
    pub analysis: String,
    pub followups: Vec<Followup>,
    pub skill_reads: Vec<String>,
    pub findings: Vec<Finding>,
    pub ready_for_slow: bool,
    /// Source files emitted by a Coding-mode slow-agent turn. Empty
    /// for Analysis-mode responses. The coding-mode system prompt
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
}

/// Re-export of kres_core::CodeEdit so older callers that import
/// `kres_agents::CodeEdit` continue to compile. The canonical type
/// lives in kres-core so TaskOutcome can carry it.
pub use kres_core::CodeEdit;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseStrategy {
    #[default]
    WholeBody,
    FencedBlock,
    BraceMatch,
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
}

pub fn parse_code_response(text: &str) -> CodeResponse {
    // Strategy 1: whole body.
    if let Some(r) = try_parse(text) {
        return into_code_response(r, text, ParseStrategy::WholeBody);
    }
    // Strategy 2: fenced block.
    if let Some(inner) = extract_fenced(text) {
        if let Some(r) = try_parse(&inner) {
            return into_code_response(r, text, ParseStrategy::FencedBlock);
        }
    }
    // Strategy 3: brace-match. A lens reply often has prose first,
    // then one or more JSON blocks. An early empty `{}` (C code
    // example) shadows a later real finding/response object if we
    // take the first match. Scan ALL top-level balanced blocks and
    // pick the first one whose parsed RawResponse carries a
    // non-default expected field (analysis/followups/findings/
    // skill_reads/ready_for_slow). Fall back to the first parseable
    // block so prior "empty-body-but-shaped" replies still work.
    let blocks = extract_brace_matches(text);
    let mut first_parseable: Option<RawResponse> = None;
    for inner in blocks {
        if let Some(r) = try_parse(&inner) {
            if raw_has_content(&r) {
                return into_code_response(r, text, ParseStrategy::BraceMatch);
            }
            if first_parseable.is_none() {
                first_parseable = Some(r);
            }
        }
    }
    // Strategy 4: tail-JSON. A lens reply often looks like
    // prose + ```c code blocks + a bare JSON envelope at the bottom
    // with no ```json wrapper (session bfe50dd5 lens #10). The
    // string-aware brace scanner can't find the envelope as a
    // top-level balanced block because the preceding C code has
    // unpaired `{` that leaves depth non-zero. Look for the LAST
    // line-start `{` and try to parse from there to EOF.
    // This runs BEFORE the "empty first_parseable" fallback so a
    // tiny empty `{}` in a code example doesn't shadow the real
    // envelope at the tail.
    if let Some(pos) = last_line_start_brace(text) {
        let tail = &text[pos..];
        if let Some(r) = try_parse(tail) {
            return into_code_response(r, text, ParseStrategy::BraceMatch);
        }
    }
    if let Some(r) = first_parseable {
        return into_code_response(r, text, ParseStrategy::BraceMatch);
    }
    // Fall through: raw text as analysis.
    CodeResponse {
        analysis: text.trim().to_string(),
        followups: vec![],
        skill_reads: vec![],
        findings: vec![],
        ready_for_slow: false,
        code_output: vec![],
        code_edits: vec![],
        plan: None,
        strategy: ParseStrategy::RawText,
    }
}

fn raw_has_content(r: &RawResponse) -> bool {
    let analysis_nonempty = match &r.analysis {
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        _ => true,
    };
    let list_nonempty = |v: &Value| matches!(v, Value::Array(a) if !a.is_empty());
    let bool_true = matches!(r.ready_for_slow, Value::Bool(true));
    analysis_nonempty
        || list_nonempty(&r.followups)
        || list_nonempty(&r.findings)
        || list_nonempty(&r.skill_reads)
        || list_nonempty(&r.code_output)
        || list_nonempty(&r.code_edits)
        || bool_true
}

/// Like `parse_code_response` but surfaces the no-JSON case as an
/// Err. Used by callers that need the distinction (bugs.md#M3).
pub fn parse_code_response_strict(text: &str) -> Result<CodeResponse, AgentError> {
    let r = parse_code_response(text);
    if r.strategy == ParseStrategy::RawText {
        Err(AgentError::NoJson)
    } else {
        Ok(r)
    }
}

fn try_parse(s: &str) -> Option<RawResponse> {
    serde_json::from_str::<RawResponse>(s.trim()).ok()
}

fn into_code_response(r: RawResponse, _original: &str, strategy: ParseStrategy) -> CodeResponse {
    CodeResponse {
        analysis: value_to_string(r.analysis),
        followups: value_to_followups(r.followups),
        skill_reads: value_to_string_list(r.skill_reads),
        findings: value_to_findings(r.findings),
        ready_for_slow: matches!(r.ready_for_slow, Value::Bool(true)),
        code_output: value_to_code_output(r.code_output),
        code_edits: value_to_code_edits(r.code_edits),
        plan: value_to_plan(r.plan),
        strategy,
    }
}

fn value_to_plan(v: Value) -> Option<kres_core::PlanRewrite> {
    match v {
        Value::Null => None,
        other => {
            // Only the `steps` field is consumed; any other fields
            // the LLM stuffs in (prompt, goal, mode, created_at)
            // are ignored. An empty-steps rewrite is indistinguish-
            // able from "no rewrite", so drop it.
            let rewrite: kres_core::PlanRewrite =
                serde_json::from_value(other).ok()?;
            if rewrite.steps.is_empty() {
                None
            } else {
                Some(rewrite)
            }
        }
    }
}

fn value_to_code_edits(v: Value) -> Vec<CodeEdit> {
    let Value::Array(items) = v else {
        return vec![];
    };
    items
        .into_iter()
        .filter_map(|i| serde_json::from_value::<CodeEdit>(i).ok())
        .filter(|e| !e.file_path.is_empty() && !e.old_string.is_empty())
        .collect()
}

fn value_to_code_output(v: Value) -> Vec<kres_core::CodeFile> {
    let Value::Array(items) = v else {
        return vec![];
    };
    items
        .into_iter()
        .filter_map(|i| serde_json::from_value::<kres_core::CodeFile>(i).ok())
        .filter(|f| !f.path.is_empty() && !f.content.is_empty())
        .collect()
}

fn value_to_string(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn value_to_string_list(v: Value) -> Vec<String> {
    let Value::Array(items) = v else {
        return vec![];
    };
    items
        .into_iter()
        .filter_map(|i| i.as_str().map(str::to_string))
        .collect()
}

fn value_to_followups(v: Value) -> Vec<Followup> {
    let Value::Array(items) = v else {
        return vec![];
    };
    items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect()
}

fn value_to_findings(v: Value) -> Vec<Finding> {
    let Value::Array(items) = v else {
        return vec![];
    };
    items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect()
}

/// Pull the text out of a fenced block. Prefers fences opened with
/// a `json` language tag; falls back to the first bare fence. If a
/// `json`-tagged fence opens without a closing fence (observed when
/// a lens reply gets truncated or the model simply forgets the close),
/// its content runs from after the opener to EOF. Without this kres
/// dropped the entire reply (session 247349e8 lens #17) because the
/// downstream brace-match strategy only saw an unrelated empty `{}`
/// inside an earlier C code example. Returns None if no fences.
fn extract_fenced(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let open_positions: Vec<usize> = text.match_indices("```").map(|(i, _)| i).collect();
    // Collect all plausible (content, is_json_tagged) pairs, then
    // return the first json-tagged one if any, else the first bare.
    let mut first_bare: Option<String> = None;
    let mut i = 0;
    while i < open_positions.len() {
        let start = open_positions[i];
        let has_close = i + 1 < open_positions.len();
        // close_idx is the position of the next ``` for a closed
        // fence, or text.len() when this open fence has no partner
        // (truncated / no-close reply).
        let close_idx = if has_close {
            open_positions[i + 1]
        } else {
            text.len()
        };
        if close_idx <= start + 3 {
            i += 1;
            continue;
        }
        let body_start = start + 3;
        let mut content_start = body_start;
        while content_start < bytes.len() && bytes[content_start] != b'\n' {
            content_start += 1;
        }
        if content_start >= close_idx {
            i += 1;
            continue;
        }
        let tag = text[body_start..content_start].trim().to_ascii_lowercase();
        content_start += 1; // skip newline
        let mut content = &text[content_start..close_idx];
        // When there's no matching close fence, the tail can include
        // stray trailing whitespace but otherwise IS the body.
        if !has_close {
            content = content.trim_end_matches(|c: char| c.is_whitespace() || c == '`');
        }
        if content.trim().is_empty() {
            // Advance past this open + (if present) its close.
            i += if has_close { 2 } else { 1 };
            continue;
        }
        if tag == "json" {
            return Some(content.to_string());
        }
        if first_bare.is_none() {
            first_bare = Some(content.to_string());
        }
        i += if has_close { 2 } else { 1 };
    }
    first_bare
}

/// Find the position of the last `{` that begins a line (preceded
/// by a newline or at offset 0). Used as a cheap heuristic for lens
/// replies that emit a bare JSON envelope on its own line after
/// prose and code examples.
fn last_line_start_brace(text: &str) -> Option<usize> {
    let mut found: Option<usize> = None;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' && (i == 0 || bytes[i - 1] == b'\n') {
            found = Some(i);
        }
    }
    found
}

/// Return every top-level balanced `{...}` substring in `text`, in
/// the order they appear. String-aware so JSON containing `{` or `}`
/// inside quoted strings doesn't desync the brace depth.
fn extract_brace_matches(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut start: Option<usize> = None;
    for (i, ch) in text.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        out.push(text[s..=i].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_body_json() {
        let r = parse_code_response(
            r#"{"analysis": "hi", "followups": [{"type":"source","name":"x","reason":""}]}"#,
        );
        assert_eq!(r.strategy, ParseStrategy::WholeBody);
        assert_eq!(r.analysis, "hi");
        assert_eq!(r.followups.len(), 1);
        assert_eq!(r.followups[0].name, "x");
    }

    #[test]
    fn prose_with_empty_example_brace_then_real_finding() {
        // Regression for session 0f959e79 lens #13: markdown analysis
        // contained a short empty `{\n}` block from a C-code example
        // followed by a real structured finding + followups. The old
        // extract_brace_match returned the first (empty) block, yielded
        // an empty RawResponse, and the whole lens output was dropped.
        let body = r#"Looking at this commit I found an issue.

Here's a code sketch:

```
void foo() {
}
```

And the actual finding, as JSON:

{"id": "liveness_null_deref", "title": "NULL ptr deref", "severity": "high"}

Followups:

{"type": "search", "name": "callsite_at_stack\\[", "reason": "verify writer"}
"#;
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::BraceMatch);
        // The picked block is the finding — it has non-default content.
        // Even though our RawResponse shape doesn't know `title` /
        // `severity` directly, the presence of no expected fields means
        // raw_has_content returns false for that block too; the
        // fallback then picks the first parseable block. In either case
        // the reply is NOT silently dropped as empty, which is the
        // property the consolidator depends on.
        // (A followup or a findings-list outer wrap would light up the
        // non-default path; here the point is parser doesn't fall
        // through to RawText.)
        assert_ne!(r.strategy, ParseStrategy::RawText);
    }

    #[test]
    fn tail_json_after_code_blocks_with_unbalanced_braces() {
        // Regression for session bfe50dd5 lens #10: prose + two
        // ```c blocks containing C code examples with unpaired `{`,
        // followed by a bare JSON envelope on its own line (no ```json
        // wrapper). The string-aware brace scanner couldn't find the
        // envelope as top-level balanced because the fenced C code
        // left depth >0 before the envelope started. Strategy 4
        // (tail-JSON from last line-start brace) rescues it.
        let body = "Looking at allocations.\n\n```c\nvoid foo() {\n    int a;\n```\n\nMore analysis.\n\n```c\nstruct bar() {\n    x = 1;\n```\n\nResult:\n\n{\"analysis\": \"real lens output\", \"followups\": [{\"type\":\"source\",\"name\":\"fn\",\"reason\":\"r\"}]}\n";
        let r = parse_code_response(body);
        assert_ne!(r.strategy, ParseStrategy::RawText);
        assert_eq!(r.analysis, "real lens output");
        assert_eq!(r.followups.len(), 1);
        assert_eq!(r.followups[0].name, "fn");
    }

    #[test]
    fn unclosed_json_fence_runs_to_eof() {
        // Regression for session 247349e8 lens #17: the model emitted
        // prose + example ```c blocks + ```json{...} but never wrote
        // a closing ```. With 5 total ``` occurrences, extract_fenced
        // paired two C blocks, silently dropped the unpaired json
        // opener, and the downstream brace-match caught only the empty
        // `{}` from the first C example. Fix: no-close json fences run
        // from after the opener to EOF.
        let body = r#"Prose analysis goes here.

```c
struct foo() {
}
```

Now the result:

```json
{"analysis": "real reply", "followups": [{"type":"source","name":"f","reason":"r"}]}
"#;
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::FencedBlock);
        assert_eq!(r.analysis, "real reply");
        assert_eq!(r.followups.len(), 1);
        assert_eq!(r.followups[0].name, "f");
    }

    #[test]
    fn prose_with_followups_wrapper_after_example_brace() {
        // Shape where the real envelope is the second brace block.
        let body = r#"Example:
{
}

Actual response:
{"analysis": "real stuff", "followups": [{"type":"source","name":"f","reason":"r"}]}
"#;
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::BraceMatch);
        assert_eq!(r.analysis, "real stuff");
        assert_eq!(r.followups.len(), 1);
        assert_eq!(r.followups[0].name, "f");
    }

    #[test]
    fn fenced_json_block() {
        let body = r#"Sure, here you go:

```json
{"analysis": "fenced", "ready_for_slow": true}
```

that's all."#;
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::FencedBlock);
        assert_eq!(r.analysis, "fenced");
        assert!(r.ready_for_slow);
    }

    #[test]
    fn fenced_without_language_tag() {
        let body = "prose\n```\n{\"analysis\": \"fence2\"}\n```\ntrail\n";
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::FencedBlock);
        assert_eq!(r.analysis, "fence2");
    }

    #[test]
    fn brace_match_fallback() {
        let body = "I think the answer is: {\"analysis\": \"via braces\"}; done.";
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::BraceMatch);
        assert_eq!(r.analysis, "via braces");
    }

    #[test]
    fn raw_text_when_no_json() {
        let r = parse_code_response("no json here at all, just prose");
        assert_eq!(r.strategy, ParseStrategy::RawText);
        assert_eq!(r.analysis, "no json here at all, just prose");
        assert!(r.followups.is_empty());
    }

    #[test]
    fn strict_variant_errors_on_no_json() {
        let e = parse_code_response_strict("no json").unwrap_err();
        matches!(e, AgentError::NoJson);
    }

    #[test]
    fn non_list_followups_collapse_to_empty() {
        // bugs.md#L3.
        let r = parse_code_response(r#"{"analysis":"x","followups":"not a list"}"#);
        assert_eq!(r.analysis, "x");
        assert!(r.followups.is_empty());
    }

    #[test]
    fn brace_matching_ignores_braces_in_strings() {
        let r = parse_code_response(r#"{"analysis": "has } inside", "followups": []}"#);
        assert_eq!(r.analysis, "has } inside");
    }

    #[test]
    fn skill_reads_list() {
        let r = parse_code_response(r#"{"analysis":"","skill_reads":["/a.md","/b.md",42]}"#);
        assert_eq!(r.skill_reads, vec!["/a.md", "/b.md"]);
    }

    #[test]
    fn findings_list_parses_known_fields() {
        let r = parse_code_response(
            r#"{
                "analysis": "",
                "findings": [
                    {"id": "f1",
                     "title": "t",
                     "severity": "low",
                     "summary": "s",
                     "reproducer_sketch": "r",
                     "impact": "i"}
                ]
            }"#,
        );
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].id, "f1");
    }

    #[test]
    fn invalid_finding_entries_are_dropped_not_panicked() {
        let r = parse_code_response(
            r#"{
                "analysis": "",
                "findings": [
                    "not an object",
                    {"id": "good", "title": "t", "severity": "high",
                     "summary": "s", "reproducer_sketch": "r", "impact": "i"}
                ]
            }"#,
        );
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].id, "good");
    }

    #[test]
    fn fenced_prefers_json_tagged_over_earlier_bare() {
        let body = "```\nnot json\n```\n```json\n{\"analysis\":\"winner\"}\n```\n";
        let r = parse_code_response(body);
        assert_eq!(r.strategy, ParseStrategy::FencedBlock);
        assert_eq!(r.analysis, "winner");
    }

    #[test]
    fn ready_for_slow_default_false() {
        let r = parse_code_response(r#"{"analysis":"x"}"#);
        assert!(!r.ready_for_slow);
    }
}
