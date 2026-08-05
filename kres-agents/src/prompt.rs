//! Prompt-JSON builders for fast and slow agents.
//!
//! The builds a JSON envelope with `question`,
//! optional `symbols`, `context`, `skills`, and
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
// previous_findings?, parallel_lenses?,
// lens_instruction?, skills?.
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
    pub previous_findings: Option<&'a [Finding]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_lenses: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens_instruction: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub common_skills: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<&'a Value>,
    /// Compact plan projection produced from `define_plan`. It appears on the
    /// first fast conversation turn and slow synthesis/lens calls; later fast
    /// turns retain it through conversation history rather than serializing it
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<kres_core::PlanPromptView<'a>>,
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
            previous_findings: None,
            parallel_lenses: None,
            lens_instruction: None,
            common_skills: None,
            skills: None,
            plan: None,
            plan_rewrite_allowed: None,
        }
    }

    pub fn with_plan(mut self, plan: &'a kres_core::Plan, active_step_id: Option<&'a str>) -> Self {
        self.plan = Some(plan.prompt_view(active_step_id));
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

    pub fn with_common_skills(mut self, skills: &'a Value) -> Self {
        self.common_skills = Some(skills);
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

    pub fn with_lens_instruction(mut self, instruction: &'a str) -> Self {
        if !instruction.is_empty() {
            self.lens_instruction = Some(instruction);
        }
        self
    }

    pub fn to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Split the envelope into two independently valid JSON documents: a
    /// stable one carrying the fields named in `stable_keys`, and a delta
    /// carrying everything else.
    ///
    /// The stable document goes in a `Message::cached_prefix` block so it can
    /// cache-hit across gather rounds and across every lens in a fan-out,
    /// while the delta changes per call. The two are concatenated on the wire;
    /// the stable document ends in a newline so the pair reads as two
    /// whitespace-separated JSON values, which is what the agents are told to
    /// expect.
    ///
    /// Splitting at the document boundary rather than inside one object is
    /// what makes this safe. The previous approach chopped the closing brace
    /// off the stable half, appended a comma, and stripped the opening brace
    /// from the delta, so neither half was parseable alone, an empty delta
    /// needed a sentinel key to keep the trailing comma legal, and the stable
    /// bytes silently depended on which optional fields serde happened to
    /// emit and in what order.
    ///
    /// When no stable field is present, returns an empty stable document and
    /// the complete prompt as the delta, so the caller sends one block.
    pub fn to_split_documents(&self, stable_keys: &[&str]) -> serde_json::Result<SplitPrompt> {
        let (stable_map, delta_map) = self.split_static_volatile(stable_keys)?;
        if stable_map.is_empty() {
            return Ok(SplitPrompt {
                stable: String::new(),
                delta: serde_json::to_string_pretty(self)?,
            });
        }
        Ok(SplitPrompt {
            stable: stable_document(stable_map)?,
            delta: delta_document(delta_map)?,
        })
    }

    /// Return only the delta document that belongs after a stable document
    /// built from the same `stable_keys`. Used by lens fan-out, where every
    /// call reuses one already-rendered stable document verbatim.
    pub fn to_delta_document(&self, stable_keys: &[&str]) -> serde_json::Result<String> {
        let (stable_map, delta_map) = self.split_static_volatile(stable_keys)?;
        if stable_map.is_empty() {
            // No external stable document can be valid for this prompt shape,
            // so emit a standalone complete prompt.
            return serde_json::to_string_pretty(self);
        }
        delta_document(delta_map)
    }

    fn split_static_volatile(
        &self,
        static_keys: &[&str],
    ) -> serde_json::Result<(
        serde_json::Map<String, Value>,
        serde_json::Map<String, Value>,
    )> {
        use serde_json::{Map, Value};
        let full = serde_json::to_value(self)?;
        let Value::Object(map) = full else {
            return Ok((Map::new(), Map::new()));
        };
        let mut static_map: Map<String, Value> = Map::new();
        let mut volatile_map: Map<String, Value> = Map::new();
        // serde_json without `preserve_order` sorts keys, so both documents
        // are deterministic for a given field set — which is what the cache
        // needs.
        for (k, v) in map {
            if static_keys.contains(&k.as_str()) {
                static_map.insert(k, v);
            } else {
                volatile_map.insert(k, v);
            }
        }
        Ok((static_map, volatile_map))
    }
}

/// A prompt rendered as two concatenable JSON documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPrompt {
    /// Byte-stable across calls that share the same stable field values.
    /// Empty when the prompt has no stable field, in which case `delta` is
    /// the complete prompt.
    pub stable: String,
    /// Per-call fields. `{}` when this call adds nothing, which keeps the
    /// stable bytes unchanged without needing a sentinel key.
    pub delta: String,
}

