//! Todo-agent: maintains the todo list based on task output.
//!
//! Port of
//!
//! After each task completes, the caller feeds this module:
//!   - the prompt that drove the task (completed_query)
//!   - the task's analysis text (analysis_summary)
//!   - the followups the slow agent produced (new_followups)
//!   - the current todo list
//!   - optional session-wide lenses
//!
//! The module packages that into a JSON request (with
//! REPRIORITIZE + DEDUP + COVERAGE instructions)
//! and sends it through a dedicated todo-agent inference. The response
//! is parsed back into a new todo list with:
//!   - done items the agent dropped preserved (coverage signal)
//!   - missing coverage on done items carried forward
//!   - a programmatic dedup backstop for pending items
//!   - plan-linked pending items the agent forgot are restored
//!
//! On any failure we fall back to a token-overlap dedup that merges
//! the new followups into the existing list — the todo list must
//! never regress because of a flaky API call.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use kres_core::lens::LensSpec;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::todo::{TodoItem, TodoStatus};
use kres_core::UsageTracker;
use kres_llm::{
    client::Client, config::CallConfig, model::ThinkingBudget, request::Message, Model,
};

use crate::error::AgentError;

pub const TODO_INSTRUCTIONS: &str = include_str!("prompts/todo.txt");

/// Config bundle for the todo agent.
#[derive(Clone)]
pub struct TodoClient {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub thinking: Option<ThinkingBudget>,
    pub usage: Option<Arc<UsageTracker>>,
}

fn record_usage(tc: &TodoClient, usage: &kres_llm::request::Usage) {
    if let Some(tracker) = &tc.usage {
        tracker.record(
            "todo",
            tc.model.id.clone(),
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
    }
}

/// Parsed response shape from the todo agent.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodoUpdateResponse {
    todo: Vec<TodoItem>,
    /// Optional rewritten plan the agent wants to substitute. Agents
    /// may emit this when the existing plan no longer matches the
    /// work actually being done (e.g. a step is complete and the
    /// sweep needs a new axis). Absent / null leaves the manager's
    /// current plan in place.
    ///
    /// Wire shape is `{steps: [...]}` (only the steps are mutable);
    /// the caller merges with the existing plan's metadata via
    /// `kres_core::PlanRewrite::apply_to` at the apply site. Parsing
    /// just the steps means a forgotten metadata field cannot
    /// silently drop the rewrite.
    plan: Option<kres_core::PlanRewrite>,
}

/// Combined return value of `update_todo_via_agent*`: the reconciled
/// todo list plus an optional rewritten plan. `plan` is a rewrite
/// (steps-only); the caller applies it against the existing plan.
#[derive(Debug, Clone, Default)]
pub struct TodoUpdate {
    pub todo: Vec<TodoItem>,
    pub plan: Option<kres_core::PlanRewrite>,
}

/// Per-call inputs threaded into the todo agent. Bundles the
/// caller-side context so the public function signatures stay narrow.
pub struct TodoAgentInputs<'a> {
    pub completed_query: &'a str,
    /// Stable id/name carried by the task that just completed. The
    /// model sees human-readable `completed_query`, but Rust uses
    /// this identity to make completion deterministic.
    pub completed_todo_id: Option<&'a str>,
    pub analysis_summary: &'a str,
    pub new_followups: &'a [Value],
    pub current_todo: &'a [TodoItem],
    pub lenses: &'a [LensSpec],
    pub plan: Option<&'a kres_core::Plan>,
}

/// Run the todo agent. Returns an updated todo list plus an
/// optionally-rewritten plan.
pub async fn update_todo_via_agent(
    tc: &TodoClient,
    inputs: TodoAgentInputs<'_>,
) -> Result<TodoUpdate, AgentError> {
    update_todo_via_agent_with_logger(tc, inputs, None, None).await
}

/// Same as `update_todo_via_agent` but also logs the user+assistant
/// turns to the provided TurnLogger's `main.jsonl`
/// Fields of an `update_todo` request that repeat across reaps. The todo list
/// itself and the newly emitted followups are the per-call half.
const UPDATE_TODO_STABLE_FIELDS: &[&str] =
    &["task", "instructions", "plan", "lenses", "completed_query"];

