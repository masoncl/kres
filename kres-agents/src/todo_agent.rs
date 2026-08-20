//! Todo-agent: maintains the todo list based on task output.
//!
//! Port of
//!
//! After each task completes, the caller feeds this module:
//!   - one `completed` entry per task reaped in this batch, each
//!     carrying the prompt that drove it (`query`), its analysis text
//!     (`analysis`), and the followups its slow agent produced
//!   - the current todo list
//!
//! The module packages that into a JSON request (with
//! DEDUP + COVERAGE instructions)
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

use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::todo::{TodoItem, TodoStatus};
use kres_core::UsageTracker;
use kres_llm::{
    client::Client,
    config::CallConfig,
    model::ThinkingBudget,
    request::{CachedPrefix, Message},
    Model,
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

/// One completion the agent declares this round. Replaces re-emitting
/// the whole done row: Rust already owns every field of a done item
/// except the coverage sentence, which is written exactly once — when
/// the item first reaches Done.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DoneMark {
    /// `id` of an item in `current_todo`.
    id: String,
    /// 1-2 sentences naming the files, symbols and line ranges the
    /// analysis examined, plus the bottom-line finding. Consumed by
    /// the DEDUP step of later calls.
    #[serde(default)]
    coverage: String,
}

/// One deliberate retirement. Omitting a pending row used to be the
/// only way to delete it, which made deletion indistinguishable from
/// a truncated or forgetful reply — at call 20 of the 2026-08-05
/// mm/page_alloc.c review the agent was handed 57 rows and returned
/// 34, and nothing restored the missing 23.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RetireMark {
    /// `id` of a pending item in `current_todo`.
    id: String,
    /// Why the work is no longer worth doing. Logged, not stored.
    #[serde(default)]
    reason: String,
}

/// Parsed response shape from the todo agent.
/// One row of the agent's `todo` array.
///
/// Deliberately NOT `TodoItem`: this is an edit, not a record. Every
/// mutable field is `Option`, so `None` means "leave it alone" and is
/// distinct from `Some("")` meaning "clear it". `TodoItem` requires
/// `name`, which made the id-only reply the prompt asks for fail
/// schema validation — five of six calls in the 2026-08-06
/// mm/page_alloc.c run were rejected with "missing field `name`" and
/// went through a repair round trip, and the repairing model
/// satisfied the required field by copying the id over the row's real
/// name. 27 of 28 rows in that session ended up with `name == id`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodoEdit {
    /// Handle for an existing row. Empty on a row being created.
    #[serde(default)]
    id: String,
    /// Prose. Required when creating a row, omitted to leave an
    /// existing row's name untouched.
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    /// Accepted for a completion declared inline rather than through
    /// `newly_done`. Rust restores it for every other row.
    #[serde(default)]
    status: Option<TodoStatus>,
    #[serde(default)]
    coverage: Option<String>,
    /// Meaningful only on a row being created; restored from the
    /// original otherwise.
    #[serde(default)]
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    step_id: Option<String>,
}

