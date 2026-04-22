//! Prompt-JSON builders for fast and slow agents.
//!
//! The builds a JSON envelope with `question`,
//! optional `symbols`, `context`, `skills`, `previously_fetched`, and
//! for the slow agent `previous_findings` and optional
//! `parallel_lenses`. Keeping the builder on the Rust side means
//! every invariant (delta shipping, key names, untouched skill
//! handling) is enforced by the type system rather than the prompt
//! template.

use serde::Serialize;
use serde_json::Value;

use kres_core::findings::Finding;

// §41: field order
//question, symbols?, context?,
// previously_fetched?, previous_findings?, parallel_lenses?, skills?.
// Serde preserves declaration order, so keeping the list aligned with
// means prompt-cache hits don't shift between the two runtimes.
#[derive(Debug, Serialize)]
pub struct CodePrompt<'a> {
    pub question: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<&'a [Value]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<&'a [Value]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previously_fetched: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_findings: Option<&'a [Finding]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_lenses: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<&'a Value>,
    /// Plan produced by `define_plan` for the top-level prompt.
    /// Forwarded to every fast and slow agent turn as a top-level
    /// `plan` field so the agents see the operator-level
    /// decomposition alongside their narrow per-task brief. The
    /// plan is a stable-across-a-task payload, so callers place it
    /// in the cached prefix half of `to_cached_split_json` for
    /// free cache hits on round 2+.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<&'a kres_core::Plan>,
    /// When `Some(true)`, invites the slow agent to return a
    /// top-level `plan` object in its response replacing the
    /// current plan. Set on the first slow call per top-level
    /// prompt (see `RunContext.allow_plan_rewrite`); left out
    /// otherwise. Serialised as a top-level boolean so the
    /// agent can trivially test for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_rewrite_allowed: Option<bool>,
}

impl<'a> CodePrompt<'a> {
    pub fn new(question: &'a str) -> Self {
        Self {
            question,
            symbols: None,
            context: None,
            previously_fetched: None,
            previous_findings: None,
            parallel_lenses: None,
            skills: None,
            plan: None,
            plan_rewrite_allowed: None,
        }
    }

    pub fn with_plan(mut self, plan: &'a kres_core::Plan) -> Self {
        self.plan = Some(plan);
        self
    }

    pub fn with_plan_rewrite_allowed(mut self, allowed: bool) -> Self {
        self.plan_rewrite_allowed = Some(allowed);
        self
    }

    pub fn with_symbols(mut self, symbols: &'a [Value]) -> Self {
        if !symbols.is_empty() {
            self.symbols = Some(symbols);
        }
        self
    }

    pub fn with_context(mut self, context: &'a [Value]) -> Self {
        if !context.is_empty() {
            self.context = Some(context);
        }
        self
    }

    pub fn with_skills(mut self, skills: &'a Value) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn with_previously_fetched(mut self, pf: &'a Value) -> Self {
        self.previously_fetched = Some(pf);
        self
    }

    pub fn with_previous_findings(mut self, findings: &'a [Finding]) -> Self {
        if !findings.is_empty() {
            self.previous_findings = Some(findings);
        }
        self
    }

    pub fn with_parallel_lenses(mut self, pl: &'a Value) -> Self {
        self.parallel_lenses = Some(pl);
        self
    }

