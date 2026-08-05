//! Symbol + context helpers.
//!
//! - `parse_semcode_symbol` — split the textual output of
//!   `find_function` / `find_type` into a structured symbol dict.
//! - `append_symbol` — dedup against an existing symbol list, merging
//!   adjacent file-read ranges.
//! - `append_context` — skip exact-duplicate context entries.
//! - `tool_source` — build a context-dedup source label from a
//!   main-agent action.

use std::collections::HashSet;

use serde_json::{json, Map, Value};
use uuid::Uuid;

/// Canonical representation decision for semcode function/type output.
/// Successful single-result lookups become one normalized symbol. Ambiguous
/// multi-result output stays raw so no candidate is hidden. Missing or
/// unparseable output stays raw and requests the mandatory local fallback.
#[derive(Debug)]
pub struct SemcodeEvidence {
    pub symbol: Option<Value>,
    pub preserve_raw: bool,
    pub needs_local_fallback: bool,
}

pub fn canonical_semcode_evidence(output: &str, tool_name: &str) -> SemcodeEvidence {
    let header = if tool_name == "find_function" {
        "Function: "
    } else {
        "Type: "
    };
    let candidates = output
        .lines()
        .filter(|line| line.starts_with(header))
        .count();
    let parsed = parse_semcode_symbol(output, tool_name);
    match (candidates, parsed) {
        (1, Some(symbol)) => SemcodeEvidence {
            symbol: Some(symbol),
            preserve_raw: false,
            needs_local_fallback: false,
        },
        (n, _) if n > 1 => SemcodeEvidence {
            symbol: None,
            preserve_raw: true,
            needs_local_fallback: false,
        },
        _ => SemcodeEvidence {
            symbol: None,
            preserve_raw: true,
            needs_local_fallback: true,
        },
    }
}

fn evidence_bytes(value: &Value) -> Vec<u8> {
    let mut identity = value.clone();
    if let Some(object) = identity.as_object_mut() {
        object.remove("evidence_id");
    }
    serde_json::to_vec(&identity).unwrap_or_default()
}

fn with_evidence_id(mut value: Value, prefix: &str) -> Value {
    let encoded = evidence_bytes(&value);
    if let Some(obj) = value.as_object_mut() {
        // Never trust an evidence id supplied by a tool or stale accumulator.
        // It must describe the current exact record, especially after range
        // merging or metadata changes.
        obj.insert(
            "evidence_id".to_string(),
            Value::String(format!(
                "{prefix}-{}",
                Uuid::new_v5(&Uuid::NAMESPACE_OID, &encoded).simple()
            )),
        );
    }
    value
}

/// Attach compact retrieval provenance to a normalized source record. The
/// source body remains represented only once, in `definition`.
pub fn with_retrieval_source(mut symbol: Value, source: impl Into<String>) -> Value {
    if let Some(object) = symbol.as_object_mut() {
        object.insert("retrieval_source".into(), Value::String(source.into()));
    }
    symbol
}

fn split_semcode_body(output: &str) -> Option<&str> {
    for marker in ["Body:\n", "Body:\r\n"] {
        if let Some(start) = output.find(marker) {
            return Some(&output[start + marker.len()..]);
        }
    }
    None
}

/// Append a canonical prompt record without mutating or reordering records
/// already sent in earlier gather rounds. Exact duplicates are ignored.
pub fn append_prompt_evidence(records: &mut Vec<Value>, value: Value) -> bool {
    if records.iter().any(|existing| existing == &value) {
        return false;
    }
    records.push(value);
    true
}

fn canonical_records(records: &[Value], prefix: &str) -> Vec<Value> {
    let mut canonical = Vec::with_capacity(records.len());
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for record in records {
        let record = with_evidence_id(record.clone(), prefix);
        if seen.insert(evidence_bytes(&record)) {
            canonical.push(record);
        }
    }
    canonical
}