impl TodoEdit {
    /// Build the row this edit is creating. Returns None when the
    /// edit names no existing row and carries no name, which is not
    /// something Rust can turn into work.
    fn into_new_item(self) -> Option<TodoItem> {
        let name = self.name.filter(|n| !n.trim().is_empty())?;
        Some(TodoItem {
            name,
            kind: self.kind.unwrap_or_default(),
            status: TodoStatus::Pending,
            reason: self.reason.unwrap_or_default(),
            depends_on: self.depends_on.unwrap_or_default(),
            coverage: String::new(),
            id: self.id,
            step_id: self.step_id.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodoUpdateResponse {
    /// The PENDING list only. Done rows are Rust-owned and must not
    /// appear here.
    todo: Vec<TodoEdit>,
    /// Items that reached Done this round.
    #[serde(default)]
    newly_done: Vec<DoneMark>,
    /// Pending items the agent is deliberately abandoning.
    #[serde(default)]
    retired: Vec<RetireMark>,
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
/// One completed task in a reaped batch.
///
/// The reaper drains every terminal task in one call, so a wave of
/// parallel work arrives together. Reconciling them in one round is
/// both cheaper and more correct than N sequential rounds: the agent
/// sees the whole set at once and can dedup a followup emitted twice
/// by two siblings, which sequential rounds can only do after the
/// first sibling's followup has already become a row.
pub struct CompletedTask<'a> {
    /// Human-readable prompt the task ran under.
    pub query: &'a str,
    /// Stable id/name of the todo row this task was executing. The
    /// model sees `query`; Rust uses this identity to make completion
    /// deterministic.
    pub todo_id: Option<&'a str>,
    pub analysis: &'a str,
    pub followups: &'a [Value],
}

pub struct TodoAgentInputs<'a> {
    /// Every task reaped in this batch, oldest first. Never empty —
    /// callers with nothing to report must not make the call.
    pub completed: &'a [CompletedTask<'a>],
    pub current_todo: &'a [TodoItem],
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

/// Fields of an `update_todo` request that repeat across reaps.
///
/// A field belongs here only if it is the SAME BYTES on the next reap.
/// The stable half is one cached prefix, so a single volatile member
/// rewrites the entry for every other member too — and because
/// serde_json orders keys (no `preserve_order` feature in this
/// workspace), a volatile key that sorts early poisons everything
/// after it.
///
/// Measured over the 51 todo calls of the 2026-08-05 mm/page_alloc.c
/// review, by how many of the 50 transitions changed the field:
///
///   task              0/50       13 chars
///   instructions      0/50    6,522 chars
///   plan             10/50   12,592 chars
///   completed_query  36/50      831 chars   <- was in here
///   analysis_summary 50/50    4,023 chars
///   new_followups    50/50    8,586 chars
///   current_todo     50/50   46,378 chars
///
/// `completed_query` is the reaped task's name, so it changes on
/// nearly every call, and it sorts ahead of `instructions`, `plan`
/// and `task` — it was invalidating the entire prefix from the front. That run wrote 323,010 cache-creation tokens against
/// 154,229 cache reads.
///
/// `plan` stays: it holds at 40 of 50 transitions and is the largest
/// stable member, so caching it and eating the occasional rewrite
/// beats never caching it at all.
const UPDATE_TODO_STABLE_FIELDS: &[&str] = &["task", "instructions", "plan"];

/// Same as `update_todo_via_agent` but also logs the user+assistant
/// turns to the provided TurnLogger's `main.jsonl`.
pub async fn update_todo_via_agent_with_logger(
    tc: &TodoClient,
    inputs: TodoAgentInputs<'_>,
    logger: Option<Arc<TurnLogger>>,
    shutdown: Option<kres_core::Shutdown>,
) -> Result<TodoUpdate, AgentError> {
    let TodoAgentInputs {
        completed,
        current_todo,
        plan,
    } = inputs;
    let completed_todo_ids: Vec<&str> = completed.iter().filter_map(|c| c.todo_id).collect();
    // Every followup the batch emitted, in batch order. The fallback
    // paths below promote these to rows without an agent, so they must
    // see the whole batch for the same reason the agent does.
    let batch_followups: Vec<Value> = completed
        .iter()
        .flat_map(|task| task.followups.iter().cloned())
        .collect();
    // --- Prepare inputs ------------------------------------------------
    let mut todo_list = current_todo.to_vec();
    assign_ids(&mut todo_list);
    mark_completed_todo(&mut todo_list, &completed_todo_ids);
    // The reaped item is Done before the call so the agent cannot
    // reopen it, but its coverage is left empty on purpose: the whole
    // point of this round is for the agent to write that sentence via
    // `newly_done`. `stamp_missing_coverage` fills in a placeholder
    // afterwards only if it declined.
    let current_payload: Vec<Value> = todo_list.iter().map(todo_to_payload).collect();

    let mut request = serde_json::Map::new();
    request.insert("task".into(), json!("update_todo"));
    // One entry per task reaped in this batch. `mark_completed_todo`
    // already flipped each reaped row to Done, so from the agent's
    // side they did not "reach Done this round" and it never listed
    // them in `newly_done`. Five of six calls in the 2026-08-06
    // mm/page_alloc.c run left the completion carrying Rust's
    // placeholder coverage, which is write-once and therefore
    // permanent. Name each row explicitly and demand its evidence.
    let completed_payload: Vec<Value> = completed
        .iter()
        .map(|task| {
            let mut entry = serde_json::Map::new();
            entry.insert("query".into(), json!(task.query));
            if let Some(id) = task.todo_id {
                entry.insert("just_completed".into(), json!(id));
            }
            entry.insert("analysis".into(), json!(task.analysis));
            if !task.followups.is_empty() {
                entry.insert("followups".into(), json!(task.followups));
            }
            Value::Object(entry)
        })
        .collect();
    request.insert("completed".into(), json!(completed_payload));
    request.insert("current_todo".into(), json!(current_payload));
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
    request.insert("instructions".into(), json!(build_instructions(has_plan)));
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
        cached_prefixes: Vec::from_iter(
            (!split.stable.is_empty()).then(|| CachedPrefix::short(split.stable.clone())),
        ),
    }];
    if let Some(lg) = &logger {
        let request = cfg.request_meta();
        lg.log_main_with_request(
            "user",
            Some("phase=todo"),
            &request_text,
            None,
            None,
            Some(&request),
        );
    }
    let resp_result = if let Some(shutdown) = shutdown.clone() {
        tokio::select! {
            _ = shutdown.cancelled() => {
                return Ok(TodoUpdate {
                    todo: fallback_dedup(&todo_list, &batch_followups),
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
                todo: fallback_dedup(&todo_list, &batch_followups),
                plan: None,
            });
        }
    };
    record_usage(tc, &resp.usage);
    let text = extract_text(&resp);
    if let Some(lg) = &logger {
        lg.log_main(
            "assistant",
            Some("phase=todo"),
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
                instructions: "Preserve every pending todo id, every newly_done id and its coverage sentence, every retired id, and the plan decision. Correct representation and field types only. NEVER invent a value for an absent field — every field except `id` is optional and absent means unchanged, so omit it rather than filling it in. In particular never copy an id into `name`: that overwrites the row's real title.",
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
                fields: TODO_RESPONSE_FIELDS,
            };
            if let Ok(response) = contract.accept_repair::<TodoUpdateResponse>(&repaired.text)
            {
                parsed_envelope = Some(ParsedTodoUpdate::from(response));
            } else {
                tracing::warn!(target: "kres_agents", "todo JSON repair failed the strict response contract");
            }
        }
    }
    let Some(parsed) = parsed_envelope else {
        tracing::warn!(
            target: "kres_agents",
            "todo agent returned no parseable list; falling back"
        );
        return Ok(TodoUpdate {
            todo: fallback_dedup(&todo_list, &batch_followups),
            plan: None,
        });
    };
    let returned_plan = parsed.plan.clone();

    let Reconciled {
        done: done_final,
        pending: pending_from_agent,
    } = reconcile_update(&todo_list, parsed, plan);

    let filtered_pending = dedup_pending_rows(&done_final, pending_from_agent);
    // Order: done rows (Rust-owned), then live rows in stable storage
    // order. This is not a ranking — see `crate::prioritize`.
    let mut result = Vec::with_capacity(done_final.len() + filtered_pending.len());
    result.extend(done_final);
    result.extend(filtered_pending);

    // The agent is told to emit `id` for every item but new pending
    // followups it creates this round can come back with an empty id.
    // Without an id, depends_on can't reference the item and the
    // dispatch/resolve loop in cmd_continue / cmd_next /
    // should_auto_continue treats it as nameless. Synthesize stable
    // ids here before returning so every downstream consumer can
    // count on `id` being populated.
    assign_ids(&mut result);
    stamp_missing_coverage(&mut result);

    Ok(TodoUpdate {
        todo: result,
        plan: returned_plan,
    })
}

/// Fields the todo agent may return at top level. Used by the strict
/// object contract so a reply that only carries e.g. `newly_done` is
/// still recognised as a todo update rather than rejected outright.
const TODO_RESPONSE_FIELDS: &[&str] = &["todo", "newly_done", "retired", "plan"];

/// Owned form of `TodoUpdateResponse`, kept separate so the envelope
/// can be cloned across the JSON-repair retry.
#[derive(Debug, Clone)]
struct ParsedTodoUpdate {
    todo: Vec<TodoEdit>,
    newly_done: Vec<DoneMark>,
    retired: Vec<RetireMark>,
    plan: Option<kres_core::PlanRewrite>,
}

impl From<TodoUpdateResponse> for ParsedTodoUpdate {
    fn from(r: TodoUpdateResponse) -> Self {
        Self {
            todo: r.todo,
            newly_done: r.newly_done,
            retired: r.retired,
            plan: r.plan,
        }
    }
}

/// Result of folding one agent reply into the authoritative list.
struct Reconciled {
    /// Every terminal row, in the order it has always had.
    done: Vec<TodoItem>,
    /// Live rows in stable storage order: surviving rows keep their
    /// original position, newly created rows are appended.
    pending: Vec<TodoItem>,
}

/// Fold an agent reply into the caller's list.
///
/// The list is Rust-owned; the reply is a set of edits against it. The
/// agent controls prose (`name`, `reason`, `type`), completion
/// (`newly_done`) and retirement (`retired`). It controls nothing
/// else — in particular it does not control ORDER, which is stable
/// storage order: existing rows keep their position and new rows are
/// appended. Choosing what runs next belongs to `crate::prioritize`.
/// In particular:
///
///   * `id`, `step_id` and `depends_on` on an existing row are restored
///     from the original, so the agent need not re-emit them and cannot
///     detach a plan link or a dependency edge by paraphrasing.
///   * Done rows are reconstructed here rather than echoed back, and a
///     coverage sentence is written exactly once — when the row first
///     reaches Done. Later rounds cannot paraphrase it away.
///   * A pending row the agent simply forgot is restored. Deleting work
///     requires naming it in `retired`.
fn reconcile_update(
    originals: &[TodoItem],
    parsed: ParsedTodoUpdate,
    plan: Option<&kres_core::Plan>,
) -> Reconciled {
    let mut state: Vec<TodoItem> = originals.to_vec();

    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (idx, item) in state.iter().enumerate() {
        if !item.id.is_empty() {
            by_id.entry(item.id.clone()).or_insert(idx);
        }
        if !item.name.is_empty() {
            by_name.entry(item.name.to_ascii_lowercase()).or_insert(idx);
        }
    }
    let resolve = |id: &str, name: &str| -> Option<usize> {
        by_id
            .get(id)
            .copied()
            .or_else(|| by_name.get(&name.to_ascii_lowercase()).copied())
    };

    // --- Completions --------------------------------------------------
    for mark in &parsed.newly_done {
        let Some(idx) = resolve(&mark.id, &mark.id) else {
            tracing::info!(
                target: "kres_agents",
                "todo agent marked unknown id '{}' done; ignoring",
                truncate(&mark.id, 60)
            );
            continue;
        };
        state[idx].status = TodoStatus::Done;
        let coverage = mark.coverage.trim();
        if kres_core::coverage_is_unwritten(&state[idx].coverage) && !coverage.is_empty() {
            state[idx].coverage = coverage.to_string();
        }
    }

    // --- Retirements ----------------------------------------------------
    // Only live rows can be retired; a done row is history and stays.
    let mut retired: HashSet<usize> = HashSet::new();
    let mut retired_log: Vec<String> = Vec::new();
    for mark in &parsed.retired {
        let Some(idx) = resolve(&mark.id, &mark.id) else {
            continue;
        };
        if state[idx].status.is_terminal() || !retired.insert(idx) {
            continue;
        }
        retired_log.push(format!(
            "{}: {}",
            truncate(&state[idx].id, 40),
            truncate(mark.reason.trim(), 80)
        ));
    }
    if !retired.is_empty() {
        tracing::info!(
            target: "kres_agents",
            "todo agent retired {} live item(s): {}",
            retired.len(),
            retired_log.join("; ")
        );
    }

    // --- Live rows the agent re-emitted --------------------------------
    // Prose updates are folded into `state` in place. The emission
    // ORDER is deliberately discarded: the list is stable storage and
    // the prioritization agent picks what runs next, so a row that
    // moves to the top of the reply must not thereby jump the queue.
    let mut emitted: HashSet<usize> = HashSet::new();
    let mut appended: Vec<TodoItem> = Vec::new();
    let mut new_ids: HashSet<String> = HashSet::new();
    for row in parsed.todo {
        let lookup_name = row.name.clone().unwrap_or_default();
        let Some(idx) = resolve(&row.id, &lookup_name) else {
            // Genuinely new work. The agent owns every field here,
            // including step_id and depends_on, because there is no
            // prior row to restore them from. New rows land after the
            // existing ones, in the order the agent created them.
            let key = if row.id.is_empty() {
                lookup_name.to_ascii_lowercase()
            } else {
                row.id.clone()
            };
            if !key.is_empty() && !new_ids.insert(key) {
                continue;
            }
            let handle = if row.id.is_empty() {
                lookup_name.clone()
            } else {
                row.id.clone()
            };
            match row.into_new_item() {
                Some(item) => appended.push(item),
                None => tracing::info!(
                    target: "kres_agents",
                    "todo agent emitted a row naming no known item and no name ({}); ignoring",
                    truncate(&handle, 60)
                ),
            }
            continue;
        };
        if retired.contains(&idx) || !emitted.insert(idx) {
            continue;
        }
        // Prose is the agent's, and only where it said so: `None`
        // means "unchanged", which is how an id-only edit leaves the
        // stored name alone instead of blanking it.
        if let Some(name) = row.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            state[idx].name = name.to_string();
        }
        if let Some(reason) = row.reason {
            state[idx].reason = reason;
        }
        if let Some(kind) = row.kind.filter(|k| !k.is_empty()) {
            state[idx].kind = kind;
        }
        // A completion declared inline rather than via `newly_done`.
        if row.status.is_some_and(|s| s.is_terminal()) && !state[idx].status.is_terminal() {
            state[idx].status = TodoStatus::Done;
            let coverage = row.coverage.unwrap_or_default();
            let coverage = coverage.trim();
            if kres_core::coverage_is_unwritten(&state[idx].coverage) && !coverage.is_empty() {
                state[idx].coverage = coverage.to_string();
            }
        }
    }