pub async fn update_todo_via_agent_with_logger(
    tc: &TodoClient,
    inputs: TodoAgentInputs<'_>,
    logger: Option<Arc<TurnLogger>>,
    shutdown: Option<kres_core::Shutdown>,
) -> Result<TodoUpdate, AgentError> {
    let TodoAgentInputs {
        completed_query,
        completed_todo_id,
        analysis_summary,
        new_followups,
        current_todo,
        lenses,
        plan,
    } = inputs;
    // --- Prepare inputs ------------------------------------------------
    let mut todo_list = current_todo.to_vec();
    assign_ids(&mut todo_list);
    mark_completed_todo(&mut todo_list, completed_todo_id);
    let current_payload: Vec<Value> = todo_list.iter().map(todo_to_payload).collect();

    let lens_payload: Vec<Value> = lenses
        .iter()
        .map(|l| {
            json!({
                "type": l.kind,
                "name": l.name,
                "reason": l.reason,
            })
        })
        .collect();

    let mut request = serde_json::Map::new();
    request.insert("task".into(), json!("update_todo"));
    request.insert("completed_query".into(), json!(completed_query));
    request.insert("analysis_summary".into(), json!(analysis_summary));
    request.insert("new_followups".into(), json!(new_followups));
    request.insert("current_todo".into(), json!(current_payload));
    if !lens_payload.is_empty() {
        request.insert("lenses".into(), json!(lens_payload));
    }
    // Ship the current plan (if any) so the agent can attach
    // `step_id` to each emitted todo; `build_instructions` flips
    // its plan-linking paragraph on when has_plan is true.
    let has_plan = if let Some(p) = plan {
        if let Ok(v) = serde_json::to_value(p) {
            request.insert("plan".into(), v);
            true
        } else {
            false
        }
    } else {
        false
    };
    request.insert(
        "instructions".into(),
        json!(build_instructions(!lens_payload.is_empty(), has_plan)),
    );
    let split =
        crate::prompt::split_request_documents(&Value::Object(request), UPDATE_TODO_STABLE_FIELDS)?;
    let request_text = split.rendered();

    // --- Send inference ------------------------------------------------
    let mut cfg = CallConfig::defaults_for(tc.model.clone())
        .with_max_tokens(tc.max_tokens)
        .with_stream_label("todo update");
    if let Some(s) = &tc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = tc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    if let Some(thinking) = tc.thinking {
        cfg = cfg.with_thinking(thinking);
    }
    // The per-call half is one-shot (one inference per reap) so it gets no
    // cache marker and pays no write tax. The head repeats on every reap —
    // over one mm/vmscan.c review that was 21KB of instructions plus plan
    // re-sent fresh fourteen times, from only nine distinct plan payloads.
    let messages = vec![Message {
        role: "user".into(),
        content: split.delta.clone(),
        cache: false,
        cached_prefix: (!split.stable.is_empty()).then(|| split.stable.clone()),
    }];
    if let Some(lg) = &logger {
        let request = cfg.request_meta();
        lg.log_main_with_request("user", &request_text, None, None, Some(&request));
    }
    let resp_result = if let Some(shutdown) = shutdown.clone() {
        tokio::select! {
            _ = shutdown.cancelled() => {
                return Ok(TodoUpdate {
                    todo: fallback_dedup(&todo_list, new_followups),
                    plan: None,
                });
            }
            result = tc.client.messages_streaming(&cfg, &messages) => result,
        }
    } else {
        tc.client.messages_streaming(&cfg, &messages).await
    };
    let resp = match resp_result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "todo agent call failed: {e}; falling back");
            return Ok(TodoUpdate {
                todo: fallback_dedup(&todo_list, new_followups),
                plan: None,
            });
        }
    };
    record_usage(tc, &resp.usage);
    let text = extract_text(&resp);
    if let Some(lg) = &logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }
    // --- Parse response ------------------------------------------------
    let initial = parse_todo_update_full(&text);
    let mut parsed_envelope = initial.as_ref().ok().cloned();
    if let Err(errors) = initial {
        let schema = serde_json::to_string(&schemars::schema_for!(TodoUpdateResponse))
            .expect("generated todo schema is serializable");
        if let Ok(repaired) = crate::json_repair::repair_json_response(crate::json_repair::JsonRepairCall {
            client: tc.client.clone(),
            model: tc.model.clone(),
            max_tokens: tc.max_tokens,
            max_input_tokens: tc.max_input_tokens,
            thinking: tc.thinking,
            contract: crate::json_repair::JsonContract {
                name: "todo-update",
                schema: &schema,
                instructions: "Preserve every todo id, status, dependency, reason, coverage field, and plan decision. Correct representation and field types only.",
            },
            rejected_response: &text,
            validation_errors: &errors,
            logger: logger.clone(),
            log_kind: crate::json_repair::RepairLogKind::Main,
            shutdown,
        })
        .await
        {
            record_usage(tc, &repaired.usage);
            let contract = crate::json_repair::JsonObjectContract {
                name: "todo-update",
                fields: &["todo", "plan"],
            };
            if let Ok(response) = contract.accept_repair::<TodoUpdateResponse>(&repaired.text)
            {
                parsed_envelope = Some((response.todo, response.plan));
            } else {
                tracing::warn!(target: "kres_agents", "todo JSON repair failed the strict response contract");
            }
        }
    }
    let (mut parsed, returned_plan) = match parsed_envelope {
        Some((todo, plan)) => (todo, plan),
        None => {
            tracing::warn!(
                target: "kres_agents",
                "todo agent returned no parseable list; falling back"
            );
            return Ok(TodoUpdate {
                todo: fallback_dedup(&todo_list, new_followups),
                plan: None,
            });
        }
    };

    reconcile_agent_identities(&mut parsed, &todo_list, completed_todo_id);

    // --- Reconcile with existing done items ---------------------------
    let (done_from_agent, mut pending_from_agent): (Vec<TodoItem>, Vec<TodoItem>) = parsed
        .into_iter()
        .partition(|t| t.status == TodoStatus::Done);
    let original_done: HashMap<String, TodoItem> = todo_list
        .iter()
        .filter(|t| t.status == TodoStatus::Done)
        .filter(|t| !t.id.is_empty())
        .map(|t| (t.id.clone(), t.clone()))
        .collect();
    let agent_done_ids: HashSet<String> = done_from_agent
        .iter()
        .filter(|t| !t.id.is_empty())
        .map(|t| t.id.clone())
        .collect();
    let preserved: Vec<TodoItem> = original_done
        .iter()
        .filter(|(id, _)| !agent_done_ids.contains(*id))
        .map(|(_, t)| t.clone())
        .collect();

    // Carry forward prior coverage when the agent dropped it.
    let mut done_final = done_from_agent;
    for d in &mut done_final {
        if d.coverage.is_empty() {
            if let Some(orig) = original_done.get(&d.id) {
                if !orig.coverage.is_empty() {
                    d.coverage = orig.coverage.clone();
                }
            }
        }
    }

    // --- Preserve plan-linked pending items the agent dropped --------
    // The reconcile loop above trusts the agent for the pending list:
    // whatever it returns is the new pending state. That breaks when
    // the agent's response is truncated or it just forgets an item —
    // a linked plan step is left orphaned, the rollup never flips it
    // to done, and dependent steps stall. We can't know which case we
    // are in, but we DO know which pending items are load-bearing for
    // the plan: those with a `step_id` pointing at a step that is
    // still alive (not Done/Skipped). Restore those silently.
    //
    // Items without a step_id are operator/agent ad-hoc adds — those
    // we still trust the agent on. Items whose step IS terminal are
    // legitimately stale and should disappear.
    if let Some(plan) = plan {
        let active_step_ids: HashSet<String> = plan
            .steps
            .iter()
            .filter(|s| !s.status.is_terminal())
            .map(|s| s.id.clone())
            .collect();
        let mut agent_emitted_ids: HashSet<String> = HashSet::new();
        for t in done_final.iter().chain(pending_from_agent.iter()) {
            if !t.id.is_empty() {
                agent_emitted_ids.insert(t.id.clone());
            }
        }
        let mut restored: Vec<String> = Vec::new();
        for orig in todo_list.iter() {
            if orig.status != TodoStatus::Pending
                && orig.status != TodoStatus::InProgress
                && orig.status != TodoStatus::Blocked
            {
                continue;
            }
            if orig.id.is_empty() || orig.step_id.is_empty() {
                continue;
            }
            if agent_emitted_ids.contains(&orig.id) {
                continue;
            }
            if !active_step_ids.contains(&orig.step_id) {
                continue;
            }
            // Agent dropped a pending item that's still tied to a
            // live plan step. Restore it as Pending so the work
            // stays visible; the agent gets another chance next
            // round to mark it done or genuinely retire it.
            let mut item = orig.clone();
            item.status = if orig.status == TodoStatus::InProgress {
                TodoStatus::InProgress
            } else {
                TodoStatus::Pending
            };
            restored.push(item.id.clone());
            pending_from_agent.push(item);
        }
        if !restored.is_empty() {
            tracing::info!(
                target: "kres_agents",
                "todo agent dropped {} plan-linked pending item(s); \
                 restored: {}",
                restored.len(),
                restored.join(", ")
            );
        }
    }

    // --- Programmatic dedup backstop for pending items ----------------
    // Two items are duplicates only when they refer to the same code:
    // either both bags lack file-path tokens (pure-prose tasks like
    // "investigate slab corruption") and ≥70% of remaining tokens
    // overlap, OR their path-token sets share at least one path AND
    // overall token overlap ≥70%. Items whose path-token sets are
    // both non-empty and disjoint are NEVER duplicates — they
    // operate on different files. This is what keeps sibling
    // compile-verify-v4 / compile-verify-v6 steps from collapsing
    // into one (their .o paths differ even though the surrounding
    // prose is near-identical).
    let mut ref_entries: Vec<DedupEntry> = Vec::new();
    let mut completed_ids: HashSet<String> = HashSet::new();
    let mut completed_names: HashSet<String> = HashSet::new();
    for d in done_final.iter().chain(preserved.iter()) {
        if !d.id.is_empty() {
            completed_ids.insert(d.id.clone());
        }
        if !d.name.is_empty() {
            completed_names.insert(d.name.to_ascii_lowercase());
        }
        let bag = format!("{} {} {}", d.name, d.reason, d.coverage);
        let entry = DedupEntry::from_bag(d.name.clone(), &bag);
        if !entry.is_empty() {
            ref_entries.push(entry);
        }
    }
    let mut filtered_pending: Vec<TodoItem> = Vec::new();
    let mut dropped: Vec<(String, String)> = Vec::new();
    for p in pending_from_agent.into_iter() {
        if pending_matches_completed_exact(&p, &completed_ids, &completed_names) {
            dropped.push((p.name.clone(), "completed item".to_string()));
            continue;
        }
        let bag = format!("{} {}", p.name, p.reason);
        let entry = DedupEntry::from_bag(p.name.clone(), &bag);
        if entry.is_empty() {
            filtered_pending.push(p);
            continue;
        }
        let mut dup = false;
        for r in &ref_entries {
            if entry.is_duplicate_of(r) {
                dup = true;
                dropped.push((p.name.clone(), r.label.clone()));
                break;
            }
        }
        if !dup {
            ref_entries.push(entry);
            filtered_pending.push(p);
        }
    }

    if !dropped.is_empty() {
        tracing::info!(
            target: "kres_agents",
            "todo agent dedup dropped {} pending item(s): {}",
            dropped.len(),
            dropped
                .iter()
                .take(3)
                .map(|(p, d)| format!("{}≈{}", truncate(p, 40), truncate(d, 40)))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    // Order: done-from-agent, preserved-done, filtered-pending
    let mut result =
        Vec::with_capacity(done_final.len() + preserved.len() + filtered_pending.len());
    result.extend(done_final);
    result.extend(preserved);
    result.extend(filtered_pending);

    // The agent is told to emit `id` for every item but new pending
    // followups it creates this round can come back with an empty id.
    // Without an id, depends_on can't reference the item and the
    // dispatch/resolve loop in cmd_continue / cmd_next /
    // should_auto_continue treats it as nameless. Synthesize stable
    // ids here before returning so every downstream consumer can
    // count on `id` being populated.
    assign_ids(&mut result);

    Ok(TodoUpdate {
        todo: result,
        plan: returned_plan,
    })
}

/// Preserve scheduler-owned identity and execution state across an
/// LLM todo rewrite. The model may reprioritize prose, but it cannot
/// rename existing IDs, detach plan/dependency links, reopen the
/// completed item, or make another currently-running task dispatchable.
fn reconcile_agent_identities(
    parsed: &mut Vec<TodoItem>,
    originals: &[TodoItem],
    completed_todo_id: Option<&str>,
) {
    let by_id: HashMap<&str, &TodoItem> = originals
        .iter()
        .filter(|item| !item.id.is_empty())
        .map(|item| (item.id.as_str(), item))
        .collect();
    let by_name: HashMap<String, &TodoItem> = originals
        .iter()
        .filter(|item| !item.name.is_empty())
        .map(|item| (item.name.to_ascii_lowercase(), item))
        .collect();

    for item in parsed.iter_mut() {
        let original = by_id
            .get(item.id.as_str())
            .copied()
            .or_else(|| by_name.get(&item.name.to_ascii_lowercase()).copied());
        let Some(original) = original else {
            continue;
        };
        item.id.clone_from(&original.id);
        item.step_id.clone_from(&original.step_id);
        item.depends_on.clone_from(&original.depends_on);
        if item.kind.is_empty() {
            item.kind.clone_from(&original.kind);
        }
        let is_completed = completed_todo_id
            .is_some_and(|id| original.id == id || (original.id.is_empty() && original.name == id));
        if is_completed {
            item.status = TodoStatus::Done;
            if item.coverage.is_empty() {
                item.coverage = "completed by the reaped task".to_string();
            }
        } else if original.status == TodoStatus::InProgress {
            item.status = TodoStatus::InProgress;
        }
    }

    // Exact-name duplicates commonly appear once with the stable
    // original id and once with a title-derived id. After identity
    // restoration retain one row, preferring terminal state.
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<TodoItem> = Vec::with_capacity(parsed.len());
    for item in parsed.drain(..) {
        let key = if !item.id.is_empty() {
            format!("id:{}", item.id)
        } else {
            format!("name:{}", item.name.to_ascii_lowercase())
        };
        if let Some(index) = positions.get(&key).copied() {
            if item.status.is_terminal() && !deduped[index].status.is_terminal() {
                deduped[index] = item;
            }
            continue;
        }
        positions.insert(key, deduped.len());
        deduped.push(item);
    }
    *parsed = deduped;
}

fn mark_completed_todo(items: &mut [TodoItem], completed_todo_id: Option<&str>) {
    let Some(completed_id) = completed_todo_id else {
        return;
    };
    if let Some(item) = items
        .iter_mut()
        .find(|item| item.id == completed_id || (item.id.is_empty() && item.name == completed_id))
    {
        item.status = TodoStatus::Done;
        if item.coverage.is_empty() {
            item.coverage = "completed by the reaped task".to_string();
        }
    }
}

fn pending_matches_completed_exact(
    pending: &TodoItem,
    completed_ids: &HashSet<String>,
    completed_names: &HashSet<String>,
) -> bool {
    (!pending.id.is_empty() && completed_ids.contains(&pending.id))
        || (!pending.name.is_empty()
            && completed_names.contains(&pending.name.to_ascii_lowercase()))
}

fn todo_to_payload(t: &TodoItem) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(t.id));
    obj.insert("type".into(), json!(t.kind));
    obj.insert("name".into(), json!(t.name));
    obj.insert("reason".into(), json!(t.reason));
    obj.insert(
        "status".into(),
        json!(match t.status {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "pending",
            TodoStatus::Blocked => "pending",
            TodoStatus::Done => "done",
            TodoStatus::Skipped => "done",
        }),
    );
    obj.insert("depends_on".into(), json!(t.depends_on));
    if !t.coverage.is_empty() {
        obj.insert("coverage".into(), json!(t.coverage));
    }
    if !t.step_id.is_empty() {
        obj.insert("step_id".into(), json!(t.step_id));
    }
    Value::Object(obj)
}

