//! Shared, string-aware, depth-clamped brace scanner.
//!
//! Used by every "find a JSON object embedded in an agent's
//! response" caller in the workspace (kres-agents response parser,
//! workflow output extractor, goal/todo agent parsers, and the
//! `PLAN:` block parser in `kres_core::plan`). Living here keeps
//! the contract — string-awareness plus depth-clamping — in a
//! single place.

/// Visit every balanced top-level `{...}` substring in `text`,
/// passing the byte offset of the opening `{` and the substring to
/// `visit`. The first `Some(t)` the visitor returns stops the scan
/// and becomes the function's return value. Returns `None` if the
/// visitor never matched.
///
/// String-aware: braces inside double-quoted strings are ignored
/// (with backslash-escape handling), so a JSON value containing
/// `{` / `}` won't desync the scanner.
///
/// Fenced-code-block-aware: triple-backtick fenced blocks get a
/// separate inner depth/start counter that is discarded on fence
/// exit. Without this, a prose code snippet like
/// `// fs/foo.c:bar() {` inside ```c ... ``` (an opening `{` with no
/// matching `}` because the snippet is just the function header)
/// would push the outer depth permanently above zero and the JSON
/// envelope further down would never be visited as a top-level block.
/// A JSON envelope wrapped in ```json ... ``` is still findable —
/// the inner counter sees the balanced `{...}` and the visitor fires
/// as normal inside the fence sub-context. Fence toggling itself is
/// suppressed inside a string literal so a `"foo ``` bar"` string
/// value cannot accidentally flip the state.
///
/// Depth-clamped: when a stray `}` would take depth below zero we
/// clamp it back to zero and drop any in-flight `start`. Without
/// the clamp, a single unbalanced close in prose (very common when
/// the slow agent quotes the end of a C function but elides its
/// opening) desynchronizes the scanner for the rest of the input
/// and the canonical JSON envelope further down becomes invisible.
///
/// The offset lets callers that need to compute an end-of-block
/// position avoid a separate `text.find('{')` — that pre-locate
/// step is not string-aware, so a `{` inside an earlier quoted
/// string would misdirect the scanner. Callers that don't need the
/// offset can use [`first_top_level_brace`].
pub fn first_top_level_brace_with_offset<F, T>(text: &str, mut visit: F) -> Option<T>
where
    F: FnMut(usize, &str) -> Option<T>,
{
    // All scanner-controlling characters (`{`, `}`, `"`, `\`, backtick)
    // are ASCII (< 0x80) and only appear as themselves in valid UTF-8.
    // Byte-level scanning is therefore safe and lets us peek ahead two
    // bytes to detect ``` ``` ``` without char-iterator gymnastics. The
    // visitor receives `text[s..=i]` where `s` is a `{` byte and `i` is
    // a `}` byte — both single-byte chars — so the slice always lands
    // on UTF-8 char boundaries.
    let bytes = text.as_bytes();
    // Two parallel depth/start trackers. The outer pair runs against
    // text outside any fenced code block; the inner pair runs against
    // the body of the current fence and is reset on every fence-toggle
    // so unbalanced prose braces inside one fence cannot leak into
    // either the outer scan or a later fence.
    let mut outer_depth: i32 = 0;
    let mut outer_start: Option<usize> = None;
    let mut inner_depth: i32 = 0;
    let mut inner_start: Option<usize> = None;
    let mut in_fence = false;
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        let b = bytes[i];
        // Fence delimiter: three consecutive backticks toggle in_fence.
        // Suppressed inside a string literal so a JSON string value
        // containing "```" cannot flip state.
        if !in_string
            && b == b'`'
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'`'
            && bytes[i + 2] == b'`'
        {
            in_fence = !in_fence;
            // Drop any in-flight inner-fence state at the boundary so
            // an unbalanced prose brace inside the fence we are leaving
            // (or pre-existing state from a malformed earlier fence)
            // cannot persist into what follows.
            inner_depth = 0;
            inner_start = None;
            i += 3;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        // Choose which (depth, start) pair to update for this byte.
        let (depth, start) = if in_fence {
            (&mut inner_depth, &mut inner_start)
        } else {
            (&mut outer_depth, &mut outer_start)
        };
        match b {
            b'"' => in_string = true,
            b'{' => {
                if *depth == 0 {
                    *start = Some(i);
                }
                *depth += 1;
            }
            b'}' => {
                *depth -= 1;
                if *depth == 0 {
                    if let Some(s) = start.take() {
                        if let Some(t) = visit(s, &text[s..=i]) {
                            return Some(t);
                        }
                    }
                } else if *depth < 0 {
                    *depth = 0;
                    *start = None;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Visit every balanced top-level `{...}` substring in `text`.
/// Thin wrapper around [`first_top_level_brace_with_offset`] for
/// callers that don't need the opening-brace offset.
pub fn first_top_level_brace<F, T>(text: &str, mut visit: F) -> Option<T>
where
    F: FnMut(&str) -> Option<T>,
{
    first_top_level_brace_with_offset(text, |_offset, slice| visit(slice))
}

/// Collect every balanced top-level `{...}` substring in `text`.
/// Convenience wrapper around [`first_top_level_brace`] for the
/// "give me all candidates" callers.
pub fn extract_brace_objects(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let _: Option<()> = first_top_level_brace(text, |slice| {
        out.push(slice.to_string());
        None
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_circuits_on_first_match() {
        let text = "{\"a\":1} prose {\"b\":2} more {\"c\":3}";
        let mut seen: Vec<String> = Vec::new();
        let hit: Option<String> = first_top_level_brace(text, |slice| {
            seen.push(slice.to_string());
            if slice.contains("\"b\"") {
                Some(slice.to_string())
            } else {
                None
            }
        });
        assert_eq!(hit.as_deref(), Some(r#"{"b":2}"#));
        assert_eq!(seen.len(), 2, "saw: {seen:?}");
    }

    #[test]
    fn ignores_braces_inside_strings() {
        let text = r#"prose {"k": "value with }} braces { in it"} tail"#;
        let hit: Option<String> = first_top_level_brace(text, |slice| Some(slice.to_string()));
        assert_eq!(
            hit.as_deref(),
            Some(r#"{"k": "value with }} braces { in it"}"#)
        );
    }

    #[test]
    fn clamps_stray_close() {
        // A stray `}` outside any open object must not corrupt the
        // scanner for the rest of the input — the JSON envelope
        // further down stays findable.
        let text = "```c\nfoo() {} \n}\n```\nResult: {\"verdict\": \"ok\"}";
        let hit: Option<String> = first_top_level_brace(text, |slice| {
            if slice.contains("verdict") {
                Some(slice.to_string())
            } else {
                None
            }
        });
        assert_eq!(hit.as_deref(), Some(r#"{"verdict": "ok"}"#));
    }

    #[test]
    fn extract_brace_objects_collects_all() {
        let text = "{\"a\":1} {\"b\":2}";
        let out = extract_brace_objects(text);
        assert_eq!(out, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn extract_brace_objects_skips_strays() {
        // Lone `}` before the first balanced object must not hide it.
        let text = "}}}\n{\"k\":1}";
        let out = extract_brace_objects(text);
        assert_eq!(out, vec![r#"{"k":1}"#]);
    }

    #[test]
    fn unbalanced_brace_in_fenced_code_does_not_hide_tail_json() {
        // Regression: in linux.bpf_wq_work_prog_map_uaf_window the slow
        // agent annotated quoted comment blocks with a function-header
        // style line like `// kernel/bpf/hashtab.c:free_htab_elem() {`
        // inside a ```c fence with no matching `}`. The pre-fix scanner
        // was string-aware but not fence-aware: each stray `{` pushed
        // outer depth above zero and the canonical JSON envelope at the
        // tail of the response was never visited as a top-level block,
        // so extract_outputs reported "response had no top-level JSON
        // object" and the lens was sent for repair on every retry.
        let text = "Looking at the patch:\n\n\
                    ```c\n\
                    // kernel/bpf/hashtab.c:free_htab_elem() {\n\
                    * Pin the map across the GP.\n\
                    ```\n\n\
                    ```c\n\
                    // kernel/bpf/helpers.c:bpf_wq_cancel_and_free_defer() {\n\
                    * Drop the deferred release.\n\
                    ```\n\n\
                    {\"analysis\": \"all clean\", \"clean\": true, \"defects\": []}";
        let out = extract_brace_objects(text);
        assert_eq!(
            out,
            vec![r#"{"analysis": "all clean", "clean": true, "defects": []}"#],
            "fence-internal unbalanced `{{` must not desync the outer scan"
        );
    }

    #[test]
    fn json_envelope_inside_fenced_block_remains_findable() {
        // Slow agents commonly emit the typed-output envelope inside a
        // ```json fence. The fence-aware scanner runs a fresh
        // depth/start sub-context inside each fence so a balanced
        // `{...}` there is still visited as a top-level block — just
        // with the fence body as its enclosing context rather than the
        // outer text.
        let text = "Prose analysis here.\n\n\
                    ```json\n\
                    {\"analysis\": \"ok\", \"clean\": true, \"defects\": []}\n\
                    ```";
        let out = extract_brace_objects(text);
        assert_eq!(
            out,
            vec![r#"{"analysis": "ok", "clean": true, "defects": []}"#]
        );
    }

    #[test]
    fn fence_toggle_suppressed_inside_string_literal() {
        // A JSON string value containing "```" must not flip fence
        // state — otherwise everything after the string would silently
        // route to the discarded inner scan and a later JSON envelope
        // would vanish.
        let text = "{\"note\": \"see ``` for examples\"} tail {\"k\":1}";
        let out = extract_brace_objects(text);
        assert_eq!(
            out,
            vec![r#"{"note": "see ``` for examples"}"#, r#"{"k":1}"#,]
        );
    }

    #[test]
    fn separate_fences_have_independent_inner_state() {
        // Unbalanced `{` in one fence must not bleed into the next
        // fence's depth counter; closing the first fence drops the
        // in-flight inner state, and the second fence's JSON envelope
        // is visited cleanly.
        let text = "```c\nfoo() {\n```\n```json\n{\"k\":1}\n```";
        let out = extract_brace_objects(text);
        assert_eq!(out, vec![r#"{"k":1}"#]);
    }

    #[test]
    fn offset_variant_returns_opening_brace_position() {
        // Visitor sees the byte offset of `{`, so the caller can
        // compute "start of JSON" without a separate `text.find('{')`
        // (which is not string-aware and would misdirect on a prose
        // preamble containing `{` inside a quoted string).
        let text = r#"some prose "{"} then  {"verdict": "ok"}"#;
        //           0         1         2         3
        //           0123456789012345678901234567890123456789
        let hit: Option<(usize, String)> = first_top_level_brace_with_offset(text, |off, slice| {
            if slice.contains("verdict") {
                Some((off, slice.to_string()))
            } else {
                None
            }
        });
        assert!(hit.is_some(), "expected to find verdict block");
        let (off, slice) = hit.unwrap();
        assert_eq!(slice, r#"{"verdict": "ok"}"#);
        // The opening `{` of the verdict object is past the quoted
        // string. Verify by indexing the input at the reported offset.
        assert_eq!(&text[off..off + 1], "{");
        assert!(
            &text[..off].contains(r#""{"}"#),
            "the prose '{{' inside a quoted string lives before the matched offset"
        );
    }
}