    // --- Rows the agent neither kept, completed, nor retired -----------
    // Omission is not deletion. Before this, only rows carrying a
    // step_id that pointed at a live plan step were rescued, which
    // covered none of the 23 rows dropped at call 20 of the 2026-08-05
    // mm/page_alloc.c review. Restore them all and say so; the agent
    // gets another chance next round to retire them on the record.
    let live_steps: Option<HashSet<&str>> = plan.map(|p| {
        p.steps
            .iter()
            .filter(|s| !s.status.is_terminal())
            .map(|s| s.id.as_str())
            .collect()
    });
    let mut restored: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut pending: Vec<TodoItem> = Vec::new();
    for (idx, item) in state.iter().enumerate() {
        if item.status.is_terminal() || retired.contains(&idx) {
            continue;
        }
        if !emitted.contains(&idx) {
            // A row bound to a step the plan has since finished is
            // stale by construction, not forgotten. Dropping it is
            // correct, but dropping it silently is the failure this
            // whole function exists to prevent, so say so.
            if let (Some(live), false) = (live_steps.as_ref(), item.step_id.is_empty()) {
                if !live.contains(item.step_id.as_str()) {
                    stale.push(format!("{} (step {})", item.id, item.step_id));
                    continue;
                }
            }
            restored.push(item.id.clone());
        }
        pending.push(item.clone());
    }
    if !stale.is_empty() {
        // Also a deletion, so also operator-visible. See the dedup
        // drop above for why tracing alone is not enough.
        kres_core::async_eprintln!(
            "[todo update] dropped {} unemitted item(s) bound to a finished plan step: {}",
            stale.len(),
            stale
                .iter()
                .take(5)
                .map(|entry| truncate(entry, 60))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    pending.extend(appended);
    if !restored.is_empty() {
        tracing::info!(
            target: "kres_agents",
            "todo agent dropped {} live item(s) without retiring them; restored: {}",
            restored.len(),
            restored
                .iter()
                .take(5)
                .map(|id| truncate(id, 40))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let done: Vec<TodoItem> = state
        .into_iter()
        .filter(|item| item.status.is_terminal())
        .collect();
    Reconciled { done, pending }
}

/// Programmatic near-duplicate dedup of the agent's pending rows.
///
/// Pure so the invariants below are testable without an API call.
/// Returns the surviving rows in order.
fn dedup_pending_rows(done_final: &[TodoItem], pending_from_agent: Vec<TodoItem>) -> Vec<TodoItem> {
    // --- Programmatic dedup backstop for pending items ----------------
    // Two items are duplicates only when they refer to the same code:
    // either both bags lack file-path tokens (pure-prose tasks like
    // "investigate slab corruption") and >=70% of remaining tokens
    // overlap, OR their path-token sets share at least one path AND
    // overall token overlap >=70%. Items whose path-token sets are
    // both non-empty and disjoint are NEVER duplicates -- they
    // operate on different files. This is what keeps sibling
    // compile-verify-v4 / compile-verify-v6 steps from collapsing
    // into one (their .o paths differ even though the surrounding
    // prose is near-identical).
    //
    // A survey group row takes no part in this, on either side. The
    // groups are a PARTITION of one file's function list, generated by
    // Rust, so two of them are disjoint by construction and a prose
    // verdict about them can only ever be wrong. Their bags are also
    // the worst possible input to the heuristic: every row carries the
    // same Rust-authored "audit every function in this list"
    // instruction, none carries a path token (they name functions, not
    // files), so `is_duplicate_of` skips the disjoint-footprint guard
    // and decides on overlap alone -- and `denom` is the SMALLER bag,
    // so the victim is always the group with the least prose of its
    // own.
    //
    // Measured on the 2026-08-22 arch/x86/kvm/mmu/mmu.c review
    // (kvm27), which is what found this: of 49 group rows the todo
    // agent emitted correctly, three were deleted here --
    // audit-group-19 at 0.75 overlap against group-06, -33 at 0.73
    // against group-14, -49 at 0.72 against group-06. Groups 33 and 49
    // hold two functions each and 19 had the shortest rationale. That
    // silently removed 11 functions, among them is_cr0_pg,
    // kvm_calc_cpu_role and kvm_arch_vcpu_pre_fault_memory, from a
    // review whose whole contract is that every function is covered.
    let mut ref_entries: Vec<DedupEntry> = Vec::new();
    let mut completed_ids: HashSet<String> = HashSet::new();
    let mut completed_names: HashSet<String> = HashSet::new();
    for d in done_final.iter() {
        if !d.id.is_empty() {
            completed_ids.insert(d.id.clone());
        }
        if !d.name.is_empty() {
            completed_names.insert(d.name.to_ascii_lowercase());
        }
        if kres_core::is_survey_group_row(d) {
            continue;
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
        if kres_core::is_survey_group_row(&p) {
            filtered_pending.push(p);
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
        // Deleting work is a scheduling decision and belongs in the
        // operator-visible narrative, not only in tracing. On kvm27
        // three group rows were removed here and `console.jsonl` --
        // the record that exists to explain WHY the scheduler did what
        // it did -- said nothing at all, so the loss was only found by
        // diffing the agent's reply against session.json.
        kres_core::async_eprintln!(
            "[todo update] dedup dropped {} pending item(s): {}",
            dropped.len(),
            dropped
                .iter()
                .take(3)
                .map(|(p, d)| format!("{}~{}", truncate(p, 40), truncate(d, 40)))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    filtered_pending
}

fn mark_completed_todo(items: &mut [TodoItem], completed_todo_ids: &[&str]) {
    for completed_id in completed_todo_ids {
        if let Some(item) = items.iter_mut().find(|item| {
            item.id == *completed_id || (item.id.is_empty() && item.name == *completed_id)
        }) {
            item.status = TodoStatus::Done;
        }
    }
}

/// Last-resort coverage for a done item the agent never described.
/// Applied after reconciliation so a real `newly_done.coverage` always
/// wins; a done item with empty coverage is invisible to the DEDUP
/// step of later calls, which is worse than a vague sentence.
fn stamp_missing_coverage(items: &mut [TodoItem]) {
    for item in items.iter_mut() {
        if item.status.is_terminal() && kres_core::coverage_is_unwritten(&item.coverage) {
            item.coverage = kres_core::PLACEHOLDER_COVERAGE.to_string();
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
        let base = slugify_todo_id(&t.name);
        let mut id = base.clone();
        let mut counter = 2u32;
        // Suffix the WHOLE slug rather than re-cutting it. Re-cutting
        // made the first row's id depend on whether a second row
        // happened to be present that round, so the same followup
        // could land under different ids on different reaps and any
        // `depends_on` minted against the earlier one dangled.
        while seen.contains(&id) {
            id = format!("{base}-{counter}");
            counter += 1;
        }
        seen.insert(id.clone());
        t.id = id;
    }
}

/// Longest id `slugify_todo_id` will produce, in characters. Long
/// enough to stay readable, short enough that the prioritizer's output
/// stays small — it echoes one id per pick and `crate::prioritize`
/// exists to be input-bound.
const TODO_ID_MAX_CHARS: usize = 48;

/// Derive a stable, readable id from a todo's name.
///
/// The old derivation was `name.chars().take(40)`, which produced ids
/// like `Prove pcp->batch can never be 0 on a liv` — 27 of 40 ready
/// rows in the 2026-08-06 mm/page_alloc.c run were mid-word slices of
/// their name. Followups arrive with no id (`{type, name, reason,
/// path?}`), so every promoted followup went through this path.
///
/// Cut on token boundaries, never mid-word, and emit only
/// `[a-z0-9-]` so an id can be typed, logged, and matched without
/// quoting.
fn slugify_todo_id(name: &str) -> String {
    let mut slug = String::with_capacity(TODO_ID_MAX_CHARS);
    for token in name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let token = token.to_ascii_lowercase();
        // Stop at a whole-token boundary rather than truncating one.
        if !slug.is_empty() && slug.len() + 1 + token.len() > TODO_ID_MAX_CHARS {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        // A single token longer than the cap is the only case where a
        // cut is unavoidable.
        if token.len() > TODO_ID_MAX_CHARS {
            slug.push_str(&token[..TODO_ID_MAX_CHARS]);
            break;
        }
        slug.push_str(&token);
    }
    if slug.is_empty() {
        // A name with no alphanumerics at all. Callers still need a
        // handle; the collision loop makes it unique.
        slug.push_str("todo");
    }
    slug
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
///
/// The reaped item arrives here already flipped to Done but with empty
/// coverage, because the agent round that was supposed to write that
/// sentence is the one that just failed. Stamp the placeholder before
/// deduping: a done item with no coverage is invisible to the DEDUP
/// step of every later call, so the same work gets re-added forever.
fn fallback_dedup(existing: &[TodoItem], new_followups: &[Value]) -> Vec<TodoItem> {
    let mut out = existing.to_vec();
    stamp_missing_coverage(&mut out);
    // Survey group rows are excluded for the same reason they are
    // excluded from `dedup_pending_rows`: their bags are one
    // Rust-authored instruction paragraph repeated across every group,
    // with no path token to separate them, so a short incoming
    // followup is a near-subset of any of them and `denom` is the
    // smaller bag. This path cannot delete a group row -- it only
    // filters arriving followups -- but it can silently swallow the
    // followup a group's own audit just raised.
    let mut existing_tokens: Vec<HashSet<String>> = out
        .iter()
        .filter(|t| !kres_core::is_survey_group_row(t))
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

fn build_instructions(has_plan: bool) -> String {
    let mut s = String::from(
        "Update the todo list. Return raw, unfenced JSON only:\n\
         {\n\
         \u{20}\"todo\": [{\"id\":\"ID\"}, {\"type\":\"T\",\"name\":\"N\",\"reason\":\"R\"}],\n\
         \u{20}\"newly_done\": [{\"id\":\"ID\",\"coverage\":\"C\"}],\n\
         \u{20}\"retired\": [{\"id\":\"ID\",\"reason\":\"R\"}]\n\
         }\n\n",
    );
    s.push_str(
        "WHAT EACH FIELD IS FOR — the list is owned by the pipeline, \
         and your reply is a set of edits against it, not a rewrite \
         of it:\n\
         - `todo` — the PENDING items ONLY. Never put a done item \
         here. The pipeline keeps every done item itself; re-listing \
         them wastes your output budget and cannot change them. Order \
         is NOT a channel: the list is stable storage and a separate \
         prioritization agent decides what runs next, so do not try to \
         signal urgency by position.\n\
         - `newly_done` — items that reached Done this round, each with \
         the coverage sentence described below. This is the ONLY way to \
         complete an item, and coverage is written once: later rounds \
         cannot reword it.\n\
         - `just_completed` (on a `completed` entry, when present) is \
         the id of the item that task was executing. Its row already \
         shows status done, but its coverage is EMPTY and only you can \
         write it. You MUST return EVERY such id in `newly_done` with a \
         real coverage sentence drawn from that entry's own `analysis` \
         — one per completed entry, not one for the batch. Skipping any \
         leaves that row permanently uncovered and blinds every later \
         dedup pass to what the run has already examined.\n\
         - `retired` — pending items you are deliberately abandoning, \
         with the reason. Leaving an item out of `todo` does NOT delete \
         it; the pipeline restores anything you neither kept, completed, \
         nor retired, because a forgotten item and an abandoned one look \
         identical on the wire. Retire on purpose, on the record.\n\
         - Omit `newly_done` or `retired` entirely when empty.\n\n",
    );
    s.push_str(
        "FIELDS YOU DO NOT EMIT for an item that already exists in \
         `current_todo`: `status`, `coverage`, `depends_on`, `step_id`. \
         The pipeline restores all four from its own copy and discards \
         whatever you send, so emitting them only costs output. An \
         unchanged pending row is exactly `{\"id\":\"ID\"}` and \
         nothing more. Add `name`, `reason` or `type` ONLY to change \
         that field; an omitted field means unchanged, so never repeat \
         a value you are not editing and never fill one in just to \
         satisfy a shape. For an item you are creating THIS round \
         there is no prior copy, so a new item DOES carry `type`, \
         `name`, `reason`, `depends_on` and `step_id` — `name` is \
         required there and is the only way the row can exist.\n\n",
    );
    if has_plan {
        s.push_str(
            "PLAN LINKAGE — a `plan` field is present with `steps:[{id,\
             title,description}]`. For every NEW todo item you create \
             this round, set `step_id` to the id of the \
             plan step whose title/description best matches the todo's \
             target. Match on file, symbol, subsystem, or investigation \
             angle — not just keyword overlap. If NO step is a clear \
             fit, set `step_id` to the empty string. Do not invent step \
             ids; only use ids listed under `plan.steps`. An item \
             already in `current_todo` keeps its step_id \
             automatically — do not re-emit it.\n\n",
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
             - After rewriting, set step_id on every NEW todo to \
               an id from the NEW plan — do not reference ids you \
               just removed.\n\
             Omit the `plan` field entirely when no rewrite is \
             warranted — that is the common case.\n\n",
        );
    }
    s.push_str(
        "DEDUP ALGORITHM — run this for EVERY followup in EVERY \
         `completed` entry, treating them as one pooled list:\n\
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
         4. Compare each surviving followup against the ones from the \
         OTHER completed entries in this same round. Parallel tasks \
         routinely rediscover the same gap; emit one row, not one per \
         task that noticed it.\n\
         5. Read every entry's 'analysis' to identify which file:line \
         pairs this round touched; use them to decide which done-item \
         coverage to update.\n\
         6. Only followups that introduce genuinely new files, \
         symbols, or analysis angles survive.\n\
         Emit the dropped followup ids/names nowhere — just omit them.\n\n",
    );
    s.push_str(
        "WHEN TO RETIRE — `retired` went unused across the first six \
         calls of the 2026-08-06 mm/page_alloc.c run while the list \
         grew 5 -> 28 rows, so here is what earns a retirement. These \
         are the only reasons; none of them is \"the list is long\":\n\
         - The evidence a row was going to gather has since arrived \
         through another item's coverage, so running it would re-read \
         what is already known.\n\
         - The premise is dead: the finding it was going to confirm is \
         invalidated, or the code path it targets does not exist as \
         the earlier analysis assumed.\n\
         - It was created to answer a question a later analysis \
         answered outright.\n\
         - Another pending row strictly subsumes it — same files, same \
         symbols, broader scope. Retire the narrow one and say which \
         row absorbs it.\n\
         Give the concrete reason, naming the item or coverage that \
         made it obsolete. \"No longer needed\" is not a reason. When \
         in doubt keep the row: unrun work costs nothing, and \
         retiring a live question destroys it.\n\n",
    );
    s.push_str(
        "COVERAGE FIELD — required on every `newly_done` entry:\n\
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
         - Coverage is write-once. An item already carrying coverage \
         in `current_todo` is settled; do not restate or reword it.\n\
         - Do NOT leave coverage empty on a `newly_done` entry. Future \
         dedup calls depend on it.\n\n",
    );
    s.push_str(
        "OTHER RULES:\n\
         - Each NEW item gets a short unique id (use the name, shortened)\n\
         - Done items still appear in `current_todo` on every call so \
         you can dedup against their coverage — read them, do not \
         re-emit them\n\
         - Mark items done via `newly_done` when the analysis addressed \
         them\n\
         - Keep pending items that are still relevant in `todo`\n\
         - Move ONLY no-longer-relevant pending items to `retired`\n\
         - There is no limit on how many items may be pending. Keep \
         every item that is still worth doing. A separate \
         prioritization agent chooses which of them run next, and \
         whatever is left drains to the deferred list when the session \
         ends, so a long list costs nothing. Never retire an item to \
         hit a count.\n\
         - PARALLELISM: most items can run in parallel. Only add \
         depends_on when an item truly requires another's results first.\n\
         - FIX-AND-AMEND INVALIDATION: when the analysis shows that \
         code_edits were applied and a commit was amended (the patch \
         changed since the last review), any done todo that reviewed \
         or verified the PRIOR version of the patch is now stale. \
         Retire it and add a NEW pending item (new id, same step_id) so \
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
fn parse_todo_update_full(text: &str) -> Result<ParsedTodoUpdate, Vec<String>> {
    let r = crate::json_repair::JsonObjectContract {
        name: "todo-update",
        fields: TODO_RESPONSE_FIELDS,
    }
    .parse::<TodoUpdateResponse>(text)?;
    Ok(ParsedTodoUpdate::from(r))
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

    fn original(id: &str, name: &str, step: &str, status: TodoStatus) -> TodoItem {
        let mut item = TodoItem::new(name, "review");
        item.id = id.to_string();
        item.step_id = step.to_string();
        item.depends_on = vec!["gate".to_string()];
        item.status = status;
        item
    }

    fn update(todo: Vec<TodoEdit>) -> ParsedTodoUpdate {
        ParsedTodoUpdate {
            todo,
            newly_done: Vec::new(),
            retired: Vec::new(),
            plan: None,
        }
    }

    /// An edit naming an existing row and restating its prose.
    fn pending_row(id: &str, name: &str) -> TodoEdit {
        TodoEdit {
            id: id.into(),
            name: Some(name.into()),
            kind: Some("review".into()),
            reason: None,
            status: None,
            coverage: None,
            depends_on: None,
            step_id: None,
        }
    }

    /// The id-only edit the prompt actually asks for: "this row is
    /// still pending, nothing about it changed".
    fn keep(id: &str) -> TodoEdit {
        TodoEdit {
            id: id.into(),
            name: None,
            kind: None,
            reason: None,
            status: None,
            coverage: None,
            depends_on: None,
            step_id: None,
        }
    }

    #[test]
    fn reconciliation_preserves_completed_id_and_running_siblings() {
        let mut originals = vec![
            original(
                "review-write",
                "Trace write path",
                "write",
                TodoStatus::InProgress,
            ),
            original(
                "review-read",
                "Trace read path",
                "read",
                TodoStatus::InProgress,
            ),
            original(
                "review-final",
                "Verify shared contracts",
                "final",
                TodoStatus::InProgress,
            ),
        ];
        mark_completed_todo(&mut originals, &["review-write"]);

        // Mirrors sol4: the model rewrote stable IDs from titles and
        // emitted a duplicate of the completed task.
        let parsed = ParsedTodoUpdate {
            todo: vec![
                pending_row("Trace write path", "Trace write path"),
                pending_row("review-write", "Trace write path"),
                pending_row("Trace read path", "Trace read path"),
                pending_row("Verify shared contracts", "Verify shared contracts"),
            ],
            newly_done: Vec::new(),
            retired: Vec::new(),
            plan: None,
        };

        let out = reconcile_update(&originals, parsed, None);
        assert_eq!(out.done.len(), 1);
        assert_eq!(out.done[0].id, "review-write");
        assert_eq!(out.done[0].step_id, "write");
        assert_eq!(out.pending.len(), 2);
        for item in &out.pending {
            assert_eq!(item.status, TodoStatus::InProgress);
            assert_eq!(item.depends_on, vec!["gate".to_string()]);
        }
    }

    /// T2: done rows leave the output contract. They were 44.6% of the
    /// todo agent's emitted characters over the 2026-08-05
    /// mm/page_alloc.c review, and every field of them is Rust-owned.
    #[test]
    fn done_rows_are_reconstructed_when_the_agent_omits_them() {
        let mut originals = vec![
            original("done-a", "Audit alloc path", "a", TodoStatus::Done),
            original("done-b", "Audit free path", "b", TodoStatus::Done),
            original("live-c", "Audit pcp path", "c", TodoStatus::Pending),
        ];
        originals[0].coverage = "read mm/page_alloc.c:2266-2400; clean".into();
        originals[1].coverage = "read mm/page_alloc.c:2900-2960; clean".into();

        let out = reconcile_update(
            &originals,
            update(vec![pending_row("live-c", "Audit pcp path")]),
            None,
        );

        assert_eq!(out.done.len(), 2, "both done rows survive the omission");
        assert_eq!(
            out.done[0].coverage,
            "read mm/page_alloc.c:2266-2400; clean"
        );
        assert_eq!(
            out.done[1].coverage,
            "read mm/page_alloc.c:2900-2960; clean"
        );
        assert_eq!(out.pending.len(), 1);
    }

    /// T4: coverage is write-once. `reason` and `coverage` were
    /// rewritten on 28.0% and 27.4% of re-emitted rows respectively,
    /// against an instruction to keep coverage verbatim.
    #[test]
    fn coverage_is_written_once_and_cannot_be_paraphrased() {
        let mut originals = vec![
            original("settled", "Audit alloc path", "a", TodoStatus::Done),
            original("finishing", "Audit free path", "b", TodoStatus::Pending),
        ];
        originals[0].coverage = "read mm/page_alloc.c:2266-2400; clean".into();

        let parsed = ParsedTodoUpdate {
            todo: Vec::new(),
            newly_done: vec![
                DoneMark {
                    id: "settled".into(),
                    coverage: "looked at the allocator, seemed fine".into(),
                },
                DoneMark {
                    id: "finishing".into(),
                    coverage: "read mm/page_alloc.c:2900-2960; found the pcp leak".into(),
                },
            ],
            retired: Vec::new(),
            plan: None,
        };

        let out = reconcile_update(&originals, parsed, None);
        assert!(out.pending.is_empty());
        assert_eq!(out.done.len(), 2);
        let settled = out.done.iter().find(|i| i.id == "settled").unwrap();
        assert_eq!(
            settled.coverage, "read mm/page_alloc.c:2266-2400; clean",
            "existing coverage must not be reworded"
        );
        let finishing = out.done.iter().find(|i| i.id == "finishing").unwrap();
        assert_eq!(
            finishing.coverage, "read mm/page_alloc.c:2900-2960; found the pcp leak",
            "a first completion writes coverage"
        );
    }

    /// T3: omission is not deletion. At call 20 of the 2026-08-05
    /// review the agent was handed 57 rows and returned 34; the
    /// plan-linked-only rescue restored none of the missing 23.
    #[test]
    fn silently_dropped_live_items_are_restored() {
        let originals = vec![
            original("keep-me", "Audit alloc path", "", TodoStatus::Pending),
            original("forgotten", "Audit free path", "", TodoStatus::Pending),
            original("running", "Audit pcp path", "", TodoStatus::InProgress),
        ];

        let out = reconcile_update(
            &originals,
            update(vec![pending_row("keep-me", "Audit alloc path")]),
            None,
        );

        let ids: Vec<&str> = out.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["keep-me", "forgotten", "running"]);
        assert_eq!(
            out.pending[2].status,
            TodoStatus::InProgress,
            "a running task must not be reset by the restore"
        );
    }

    #[test]
    fn retiring_an_item_deletes_it_and_marking_it_done_keeps_it() {
        let originals = vec![
            original("abandon", "Audit alloc path", "", TodoStatus::Pending),
            original("finish", "Audit free path", "", TodoStatus::Pending),
        ];
        let parsed = ParsedTodoUpdate {
            todo: Vec::new(),
            newly_done: vec![DoneMark {
                id: "finish".into(),
                coverage: "read mm/page_alloc.c:1-10".into(),
            }],
            retired: vec![RetireMark {
                id: "abandon".into(),
                reason: "subsumed by the free-path audit".into(),
            }],
            plan: None,
        };

        let out = reconcile_update(&originals, parsed, None);
        assert!(out.pending.is_empty(), "retired item must not be restored");
        assert_eq!(out.done.len(), 1);
        assert_eq!(out.done[0].id, "finish");
    }

    /// A row bound to a plan step the plan has since finished is stale
    /// by construction, not forgotten, so the restore must skip it.
    #[test]
    fn restore_skips_items_tied_to_a_finished_plan_step() {
        use kres_core::{Plan, PlanStep};
        let originals = vec![
            original(
                "stale",
                "Audit alloc path",
                "retired-step",
                TodoStatus::Pending,
            ),
            original("live", "Audit free path", "live-step", TodoStatus::Pending),
        ];
        let mut plan = Plan::new("review", "goal", kres_core::TaskMode::Audit);
        plan.steps = vec![
            PlanStep {
                status: kres_core::PlanStepStatus::Done,
                ..PlanStep::new("retired-step", "Alloc path")
            },
            PlanStep::new("live-step", "Free path"),
        ];

        let out = reconcile_update(&originals, update(Vec::new()), Some(&plan));
        let ids: Vec<&str> = out.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
    }

    /// T1: `step_id` and `depends_on` are Rust-owned (5.1% and 2.4% of
    /// emitted characters) and a paraphrasing agent must not be able
    /// to detach a plan link or a dependency edge.
    #[test]
    fn identity_fields_survive_an_agent_that_omits_or_mangles_them() {
        let originals = vec![original(
            "audit-alloc",
            "Audit alloc path",
            "alloc-step",
            TodoStatus::Pending,
        )];
        let mut mangled = pending_row("audit-alloc", "Audit alloc path, take two");
        mangled.step_id = Some("some-other-step".into());
        mangled.depends_on = Some(vec!["invented".into()]);
        mangled.reason = Some("new rationale".into());

        let out = reconcile_update(&originals, update(vec![mangled]), None);
        assert_eq!(out.pending.len(), 1);
        let item = &out.pending[0];
        assert_eq!(item.step_id, "alloc-step");
        assert_eq!(item.depends_on, vec!["gate".to_string()]);
        assert_eq!(
            item.name, "Audit alloc path, take two",
            "prose is the agent's"
        );
        assert_eq!(item.reason, "new rationale");
    }

    #[test]
    fn parse_accepts_the_new_channels_and_rejects_unknown_ones() {
        let ok = r#"{"todo":[{"id":"a","name":"a","type":"review"}],
                     "newly_done":[{"id":"b","coverage":"read x.c:1-2"}],
                     "retired":[{"id":"c","reason":"subsumed"}]}"#;
        let parsed = parse_todo_update_full(ok).expect("parses");
        assert_eq!(parsed.todo.len(), 1);
        assert_eq!(parsed.newly_done[0].id, "b");
        assert_eq!(parsed.retired[0].reason, "subsumed");

        let bad = r#"{"todo":[],"newly_done":[{"id":"b","covrage":"typo"}]}"#;
        assert!(parse_todo_update_full(bad).is_err());
    }

    /// A numeric ceiling on pending work is an instruction to discard
    /// real work for a reason unrelated to its value. Retirement must
    /// be justified by the item, never by the list length.
    /// The stable half is one cached prefix: a member that changes
    /// between reaps rewrites the entry for every other member. Only
    /// fields that are byte-identical on the next call belong there.
    #[test]
    fn the_cached_prefix_holds_no_per_reap_field() {
        for volatile in ["completed", "current_todo"] {
            assert!(
                !UPDATE_TODO_STABLE_FIELDS.contains(&volatile),
                "`{volatile}` changes every reap and must stay in the delta half"
            );
        }
        assert!(UPDATE_TODO_STABLE_FIELDS.contains(&"instructions"));
        assert!(UPDATE_TODO_STABLE_FIELDS.contains(&"plan"));
    }

    /// The split is by key, so the reaped batch — task names, analyses
    /// and followups alike — must land in the delta document, after
    /// the cache breakpoint.
    #[test]
    fn the_completed_batch_is_split_into_the_delta_document() {
        let request = json!({
            "task": "update_todo",
            "instructions": "INSTRUCTIONS",
            "plan": {"steps": []},
            "completed": [{
                "query": "[review] Audit __alloc_pages_slowpath",
                "analysis": "SUMMARY",
            }],
            "current_todo": [],
        });
        let split =
            crate::prompt::split_request_documents(&request, UPDATE_TODO_STABLE_FIELDS).unwrap();
        assert!(!split.stable.contains("__alloc_pages_slowpath"));
        assert!(split.delta.contains("__alloc_pages_slowpath"));
        assert!(split.stable.contains("INSTRUCTIONS"));

        // Two reaps of different tasks must share the same prefix.
        let mut other = request.clone();
        other["completed"] = json!([{
            "query": "[review] Audit free_pcppages_bulk",
            "analysis": "OTHER",
        }]);
        other["current_todo"] = json!([{"id": "x", "name": "x", "status": "pending"}]);
        let split2 =
            crate::prompt::split_request_documents(&other, UPDATE_TODO_STABLE_FIELDS).unwrap();
        assert_eq!(split.stable, split2.stable);
    }

    /// Ordering left the todo agent's contract when prioritization
    /// became its own agent. Reordering the reply must not reorder the
    /// list, or "stable storage" is a lie and two agents fight over
    /// the same channel.
    #[test]
    fn the_agents_emission_order_does_not_reorder_the_list() {
        let originals = vec![
            original("first", "Audit alloc path", "", TodoStatus::Pending),
            original("second", "Audit free path", "", TodoStatus::Pending),
            original("third", "Audit pcp path", "", TodoStatus::Pending),
        ];
        // The agent replies in the exact reverse order.
        let out = reconcile_update(
            &originals,
            update(vec![
                pending_row("third", "Audit pcp path"),
                pending_row("second", "Audit free path"),
                pending_row("first", "Audit alloc path"),
            ]),
            None,
        );
        let ids: Vec<&str> = out.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }

    #[test]
    fn new_rows_are_appended_after_the_existing_ones() {
        let originals = vec![original(
            "existing",
            "Audit alloc path",
            "",
            TodoStatus::Pending,
        )];
        let mut fresh = pending_row("", "Audit a newly discovered helper");
        fresh.reason = Some("followup from this round".into());
        // Emitted first, but it is new, so it lands last.
        let out = reconcile_update(
            &originals,
            update(vec![fresh, pending_row("existing", "Audit alloc path")]),
            None,
        );
        let ids: Vec<&str> = out.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "existing");
        assert_eq!(out.pending[1].name, "Audit a newly discovered helper");
    }

    /// Prioritization moved to `crate::prioritize`. Any surviving
    /// ranking language here would have two agents ordering one list.
    #[test]
    fn todo_instructions_contain_no_prioritization_language() {
        let body = build_instructions(true);
        for banned in [
            "REPRIORITIZE",
            "MOST LIKELY",
            "expected payoff",
            "top-down",
            "in the order you want",
        ] {
            assert!(
                !body.contains(banned),
                "prioritization language survived: {banned}"
            );
        }
        assert!(
            body.contains("Order \nis NOT a channel") || body.contains("Order is NOT a channel")
        );
        assert!(body.contains("DEDUP ALGORITHM"), "dedup must stay");
        assert!(body.contains("COVERAGE FIELD"), "completion must stay");
    }

    /// Bug 1 of todo-bugs.md: the prompt asks for `{"id":"..."}` and
    /// `TodoItem` rejected it for a missing `name`. Five of six calls
    /// in the 2026-08-06 run paid a repair round trip for obeying the
    /// instructions.
    #[test]
    fn an_id_only_row_parses_and_leaves_prose_alone() {
        let parsed = parse_todo_update_full(r#"{"todo":[{"id":"keep-me"}]}"#)
            .expect("an id-only row is the documented shape");
        assert_eq!(parsed.todo.len(), 1);
        assert!(parsed.todo[0].name.is_none(), "absent means unchanged");

        let mut originals = vec![original(
            "keep-me",
            "Audit the allocator slow path",
            "step",
            TodoStatus::Pending,
        )];
        originals[0].reason = "the original rationale".into();

        let out = reconcile_update(&originals, update(vec![keep("keep-me")]), None);
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].name, "Audit the allocator slow path");
        assert_eq!(out.pending[0].reason, "the original rationale");
        assert_eq!(out.pending[0].step_id, "step");
    }

    /// Bug 2: the repair satisfied the missing required field by
    /// copying the id into `name`, and reconcile only guarded against
    /// an EMPTY name, so 27 of 28 rows ended the session with
    /// `name == id`. An absent name can no longer overwrite anything.
    #[test]
    fn a_blank_or_absent_name_cannot_erase_the_stored_title() {
        let originals = vec![original(
            "audit-alloc",
            "Audit the allocator slow path",
            "",
            TodoStatus::Pending,
        )];
        let mut blanked = keep("audit-alloc");
        blanked.name = Some("   ".into());
        let out = reconcile_update(&originals, update(vec![blanked]), None);
        assert_eq!(out.pending[0].name, "Audit the allocator slow path");
    }

    #[test]
    fn a_row_naming_nothing_and_carrying_no_name_is_dropped() {
        let originals = vec![original("real", "Audit alloc", "", TodoStatus::Pending)];
        let out = reconcile_update(&originals, update(vec![keep("ghost"), keep("real")]), None);
        let ids: Vec<&str> = out.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["real"],
            "an unresolvable, nameless row is not work"
        );
    }

    /// Bug 3: `mark_completed_todo` flips the reaped row to Done
    /// before the payload is built, so the agent saw a done row and
    /// never listed it in `newly_done`. Three of four completions in
    /// the 2026-08-06 run kept Rust's placeholder coverage, which is
    /// write-once and therefore permanent.
    #[test]
    fn mark_completed_flips_every_row_in_the_batch() {
        let mut list = vec![
            TodoItem {
                id: "a".into(),
                ..TodoItem::new("a", "review")
            },
            TodoItem {
                id: "b".into(),
                ..TodoItem::new("b", "review")
            },
            TodoItem {
                id: "c".into(),
                ..TodoItem::new("c", "review")
            },
        ];
        mark_completed_todo(&mut list, &["a", "c"]);
        assert_eq!(list[0].status, TodoStatus::Done);
        assert_eq!(list[1].status, TodoStatus::Pending);
        assert_eq!(list[2].status, TodoStatus::Done);
    }

    #[test]
    fn the_request_names_every_row_that_just_completed() {
        let body = build_instructions(true);
        assert!(body.contains("just_completed"));
        assert!(body.contains("its coverage is EMPTY"));
        // A batch completes more than one row, and each needs its own
        // coverage sentence drawn from its own task's analysis.
        assert!(body.contains("MUST return EVERY such id in `newly_done`"));
        assert!(body.contains("one per completed entry, not one for the batch"));
    }

    /// Parallel tasks rediscover the same gap. Reconciling the batch
    /// in one round is only worth it if the agent is told to pool the
    /// followups rather than dedup each entry in isolation.
    #[test]
    fn the_dedup_algorithm_pools_followups_across_the_batch() {
        let body = build_instructions(true);
        assert!(body.contains("EVERY `completed` entry, treating them as one pooled list"));
        assert!(body.contains("OTHER completed entries in this same round"));
    }

    #[test]
    fn instructions_show_the_id_only_row_and_forbid_padding() {
        let body = build_instructions(true);
        assert!(body.contains("exactly `{\"id\":\"ID\"}`"));
        assert!(body.contains("an omitted field means unchanged"));
        assert!(body.contains("never fill one in just to satisfy a shape"));
    }

    /// Bug 4: the retirement channel existed with no criteria, so
    /// the agent never used it. Criteria must stay reason-based —
    /// list length is explicitly not one, since the cap was removed
    /// in 00d3bd8 on purpose.
    #[test]
    fn retirement_criteria_are_about_the_item_not_the_list_length() {
        let body = build_instructions(true);
        assert!(body.contains("WHEN TO RETIRE"));
        assert!(body.contains("none of them is \"the list is long\""));
        assert!(body.contains("When in doubt keep the row"));
        assert!(!body.contains("Max 20"));
    }

    #[test]
    fn todo_instructions_do_not_cap_the_pending_list() {
        let body = build_instructions(true);
        assert!(!body.contains("Max 20"));
        assert!(body.contains("no limit on how many items may be pending"));
        assert!(body.contains("Never retire an item to hit a count"));
    }

    #[test]
    fn todo_instructions_state_that_omission_is_not_deletion() {
        let body = build_instructions(true);
        assert!(body.contains("does NOT delete"));
        assert!(body.contains("`retired`"));
        assert!(body.contains("Never put a done item here"));
        assert!(body.contains("FIELDS YOU DO NOT EMIT"));
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
        let got = parse_todo_update_full(text).unwrap();
        assert_eq!(got.todo.len(), 1);
        assert_eq!(got.todo[0].name.as_deref(), Some("x"));
    }

    #[test]
    fn parse_todo_response_rejects_transport_wrapper() {
        let text = r#"{"result":{"todo":[]}}"#;
        assert!(parse_todo_update_full(text).is_err());
    }

    #[test]
    fn parse_todo_response_rejects_entire_array_when_one_item_is_invalid() {
        let text = r#"{"todo":[{"id":"good","name":"good","type":"review","status":"pending"},{"id":42}]}"#;
        assert!(parse_todo_update_full(text).is_err());
    }

    #[test]
    fn parse_todo_response_rejects_embedded_object() {
        let text =
            r#"Here you go: {"todo": [{"name": "y", "type": "read", "status": "done"}]} bye."#;
        assert!(parse_todo_update_full(text).is_err());
    }

    #[test]
    fn parse_todo_response_rejects_multiple_candidates() {
        let text = r#"Example: {"todo":[{"name":"example","status":"pending"}]}
Actual: {"todo":[{"name":"actual","status":"done"}]}"#;
        assert!(parse_todo_update_full(text).is_err());
    }

    #[test]
    fn parse_todo_response_bad_json_returns_error() {
        assert!(parse_todo_update_full("not a json object").is_err());
        assert!(parse_todo_update_full("{}").is_err());
        let error = parse_todo_update_full(r#"{"todo":[]} trailing"#).unwrap_err();
        assert!(error.iter().any(|message| message.contains("trailing")));
    }

    #[test]
    fn parse_todo_response_rejects_unknown_item_fields() {
        let text = r#"{"todo":[{"name":"x","status":"pending","depends_onn":[]}]}"#;
        assert!(parse_todo_update_full(text).is_err());
    }

    #[test]
    fn todo_instructions_keep_trace_steps_pending_without_evidence() {
        let body = build_instructions(true);
        assert!(body.contains("trace unchanged callers, callees"));
        assert!(body.contains("status=done requires concrete cited evidence"));
        assert!(body.contains("Do NOT mark such a step done"));
        assert!(body.contains("no remaining users"));
    }

    /// The 2026-08-06 mm/page_alloc.c run handed the prioritizer 40
    /// ready rows, 27 of them with an id that was a 40-character
    /// mid-word slice of the name.
    #[test]
    fn synthesized_ids_are_slugs_not_mid_word_slices() {
        let mut items = vec![
            TodoItem::new(
                "Prove pcp->batch can never be 0 on a live pageset (zone_batchsize, zone_set_pageset_high_and_batch)",
                "review",
            ),
            TodoItem::new(
                "start_isolate_page_range()/undo_isolate_page_range()/set_pageblock_isolate",
                "review",
            ),
        ];
        assign_ids(&mut items);
        for item in &items {
            assert!(
                item.id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "id is not a slug: {:?}",
                item.id
            );
            assert!(
                !item.id.starts_with('-') && !item.id.ends_with('-'),
                "{:?}",
                item.id
            );
            assert!(item.id.len() <= TODO_ID_MAX_CHARS, "{:?}", item.id);
        }
        assert_eq!(
            items[0].id,
            "prove-pcp-batch-can-never-be-0-on-a-live-pageset"
        );
        assert_eq!(
            items[1].id,
            "start-isolate-page-range-undo-isolate-page-range"
        );
    }

    /// The collision path is where the old code actually broke: it
    /// re-cut the base to 37 chars, so row A's id changed depending on
    /// whether row B was in the same batch, and any `depends_on`
    /// minted against the pre-collision id dangled.
    #[test]
    fn a_collision_suffixes_the_slug_and_never_rewrites_the_first_row() {
        let long_a = "Audit the pageblock migratetype bitmap helpers and the alpha path";
        let long_b = "Audit the pageblock migratetype bitmap helpers and the beta path";
        // Both names share far more than 40 leading characters.
        assert_eq!(long_a[..48], long_b[..48]);

        let mut alone = vec![TodoItem::new(long_a, "review")];
        assign_ids(&mut alone);
        let solo_id = alone[0].id.clone();

        let mut together = vec![
            TodoItem::new(long_a, "review"),
            TodoItem::new(long_b, "review"),
        ];
        assign_ids(&mut together);

        assert_eq!(
            together[0].id, solo_id,
            "the first row's id must not depend on the second row's presence"
        );
        assert_ne!(together[0].id, together[1].id);
        assert_eq!(together[1].id, format!("{solo_id}-2"));
    }

    #[test]
    fn a_name_with_no_alphanumerics_still_yields_usable_ids() {
        let mut items = vec![
            TodoItem::new("///", "review"),
            TodoItem::new("***", "review"),
        ];
        assign_ids(&mut items);
        assert_eq!(items[0].id, "todo");
        assert_eq!(items[1].id, "todo-2");
    }

    #[test]
    fn a_single_oversized_token_is_cut_only_as_a_last_resort() {
        let name = "a".repeat(TODO_ID_MAX_CHARS + 20);
        assert_eq!(slugify_todo_id(&name).len(), TODO_ID_MAX_CHARS);
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

    /// `mark_completed_todo` deliberately leaves coverage empty so the
    /// agent can write the real sentence via `newly_done`. When the
    /// call fails there is no agent sentence, and a done row with no
    /// coverage is invisible to every later DEDUP pass.
    #[test]
    fn fallback_stamps_coverage_on_the_reaped_item() {
        let mut list = vec![TodoItem::new("Audit alloc path", "review")];
        list[0].id = "audit-alloc".into();
        mark_completed_todo(&mut list, &["audit-alloc"]);
        assert!(
            list[0].coverage.is_empty(),
            "empty until the agent writes it"
        );

        let merged = fallback_dedup(&list, &[]);
        assert_eq!(merged[0].status, TodoStatus::Done);
        assert!(
            !merged[0].coverage.is_empty(),
            "the fallback must not leave a done row uncovered"
        );
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

    /// A survey group row carries a Rust-authored instruction
    /// paragraph identical across every group, and names functions
    /// rather than files, so it has no path token to separate it from
    /// its siblings. That is precisely the input the prose heuristic
    /// mishandles, and it deleted three of kvm27's 49 groups.
    fn survey_group_row(n: u32, title: &str, members: &str) -> TodoItem {
        let mut item = TodoItem::new(
            format!("Audit {title}: {members}"),
            // `kind` is deliberately not "review": the group row is
            // identified by id, and kvm27's todo agent had dropped
            // the field from every surviving group row.
            "investigate",
        );
        item.step_id = format!("audit-group-{n:02}");
        item.id = format!("review-{}", item.step_id);
        item.reason = format!(
            "WHY THESE FUNCTIONS ARE ONE GROUP: {title}.\n\nRead and audit the \
             body of EVERY function in this list, and the neighbours they \
             call, for defects in this group's contract. Do not stop after \
             the first issue. Emit typed followups when more source, \
             callers, history, or API context is needed to be confident."
        );
        item
    }

    #[test]
    fn survey_group_rows_are_never_deduped_against_each_other() {
        // Shapes taken from the 2026-08-22 arch/x86/kvm/mmu/mmu.c run:
        // one large group, then the three small ones the heuristic
        // scored at 0.75, 0.73 and 0.72 overlap and removed.
        let rows = vec![
            survey_group_row(
                6,
                "MMU context initialization",
                "__kvm_mmu_refresh_passthrough_bits, init_kvm_nested_mmu, \
                 kvm_init_mmu, kvm_mmu_reset_context, reset_guest_paging_metadata",
            ),
            survey_group_row(
                19,
                "MMU role computation",
                "is_cr0_pg, is_cr4_pae, kvm_calc_cpu_role",
            ),
            survey_group_row(
                33,
                "Pre-fault mapping ioctl support",
                "kvm_arch_vcpu_pre_fault_memory, kvm_tdp_map_page",
            ),
            survey_group_row(
                49,
                "Guest page-table root accessors",
                "get_guest_cr3, kvm_mmu_get_guest_pgd",
            ),
        ];
        let expected: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();

        let kept = dedup_pending_rows(&[], rows);

        let got: Vec<String> = kept.iter().map(|r| r.id.clone()).collect();
        assert_eq!(
            got, expected,
            "every group is a disjoint slice of one file's function list; \
             none may be dropped as a near-duplicate of another"
        );
    }

    #[test]
    fn a_group_row_does_not_suppress_an_ordinary_followup() {
        // The exemption is symmetric: a group row must not enter the
        // reference set either, or its boilerplate becomes a template
        // that swallows short followups raised while auditing it.
        let mut followup = TodoItem::new(
            "Read and audit the body of every function in this list for \
             contract defects",
            "review",
        );
        followup.id = "some-followup".into();
        followup.step_id = "audit-group-19".into();

        let rows = vec![
            survey_group_row(19, "MMU role computation", "is_cr0_pg, is_cr4_pae"),
            followup,
        ];
        let kept = dedup_pending_rows(&[], rows);
        assert_eq!(kept.len(), 2, "followup was swallowed by group boilerplate");
    }

    #[test]
    fn the_api_failure_path_does_not_swallow_a_followup_either() {
        // fallback_dedup is a second copy of the same heuristic, taken
        // when the todo call fails or the session is cancelled. It
        // cannot delete a group row, but a group row left in its
        // reference set eats the followups that group's own audit
        // raised.
        let existing = vec![survey_group_row(
            19,
            "MMU role computation",
            "is_cr0_pg, is_cr4_pae",
        )];
        let arriving = vec![serde_json::json!({
            "type": "source",
            "name": "Read and audit the body of every function in this list \
                     for contract defects",
            "reason": "the group's contract is established elsewhere",
        })];
        let merged = fallback_dedup(&existing, &arriving);
        assert_eq!(
            merged.len(),
            2,
            "followup was swallowed by group boilerplate"
        );
    }

    #[test]
    fn ordinary_prose_rows_are_still_deduped() {
        // The exemption must not disable the backstop for the rows it
        // was written for.
        let a = TodoItem::new("Investigate slab corruption in the free path", "review");
        let b = TodoItem::new("Investigate slab corruption in the free path", "review");
        let kept = dedup_pending_rows(&[], vec![a, b]);
        assert_eq!(kept.len(), 1);
    }
}