/// Assign a short unique id to every item that doesn't have one.
fn assign_ids(list: &mut [TodoItem]) {
    let mut seen: HashSet<String> = HashSet::new();
    for t in list.iter_mut() {
        if !t.id.is_empty() && !seen.contains(&t.id) {
            seen.insert(t.id.clone());
            continue;
        }
        let base: String = t.name.chars().take(40).collect();
        let mut id = base.clone();
        let mut counter = 2u32;
        while seen.contains(&id) {
            let short: String = base.chars().take(37).collect();
            id = format!("{short}_{counter}");
            counter += 1;
        }
        seen.insert(id.clone());
        t.id = id;
    }
}

/// DEDUP_STOP_TOKENS — common words we don't want skewing the token
/// overlap when deduping todo items.
const DEDUP_STOP_TOKENS: &[&str] = &[
    "this", "from", "into", "when", "what", "which", "same", "each", "also", "then", "than",
    "there", "their", "before", "after", "entry", "entries", "show", "dump", "print", "name",
    "names", "path", "paths", "point", "points", "case", "cases", "call", "calls", "data", "head",
    "tail",
];

/// One side of a dedup comparison: the full token bag plus the
/// subset that looks like a file path (has at least one `.` or
/// `/`). Two entries are duplicates when they describe the same
/// code, which we approximate as: same file paths involved AND
/// ≥70% overall token overlap. Empty path-sets fall back to
/// pure overlap so prose-only tasks ("investigate slab corruption")
/// still dedup correctly.
struct DedupEntry {
    label: String,
    all: HashSet<String>,
    paths: HashSet<String>,
}