/// Canonicalize evidence immediately before it is sent to an agent:
///
/// - add stable structured ids used by diagnostics and dependent steps;
/// - drop exact duplicate records;
/// - preserve gather order so multi-turn inference and final synthesis receive
///   byte-stable canonical evidence.
///
/// Choosing between a normalized symbol and raw semcode text is NOT done here.
/// That decision belongs to the fetcher, which is the only layer that holds the
/// tool name and the untouched tool output together; see
/// [`canonical_semcode_evidence`]. Re-deriving it later would mean guessing the
/// tool from a source label, and a wrong guess could hide a candidate.
pub fn canonicalize_prompt_evidence(
    symbols: &[Value],
    context: &[Value],
) -> (Vec<Value>, Vec<Value>) {
    (
        canonical_records(symbols, "sym"),
        canonical_records(context, "ctx"),
    )
}

/// Parse the textual output of semcode's `find_function` /
/// `find_type` into a structured symbol JSON object.
///
/// Returns `None` when the response is missing the critical `name` +
/// `Body:` block pair — callers fall back to emitting a plain
/// "context" entry with the raw output, so slow-agent information
/// isn't lost.
pub fn parse_semcode_symbol(output: &str, tool_name: &str) -> Option<Value> {
    let exact_body = split_semcode_body(output)?;
    let mut sym_type = if tool_name == "find_function" {
        "function".to_string()
    } else {
        "struct".to_string()
    };
    let mut name: Option<String> = None;
    let mut filename: Option<String> = None;
    let mut line_num: Option<i64> = None;
    let mut body: Option<String> = None;
    let mut calls_count: Option<i64> = None;
    let mut called_by_count: Option<i64> = None;

    for l in output.split('\n') {
        if let Some(rest) = l.strip_prefix("Function: ") {
            let head = rest.split_whitespace().next().unwrap_or("").to_string();
            if !head.is_empty() {
                name = Some(head);
            }
        } else if let Some(rest) = l.strip_prefix("Type: ") {
            let (kind, head) = parse_type_header(rest);
            if !kind.is_empty() {
                sym_type = kind;
            }
            if !head.is_empty() {
                name = Some(head);
            }
        } else if let Some(rest) = l.strip_prefix("File: ") {
            if let Some((file_part, line_part)) = rest.rsplit_once(':') {
                let start_line = line_part
                    .trim()
                    .split_once('-')
                    .map(|(start, _)| start)
                    .unwrap_or_else(|| line_part.trim())
                    .trim();
                if let Ok(n) = start_line.parse::<i64>() {
                    filename = Some(file_part.to_string());
                    line_num = Some(n);
                }
            }
        } else if let Some(rest) = l.strip_prefix("Calls: ") {
            if let Ok(n) = rest.trim().parse::<i64>() {
                calls_count = Some(n);
            }
        } else if let Some(rest) = l.strip_prefix("Called by: ") {
            if let Ok(n) = rest.trim().parse::<i64>() {
                called_by_count = Some(n);
            }
        } else if l.trim_end_matches('\r') == "Body:" {
            body = Some(exact_body.to_string());
            break;
        }
    }

    let (name, body) = match (name, body) {
        (Some(n), Some(b)) if !b.is_empty() => (n, b),
        _ => return None,
    };
    let mut obj = Map::new();
    obj.insert("name".into(), json!(name));
    obj.insert("type".into(), json!(sym_type));
    obj.insert(
        "filename".into(),
        json!(filename.unwrap_or_else(|| "?".into())),
    );
    obj.insert("line".into(), json!(line_num.unwrap_or(0)));
    // The source body is represented exactly once, here. The semcode header
    // lines it was parsed out of are not repeated: `name`, `type`, `filename`,
    // `line`, and the counts below already carry everything they stated.
    obj.insert("definition".into(), json!(body));
    if let Some(c) = calls_count {
        obj.insert("calls_count".into(), json!(c));
    }
    if let Some(c) = called_by_count {
        obj.insert("called_by_count".into(), json!(c));
    }
    Some(Value::Object(obj))
}