    pub fn to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Split the envelope into a stable prefix + a volatile suffix
    /// whose concatenation is the same JSON `to_json_string` would
    /// produce. The prefix carries only the fields named in
    /// `static_keys` (typically `question`, `skills`,
    /// `parallel_lenses`, `previous_findings`); the suffix carries
    /// everything else (`symbols`, `context`, `previously_fetched`).
    ///
    /// The prefix is designed to be dropped into a `Message` as a
    /// `cached_prefix` block so it can cache-hit across rounds of a
    /// gather loop where only the volatile half changes.
    ///
    /// When either side is empty the function returns
    /// `(String::new(), full_json)` so the caller can fall back to
    /// a single-block send.
    pub fn to_cached_split_json(
        &self,
        static_keys: &[&str],
    ) -> serde_json::Result<(String, String)> {
        use serde_json::{Map, Value};
        let full = serde_json::to_value(self)?;
        let Value::Object(map) = full else {
            let s = serde_json::to_string_pretty(self)?;
            return Ok((String::new(), s));
        };
        let mut static_map: Map<String, Value> = Map::new();
        let mut volatile_map: Map<String, Value> = Map::new();
        // Preserve the struct's declaration order by iterating the
        // keys of the full map (which, without serde_json's
        // preserve_order feature, sorts alphabetically — still
        // deterministic, which is what the cache needs).
        for (k, v) in map {
            if static_keys.contains(&k.as_str()) {
                static_map.insert(k, v);
            } else {
                volatile_map.insert(k, v);
            }
        }
        if static_map.is_empty() {
            // Nothing to cache as a stable prefix.
            let whole = serde_json::to_string_pretty(self)?;
            return Ok((String::new(), whole));
        }
        let static_s = serde_json::to_string_pretty(&Value::Object(static_map))?;
        // Prefix is the static object with its closing brace chopped
        // and a trailing comma appended. Byte-identical across rounds
        // iff the static fields are unchanged — the whole point.
        let prefix = {
            let trimmed = static_s.trim_end();
            let body = trimmed.strip_suffix('}').unwrap_or(trimmed).trim_end();
            if body.trim_start_matches('{').trim().is_empty() {
                String::new()
            } else {
                format!("{body},\n")
            }
        };
        if prefix.is_empty() {
            let whole = serde_json::to_string_pretty(self)?;
            return Ok((String::new(), whole));
        }
        // Suffix: every path produces text that, concatenated with
        // `prefix`, parses as a single JSON object. On rounds where
        // volatile is empty we emit a deterministic sentinel so the
        // prefix bytes don't have to change. This costs one extra
        // top-level key ("_empty_tail") in the rendered prompt —
        // trivially ignorable by the agents and worth the cache hit
        // on round 2+.
        let suffix = if volatile_map.is_empty() {
            String::from("  \"_empty_tail\": true\n}\n")
        } else {
            let volatile_s = serde_json::to_string_pretty(&Value::Object(volatile_map))?;
            let trimmed = volatile_s.trim_start();
            let body = trimmed.strip_prefix('{').unwrap_or(trimmed);
            body.trim_start_matches('\n').to_string()
        };
        Ok((prefix, suffix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omits_absent_fields() {
        let p = CodePrompt::new("hi");
        let s = p.to_json_string().unwrap();
        assert!(s.contains("\"question\""));
        assert!(!s.contains("symbols"));
        assert!(!s.contains("context"));
        assert!(!s.contains("skills"));
        assert!(!s.contains("previously_fetched"));
    }

    #[test]
    fn skips_empty_arrays() {
        let syms: Vec<Value> = vec![];
        let p = CodePrompt::new("hi").with_symbols(&syms);
        let s = p.to_json_string().unwrap();
        assert!(!s.contains("symbols"));
    }

    #[test]
    fn includes_non_empty_symbols() {
        let syms = vec![json!({"name": "x"})];
        let p = CodePrompt::new("hi").with_symbols(&syms);
        let s = p.to_json_string().unwrap();
        assert!(s.contains("\"symbols\""));
        assert!(s.contains("\"name\": \"x\""));
    }

    #[test]
    fn field_order_is_stable() {
        // `question → symbols → context → previously_fetched →
        // previous_findings → parallel_lenses → skills`.
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let pf = json!([]);
        let lenses = json!({"your_lens": {"name": "memory"}});
        let sk = json!({"kernel": "..."});
        let p = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_previously_fetched(&pf)
            .with_parallel_lenses(&lenses)
            .with_skills(&sk);
        let s = p.to_json_string().unwrap();
        let q = s.find("\"question\"").unwrap();
        let sy = s.find("\"symbols\"").unwrap();
        let ctxp = s.find("\"context\"").unwrap();
        let pfp = s.find("\"previously_fetched\"").unwrap();
        let plp = s.find("\"parallel_lenses\"").unwrap();
        let skp = s.find("\"skills\"").unwrap();
        assert!(q < sy && sy < ctxp && ctxp < pfp && pfp < plp && plp < skp);
    }

    #[test]
    fn cached_split_reassembles_to_valid_json() {
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let sk = json!({"kernel": "skill body"});
        let p = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_skills(&sk);
        let (prefix, suffix) = p
            .to_cached_split_json(&["question", "skills"])
            .expect("split");
        assert!(!prefix.is_empty(), "prefix should carry question+skills");
        assert!(!suffix.is_empty());
        let reassembled = format!("{prefix}{suffix}");
        let parsed: serde_json::Value = serde_json::from_str(&reassembled).expect("valid JSON");
        assert_eq!(parsed.get("question").and_then(|v| v.as_str()), Some("q"));
        assert!(parsed.get("skills").is_some());
        assert!(parsed.get("symbols").is_some());
        assert!(parsed.get("context").is_some());
    }

    #[test]
    fn cached_split_returns_empty_prefix_when_no_static_fields() {
        let syms = vec![json!({"name": "a"})];
        let p = CodePrompt::new("q").with_symbols(&syms);
        // "skills" not present → static side empty → prefix is ""
        let (prefix, suffix) = p.to_cached_split_json(&["skills"]).expect("split");
        assert!(prefix.is_empty());
        // Suffix is the whole thing; must parse on its own.
        let _: serde_json::Value = serde_json::from_str(&suffix).expect("valid JSON");
    }

    #[test]
    fn slow_agent_prompt_contains_full_skills_payload() {
        // Mirror what pipeline.rs's slow-agent path builds after
        // the cache fix landed in commit 61386db. Verifies the
        // slow agent receives the ENTIRE skills JSON — both
        // `content` and every file in `files`.
        let skills = json!({
            "kernel": {
                "content": "## kernel review guide\n...some prose...",
                "files": {
                    "/abs/path/technical-patterns.md": "body-of-technical-patterns",
                    "/abs/path/subsystem.md": "body-of-subsystem-index",
                    "/abs/path/networking.md": "body-of-networking-guide",
                }
            }
        });
        let ctx = vec![json!({"source": "git:show HEAD", "content": "diff ..."})];
        let slow_cp = CodePrompt::new("explain the HEAD commit")
            .with_context(&ctx)
            .with_skills(&skills);
        let (prefix, suffix) = slow_cp
            .to_cached_split_json(&["question", "skills", "parallel_lenses", "previous_findings"])
            .expect("split");
        let full = format!("{prefix}{suffix}");
        let parsed: Value = serde_json::from_str(&full).expect("valid JSON");
        // Skills must be present at top level.
        let sk = parsed
            .get("skills")
            .and_then(|v| v.as_object())
            .expect("skills top-level");
        let kernel = sk
            .get("kernel")
            .and_then(|v| v.as_object())
            .expect("kernel skill");
        // Content preserved.
        assert!(kernel
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("kernel review guide"));
        // Every pre-loaded file must survive to the slow agent.
        let files = kernel
            .get("files")
            .and_then(|v| v.as_object())
            .expect("files sub-map");
        assert_eq!(files.len(), 3, "all three files present");
        assert_eq!(
            files
                .get("/abs/path/technical-patterns.md")
                .and_then(|v| v.as_str()),
            Some("body-of-technical-patterns")
        );
        assert_eq!(
            files.get("/abs/path/subsystem.md").and_then(|v| v.as_str()),
            Some("body-of-subsystem-index")
        );
        assert_eq!(
            files
                .get("/abs/path/networking.md")
                .and_then(|v| v.as_str()),
            Some("body-of-networking-guide")
        );
        // Skills must be in the CACHED PREFIX — that's the whole
        // point of the fix. The prefix is the side that can hit
        // Anthropic's prompt cache on subsequent runs.
        assert!(
            prefix.contains("kernel review guide"),
            "skills content must land in the cached prefix"
        );
        assert!(
            prefix.contains("body-of-technical-patterns"),
            "skills files must land in the cached prefix too"
        );
        assert!(
            !suffix.contains("kernel review guide"),
            "skills must NOT be in the volatile suffix"
        );
    }

    #[test]
    fn cached_split_emits_stable_prefix_even_with_empty_volatile() {
        // Key invariant: round 1 of a gather loop (empty volatile)
        // and round 2 (non-empty volatile) must produce the
        // BYTE-IDENTICAL `prefix` so the Anthropic prompt cache can
        // hit on round 2+.
        let sk = json!({"kernel": "body"});
        // Round 1: no symbols/context yet.
        let r1 = CodePrompt::new("q").with_skills(&sk);
        let (r1_prefix, r1_suffix) = r1
            .to_cached_split_json(&["question", "skills"])
            .expect("split");
        // Round 2: some symbols came back.
        let syms = vec![json!({"name": "a"})];
        let r2 = CodePrompt::new("q").with_skills(&sk).with_symbols(&syms);
        let (r2_prefix, r2_suffix) = r2
            .to_cached_split_json(&["question", "skills"])
            .expect("split");
        assert_eq!(r1_prefix, r2_prefix, "prefix must be byte-identical");
        assert!(!r1_prefix.is_empty());
        // Both reassemble to valid JSON.
        let j1: serde_json::Value =
            serde_json::from_str(&format!("{r1_prefix}{r1_suffix}")).expect("r1 json");
        let j2: serde_json::Value =
            serde_json::from_str(&format!("{r2_prefix}{r2_suffix}")).expect("r2 json");
        assert_eq!(j1.get("question").and_then(|v| v.as_str()), Some("q"));
        assert_eq!(
            j2.get("symbols").and_then(|v| v.as_array()).map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn includes_previously_fetched() {
        let pf = json!([{"name":"x","type":"function"}]);
        let p = CodePrompt::new("hi").with_previously_fetched(&pf);
        let s = p.to_json_string().unwrap();
        assert!(s.contains("\"previously_fetched\""));
    }

    #[test]
    fn includes_parallel_lenses() {
        let pl = json!({
            "your_lens": {"type": "investigate", "name": "memory"},
            "other_lenses": []
        });
        let p = CodePrompt::new("hi").with_parallel_lenses(&pl);
        let s = p.to_json_string().unwrap();
        assert!(s.contains("parallel_lenses"));
        assert!(s.contains("your_lens"));
    }
}