impl DedupEntry {
    fn from_bag(label: String, bag: &str) -> Self {
        let all = dedup_tokens(bag);
        let paths: HashSet<String> = all
            .iter()
            .filter(|t| t.contains('/') || t.contains('.'))
            .cloned()
            .collect();
        Self { label, all, paths }
    }

    fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    fn is_duplicate_of(&self, other: &DedupEntry) -> bool {
        // Different file footprints → different work, even when prose
        // matches. Both sides must have paths for the disjoint test
        // to apply; if either side is path-free the heuristic falls
        // back to overlap-only.
        if !self.paths.is_empty() && !other.paths.is_empty() && self.paths.is_disjoint(&other.paths)
        {
            return false;
        }
        let overlap = self.all.intersection(&other.all).count();
        let denom = self.all.len().min(other.all.len());
        denom > 0 && (overlap as f64) / (denom as f64) >= 0.7
    }
}

/// Extract tokens useful for near-duplicate detection of todo items.
/// Lowercased file paths, section refs (§3b), and C-identifier-like
/// substrings of length >= 5.
///
/// The path-extension list covers kernel sources (`.c`/`.h`/`.S`),
/// kernel build artifacts (`.o`/`.ko`/`.a`/`.so`), and the other
/// languages kres analysis touches (`.rs`/`.go`/`.py`/`.md`/`.sh`).
/// Build artifacts MUST be in the list — sibling compile-verify
/// steps name `.o` targets and the path is the only thing that
/// disambiguates them; without it the heuristic sees only the
/// shared prose ("compile cleanly", "stderr", "warnings") and
/// drops the second sibling as a duplicate.
pub fn dedup_tokens(s: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    if s.is_empty() {
        return out;
    }
    let lower = s.to_lowercase();
    // Pass 1: file-path tokens.
    for ext in &[
        ".bpf.c", // longest-match before .c
        ".c", ".h", ".s", ".o", ".ko", ".so", ".a", ".rs", ".go", ".py", ".md", ".sh",
    ] {
        let mut start = 0;
        while let Some(off) = lower[start..].find(ext) {
            let abs = start + off;
            let after = abs + ext.len();
            // Next char must not be alpha-numeric (avoid "foo.cpp" etc).
            let after_ok = lower
                .as_bytes()
                .get(after)
                .map(|c| !(*c as char).is_ascii_alphanumeric())
                .unwrap_or(true);
            if after_ok {
                // Walk back over path-allowed characters.
                let mut p = abs;
                while p > 0 {
                    let c = lower.as_bytes()[p - 1] as char;
                    if c.is_ascii_alphanumeric()
                        || c == '.'
                        || c == '/'
                        || c == '_'
                        || c == '+'
                        || c == '-'
                    {
                        p -= 1;
                    } else {
                        break;
                    }
                }
                if after > p + ext.len() {
                    out.insert(lower[p..after].to_string());
                }
            }
            start = after;
        }
    }
    // Pass 2: section refs like "§3b".
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '§' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                let mut end = j;
                if end < chars.len() && chars[end].is_ascii_lowercase() {
                    end += 1;
                }
                let tok: String = chars[i..end].iter().collect();
                out.insert(tok.to_lowercase());
                i = end;
                continue;
            }
        }
        i += 1;
    }
    // Pass 3: identifiers of length >= 5 that aren't stop words.
    let mut tok = String::new();
    let bytes = lower.as_bytes();
    for &b in bytes {
        let c = b as char;
        if c.is_ascii_alphanumeric() || c == '_' {
            tok.push(c);
        } else {
            flush_tok(&mut tok, &mut out);
        }
    }
    flush_tok(&mut tok, &mut out);
    out
}

