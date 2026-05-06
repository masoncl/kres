use serde_json::Value;

const MAX_SENTENCES: usize = 3;
const MAX_CHARS: usize = 420;

pub fn agent_blurb(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let blurb = json_blurb(&value);
        if !blurb.is_empty() {
            return blurb;
        }
    }
    if let Some(value) = parse_embedded_json(trimmed) {
        let blurb = json_blurb(&value);
        if !blurb.is_empty() {
            return blurb;
        }
    }
    sentence_blurb(trimmed)
}

fn parse_embedded_json(text: &str) -> Option<Value> {
    for fenced in fenced_blocks(text) {
        if let Ok(value) = serde_json::from_str::<Value>(fenced.trim()) {
            return Some(value);
        }
    }
    first_balanced_json(text).and_then(|json| serde_json::from_str::<Value>(json).ok())
}

fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after_open = &rest[open + 3..];
        let body_start = after_open.find('\n').map(|idx| idx + 1).unwrap_or(0);
        let after_lang = &after_open[body_start..];
        let Some(close) = after_lang.find("```") else {
            break;
        };
        out.push(&after_lang[..close]);
        rest = &after_lang[close + 3..];
    }
    out
}

fn first_balanced_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| matches!(b, b'{' | b'['))?;
    let opener = bytes[start];
    let closer = if opener == b'{' { b'}' } else { b']' };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (rel, b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }

        match *b {
            b'"' => in_string = true,
            b if b == opener => depth += 1,
            b if b == closer => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return text.get(start..start + rel + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn json_blurb(value: &Value) -> String {
    let Some(obj) = value.as_object() else {
        return compact_json(value);
    };

    let mut pieces = Vec::new();

    push_string_field(&mut pieces, obj.get("task"), "task", FieldStyle::Raw);
    for (key, label) in [
        ("question", "question"),
        ("query", "query"),
        ("original_prompt", "prompt"),
        ("completed_query", "completed"),
        ("goal", "goal"),
        ("mode", "mode"),
    ] {
        push_string_field(&mut pieces, obj.get(key), label, FieldStyle::Sentences);
    }
    push_string_field(
        &mut pieces,
        obj.get("analysis"),
        "analysis",
        FieldStyle::Sentences,
    );
    push_string_field(
        &mut pieces,
        obj.get("analysis_summary"),
        "analysis",
        FieldStyle::Sentences,
    );

    if let Some(plan) = obj.get("plan").and_then(|v| v.as_object()) {
        if let Some(summary) = summarize_plan(plan) {
            pieces.push(summary);
        }
    }

    summarize_count(&mut pieces, obj.get("symbols"), "symbols");
    summarize_count(&mut pieces, obj.get("context"), "context");
    summarize_count(&mut pieces, obj.get("skills"), "skills");
    summarize_count(
        &mut pieces,
        obj.get("previous_findings"),
        "previous findings",
    );
    summarize_count(&mut pieces, obj.get("current_todo"), "current todo");
    summarize_count(&mut pieces, obj.get("new_followups"), "new followups");
    summarize_count(&mut pieces, obj.get("code_edits"), "code edits");
    summarize_count(&mut pieces, obj.get("code_output"), "code output");
    summarize_count(&mut pieces, obj.get("skill_reads"), "skill reads");

    if let Some(followups) = summarize_followups(obj.get("followups")) {
        pieces.push(followups);
    }
    if let Some(followups) = summarize_followups(obj.get("code_agent_followups")) {
        pieces.push(format!("requested: {followups}"));
    }
    if let Some(findings) = summarize_findings(obj.get("findings")) {
        pieces.push(findings);
    }
    if let Some(todo) = summarize_todo(obj.get("todo")) {
        pieces.push(todo);
    }
    if let Some(missing) = summarize_string_array(obj.get("missing"), "missing") {
        pieces.push(missing);
    }
    if let Some(Value::Bool(ready)) = obj.get("ready_for_slow") {
        pieces.push(format!("ready_for_slow: {ready}"));
    }
    if let Some(Value::Bool(met)) = obj.get("met") {
        pieces.push(format!("met: {met}"));
    }
    if let Some(Value::Bool(clean)) = obj.get("clean") {
        pieces.push(format!("clean: {clean}"));
    }

    if pieces.is_empty() {
        return compact_json(value);
    }
    finalize_pieces(&pieces)
}

#[derive(Clone, Copy)]
enum FieldStyle {
    Raw,
    Sentences,
}

fn push_string_field(
    pieces: &mut Vec<String>,
    value: Option<&Value>,
    label: &str,
    style: FieldStyle,
) {
    let Some(s) = value.and_then(|v| v.as_str()) else {
        return;
    };
    let s = clean_ws(strip_skill_preamble(s));
    if s.is_empty() {
        return;
    }
    let body = match style {
        FieldStyle::Raw => truncate_chars(&s, 120),
        FieldStyle::Sentences => sentence_blurb_with_limit(&s, 1, 180),
    };
    pieces.push(format!("{label}: {body}"));
}

fn strip_skill_preamble(s: &str) -> &str {
    let trimmed = s.trim_start();
    let Some(after_header) = trimmed.strip_prefix("--- SKILLS ---") else {
        return s;
    };

    let mut best: Option<usize> = None;
    for marker in [
        "Step ",
        "Review ",
        "Triage ",
        "Research ",
        "Fix ",
        "COMPILE ",
    ] {
        if let Some(pos) = after_header.find(marker) {
            best = Some(best.map_or(pos, |prev| prev.min(pos)));
        }
    }
    if let Some(pos) = best {
        return after_header[pos..].trim_start();
    }

    for para in after_header.split("\n\n") {
        let p = para.trim_start();
        if p.is_empty()
            || p.starts_with("---")
            || p.starts_with("name:")
            || p.starts_with("description:")
            || p.starts_with("invocation_policy:")
            || p.starts_with('#')
        {
            continue;
        }
        return p;
    }

    s
}

fn summarize_plan(plan: &serde_json::Map<String, Value>) -> Option<String> {
    let steps = plan.get("steps")?.as_array()?;
    let total = steps.len();
    let done = steps
        .iter()
        .filter(|s| {
            s.get("status")
                .and_then(|v| v.as_str())
                .is_some_and(|status| status.eq_ignore_ascii_case("done"))
        })
        .count();
    let current = steps
        .iter()
        .find(|s| {
            !s.get("status")
                .and_then(|v| v.as_str())
                .is_some_and(|status| status.eq_ignore_ascii_case("done"))
        })
        .and_then(|s| s.get("title").and_then(|v| v.as_str()))
        .map(clean_ws)
        .filter(|s| !s.is_empty());

    Some(match current {
        Some(title) => format!(
            "plan: {done}/{total} done; current: {}",
            truncate_chars(&title, 120)
        ),
        None => format!("plan: {done}/{total} done"),
    })
}

fn summarize_count(pieces: &mut Vec<String>, value: Option<&Value>, label: &str) {
    if let Some(items) = value.and_then(|v| v.as_array()) {
        if !items.is_empty() {
            pieces.push(format!("{label}: {}", items.len()));
        }
    }
}

fn summarize_followups(value: Option<&Value>) -> Option<String> {
    let items = value?.as_array()?;
    if items.is_empty() {
        return Some("followups: none".to_string());
    }
    let labels = items
        .iter()
        .take(4)
        .map(|item| {
            let kind = item
                .get("type")
                .or_else(|| item.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let name = item
                .get("name")
                .or_else(|| item.get("path"))
                .or_else(|| item.get("file"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.is_empty() {
                kind.to_string()
            } else {
                format!("{kind}:{}", truncate_chars(&clean_ws(name), 60))
            }
        })
        .collect::<Vec<_>>();
    let tail = if items.len() > labels.len() {
        format!(", +{} more", items.len() - labels.len())
    } else {
        String::new()
    };
    Some(format!(
        "followups: {} ({labels}{tail})",
        items.len(),
        labels = labels.join(", ")
    ))
}

fn summarize_findings(value: Option<&Value>) -> Option<String> {
    let items = value?.as_array()?;
    if items.is_empty() {
        return Some("findings: none".to_string());
    }
    let first = items.first().and_then(|item| {
        item.get("what")
            .or_else(|| item.get("summary"))
            .or_else(|| item.get("title"))
            .and_then(|v| v.as_str())
    });
    Some(match first {
        Some(s) => format!(
            "findings: {}; first: {}",
            items.len(),
            truncate_chars(&sentence_blurb_with_limit(s, 1, 120), 120)
        ),
        None => format!("findings: {}", items.len()),
    })
}

fn summarize_todo(value: Option<&Value>) -> Option<String> {
    let items = value?.as_array()?;
    let mut pending = 0usize;
    let mut done = 0usize;
    let mut blocked = 0usize;
    for item in items {
        match item
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "done" => done += 1,
            "blocked" => blocked += 1,
            _ => pending += 1,
        }
    }
    Some(format!(
        "todo: {} item(s), {done} done, {pending} pending, {blocked} blocked",
        items.len()
    ))
}

fn summarize_string_array(value: Option<&Value>, label: &str) -> Option<String> {
    let items = value?.as_array()?;
    if items.is_empty() {
        return Some(format!("{label}: none"));
    }
    let labels = items
        .iter()
        .filter_map(|v| v.as_str())
        .take(3)
        .map(|s| truncate_chars(&clean_ws(s), 80))
        .collect::<Vec<_>>();
    let tail = if items.len() > labels.len() {
        format!(", +{} more", items.len() - labels.len())
    } else {
        String::new()
    };
    Some(format!("{label}: {}{}", labels.join(", "), tail))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value)
        .map(|s| sentence_blurb_with_limit(&s, 1, MAX_CHARS))
        .unwrap_or_else(|_| "(unprintable json)".to_string())
}

fn finalize_pieces(pieces: &[String]) -> String {
    let mut out = pieces
        .iter()
        .take(MAX_SENTENCES)
        .map(|p| {
            let p = p.trim().trim_end_matches(['.', '!', '?']);
            format!("{p}.")
        })
        .collect::<Vec<_>>()
        .join(" ");
    if out.chars().count() > MAX_CHARS {
        out = truncate_chars(&out, MAX_CHARS);
    }
    out
}

fn sentence_blurb(text: &str) -> String {
    sentence_blurb_with_limit(text, MAX_SENTENCES, MAX_CHARS)
}

fn sentence_blurb_with_limit(text: &str, max_sentences: usize, max_chars: usize) -> String {
    let clean = clean_ws(text);
    if clean.is_empty() {
        return "(empty)".to_string();
    }

    let mut sentence_ends = 0usize;
    let mut end_byte = clean.len();
    for (idx, ch) in clean.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            sentence_ends += 1;
            if sentence_ends >= max_sentences {
                end_byte = idx + ch.len_utf8();
                break;
            }
        }
    }

    truncate_chars(clean[..end_byte].trim(), max_chars)
}

fn clean_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", truncated.trim_end())
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::agent_blurb;

    #[test]
    fn previews_plain_text_as_three_sentences() {
        let text = "One. Two! Three? Four.";
        assert_eq!(agent_blurb(text), "One. Two! Three?");
    }

    #[test]
    fn previews_code_prompt_json_as_blurb() {
        let text = r#"{
          "question": "fix: stack bio leak. Need patch.",
          "symbols": [{"name": "bio_init"}],
          "context": [{}, {}],
          "plan": {"steps": [
            {"title": "Research", "status": "done"},
            {"title": "Write patch", "status": "pending"}
          ]}
        }"#;
        let got = agent_blurb(text);
        assert!(got.contains("question: fix: stack bio leak."));
        assert!(got.contains("plan: 1/2 done; current: Write patch."));
        assert!(got.contains("symbols: 1."));
    }

    #[test]
    fn previews_question_after_skill_preamble() {
        let text = r#"{
          "question": "--- SKILLS ---\n--- SKILL: kernel.md ---\n---\nname: kernel\ndescription: kernel guidance\n---\n\n## ALWAYS READ\n1. Load docs.\n\nStep 3: WRITE COMMIT MESSAGE ONLY.\n\nUse readonly git diff/show/log followups.",
          "skills": [{"name": "kernel.md"}],
          "symbols": [{}, {}],
          "context": [{}]
        }"#;
        let got = agent_blurb(text);
        assert!(
            got.contains("question: Step 3: WRITE COMMIT MESSAGE ONLY."),
            "got: {got}"
        );
        assert!(!got.contains("SKILL: kernel"), "got: {got}");
        assert!(got.contains("symbols: 2."), "got: {got}");
    }

    #[test]
    fn previews_fast_response_json_as_blurb() {
        let text = r#"{
          "analysis": "Need zram source. It may miss bio_uninit.",
          "followups": [
            {"type": "read", "name": "drivers/block/zram/zram_drv.c:1400+80"},
            {"type": "git", "name": "blame drivers/block/zram/zram_drv.c"}
          ],
          "ready_for_slow": false
        }"#;
        let got = agent_blurb(text);
        assert!(got.contains("analysis: Need zram source."));
        assert!(got.contains("followups: 2"));
        assert!(got.contains("ready_for_slow: false."));
    }

    #[test]
    fn previews_fenced_json_as_blurb() {
        let text = r#"Here is the result:

```json
{"analysis": "Patch is clean. No issue.", "findings": []}
```"#;
        let got = agent_blurb(text);
        assert!(got.contains("analysis: Patch is clean."));
        assert!(got.contains("findings: none."));
    }
}