fn parse_type_header(rest: &str) -> (String, String) {
    let tokens: Vec<&str> = rest
        .split_whitespace()
        .map(|s| s.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_'))
        .filter(|s| !s.is_empty())
        .collect();
    match tokens.as_slice() {
        ["struct", name, ..] => ("struct".to_string(), (*name).to_string()),
        ["union", name, ..] => ("union".to_string(), (*name).to_string()),
        ["enum", name, ..] => ("enum".to_string(), (*name).to_string()),
        ["typedef", rest @ ..] => (
            "typedef".to_string(),
            rest.last().copied().unwrap_or("").to_string(),
        ),
        [name, ..] => ("struct".to_string(), (*name).to_string()),
        [] => (String::new(), String::new()),
    }
}

/// Parse the `basename:<start>-<end>` name a file-read range symbol carries.
/// Returns `(filename, start, end)`, or `None` for function/type symbols and
/// any other shape.
fn range_info(sym: &Value) -> Option<(&str, i64, i64)> {
    let name = sym.get("name")?.as_str()?;
    let (head, tail) = name.split_once(':')?;
    if head
        .chars()
        .any(|c| matches!(c, '/' | ':') || c.is_whitespace())
    {
        return None;
    }
    let (start, end) = tail.split_once('-')?;
    let filename = sym.get("filename")?.as_str()?;
    if filename.is_empty() {
        return None;
    }
    Some((filename, start.parse().ok()?, end.parse().ok()?))
}

/// True when `outer` is a read of the same file over a line range that
/// contains `inner`'s, AND `inner`'s body is literally present in `outer`'s.
///
/// Both halves are required. Range containment alone is not enough: the two
/// reads may have happened either side of an edit, and a stale body must stay
/// visible rather than be silently represented by a newer one. Requiring the
/// exact substring means the dropped record contributes no byte the kept
/// record does not already carry.
fn range_body_contained(outer: &Value, inner: &Value) -> bool {
    let (Some((outer_file, outer_start, outer_end)), Some((inner_file, inner_start, inner_end))) =
        (range_info(outer), range_info(inner))
    else {
        return false;
    };
    if outer_file != inner_file || outer_start > inner_start || outer_end < inner_end {
        return false;
    }
    let (Some(outer_body), Some(inner_body)) = (
        outer.get("definition").and_then(Value::as_str),
        inner.get("definition").and_then(Value::as_str),
    ) else {
        return false;
    };
    !inner_body.is_empty() && outer_body.contains(inner_body)
}

/// Append a symbol, dropping records whose bytes are already present.
///
/// Exact duplicates go first. Beyond that, overlapping file reads are the one
/// case worth collapsing: a read of lines 1-100 followed by a read of 10-20
/// ships the smaller body twice inside the same request, which is precisely
/// the intra-request duplication this pipeline exists to remove. The
/// containment test above is deliberately strict, so nothing is dropped
/// without proof that the retained record already contains it verbatim.
pub fn append_symbol(symbols: &mut Vec<Value>, sym: Value) -> bool {
    if !sym.is_object() {
        return false;
    }
    if symbols
        .iter()
        .any(|existing| range_body_contained(existing, &sym))
    {
        return false;
    }
    let superseded: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter(|(_, existing)| range_body_contained(&sym, existing))
        .map(|(index, _)| index)
        .collect();
    for index in superseded.into_iter().rev() {
        symbols.remove(index);
    }
    append_prompt_evidence(symbols, sym)
}

/// Append a nonempty context record, dropping exact duplicates only. Empty
/// tool output remains meaningful when paired with its source label: it proves
/// that a search ran and found nothing.
pub fn append_context(context: &mut Vec<Value>, ctx: Value) -> bool {
    let Some(obj) = ctx.as_object() else {
        return false;
    };
    let has_payload = obj.values().any(|value| match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    });
    if !has_payload {
        return false;
    }
    if context.iter().any(|existing| existing == &ctx) {
        return false;
    }
    context.push(ctx);
    true
}

/// Build a context-dedup source label from a main-agent action. Every
/// tool kind gets a stable prefix so repeated calls with the same
/// arguments dedup cleanly inside `append_context`.
pub fn tool_source(action: &Value) -> String {
    let t = action.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    match t {
        "grep" => format!(
            "grep/{}",
            action
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        ),
        "find" => format!(
            "find/{}",
            action
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| action.get("path").and_then(|v| v.as_str()))
                .unwrap_or(".")
        ),
        "read" => {
            let fp = action
                .get("file")
                .and_then(|v| v.as_str())
                .or_else(|| action.get("path").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let line = action
                .get("line")
                .or_else(|| action.get("startLine"))
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into());
            format!("read/{fp}:{line}")
        }
        "mcp" => format!(
            "{}/{}",
            action.get("server").and_then(|v| v.as_str()).unwrap_or("?"),
            action.get("tool").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "git" => format!(
            "git/{}",
            action
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        ),
        other => other.to_string(),
    }
}