fn flush_tok(tok: &mut String, out: &mut HashSet<String>) {
    if tok.len() >= 5
        && !tok
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(true)
        && !DEDUP_STOP_TOKENS.contains(&tok.as_str())
    {
        out.insert(std::mem::take(tok));
    }
    tok.clear();
}

/// Fallback path: token-overlap dedup of new_followups into the
/// existing todo list when the API call fails.
fn fallback_dedup(existing: &[TodoItem], new_followups: &[Value]) -> Vec<TodoItem> {
    let mut out = existing.to_vec();
    let mut existing_tokens: Vec<HashSet<String>> = out
        .iter()
        .map(|t| dedup_tokens(&format!("{} {} {}", t.name, t.reason, t.coverage)))
        .filter(|s| !s.is_empty())
        .collect();
    for fu in new_followups {
        let name = fu.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let reason = fu.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let fu_toks = dedup_tokens(&format!("{name} {reason}"));
        if fu_toks.is_empty() {
            if let Ok(item) = followup_to_todo(fu) {
                out.push(item);
            }
            continue;
        }
        let mut dup = false;
        for etoks in &existing_tokens {
            let overlap = fu_toks.intersection(etoks).count();
            let denom = fu_toks.len().min(etoks.len());
            if denom > 0 && (overlap as f64) / (denom as f64) >= 0.7 {
                dup = true;
                break;
            }
        }
        if !dup {
            if let Ok(item) = followup_to_todo(fu) {
                existing_tokens.push(fu_toks);
                out.push(item);
            }
        }
    }
    out
}

fn followup_to_todo(fu: &Value) -> Result<TodoItem, serde_json::Error> {
    // Followup shape: {type, name, reason, path?}. Map to TodoItem.
    let kind = fu
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("question")
        .to_string();
    let name = fu
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reason = fu
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let step_id = fu
        .get("step_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(TodoItem {
        name,
        kind,
        status: TodoStatus::Pending,
        reason,
        depends_on: Vec::new(),
        coverage: String::new(),
        id: String::new(),
        step_id,
    })
}

