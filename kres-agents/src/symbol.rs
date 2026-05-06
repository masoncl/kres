//! Symbol + context helpers.
//!
//! - `parse_semcode_symbol` — split the textual output of
//!   `find_function` / `find_type` into a structured symbol dict.
//! - `append_symbol` — dedup against an existing symbol list, merging
//!   adjacent file-read ranges.
//! - `append_context` — skip exact-duplicate context entries.
//! - `tool_source` — build a context-dedup source label from a
//!   main-agent action.

use serde_json::{json, Map, Value};

/// Reduce a symbol dict to its identity tuple —
/// `{name, type, filename, line}` (§42). Used to build the
/// `previously_fetched` manifest the fast agent sees on round 2+
/// (§28).
pub fn sym_identity(sym: &Value) -> Value {
    const KEYS: [&str; 4] = ["name", "type", "filename", "line"];
    let mut out = Map::new();
    if let Some(o) = sym.as_object() {
        for k in KEYS {
            if let Some(v) = o.get(k) {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Reduce a context entry to `{source}` only (§42).
pub fn ctx_identity(ctx: &Value) -> Value {
    let src = ctx
        .get("source")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    json!({"source": src})
}

/// Build the `previously_fetched` manifest from the full (older)
/// symbols + context lists. The fast agent gets identity-only
/// pointers for items it already saw — full content for those is
/// suppressed in the next round's delta.
pub fn previously_fetched_manifest(symbols: &[Value], context: &[Value]) -> Value {
    let syms: Vec<Value> = symbols.iter().map(sym_identity).collect();
    let ctxs: Vec<Value> = context.iter().map(ctx_identity).collect();
    json!({
        "symbols": syms,
        "context": ctxs,
    })
}

/// Parse the textual output of semcode's `find_function` /
/// `find_type` into a structured symbol JSON object.
///
/// Returns `None` when the response is missing the critical `name` +
/// `Body:` block pair — callers fall back to emitting a plain
/// "context" entry with the raw output, so slow-agent information
/// isn't lost.
pub fn parse_semcode_symbol(output: &str, tool_name: &str) -> Option<Value> {
    let mut lines: Vec<&str> = output.split('\n').collect();
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

    for i in 0..lines.len() {
        let l = lines[i];
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
        } else if l.starts_with("Body:") {
            let rest: Vec<&str> = lines.drain(i + 1..).collect();
            body = Some(rest.join("\n").trim_end().to_string());
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

/// Key used for exact-tuple dedup of non-range (function/type) symbols.
fn symbol_key(sym: &Value) -> (String, String, i64) {
    (
        sym.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sym.get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sym.get("line").and_then(|v| v.as_i64()).unwrap_or(0),
    )
}

/// Parse the `basename:<start>-<end>` name pattern used by file-read
/// range symbols. Returns `(filename, start, end_exclusive)` or None
/// for function/type symbols (and other shapes).
fn range_info(sym: &Value) -> Option<(String, i64, i64)> {
    let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return None;
    }
    // Pattern: `^[^/:\s]+:(\d+)-(\d+)$`
    let (head, tail) = name.split_once(':')?;
    if head
        .chars()
        .any(|c| matches!(c, '/' | ':') || c.is_whitespace())
    {
        return None;
    }
    let (start_s, end_s) = tail.split_once('-')?;
    let start: i64 = start_s.parse().ok()?;
    let end: i64 = end_s.parse().ok()?;
    let filename = sym.get("filename").and_then(|v| v.as_str())?.to_string();
    if filename.is_empty() {
        return None;
    }
    Some((filename, start, end))
}

/// Concatenate the definitions of two adjacent range symbols (same
/// file, exact `a.end == b.start`). Returns the merged symbol. Caller
/// is responsible for checking adjacency.
fn merge_range_symbols(a: &Value, b: &Value) -> Value {
    let ar = range_info(a).expect("range symbol");
    let br = range_info(b).expect("range symbol");
    let filename = ar.0.clone();
    let start = ar.1;
    let end = br.2;
    let def = format!(
        "{}{}",
        a.get("definition").and_then(|v| v.as_str()).unwrap_or(""),
        b.get("definition").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let base = match filename.rsplit_once('/') {
        Some((_, b)) => b,
        None => &filename,
    };
    let mut merged = a.clone();
    let obj = merged.as_object_mut().expect("symbol is an object");
    obj.insert("name".into(), json!(format!("{base}:{start}-{end}")));
    obj.insert("line".into(), json!(start));
    obj.insert("definition".into(), json!(def));
    merged
}

/// Append a symbol with dedup + range-merge. Returns `true` when the
/// list was modified (a new entry was added or existing entries were
/// merged), `false` when the new symbol was fully covered by existing
/// entries and nothing changed.
pub fn append_symbol(symbols: &mut Vec<Value>, sym: Value) -> bool {
    if !sym.is_object() {
        return false;
    }
    // Non-range symbols use exact-tuple dedup.
    let Some((_, mut curr_start, mut curr_end)) = range_info(&sym) else {
        let key = symbol_key(&sym);
        if symbols.iter().any(|e| symbol_key(e) == key) {
            return false;
        }
        symbols.push(sym);
        return true;
    };
    let new_file = range_info(&sym).unwrap().0;
    let mut current = sym;

    // Iteratively absorb covered-or-adjacent existing entries. Restart
    // after each mutation so chained merges work (1-201 + 201-401 +
    // 401-601 collapses to 1-601).
    let mut changed = true;
    while changed {
        changed = false;
        let mut idx_remove: Option<usize> = None;
        for (i, ex) in symbols.iter().enumerate() {
            let Some((ef, es, ee)) = range_info(ex) else {
                continue;
            };
            if ef != new_file {
                continue;
            }
            // Existing fully covers the new one → drop the new symbol.
            if es <= curr_start && ee >= curr_end {
                return false;
            }
            // New fully covers the existing → drop the existing.
            if curr_start <= es && curr_end >= ee {
                idx_remove = Some(i);
                changed = true;
                break;
            }
            if ee == curr_start {
                let merged = merge_range_symbols(ex, &current);
                current = merged;
                curr_start = es;
                idx_remove = Some(i);
                changed = true;
                break;
            }
            if curr_end == es {
                let merged = merge_range_symbols(&current, ex);
                current = merged;
                curr_end = ee;
                idx_remove = Some(i);
                changed = true;
                break;
            }
        }
        if let Some(i) = idx_remove {
            symbols.remove(i);
        }
    }
    symbols.push(current);
    true
}

/// Append a context entry, dropping exact `(source, content)`
/// duplicates and empty / whitespace-only content.
pub fn append_context(context: &mut Vec<Value>, ctx: Value) -> bool {
    let Some(obj) = ctx.as_object() else {
        return false;
    };
    let content = obj
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        return false;
    }
    let src = obj.get("source").cloned().unwrap_or(Value::Null);
    for existing in context.iter() {
        let exs = existing.get("source").cloned().unwrap_or(Value::Null);
        let exc = existing
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if exs == src && exc == obj.get("content").and_then(|v| v.as_str()).unwrap_or("") {
            return false;
        }
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
    fn append_symbol_merges_adjacent_ranges() {
        let mut syms: Vec<Value> = vec![];
        append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:1-11",
                "filename": "mm/slab.c",
                "line": 1,
                "definition": "A",
            }),
        );
        append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:11-21",
                "filename": "mm/slab.c",
                "line": 11,
                "definition": "B",
            }),
        );
        assert_eq!(syms.len(), 1);
        let merged = &syms[0];
        assert_eq!(merged.get("name").unwrap(), "slab.c:1-21");
        assert_eq!(merged.get("definition").unwrap(), "AB");
    }

    #[test]
    fn append_symbol_drops_covered() {
        let mut syms = vec![json!({
            "name": "slab.c:1-100",
            "filename": "mm/slab.c",
            "line": 1,
            "definition": "big",
        })];
        let added = append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:10-20",
                "filename": "mm/slab.c",
                "line": 10,
                "definition": "small",
            }),
        );
        assert!(!added, "covered symbols should not be appended");
        assert_eq!(syms.len(), 1);
    }

    #[test]
    fn append_symbol_replaces_with_covering() {
        let mut syms = vec![json!({
            "name": "slab.c:10-20",
            "filename": "mm/slab.c",
            "line": 10,
            "definition": "small",
        })];
        let added = append_symbol(
            &mut syms,
            json!({
                "name": "slab.c:1-100",
                "filename": "mm/slab.c",
                "line": 1,
                "definition": "big",
            }),
        );
        assert!(added);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].get("name").unwrap(), "slab.c:1-100");
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
    fn append_context_skips_whitespace_only() {
        let mut ctx: Vec<Value> = vec![];
        assert!(!append_context(
            &mut ctx,
            json!({"source": "grep/x", "content": "   \n"})
        ));
        assert!(ctx.is_empty());
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