/// Route a tool's output into symbols (if parsed into a symbol) or
/// else into context. Every tool output lands somewhere — no silent
/// drops.
pub fn propagate_tool_result(
    output: &str,
    sym: Option<Value>,
    source: &str,
    symbols: &mut Vec<Value>,
    context: &mut Vec<Value>,
) {
    if let Some(s) = sym {
        append_symbol(symbols, s);
    } else {
        append_context(
            context,
            json!({
                "source": source,
                "content": output,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_function_output() {
        let raw = "Function: do_something x\n\
                   File: mm/slab.c:123\n\
                   Calls: 5\n\
                   Called by: 12\n\
                   Body:\n\
                   static int do_something(void) {\n\
                       return 0;\n\
                   }\n";
        let s = parse_semcode_symbol(raw, "find_function").unwrap();
        assert_eq!(s.get("name").unwrap(), "do_something");
        assert_eq!(s.get("type").unwrap(), "function");
        assert_eq!(s.get("filename").unwrap(), "mm/slab.c");
        assert_eq!(s.get("line").unwrap(), 123);
        assert_eq!(s.get("calls_count").unwrap(), 5);
        assert_eq!(s.get("called_by_count").unwrap(), 12);
        assert!(s
            .get("definition")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("do_something"));
    }

    #[test]
    fn parse_function_output_accepts_file_line_range() {
        let raw = "Function: example_init\n\
                   File: drivers/example/example.c:234-282\n\
                   Return Type: void\n\
                   Body:\n\
                   void example_init(struct example *ex) {}\n";
        let s = parse_semcode_symbol(raw, "find_function").unwrap();
        assert_eq!(s.get("name").unwrap(), "example_init");
        assert_eq!(s.get("type").unwrap(), "function");
        assert_eq!(s.get("filename").unwrap(), "drivers/example/example.c");
        assert_eq!(s.get("line").unwrap(), 234);
    }

    #[test]
    fn canonical_semcode_single_result_uses_only_normalized_symbol() {
        let raw = "Function: one\nFile: a.c:1\nBody:\nint one(void) { return 1; }\n";
        let evidence = canonical_semcode_evidence(raw, "find_function");
        assert!(evidence.symbol.is_some());
        assert!(!evidence.preserve_raw);
        assert!(!evidence.needs_local_fallback);
    }

    #[test]
    fn canonical_semcode_multiple_results_preserve_raw_candidates() {
        let raw = "Function: one\nFile: a.c:1\nBody:\nint one(void) {}\nFunction: one\nFile: b.c:2\nBody:\nint one(void) {}\n";
        let evidence = canonical_semcode_evidence(raw, "find_function");
        assert!(evidence.symbol.is_none());
        assert!(evidence.preserve_raw);
        assert!(!evidence.needs_local_fallback);
    }

    #[test]
    fn canonical_semcode_unparseable_result_requires_local_fallback() {
        let evidence = canonical_semcode_evidence("not found", "find_function");
        assert!(evidence.symbol.is_none());
        assert!(evidence.preserve_raw);
        assert!(evidence.needs_local_fallback);
    }

    #[test]
    fn single_result_semcode_is_represented_only_as_a_symbol() {
        // The fetcher, not the prompt layer, decides this: it holds the tool
        // name and the raw output together. A successful single-result parse
        // yields a symbol and no raw context copy.
        let raw = "Function: one\nFile: a.c:1\nBody:\nint one(void) { return 1; }\n";
        let evidence = canonical_semcode_evidence(raw, "find_function");

        assert!(!evidence.preserve_raw, "raw copy would duplicate the body");
        let symbol = evidence.symbol.expect("single result parses");
        // Every byte after the Body: marker survives verbatim, and the header
        // lines it replaced are fully represented by the structured fields.
        assert_eq!(
            symbol["definition"].as_str().unwrap(),
            "int one(void) { return 1; }\n"
        );
        assert_eq!(symbol["name"], "one");
        assert_eq!(symbol["filename"], "a.c");
        assert_eq!(symbol["line"], 1);
        assert!(
            symbol.get("retrieval_preamble").is_none(),
            "the semcode header must not ship a second time"
        );
    }

    #[test]
    fn canonicalize_never_drops_a_distinct_context_record() {
        // Prompt-layer canonicalization assigns ids and collapses exact
        // duplicates. It must not second-guess the fetcher by re-deriving a
        // tool from a source label and discarding a candidate.
        let raw = "Function: one\nFile: a.c:1\nBody:\nreturn 0;\n";
        let symbol = json!({
            "name": "one",
            "type": "function",
            "filename": "a.c",
            "line": 1,
            "definition": "return 0;\n"
        });
        let context = json!({"source":"mcp:source:one","content":raw});

        let (symbols, context) = canonicalize_prompt_evidence(&[symbol], &[context]);

        assert_eq!(symbols.len(), 1);
        assert_eq!(context.len(), 1);
        assert_eq!(context[0]["content"], raw);
    }

    #[test]
    fn canonical_prompt_evidence_keeps_ambiguous_and_local_context() {
        let ambiguous = "Function: one\nFile: a.c:1\nBody:\nint one(void) {}\nFunction: one\nFile: b.c:2\nBody:\nint one(void) {}\n";
        let context = vec![
            json!({"source":"mcp:source:one","content":ambiguous}),
            json!({"source":"grep:one","content":"a.c:1:int one(void)"}),
        ];
        let (_, context) = canonicalize_prompt_evidence(&[], &context);
        assert_eq!(context.len(), 2);
        assert!(context.iter().all(|item| item.get("evidence_id").is_some()));
    }

    #[test]
    fn canonical_prompt_evidence_preserves_gather_order() {
        let symbols = vec![
            json!({"name":"first","filename":"a.c","line":1,"definition":"a"}),
            json!({"name":"second","filename":"b.c","line":2,"definition":"b"}),
        ];
        let (symbols, _) = canonicalize_prompt_evidence(&symbols, &[]);
        assert_eq!(symbols[0]["name"], "first");
        assert_eq!(symbols[1]["name"], "second");
    }

    #[test]
    fn canonical_prompt_evidence_recomputes_untrusted_ids() {
        let original = json!({
            "evidence_id":"forged",
            "name":"one",
            "filename":"a.c",
            "line":1,
            "definition":"a"
        });
        let (symbols, _) = canonicalize_prompt_evidence(&[original], &[]);
        assert_ne!(symbols[0]["evidence_id"], "forged");
    }

    #[test]
    fn parse_type_output() {
        let raw = "Type: struct foo\nFile: include/foo.h:10\nBody:\nstruct foo { int x; };\n";
        let s = parse_semcode_symbol(raw, "find_type").unwrap();
        assert_eq!(s.get("type").unwrap(), "struct");
        assert_eq!(s.get("name").unwrap(), "foo");
    }

    #[test]
    fn parse_type_output_accepts_file_line_range() {
        let raw = "Type: struct bio\n\
                   File: include/linux/bio.h:509-552\n\
                   Body:\n\
                   struct bio { unsigned int bi_opf; };\n";
        let s = parse_semcode_symbol(raw, "find_type").unwrap();
        assert_eq!(s.get("type").unwrap(), "struct");
        assert_eq!(s.get("name").unwrap(), "bio");
        assert_eq!(s.get("filename").unwrap(), "include/linux/bio.h");
        assert_eq!(s.get("line").unwrap(), 509);
    }

    #[test]
    fn parse_type_output_accepts_union_and_typedef_names() {
        let raw = "Type: union bpf_attr\nFile: include/uapi/linux/bpf.h:1317\nBody:\nunion bpf_attr { int x; };\n";
        let s = parse_semcode_symbol(raw, "find_type").unwrap();
        assert_eq!(s.get("type").unwrap(), "union");
        assert_eq!(s.get("name").unwrap(), "bpf_attr");

        let raw = "Type: typedef u64\nFile: include/linux/types.h:1\nBody:\ntypedef __u64 u64;\n";
        let s = parse_semcode_symbol(raw, "find_type").unwrap();
        assert_eq!(s.get("type").unwrap(), "typedef");
        assert_eq!(s.get("name").unwrap(), "u64");
    }

    #[test]
    fn parse_type_output_accepts_enum_and_typedef_struct_headers() {
        let raw = "Type: enum pageflags\nFile: include/linux/page-flags.h:1\nBody:\nenum pageflags { PG_locked };\n";
        let s = parse_semcode_symbol(raw, "find_type").unwrap();
        assert_eq!(s.get("type").unwrap(), "enum");
        assert_eq!(s.get("name").unwrap(), "pageflags");

        let raw = "Type: typedef struct folio_ref\nFile: include/linux/mm_types.h:1\nBody:\ntypedef struct folio_ref folio_ref;\n";
        let s = parse_semcode_symbol(raw, "find_type").unwrap();
        assert_eq!(s.get("type").unwrap(), "typedef");
        assert_eq!(s.get("name").unwrap(), "folio_ref");
    }

    #[test]
    fn parse_missing_body_returns_none() {
        let raw = "Function: foo\nFile: a.c:1\n";
        assert!(parse_semcode_symbol(raw, "find_function").is_none());
    }

    #[test]
    fn append_symbol_keeps_adjacent_ranges_that_share_no_bytes() {
        // Adjacent but non-overlapping: neither body contains the other, so
        // both must survive.
        let mut syms: Vec<Value> = vec![];
        append_symbol(
            &mut syms,
            json!({"name": "slab.c:1-11", "filename": "mm/slab.c", "line": 1, "definition": "A"}),
        );
        append_symbol(
            &mut syms,
            json!({"name": "slab.c:11-21", "filename": "mm/slab.c", "line": 11, "definition": "B"}),
        );
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].get("definition").unwrap(), "A");
        assert_eq!(syms[1].get("definition").unwrap(), "B");
    }

    #[test]
    fn append_symbol_drops_a_range_already_contained_verbatim() {
        let mut syms = vec![json!({
            "name": "slab.c:1-100",
            "filename": "mm/slab.c",
            "line": 1,
            "definition": "line one\nline two\nline three\n",
        })];

        let added = append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:2-3",
                "filename": "mm/slab.c",
                "line": 2,
                "definition": "line two\n",
            }),
        );

        assert!(
            !added,
            "a body already present verbatim must not ship twice"
        );
        assert_eq!(syms.len(), 1);
    }

    #[test]
    fn append_symbol_replaces_a_range_it_contains_verbatim() {
        let mut syms = vec![json!({
            "name": "slab.c:2-3",
            "filename": "mm/slab.c",
            "line": 2,
            "definition": "line two\n",
        })];

        let added = append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:1-100",
                "filename": "mm/slab.c",
                "line": 1,
                "definition": "line one\nline two\nline three\n",
            }),
        );

        assert!(added);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].get("name").unwrap(), "slab.c:1-100");
    }

    #[test]
    fn append_symbol_keeps_a_contained_range_whose_body_differs() {
        // Same file, containing range, but the smaller read does not appear in
        // the larger one — the file changed between reads. Dropping either
        // would hide evidence, so both stay.
        let mut syms = vec![json!({
            "name": "slab.c:1-100",
            "filename": "mm/slab.c",
            "line": 1,
            "definition": "after the edit\n",
        })];

        let added = append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:2-3",
                "filename": "mm/slab.c",
                "line": 2,
                "definition": "before the edit\n",
            }),
        );

        assert!(added, "a stale body is evidence and must stay visible");
        assert_eq!(syms.len(), 2);
    }

    #[test]
    fn append_symbol_never_collapses_across_files() {
        let mut syms = vec![json!({
            "name": "slab.c:1-100",
            "filename": "mm/slab.c",
            "line": 1,
            "definition": "shared text\n",
        })];

        let added = append_symbol(
            &mut syms,
            json!({
                "name": "slub.c:1-10",
                "filename": "mm/slub.c",
                "line": 1,
                "definition": "shared text\n",
            }),
        );

        assert!(added);
        assert_eq!(syms.len(), 2);
    }

    #[test]
    fn append_symbol_never_collapses_function_symbols() {
        // Function/type records have no range in their name, so containment
        // never applies to them even when one body contains another.
        let mut syms = vec![json!({
            "name": "outer",
            "filename": "a.c",
            "line": 1,
            "definition": "void outer(void) { inner(); }",
        })];

        let added = append_symbol(
            &mut syms,
            json!({"name": "inner", "filename": "a.c", "line": 9, "definition": "inner()"}),
        );

        assert!(added);
        assert_eq!(syms.len(), 2);
    }

    #[test]
    fn append_symbol_dedups_non_range() {
        let mut syms: Vec<Value> = vec![];
        let s = json!({
            "name": "do_something",
            "type": "function",
            "filename": "mm/slab.c",
            "line": 123,
            "definition": "...",
        });
        append_symbol(&mut syms, s.clone());
        let added_again = append_symbol(&mut syms, s);
        assert!(!added_again);
        assert_eq!(syms.len(), 1);
    }

    #[test]
    fn append_context_dedups_exact_matches() {
        let mut ctx: Vec<Value> = vec![];
        let e = json!({"source": "grep/foo", "content": "matched line\n"});
        assert!(append_context(&mut ctx, e.clone()));
        assert!(!append_context(&mut ctx, e));
    }

    #[test]
    fn append_context_preserves_empty_tool_result_with_source() {
        let mut ctx: Vec<Value> = vec![];
        assert!(append_context(
            &mut ctx,
            json!({"source": "grep/x", "content": "   \n"})
        ));
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn append_context_preserves_and_deduplicates_error_only_evidence() {
        let mut context = Vec::new();
        let error = json!({"source":"mcp:source:x","error":"not found"});
        assert!(append_context(&mut context, error.clone()));
        assert!(!append_context(&mut context, error));
        assert_eq!(context.len(), 1);
    }

    #[test]
    fn append_context_preserves_result_envelopes() {
        let mut context = Vec::new();
        let callers = json!({"source":"mcp:callers:foo","result":"foo <- bar"});
        assert!(append_context(&mut context, callers));
        assert_eq!(context.len(), 1);
    }

    #[test]
    fn append_prompt_evidence_is_append_only_and_exact() {
        let mut records = vec![json!({"evidence_id":"sym-a","definition":"a"})];
        assert!(append_prompt_evidence(
            &mut records,
            json!({"evidence_id":"sym-b","definition":"b"})
        ));
        assert!(!append_prompt_evidence(
            &mut records,
            json!({"evidence_id":"sym-b","definition":"b"})
        ));
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn tool_source_covers_each_kind() {
        assert_eq!(
            tool_source(&json!({"type": "grep", "pattern": "foo"})),
            "grep/foo"
        );
        assert_eq!(
            tool_source(&json!({"type": "find", "name": "*.c"})),
            "find/*.c"
        );
        assert_eq!(
            tool_source(&json!({"type": "read", "file": "a.c", "line": 10})),
            "read/a.c:10"
        );
        assert_eq!(
            tool_source(&json!({"type": "mcp", "server": "semcode", "tool": "find_function"})),
            "semcode/find_function"
        );
        assert_eq!(
            tool_source(&json!({"type": "git", "command": "log -1"})),
            "git/log -1"
        );
    }

    #[test]
    fn propagate_tool_result_routes_symbol_vs_context() {
        let mut syms: Vec<Value> = vec![];
        let mut ctx: Vec<Value> = vec![];
        propagate_tool_result(
            "raw",
            Some(json!({"name":"x","type":"function","filename":"a.c","line":1,"definition":"d"})),
            "semcode/find_function",
            &mut syms,
            &mut ctx,
        );
        assert_eq!(syms.len(), 1);
        assert!(ctx.is_empty());
        propagate_tool_result("matched line", None, "grep/pattern", &mut syms, &mut ctx);
        assert_eq!(ctx.len(), 1);
    }
}