fn build_instructions(has_lenses: bool, has_plan: bool) -> String {
    let mut s = String::from(
        "Update the todo list. Return raw, unfenced JSON only:\n\
         {\"todo\": [{\"id\":\"ID\",\"type\":\"T\",\"name\":\"N\",\"reason\":\"R\",\
         \"status\":\"pending|done\",\"coverage\":\"C\",\"depends_on\":[\"ID1\",\"ID2\"],\
         \"step_id\":\"PLAN_STEP_ID_OR_EMPTY\"}]}\n\n",
    );
    if has_plan {
        s.push_str(
            "PLAN LINKAGE — a `plan` field is present with `steps:[{id,\
             title,description}]`. For EVERY todo item you emit in the \
             output (done or pending), set `step_id` to the id of the \
             plan step whose title/description best matches the todo's \
             target. Match on file, symbol, subsystem, or investigation \
             angle — not just keyword overlap. If NO step is a clear \
             fit, set `step_id` to the empty string. Do not invent step \
             ids; only use ids listed under `plan.steps`. Keep any \
             step_id already set on a current_todo item unless the new \
             analysis proves the item was actually executing a \
             different step.\n\n",
        );
        s.push_str(
            "PLAN REEVALUATION — you MAY also return a top-level \
             `plan` field alongside `todo` to rewrite the plan. Do \
             this ONLY when the analysis shows the current plan is \
             materially wrong: a step is too vague to track, a step \
             duplicates the pipeline's automatic lens fan-out and \
             produces no new signal, a new concrete step is needed \
             (e.g. a specific subsystem the prompt's sweep clearly \
             requires but the planner missed), or a step's work is \
             fully subsumed by another. Keep the plan STABLE when it \
             is still serviceable — churning step ids breaks the \
             step_id links on existing todos and wastes tokens.\n\
             Wire shape: `\"plan\": {\"steps\": [...]}`. Emit ONLY \
             the `steps` array. The pipeline keeps the existing \
             plan's `prompt`, `goal`, `mode`, and `created_at` \
             verbatim — you cannot and need not set them.\n\
             When you do rewrite:\n\
             - Prefer KEEPING existing step ids when the step's \
               intent survives (even if title/description change) so \
               the linked todos do not orphan.\n\
             - When a step's MEANING changes, assign a NEW id \
               instead of overloading the old one. The step_id → \
               semantics contract is how this module's todo-linker \
               stays honest; overloading it poisons the link.\n\
             - New ids MUST be kebab-case slugs that describe the \
               work (e.g. `audit-ring-buffer-init`). Never use \
               positional tags like `s1`/`s2`; they get accidentally \
               reassigned when steps reorder.\n\
             - Every step you emit MUST have id, title, and status. \
               Description and todo_ids are optional.\n\
             - After rewriting, set step_id on every emitted todo to \
               an id from the NEW plan — do not reference ids you \
               just removed.\n\
             Omit the `plan` field entirely when no rewrite is \
             warranted — that is the common case.\n\n",
        );
    }
    s.push_str(
        "REPRIORITIZE — every call, not just when new items arrive:\n\
         - Sort all pending items so the one MOST LIKELY to surface a \
         bug OR most advance the investigation sits first. Subsequent \
         positions descend in expected payoff.\n\
         - 'Payoff' means: likelihood of finding an exploitable bug, \
         resolving an open question from a prior analysis, or unblocking \
         many downstream items (a shared dependency).\n",
    );
    if has_lenses {
        s.push_str(
            "- A 'lenses' array is provided — these are the \
             session-wide analytic frames every task's slow agent \
             applies in parallel. Rank each pending item by its \
             payoff across ALL lenses combined, not against just one. \
             Items that feed multiple lenses outrank items that feed \
             only one.\n",
        );
    }
    s.push_str(
        "- Put this reordering in effect by emitting the pending items \
         in the order you want them executed. The scheduler processes \
         them top-down.\n\
         - Tied payoffs: prefer items with fewer dependencies and those \
         that cite files/symbols still cold (not already in any done \
         item's 'coverage').\n\n",
    );
    s.push_str(
        "DEDUP ALGORITHM — run this for EVERY item in new_followups:\n\
         1. From the followup's name+reason, list the target files, \
         symbols, line ranges, and section refs it would cover.\n\
         2. For each done item in current_todo, read its 'coverage' \
         field AND its name+reason. If the followup's targets are a \
         subset of, or heavily overlap (>=50%), what a done item \
         already covered, DROP the followup — do not emit it in the \
         output todo. Do not be clever about 'different angle' — if the \
         files and symbols match, it is a duplicate.\n\
         3. For each pending item in current_todo, apply the same \
         check. If the new followup overlaps, DROP it.\n\
         4. Read the complete 'analysis_summary' to identify which \
         file:line pairs the most recent analysis touched; use them to \
         decide which done-item coverage to update.\n\
         5. Only followups that introduce genuinely new files, \
         symbols, or analysis angles survive.\n\
         Emit the dropped followup ids/names nowhere — just omit them.\n\n",
    );
    s.push_str(
        "COVERAGE FIELD — required on every done item you emit:\n\
         - 1-2 sentences naming the concrete files, symbols, and \
         line ranges the analysis examined for that item, plus the \
         bottom-line finding.\n\
         - For plan steps that trace unchanged callers, callees, \
         callbacks, readers, writers, shared helpers, or old-contract \
         users, status=done requires concrete cited evidence from the \
         analysis: source names, search results, caller/callee lists, \
         or history. Do NOT mark such a step done from a bare statement \
         like 'all paths checked', 'no remaining users', or 'old path \
         unreachable'. Keep it pending or emit a narrow followup when \
         evidence is missing.\n\
         - If a done item already has a non-empty coverage field, \
         keep it verbatim unless the new analysis meaningfully extends \
         what it covered — in which case append one sentence.\n\
         - Do NOT leave coverage empty on done items. Future dedup \
         calls depend on it.\n\n",
    );
    s.push_str(
        "OTHER RULES:\n\
         - Each item gets a short unique id (use the name, shortened)\n\
         - KEEP all done items in the list — they prevent re-adding \
         equivalent work\n\
         - Mark items as done if the analysis addressed them\n\
         - Keep pending items that are still relevant\n\
         - Remove ONLY pending items that are no longer relevant\n\
         - Max 20 pending items (done items don't count toward the limit)\n\
         - PARALLELISM: most items can run in parallel. Only add \
         depends_on when an item truly requires another's results first.\n\
         - FIX-AND-AMEND INVALIDATION: when the analysis shows that \
         code_edits were applied and a commit was amended (the patch \
         changed since the last review), any done todo that reviewed \
         or verified the PRIOR version of the patch is now stale. \
         Re-emit it as a NEW pending item (new id, same step_id) so \
         the amended patch gets a fresh review. This is NOT a new \
         followup — it is a re-creation of a stale done item, so \
         the dedup algorithm does not apply to it. Update \
         depends_on on any downstream item (e.g. publish) to point \
         at the new id. This applies to review, verification, and \
         approval items — not to research or context-gathering \
         items whose results are still valid.",
    );
    s
}