impl SplitPrompt {
    /// The exact text the model sees. Also what gets logged, so the log and
    /// the wire never disagree.
    pub fn rendered(&self) -> String {
        format!("{}{}", self.stable, self.delta)
    }
}

/// Trailing newline separates this document from the delta that follows it,
/// and is part of the cached bytes so it never shifts.
fn stable_document(map: serde_json::Map<String, Value>) -> serde_json::Result<String> {
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(map))?
    ))
}

fn delta_document(map: serde_json::Map<String, Value>) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&Value::Object(map))
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
    fn compact_plan_does_not_duplicate_scan_from_question() {
        let scan = "WHOLE-FILE RISK SCAN: ranked functions";
        let mut plan = kres_core::Plan::new(
            format!("review file\n{scan}"),
            "audit ranked paths",
            kres_core::TaskMode::Audit,
        );
        plan.steps
            .push(kres_core::PlanStep::new("audit-path", "audit path"));
        let rendered = CodePrompt::new(scan)
            .with_plan(&plan, None)
            .to_json_string()
            .unwrap();
        assert_eq!(rendered.matches("WHOLE-FILE RISK SCAN").count(), 1);
        assert!(!rendered.contains("review file"));
    }

    #[test]
    fn field_order_is_stable() {
        // `question → symbols → context → previous_findings →
        // parallel_lenses → skills`.
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let lenses = json!({"your_lens": {"name": "memory"}});
        let sk = json!({"kernel": "..."});
        let p = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_parallel_lenses(&lenses)
            .with_skills(&sk);
        let s = p.to_json_string().unwrap();
        let q = s.find("\"question\"").unwrap();
        let sy = s.find("\"symbols\"").unwrap();
        let ctxp = s.find("\"context\"").unwrap();
        let plp = s.find("\"parallel_lenses\"").unwrap();
        let skp = s.find("\"skills\"").unwrap();
        assert!(q < sy && sy < ctxp && ctxp < plp && plp < skp);
    }

    /// Parse the concatenated stable+delta text the model receives back into
    /// one merged object, asserting that no key appears in both documents.
    fn merged(split: &SplitPrompt) -> serde_json::Map<String, Value> {
        let mut merged = serde_json::Map::new();
        let docs: Vec<Value> = serde_json::Deserializer::from_str(&split.rendered())
            .into_iter::<Value>()
            .collect::<Result<_, _>>()
            .expect("both halves parse as JSON documents");
        for doc in docs {
            for (key, value) in doc.as_object().expect("each document is an object") {
                assert!(
                    merged.insert(key.clone(), value.clone()).is_none(),
                    "field {key} appeared in both documents"
                );
            }
        }
        merged
    }

    #[test]
    fn split_documents_each_parse_alone_and_merge_to_the_whole_prompt() {
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let sk = json!({"kernel": "skill body"});
        let p = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_skills(&sk);

        let split = p
            .to_split_documents(&["question", "skills"])
            .expect("split");

        // Each half is a complete JSON document on its own — the property the
        // brace-splicing version could not offer.
        let stable: Value = serde_json::from_str(&split.stable).expect("stable parses alone");
        let delta: Value = serde_json::from_str(&split.delta).expect("delta parses alone");
        assert_eq!(stable["question"], "q");
        assert!(stable.get("skills").is_some());
        assert!(stable.get("symbols").is_none());
        assert!(delta.get("symbols").is_some());
        assert!(delta.get("context").is_some());

        // And together they carry every field of the unsplit prompt.
        let whole: Value = serde_json::from_str(&p.to_json_string().unwrap()).unwrap();
        assert_eq!(Value::Object(merged(&split)), whole);
    }

    #[test]
    fn lens_instruction_stays_out_of_shared_cache_prefix() {
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let sk = json!({"kernel": "skill body"});
        let lenses = json!({"your_lens": {"name": "memory"}});
        let shared = CodePrompt::new("shared review prompt")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_skills(&sk);
        let make_prompt = |instruction| {
            CodePrompt::new("shared review prompt")
                .with_symbols(&syms)
                .with_context(&ctx)
                .with_skills(&sk)
                .with_parallel_lenses(&lenses)
                .with_lens_instruction(instruction)
        };
        let p = make_prompt("Apply the memory lens");
        let p2 = make_prompt("Apply the races lens");
        let split_keys = [
            "question",
            "symbols",
            "context",
            "previous_findings",
            "skills",
            "plan",
        ];

        let stable = shared
            .to_split_documents(&split_keys)
            .expect("split")
            .stable;
        let delta = p.to_delta_document(&split_keys).expect("delta");
        let delta2 = p2.to_delta_document(&split_keys).expect("delta");

        // Shared evidence caches once; only the lens identity varies.
        assert!(stable.contains("shared review prompt"));
        assert!(stable.contains("\"symbols\""));
        assert!(stable.contains("\"context\""));
        assert!(stable.contains("\"skills\""));
        assert!(!stable.contains("Apply the memory lens"));
        assert!(!stable.contains("Apply the races lens"));
        assert!(!stable.contains("\"parallel_lenses\""));
        assert!(delta.contains("Apply the memory lens"));
        assert!(delta.contains("\"parallel_lenses\""));
        assert!(delta2.contains("Apply the races lens"));
        assert!(!delta2.contains("Apply the memory lens"));

        let combined = merged(&SplitPrompt {
            stable: stable.clone(),
            delta,
        });
        assert_eq!(combined["lens_instruction"], "Apply the memory lens");
    }

    #[test]
    fn every_lens_in_a_fanout_renders_byte_identical_stable_bytes() {
        // This is the property the whole split exists for: N lenses over one
        // task must produce the same stable document down to the byte, or the
        // prompt cache misses and each lens pays to serialize the shared
        // evidence again. Asserting it on the prompts themselves — not just on
        // the one `shared` value the caller happens to render — catches a
        // stable field that varies per lens.
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let sk = json!({"kernel": "skill body"});
        let lenses = json!({"your_lens": {"name": "memory"}});
        let keys = [
            "question",
            "symbols",
            "context",
            "previous_findings",
            "skills",
        ];
        let make = |instruction| {
            CodePrompt::new("shared review prompt")
                .with_symbols(&syms)
                .with_context(&ctx)
                .with_skills(&sk)
                .with_parallel_lenses(&lenses)
                .with_lens_instruction(instruction)
        };

        let a = make("Apply the memory lens")
            .to_split_documents(&keys)
            .unwrap();
        let b = make("Apply the races lens")
            .to_split_documents(&keys)
            .unwrap();
        let c = make("Apply the locking lens")
            .to_split_documents(&keys)
            .unwrap();

        assert_eq!(a.stable, b.stable);
        assert_eq!(b.stable, c.stable);
        assert_ne!(a.delta, b.delta);
        // The separating newline is part of the cached bytes, so it can never
        // shift the boundary between calls.
        assert!(a.stable.ends_with("}\n"));
    }

    #[test]
    fn delta_document_reuses_an_externally_rendered_stable_document() {
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let sk = json!({"kernel": "skill body"});
        let split_keys = ["question", "symbols", "context", "skills"];
        let shared = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_skills(&sk);
        let lens = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_skills(&sk)
            .with_lens_instruction("lens-specific tail");

        let stable = shared.to_split_documents(&split_keys).unwrap().stable;
        let delta = lens.to_delta_document(&split_keys).unwrap();

        assert!(!stable.contains("lens-specific tail"));
        let combined = merged(&SplitPrompt { stable, delta });
        assert_eq!(combined["question"], "q");
        assert!(combined.get("symbols").is_some());
        assert_eq!(combined["lens_instruction"], "lens-specific tail");
    }

    #[test]
    fn field_order_includes_lens_instruction_before_skills() {
        let syms = vec![json!({"name": "a"})];
        let ctx = vec![json!({"source": "s"})];
        let sk = json!({"kernel": "skill body"});
        let lenses = json!({"your_lens": {"name": "memory"}});
        let p = CodePrompt::new("q")
            .with_symbols(&syms)
            .with_context(&ctx)
            .with_skills(&sk)
            .with_parallel_lenses(&lenses)
            .with_lens_instruction("Apply the memory lens");
        let s = p.to_json_string().unwrap();
        let pl = s.find("\"parallel_lenses\"").unwrap();
        let li = s.find("\"lens_instruction\"").unwrap();
        let skp = s.find("\"skills\"").unwrap();
        assert!(pl < li && li < skp);
    }

    #[test]
    fn split_returns_an_empty_stable_document_when_no_stable_field_is_present() {
        let syms = vec![json!({"name": "a"})];
        let p = CodePrompt::new("q").with_symbols(&syms);

        // "skills" absent → nothing worth caching → caller sends one block.
        let split = p.to_split_documents(&["skills"]).expect("split");

        assert!(split.stable.is_empty());
        let parsed: Value = serde_json::from_str(&split.delta).expect("valid JSON");
        assert_eq!(parsed["question"], "q");
        assert!(parsed.get("symbols").is_some());
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
        let split = slow_cp
            .to_split_documents(&["question", "skills", "parallel_lenses", "previous_findings"])
            .expect("split");
        let parsed = Value::Object(merged(&split));
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
            split.stable.contains("kernel review guide"),
            "skills content must land in the cached stable document"
        );
        assert!(
            split.stable.contains("body-of-technical-patterns"),
            "skills files must land in the cached stable document too"
        );
        assert!(
            !split.delta.contains("kernel review guide"),
            "skills must NOT be in the per-call delta"
        );
    }

    #[test]
    fn stable_document_is_byte_identical_whether_or_not_the_delta_is_empty() {
        // The cache-hit invariant: round 1 of a gather loop has no evidence
        // yet, round 2 does, and the stable bytes must not move between them.
        // Under the old scheme an empty volatile half needed an `_empty_tail`
        // sentinel key to keep the spliced JSON legal; an empty delta document
        // is just `{}`.
        let sk = json!({"kernel": "body"});
        let r1 = CodePrompt::new("q").with_skills(&sk);
        let r1_split = r1
            .to_split_documents(&["question", "skills"])
            .expect("split");

        let syms = vec![json!({"name": "a"})];
        let r2 = CodePrompt::new("q").with_skills(&sk).with_symbols(&syms);
        let r2_split = r2
            .to_split_documents(&["question", "skills"])
            .expect("split");

        assert_eq!(r1_split.stable, r2_split.stable);
        assert_eq!(r1_split.delta, "{}");
        assert!(!r1_split.rendered().contains("_empty_tail"));
        assert!(r2_split.delta.contains("\"symbols\""));
        assert_eq!(merged(&r1_split).len(), 2);
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