/// Extract both the `todo` array and an optional rewritten `plan`
/// from the todo-agent response. Mirrors `parse_todo_response`'s
/// strict whole-response discipline while preserving the full
/// envelope. Returns `Some((todo, Option<Plan>))` when the response
/// carried a parseable `todo` field; returns `None` when the
/// envelope itself couldn't be parsed (callers fall back to the
/// todo-only parser, which tries harder on malformed replies).
fn parse_todo_update_full(
    text: &str,
) -> Result<(Vec<TodoItem>, Option<kres_core::PlanRewrite>), Vec<String>> {
    let r = crate::json_repair::JsonObjectContract {
        name: "todo-update",
        fields: &["todo", "plan"],
    }
    .parse::<TodoUpdateResponse>(text)?;
    Ok((r.todo, r.plan))
}

/// Extract the `todo` array from one strict JSON response object.
pub fn parse_todo_response(text: &str) -> Result<Vec<TodoItem>, Vec<String>> {
    let r = crate::json_repair::JsonObjectContract {
        name: "todo-update",
        fields: &["todo", "plan"],
    }
    .parse::<TodoUpdateResponse>(text)?;
    Ok(r.todo)
}

fn extract_text(resp: &kres_llm::request::MessagesResponse) -> String {
    let mut out = String::new();
    for block in &resp.content {
        if let kres_llm::request::ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_preserves_completed_id_and_running_siblings() {
        let mut originals = Vec::new();
        for (id, name, step) in [
            ("review-write", "Trace write path", "write"),
            ("review-read", "Trace read path", "read"),
            ("review-final", "Verify shared contracts", "final"),
        ] {
            let mut item = TodoItem::new(name, "review");
            item.id = id.to_string();
            item.step_id = step.to_string();
            item.status = TodoStatus::InProgress;
            originals.push(item);
        }
        mark_completed_todo(&mut originals, Some("review-write"));

        // Mirrors sol4: the model rewrote stable IDs from titles,
        // reopened every item as pending, and emitted a duplicate of
        // the completed task.
        let mut parsed = vec![
            TodoItem {
                id: "Trace write path".into(),
                status: TodoStatus::Pending,
                ..TodoItem::new("Trace write path", "review")
            },
            TodoItem {
                id: "review-write".into(),
                status: TodoStatus::Pending,
                ..TodoItem::new("Trace write path", "review")
            },
            TodoItem {
                id: "Trace read path".into(),
                status: TodoStatus::Pending,
                ..TodoItem::new("Trace read path", "review")
            },
            TodoItem {
                id: "Verify shared contracts".into(),
                status: TodoStatus::Pending,
                ..TodoItem::new("Verify shared contracts", "review")
            },
        ];

        reconcile_agent_identities(&mut parsed, &originals, Some("review-write"));
        assert_eq!(parsed.len(), 3);
        let write = parsed
            .iter()
            .find(|item| item.id == "review-write")
            .unwrap();
        assert_eq!(write.status, TodoStatus::Done);
        assert_eq!(write.step_id, "write");
        for id in ["review-read", "review-final"] {
            let item = parsed.iter().find(|item| item.id == id).unwrap();
            assert_eq!(item.status, TodoStatus::InProgress);
        }
    }

    #[test]
    fn dedup_tokens_catches_paths_and_idents() {
        let toks = dedup_tokens("Check drivers/net/foo.c and scrub_something helper");
        assert!(toks.contains("drivers/net/foo.c"));
        assert!(toks.contains("scrub_something"));
        // Common stopword ruled out.
        assert!(!toks.contains("there"));
    }

    #[test]
    fn dedup_tokens_skips_short_idents_and_stops() {
        let toks = dedup_tokens("the and for abc");
        // None of these are length >= 5 and non-stop.
        assert!(toks.is_empty());
    }

    #[test]
    fn dedup_tokens_extracts_object_files() {
        // Sibling compile-verify steps in the FIX flow name `.o`
        // targets. Without `.o` in the path-extension list the
        // path component is invisible to the heuristic and the
        // second sibling gets dropped as a prose-overlap dup.
        let v4 = dedup_tokens("-j$(nproc) net/ipv4/tcp_ipv4.o");
        let v6 = dedup_tokens("-j$(nproc) net/ipv6/tcp_ipv6.o");
        assert!(
            v4.contains("net/ipv4/tcp_ipv4.o"),
            "v4 path missing: {v4:?}"
        );
        assert!(
            v6.contains("net/ipv6/tcp_ipv6.o"),
            "v6 path missing: {v6:?}"
        );
        assert!(
            v4.is_disjoint(&HashSet::from(["net/ipv6/tcp_ipv6.o".to_string()])),
            "v4 should not contain v6 path"
        );
    }

    #[test]
    fn dedup_entry_keeps_v4_v6_siblings_distinct() {
        // Regression for the FIX-flow stall: compile-verify-v4 and
        // compile-verify-v6 share most prose tokens but the .o path
        // disambiguates them. The dedup heuristic must treat
        // disjoint file footprints as distinct work, otherwise v6
        // gets silently dropped and downstream depends_on breaks.
        let v4 = DedupEntry::from_bag(
            "compile-verify-v4".into(),
            concat!(
                "-j$(nproc) net/ipv4/tcp_ipv4.o ",
                "Verify v4 patch compiles cleanly; capture stderr for ",
                "new warnings or errors introduced by the inet_twsk_put ",
                "fix in tcp_ipv4.c."
            ),
        );
        let v6 = DedupEntry::from_bag(
            "compile-verify-v6".into(),
            concat!(
                "-j$(nproc) net/ipv6/tcp_ipv6.o ",
                "Verify v6 patch compiles cleanly; capture stderr for ",
                "new warnings or errors introduced by the inet_twsk_put ",
                "fix in tcp_ipv6.c."
            ),
        );
        assert!(!v6.is_duplicate_of(&v4), "v6 incorrectly marked dup of v4");
        assert!(!v4.is_duplicate_of(&v6), "v4 incorrectly marked dup of v6");
    }

    #[test]
    fn dedup_entry_still_catches_prose_only_duplicates() {
        // A pure-prose task with no path tokens: heuristic must still
        // collapse near-restatements so the agent's accidental
        // re-adds get filtered.
        let a = DedupEntry::from_bag(
            "investigate slab corruption".into(),
            "investigate slab corruption in scrub_something helper",
        );
        let b = DedupEntry::from_bag(
            "look at slab corruption".into(),
            "investigate slab corruption helper scrub_something",
        );
        assert!(b.is_duplicate_of(&a));
    }

    #[test]
    fn dedup_entry_catches_dup_when_paths_overlap() {
        // Two items both touching the same file with similar prose
        // are still duplicates — only DISJOINT path footprints
        // exempt the comparison.
        let a = DedupEntry::from_bag("audit-fs-foo".into(), "audit fs/foo.c locking around bar()");
        let b = DedupEntry::from_bag(
            "audit-fs-foo-2".into(),
            "audit fs/foo.c locking around bar() callers",
        );
        assert!(b.is_duplicate_of(&a));
    }

    #[test]
    fn dedup_tokens_extracts_kernel_module_artifacts() {
        let toks = dedup_tokens("rebuild drivers/net/ethernet/intel/ice/ice.ko after fix");
        assert!(
            toks.contains("drivers/net/ethernet/intel/ice/ice.ko"),
            ".ko path missing: {toks:?}"
        );
    }

    #[test]
    fn parse_todo_response_plain_json() {
        let text = r#"{"todo": [{"name": "x", "type": "investigate", "status": "pending"}]}"#;
        let got = parse_todo_response(text).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "x");
    }

    #[test]
    fn parse_todo_response_rejects_transport_wrapper() {
        let text = r#"{"result":{"todo":[]}}"#;
        assert!(parse_todo_response(text).is_err());
    }

    #[test]
    fn parse_todo_response_rejects_entire_array_when_one_item_is_invalid() {
        let text = r#"{"todo":[{"id":"good","name":"good","type":"review","status":"pending"},{"id":42}]}"#;
        assert!(parse_todo_response(text).is_err());
    }

    #[test]
    fn parse_todo_response_rejects_embedded_object() {
        let text =
            r#"Here you go: {"todo": [{"name": "y", "type": "read", "status": "done"}]} bye."#;
        assert!(parse_todo_response(text).is_err());
    }

    #[test]
    fn parse_todo_response_rejects_multiple_candidates() {
        let text = r#"Example: {"todo":[{"name":"example","status":"pending"}]}
Actual: {"todo":[{"name":"actual","status":"done"}]}"#;
        assert!(parse_todo_response(text).is_err());
    }

    #[test]
    fn parse_todo_response_bad_json_returns_error() {
        assert!(parse_todo_response("not a json object").is_err());
        assert!(parse_todo_response("{}").is_err());
        let error = parse_todo_response(r#"{"todo":[]} trailing"#).unwrap_err();
        assert!(error.iter().any(|message| message.contains("trailing")));
    }

    #[test]
    fn parse_todo_response_rejects_unknown_item_fields() {
        let text = r#"{"todo":[{"name":"x","status":"pending","depends_onn":[]}]}"#;
        assert!(parse_todo_response(text).is_err());
    }

    #[test]
    fn todo_instructions_keep_trace_steps_pending_without_evidence() {
        let body = build_instructions(true, true);
        assert!(body.contains("trace unchanged callers, callees"));
        assert!(body.contains("status=done requires concrete cited evidence"));
        assert!(body.contains("Do NOT mark such a step done"));
        assert!(body.contains("no remaining users"));
    }

    #[test]
    fn assign_ids_populates_unique_ids() {
        let mut items = vec![
            TodoItem::new("investigate slab", "investigate"),
            TodoItem::new("investigate slab", "investigate"),
            TodoItem::new("read a.c", "read"),
        ];
        assign_ids(&mut items);
        assert!(!items[0].id.is_empty());
        assert_ne!(items[0].id, items[1].id);
        assert!(!items[2].id.is_empty());
    }

    #[test]
    fn pending_exact_match_to_completed_is_duplicate() {
        let completed = TodoItem {
            name: "ID_REV_CHIP_ID_7800_ — enumerate all chipid-gated HW_CFG blocks".into(),
            kind: "search".into(),
            status: TodoStatus::Done,
            reason: String::new(),
            depends_on: Vec::new(),
            coverage: "Covered lan78xx.c LED/HW_CFG sites.".into(),
            id: "hw-cfg-led-save-restore-enum".into(),
            step_id: String::new(),
        };
        let mut ids = HashSet::new();
        ids.insert(completed.id.clone());
        let mut names = HashSet::new();
        names.insert(completed.name.to_ascii_lowercase());

        let same_id = TodoItem {
            status: TodoStatus::Pending,
            ..completed.clone()
        };
        assert!(pending_matches_completed_exact(&same_id, &ids, &names));

        let same_name = TodoItem {
            id: "new-id".into(),
            status: TodoStatus::Pending,
            ..completed
        };
        assert!(pending_matches_completed_exact(&same_name, &ids, &names));
    }

    #[test]
    fn fallback_dedup_preserves_existing() {
        let existing = vec![TodoItem {
            name: "scrub drivers/net/netkit.c".into(),
            kind: "investigate".into(),
            status: TodoStatus::Pending,
            reason: String::new(),
            depends_on: Vec::new(),
            coverage: String::new(),
            id: String::new(),
            step_id: String::new(),
        }];
        let new_fu = vec![json!({
            "type": "investigate",
            "name": "check drivers/net/netkit.c scrubbing",
            "reason": "possible bug in netkit scrub"
        })];
        let merged = fallback_dedup(&existing, &new_fu);
        // Overlapping tokens (drivers/net/netkit.c) → dropped.
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn fallback_dedup_keeps_distinct() {
        let existing = vec![TodoItem::new("one", "investigate")];
        let new_fu = vec![json!({
            "type": "investigate",
            "name": "completely unrelated subsystem query",
            "reason": "reason"
        })];
        let merged = fallback_dedup(&existing, &new_fu);
        assert_eq!(merged.len(), 2);
    }
}
