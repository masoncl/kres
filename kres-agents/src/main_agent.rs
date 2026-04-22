//! Main agent: LLM-driven data fetcher.
//!
//! Port of loop.
//! On each `fetch(followups)` call:
//!
//! 1. Serialise `{user_query, task_brief?, code_agent_analysis,
//!    code_agent_followups}` as the opening user message.
//! 2. Send it to the main-agent LLM.
//! 3. Parse `<actions>` / `<action>` tags from the assistant reply
//!    (`parse_actions`).
//! 4. Dispatch the actions — MCP calls to the same server batched into
//!    one `tools/call_bulk` round, non-MCP calls run in parallel.
//! 5. Append `{tools: N, result: combined}` as a new user message.
//! 6. Repeat until the agent returns no actions or `max_main_turns`
//!    rounds elapse.
//! 7. Return the accumulated (symbols, context) to the caller.
//!
//! The MainAgent is a drop-in replacement for `WorkspaceFetcher` /
//! `McpFetcher` — it implements `DataFetcher`. The rule-based fetchers
//! stay available (and are in fact used as the fallback inside
//! MainAgent for dispatching non-MCP tools).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use kres_core::cost::UsageTracker;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};
use kres_mcp::McpClient;

use crate::{
    error::AgentError,
    followup::Followup,
    pipeline::{DataFetcher, FetchResult},
    symbol::{
        append_context, append_symbol, parse_semcode_symbol, propagate_tool_result, tool_source,
    },
    tools::{
        bash_run, edit_file, find, git, grep, read_file_range, truncate_output, BashArgs, EditArgs,
        FindArgs, GitArgs, GrepArgs, ReadArgs, TOOL_OUTPUT_CAP_GREP_FIND, TOOL_OUTPUT_CAP_MCP,
    },
};

/// Default per-fetch turn cap ( `max_main_turns = 5` at
///).
pub const DEFAULT_MAX_MAIN_TURNS: u8 = 5;

/// Configuration for the main agent. The `system` prompt is augmented
/// at construction time with the live list of MCP tool descriptions.
pub struct MainAgent {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub max_main_turns: u8,
    /// Per-fetch "user query" — the top-level prompt that spawned the
    /// current task. Set by the orchestrator via a per-task clone
    /// (future work); defaults to empty for now.
    pub user_query: String,
    /// Per-task brief — short human label for the current todo item.
    /// When non-empty and distinct from `user_query`, it is included
    /// alongside `user_query` in the main-agent's opening message.
    pub task_brief: String,
    pub workspace: PathBuf,
    /// MCP servers keyed by name. Multiple-server routing is supported
    /// — a main-agent `mcp` action's `server` field picks the handle.
    pub mcp_servers: HashMap<String, Arc<Mutex<McpClient>>>,
    pub logger: Option<Arc<TurnLogger>>,
    pub usage: Option<Arc<UsageTracker>>,
    /// Allowlist of non-MCP action types the main agent is permitted
    /// to dispatch this session. An emitted action whose `type` is
    /// not in the set is rejected with an error string that names
    /// the alternatives and points at `--allow`/`settings.json`.
    /// Empty = allow all (for tests and callers that don't care).
    /// Resolved from settings.json layered with `--allow` CLI flags
    /// by `kres-repl::Settings::effective_allowed_actions`.
    pub allowed_actions: Arc<std::collections::BTreeSet<String>>,
}

impl MainAgent {
    /// Build a system-prompt suffix describing the MCP tools the agent
    /// can dispatch. Matches
    pub async fn mcp_tool_descriptions(&self) -> String {
        if self.mcp_servers.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\nAvailable MCP tools:\n");
        // Stable iteration order so prompt caching stays hit-hit-hit
        // instead of random-shuffled across runs.
        let mut names: Vec<&String> = self.mcp_servers.keys().collect();
        names.sort();
        for name in names {
            let guard = self.mcp_servers.get(name).unwrap().lock().await;
            let tools = guard.tools();
            if tools.is_empty() {
                continue;
            }
            out.push_str(&format!("\n### {name}\n"));
            for t in tools {
                let tname = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let tdesc = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                out.push_str(&format!("- `{tname}`"));
                if !tdesc.is_empty() {
                    let one_line: String = tdesc.split('\n').next().unwrap_or("").into();
                    out.push_str(": ");
                    out.push_str(&one_line);
                }
                out.push('\n');
            }
        }
        out
    }

    /// Splice the MCP tool description suffix onto the system prompt,
    /// returning the effective system string for this fetch.
    async fn effective_system(&self) -> Option<String> {
        let suffix = self.mcp_tool_descriptions().await;
        match (&self.system, suffix.is_empty()) {
            (Some(s), false) => Some(format!("{s}{suffix}")),
            (Some(s), true) => Some(s.clone()),
            (None, false) => Some(suffix.trim_start().to_string()),
            (None, true) => None,
        }
    }

    fn log_user(&self, content: &str) {
        if let Some(lg) = &self.logger {
            lg.log_main("user", content, None, None);
        }
    }

    fn log_assistant(&self, content: &str, u: &kres_llm::request::Usage) {
        if let Some(lg) = &self.logger {
            lg.log_main(
                "assistant",
                content,
                Some(LoggedUsage {
                    input: u.input_tokens,
                    output: u.output_tokens,
                    cache_creation: u.cache_creation_input_tokens,
                    cache_read: u.cache_read_input_tokens,
                }),
                None,
            );
        }
    }
}

#[async_trait]
impl DataFetcher for MainAgent {
    async fn fetch(
        &self,
        followups: &[Followup],
        plan: Option<&kres_core::Plan>,
    ) -> Result<FetchResult, AgentError> {
        let mut symbols: Vec<Value> = Vec::new();
        let mut context: Vec<Value> = Vec::new();

        let mut main_payload = serde_json::Map::new();
        if !self.user_query.is_empty() {
            main_payload.insert("user_query".into(), json!(self.user_query));
        }
        if !self.task_brief.is_empty() && self.task_brief != self.user_query {
            main_payload.insert("task_brief".into(), json!(self.task_brief));
        }
        // Include the plan the caller handed in so the main-agent
        // LLM sees the same decomposition the fast + slow agents
        // see. Per-call delivery (vs a shared slot) means two
        // concurrent tasks with different plans cannot clobber each
        // other's snapshot between the push and the LLM call.
        if let Some(plan) = plan {
            if let Ok(v) = serde_json::to_value(plan) {
                main_payload.insert("plan".into(), v);
            }
        }
        main_payload.insert("code_agent_followups".into(), json!(followups));
        let opening = serde_json::to_string_pretty(&Value::Object(main_payload))?;

        let effective_system = self.effective_system().await;
        let mut cfg = CallConfig::defaults_for(self.model.clone()).with_max_tokens(self.max_tokens);
        if let Some(s) = effective_system {
            cfg = cfg.with_system(s);
        }
        if let Some(n) = self.max_input_tokens {
            cfg = cfg.with_max_input_tokens(n);
        }

        let mut history: Vec<Message> = vec![Message {
            role: "user".into(),
            content: opening.clone(),
            cache: false,
            cached_prefix: None,
        }];
        self.log_user(&opening);

        kres_core::async_eprintln!(
            "[main] fetch: {} followup(s) from fast, {} MCP server(s) configured",
            followups.len(),
            self.mcp_servers.len()
        );
        for turn in 0..self.max_main_turns {
            tracing::debug!(
                target: "kres_agents::main_agent",
                turn = turn + 1,
                max = self.max_main_turns,
                "main agent turn"
            );
            kres_core::async_eprintln!(
                "[main turn {}/{}] history={} messages ({}k chars total)",
                turn + 1,
                self.max_main_turns,
                history.len(),
                history.iter().map(|m| m.content.len()).sum::<usize>() / 1000,
            );
            // §cache: keep BOTH the most-recent and the
            // second-most-recent user turn marked. Anthropic's
            // cache_read only fires at cache_control check points,
            // so if round N marks only its own tail, there's no
            // check point at the prior-round boundary where cache
            // was actually written — result: 0 cache reads. Keeping
            // two markers gives the server two check points:
            //   - at the older boundary → cache HIT (read prior write)
            //   - at the new tail       → cache MISS (write fresh)
            // Anthropic's per-request cap is 4 cache_control blocks
            // (system + up to 3 messages), so `n=2` stays within
            // budget whether system is cached or not.
            kres_llm::request::mark_last_n_user_cached(&mut history, 2);
            let turn_cfg = cfg
                .clone()
                .with_stream_label(format!("main turn {}", turn + 1));
            let resp = self
                .client
                .messages_streaming(&turn_cfg, &history)
                .await
                .map_err(|e| AgentError::Other(e.to_string()))?;
            if let Some(t) = &self.usage {
                t.record(
                    "main",
                    &self.model.id,
                    resp.usage.input_tokens,
                    resp.usage.output_tokens,
                    resp.usage.cache_creation_input_tokens,
                    resp.usage.cache_read_input_tokens,
                );
            }
            let text = extract_text(&resp);
            self.log_assistant(&text, &resp.usage);
            history.push(Message {
                role: "assistant".into(),
                content: text.clone(),
                cache: false,
                cached_prefix: None,
            });
            kres_core::async_eprintln!(
                "[main turn {}/{}] reply: in={} out={} cache_read={} cache_create={} ({} chars)",
                turn + 1,
                self.max_main_turns,
                resp.usage.input_tokens,
                resp.usage.output_tokens,
                resp.usage.cache_read_input_tokens,
                resp.usage.cache_creation_input_tokens,
                text.len(),
            );
            let (actions, _display) = parse_actions(&text);
            if actions.is_empty() {
                kres_core::async_eprintln!(
                    "[main turn {}/{}] no <actions> — done; accumulated {} symbol(s), {} context item(s)",
                    turn + 1,
                    self.max_main_turns,
                    symbols.len(),
                    context.len(),
                );
                break;
            }
            let labels: Vec<String> = actions.iter().take(6).map(action_label).collect();
            let tail = if actions.len() > 6 {
                format!(", +{} more", actions.len() - 6)
            } else {
                String::new()
            };
            kres_core::async_eprintln!(
                "[main turn {}/{}] dispatching {} action(s): {}{tail}",
                turn + 1,
                self.max_main_turns,
                actions.len(),
                labels.join(", "),
            );
            let combined = self.run_actions(&actions, &mut symbols, &mut context).await;
            kres_core::async_eprintln!(
                "[main turn {}/{}] tool output: {}k chars combined ({} symbols, {} context accumulated)",
                turn + 1,
                self.max_main_turns,
                combined.len() / 1000,
                symbols.len(),
                context.len(),
            );
            let tool_msg = json!({"tools": actions.len(), "result": combined}).to_string();
            self.log_user(&tool_msg);
            history.push(Message {
                role: "user".into(),
                content: tool_msg,
                cache: false,
                cached_prefix: None,
            });
        }

        Ok(FetchResult { symbols, context })
    }
}

impl MainAgent {
    /// Execute one round of actions. Groups MCP calls by server so
    /// they can be batched (§13); non-MCP calls run in parallel via
    /// `futures::join_all`. Every output is routed through
    /// `propagate_tool_result` — no silent drops (§39).
    ///
    /// Returns the human-readable combined output that gets fed back
    /// to the main agent as its next user turn.
    async fn run_actions(
        &self,
        actions: &[Value],
        symbols: &mut Vec<Value>,
        context: &mut Vec<Value>,
    ) -> String {
        let mut per_action: Vec<(usize, String)> = Vec::with_capacity(actions.len());
        // Group MCP actions by server to minimize handshake overhead.
        let mut mcp_by_server: HashMap<String, Vec<(usize, String, Value)>> = HashMap::new();
        let mut non_mcp: Vec<(usize, Value)> = Vec::new();

        for (i, action) in actions.iter().enumerate() {
            let ty = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty == "mcp" {
                let server = action
                    .get("server")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool = action
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = action.get("args").cloned().unwrap_or(json!({}));
                mcp_by_server
                    .entry(server)
                    .or_default()
                    .push((i, tool, args));
            } else {
                non_mcp.push((i, action.clone()));
            }
        }

        // MCP batches: serialised per-server (kres's McpClient is
        // single-writer today; see §13 for planned pipelining).
        for (server, calls) in &mcp_by_server {
            let client_opt = self.mcp_servers.get(server);
            for (idx, tool, args) in calls {
                let (text, sym) = match client_opt {
                    None => (format!("MCP server not found: {server}"), None),
                    Some(c) => {
                        let mut guard = c.lock().await;
                        match guard.call_tool(tool, args).await {
                            Ok(t) => {
                                let t = truncate_output(&t, TOOL_OUTPUT_CAP_MCP);
                                let sym = if tool == "find_function" || tool == "find_type" {
                                    parse_semcode_symbol(&t, tool)
                                } else {
                                    None
                                };
                                (t, sym)
                            }
                            Err(e) => (format!("{e}"), None),
                        }
                    }
                };
                let source = format!("{server}/{tool}");
                propagate_tool_result(&text, sym, &source, symbols, context);
                per_action.push((*idx, text));
            }
        }

        // Non-MCP actions: run concurrently.
        let mut non_mcp_futures = Vec::with_capacity(non_mcp.len());
        for (idx, action) in non_mcp {
            let ws = self.workspace.clone();
            let allowed = self.allowed_actions.clone();
            non_mcp_futures.push(async move {
                let out = dispatch_non_mcp(&ws, &action, &allowed).await;
                (idx, action, out)
            });
        }
        let nm_results = futures::future::join_all(non_mcp_futures).await;
        for (idx, action, (text, sym)) in nm_results {
            let source = tool_source(&action);
            match sym {
                Some(s) => {
                    append_symbol(symbols, s);
                }
                None => {
                    append_context(
                        context,
                        json!({"source": source.clone(), "content": text.clone()}),
                    );
                }
            }
            per_action.push((idx, text));
        }

        // Stitch outputs back in original order.
        per_action.sort_by_key(|(i, _)| *i);
        let mut combined = String::new();
        for (n, (orig_idx, text)) in per_action.iter().enumerate() {
            if n > 0 {
                combined.push_str("\n\n");
            }
            let label = action_label(&actions[*orig_idx]);
            combined.push_str("--- ");
            combined.push_str(&label);
            combined.push_str(" ---\n");
            combined.push_str(text);
        }
        combined
    }
}

/// Dispatch a single non-MCP action. Returns (text_output, optional
/// symbol). Actions with unknown types land in context with an error
/// message so they don't silently vanish.
///
/// `allowed_actions` is the session allowlist (resolved from
/// settings.json + CLI `--allow`). Every action's `type` must be
/// present in the set; an empty set means "deny all non-MCP
/// actions" (the explicit `"allowed": []` in settings.json
/// semantic). Malformed actions (missing `type` field) bypass the
/// gate so they hit the existing unknown-type error below — a
/// malformed action is not a gated action and deserves a clearer
/// error. MCP actions are gated separately (by server registration)
/// and don't enter this function.
async fn dispatch_non_mcp(
    workspace: &std::path::Path,
    action: &Value,
    allowed_actions: &std::collections::BTreeSet<String>,
) -> (String, Option<Value>) {
    let ty = action.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    if ty != "?" && !allowed_actions.contains(ty) {
        let allowed_list: Vec<&str> = allowed_actions.iter().map(|s| s.as_str()).collect();
        let list_display = if allowed_list.is_empty() {
            "none — every non-MCP action is denied this session".to_string()
        } else {
            allowed_list.join(", ")
        };
        return (
            format!(
                "[error] action type '{ty}' is not in the allowed-action list for this session ({list_display}). To enable it, add `{ty}` to `actions.allowed` in ~/.kres/settings.json (or <cwd>/.kres/settings.json) or re-run kres with `--allow {ty}`."
            ),
            None,
        );
    }
    match ty {
        "grep" => {
            let args = GrepArgs {
                pattern: action
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                path: action
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                limit: action
                    .get("max_count")
                    .or_else(|| action.get("limit"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                glob: action
                    .get("glob")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            };
            match grep(workspace, &args).await {
                Ok(t) => (t, None),
                Err(e) => (format!("[error] {e}"), None),
            }
        }
        "find" => {
            // `name` is the canonical `-name` glob. Accept `pattern`
            // and `glob` as aliases: the model naturally reaches for
            // `pattern` because `grep` uses that key, and session
            // 9fee284e (2026-04-21) burned a turn when a bare
            // `{"type":"find","pattern":"report.md"}` ran find with no
            // filter at all and dumped the whole workspace tree.
            let args = FindArgs {
                path: action
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                name: action
                    .get("name")
                    .or_else(|| action.get("pattern"))
                    .or_else(|| action.get("glob"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
                kind: action
                    .get("file_type")
                    .or_else(|| action.get("kind"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
            };
            match find(workspace, &args).await {
                Ok(t) => (t, None),
                Err(e) => (format!("[error] {e}"), None),
            }
        }
        "read" => {
            let file = action
                .get("file")
                .and_then(|v| v.as_str())
                .or_else(|| action.get("path").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            let args = ReadArgs {
                file: file.clone(),
                line: action
                    .get("line")
                    .or_else(|| action.get("startLine"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                count: action
                    .get("count")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                end_line: action
                    .get("end_line")
                    .or_else(|| action.get("endLine"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
            };
            match read_file_range(workspace, &args) {
                Ok(def) => {
                    let start = args.line.unwrap_or(1);
                    let line_count = def.matches('\n').count() as u32;
                    let basename = std::path::Path::new(&file)
                        .file_name()
                        .map(|o| o.to_string_lossy().into_owned())
                        .unwrap_or_else(|| file.clone());
                    let name = action
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("{basename}:{start}-{}", start + line_count));
                    let sym_type = action
                        .get("symbol_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("function")
                        .to_string();
                    let sym = json!({
                        "name": name,
                        "type": sym_type,
                        "filename": file,
                        "line": start,
                        "definition": def.clone(),
                    });
                    // Include the actual content in the turn-log
                    // text. The symbol pool (Some(sym) below) carries
                    // `def` to the slow agent, but the main agent
                    // itself reads `text` to decide its next action
                    // — and without the bytes inline it can't. Before
                    // this, a `read lines 270-370` came back as
                    // "Read file:270-370 (2584 chars), symbol 'x'"
                    // with no content, so the model resorted to
                    // `bash sed -n '270,370p' ...` which DOES put the
                    // bytes in [stdout] (session 04365bed, turn 3 of
                    // 2026-04-21). Truncate at the same envelope
                    // grep/find use so a runaway whole-file read
                    // can't blow the turn budget.
                    let body = truncate_output(&def, TOOL_OUTPUT_CAP_GREP_FIND);
                    let header = format!(
                        "Read {}:{}-{} ({} chars), symbol '{}'",
                        sym.get("filename").and_then(|v| v.as_str()).unwrap_or(""),
                        start,
                        start + line_count,
                        def.len(),
                        sym.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    let text = format!("{header}\n{body}");
                    (text, Some(sym))
                }
                Err(e) => (format!("[error] {e}"), None),
            }
        }
        "git" => {
            let args = GitArgs {
                command: action
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            };
            match git(workspace, &args).await {
                Ok(t) => (t, None),
                Err(e) => (format!("[error] {e}"), None),
            }
        }
        "edit" => {
            // Accept Claude-Code-style `file_path` + `old_string` +
            // `new_string`; allow `path` and `file` as aliases for
            // the path so follow-up-shape requests work.
            let file_path = action
                .get("file_path")
                .or_else(|| action.get("path"))
                .or_else(|| action.get("file"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let old_string = action
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let new_string = action
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let replace_all = action
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let args = EditArgs {
                file_path,
                old_string,
                new_string,
                replace_all,
            };
            match edit_file(workspace, &args).await {
                Ok(t) => (t, None),
                Err(e) => (format!("[error] {e}"), None),
            }
        }
        "bash" => {
            // Accept `command`, `cmd`, and `name` — `name` is what
            // the slow/fast agents emit when a bash call comes in as
            // a followup ({type:"bash", name:"cc -o hw hw.c && ./hw",
            // reason:"verify"}) since followup schema uses `name` for
            // the primary argument.
            let command = action
                .get("command")
                .or_else(|| action.get("cmd"))
                .or_else(|| action.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let timeout_secs = action
                .get("timeout_secs")
                .or_else(|| action.get("timeout"))
                .and_then(|v| v.as_u64());
            let cwd = action.get("cwd").and_then(|v| v.as_str()).map(String::from);
            let args = BashArgs {
                command,
                timeout_secs,
                cwd,
            };
            match bash_run(workspace, &args).await {
                Ok(t) => (t, None),
                Err(e) => (format!("[error] {e}"), None),
            }
        }
        other => (format!("unknown action type: {other}"), None),
    }
}

fn action_label(action: &Value) -> String {
    let ty = action.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    match ty {
        "mcp" => format!(
            "{}/{}",
            action.get("server").and_then(|v| v.as_str()).unwrap_or("?"),
            action.get("tool").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        "grep" | "find" => format!(
            "{ty} {}",
            action
                .get("pattern")
                .or_else(|| action.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ),
        "read" => {
            let rp = action
                .get("file")
                .and_then(|v| v.as_str())
                .or_else(|| action.get("path").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let rl = action
                .get("line")
                .or_else(|| action.get("startLine"))
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into());
            format!("read {rp}:{rl}")
        }
        "git" => format!(
            "git {}",
            action
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        ),
        "bash" => format!(
            "bash {}",
            action
                .get("command")
                .or_else(|| action.get("cmd"))
                .or_else(|| action.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        ),
        "edit" => format!(
            "edit {}",
            action
                .get("file_path")
                .or_else(|| action.get("path"))
                .or_else(|| action.get("file"))
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        ),
        other => other.to_string(),
    }
}

/// Extract `<actions>[...]</actions>` or `<action>{...}</action>` from
/// the main-agent's text reply. Returns `(list_of_actions, display)`
/// where `display` is the text with the tag-wrapped JSON stripped out
/// .
pub fn parse_actions(text: &str) -> (Vec<Value>, String) {
    if let Some((start, end, inner)) = find_tag_body(text, "actions") {
        let trimmed = inner.trim();
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(trimmed) {
            let mut display = String::new();
            display.push_str(&text[..start]);
            display.push_str(&text[end..]);
            return (arr, display.trim().to_string());
        }
    }
    if let Some((start, end, inner)) = find_tag_body(text, "action") {
        let trimmed = inner.trim();
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            let mut display = String::new();
            display.push_str(&text[..start]);
            display.push_str(&text[end..]);
            return (vec![v], display.trim().to_string());
        }
    }
    (Vec::new(), text.to_string())
}

/// Find the outer `<tag>...</tag>` range in `text`. Returns the byte
/// range of the ENTIRE match (outer-tag-inclusive) plus the inner body.
fn find_tag_body<'a>(text: &'a str, tag: &str) -> Option<(usize, usize, &'a str)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start_open = text.find(&open)?;
    let body_start = start_open + open.len();
    let end_close_rel = text[body_start..].find(&close)?;
    let body = &text[body_start..body_start + end_close_rel];
    let end_close = body_start + end_close_rel + close.len();
    Some((start_open, end_close, body))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_actions_single() {
        let text = "some prose\n<action>{\"type\":\"grep\",\"pattern\":\"foo\"}</action>\ntrailing";
        let (a, disp) = parse_actions(text);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].get("type").unwrap(), "grep");
        assert!(disp.contains("some prose"));
        assert!(disp.contains("trailing"));
        assert!(!disp.contains("<action>"));
    }

    #[test]
    fn parse_actions_plural() {
        let text = "<actions>[{\"type\":\"grep\",\"pattern\":\"x\"},{\"type\":\"read\",\"file\":\"a.c\"}]</actions>";
        let (a, disp) = parse_actions(text);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].get("type").unwrap(), "grep");
        assert_eq!(a[1].get("type").unwrap(), "read");
        assert_eq!(disp, "");
    }

    #[test]
    fn parse_actions_malformed_returns_empty() {
        let text = "<action>{not json}</action>";
        let (a, _) = parse_actions(text);
        assert!(a.is_empty());
    }

    #[test]
    fn parse_actions_no_tag_returns_empty() {
        let (a, d) = parse_actions("just prose here");
        assert!(a.is_empty());
        assert_eq!(d, "just prose here");
    }

    #[tokio::test]
    async fn mcp_tool_descriptions_empty_when_no_servers() {
        let a = MainAgent {
            client: Arc::new(Client::new("sk-unused").unwrap()),
            model: Model::opus_4_7(),
            system: None,
            max_tokens: 1000,
            max_input_tokens: None,
            max_main_turns: 1,
            user_query: "q".into(),
            task_brief: String::new(),
            workspace: PathBuf::from("/tmp"),
            mcp_servers: HashMap::new(),
            logger: None,
            usage: None,
            allowed_actions: Arc::new(std::collections::BTreeSet::new()),
        };
        let s = a.mcp_tool_descriptions().await;
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn find_accepts_pattern_alias_for_name() {
        // Regression: the dispatcher used to read only `name`, so a
        // model-emitted {"type":"find","pattern":"report.md"} ran
        // find(1) with no -name filter and dumped the workspace tree.
        let tmp = std::env::temp_dir().join(format!("kres-find-pattern-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("report.md"), b"").unwrap();
        std::fs::write(tmp.join("other.md"), b"").unwrap();
        let action = json!({"type":"find","pattern":"report.md"});
        let allow: std::collections::BTreeSet<String> =
            ["find"].iter().map(|s| s.to_string()).collect();
        let (out, _) = dispatch_non_mcp(&tmp, &action, &allow).await;
        assert!(out.contains("report.md"), "output missing report.md: {out}");
        assert!(
            !out.contains("other.md"),
            "filter not applied, got other.md: {out}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn read_text_result_contains_file_body() {
        // Regression: session 04365bed (2026-04-21 turn 3/5) called
        // `read lines 270-370` twice, each time received only a
        // `Read file:N-M (X chars), symbol '...'` header with NO
        // content in the turn-log text — the bytes were going into
        // the symbol pool only. Model gave up and used `bash sed`.
        // The turn-log text must carry the content inline.
        let tmp = std::env::temp_dir().join(format!("kres-read-text-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let body = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
        std::fs::write(tmp.join("f.txt"), body).unwrap();
        let action = json!({"type":"read","file":"f.txt","line":3,"end_line":5});
        let allow: std::collections::BTreeSet<String> =
            ["read"].iter().map(|s| s.to_string()).collect();
        let (text, _sym) = dispatch_non_mcp(&tmp, &action, &allow).await;
        assert!(text.contains("line 3\n"), "no body in text: {text:?}");
        assert!(text.contains("line 5\n"), "no body in text: {text:?}");
        // Header still present for at-a-glance scanning.
        assert!(text.contains("Read f.txt:"), "no header: {text:?}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn read_accepts_end_line_snake_case() {
        // The main-agent prompt advertises `end_line` as the canonical
        // arg name, but the dispatcher used to only look up `endLine`.
        let tmp = std::env::temp_dir().join(format!("kres-read-snake-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let body = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
        std::fs::write(tmp.join("f.txt"), body).unwrap();
        let action = json!({"type":"read","file":"f.txt","line":3,"end_line":5});
        let allow: std::collections::BTreeSet<String> =
            ["read"].iter().map(|s| s.to_string()).collect();
        let (out, sym) = dispatch_non_mcp(&tmp, &action, &allow).await;
        // dispatch returns a short summary string; the actual body
        // lands on `sym.definition`.
        assert!(!out.starts_with("[error]"), "unexpected error: {out}");
        let def = sym
            .as_ref()
            .and_then(|v| v.get("definition"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(def.contains("line 3\n"), "def missing line 3: {def:?}");
        assert!(def.contains("line 5\n"), "def missing line 5: {def:?}");
        assert!(
            !def.contains("line 6\n"),
            "def leaked line 6 past end_line: {def:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn dispatch_rejects_action_not_in_allowlist() {
        // With a non-empty allowlist that excludes "bash", a bash
        // action must bounce with an error that names the allowed
        // set and points at --allow / settings.json.
        let tmp = std::env::temp_dir().join(format!("kres-gate-bash-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let action = json!({"type":"bash","command":"echo should not run > /tmp/gated"});
        let mut allow = std::collections::BTreeSet::new();
        allow.insert("read".to_string());
        allow.insert("grep".to_string());
        let (out, sym) = dispatch_non_mcp(&tmp, &action, &allow).await;
        assert!(sym.is_none(), "gated action shouldn't emit a symbol");
        assert!(out.contains("[error]"), "got {out}");
        assert!(
            out.contains("'bash' is not in the allowed-action list"),
            "error missing action name: {out}"
        );
        assert!(
            out.contains("--allow bash"),
            "error missing fix hint: {out}"
        );
        assert!(
            out.contains("settings.json"),
            "error missing settings hint: {out}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn empty_allowlist_denies_all_actions() {
        // Contract: an empty allowlist means "deny every non-MCP
        // action" — it is the explicit `"allowed": []` in
        // settings.json semantic. Previously the dispatcher
        // short-circuited on is_empty() and allowed everything,
        // which silently neutered an operator's lockdown.
        let tmp = std::env::temp_dir().join(format!("kres-gate-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("f.txt"), "hello\n").unwrap();
        let action = json!({"type":"read","file":"f.txt"});
        let allow = std::collections::BTreeSet::new();
        let (out, sym) = dispatch_non_mcp(&tmp, &action, &allow).await;
        assert!(sym.is_none(), "denied action shouldn't emit a symbol");
        assert!(out.contains("[error]"), "expected error, got {out}");
        assert!(
            out.contains("'read' is not in the allowed-action list"),
            "expected deny message, got {out}"
        );
        assert!(
            out.contains("none — every non-MCP action is denied"),
            "expected empty-list message, got {out}"
        );
        // The file we wrote should remain unread (the dispatcher
        // bailed before touching it).
        assert!(!out.contains("hello"), "read tool ran despite deny: {out}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn malformed_action_reports_unknown_not_gated() {
        // An action with no `type` field should hit the existing
        // "unknown action type" error, NOT the allowlist-gate error.
        // A malformed action is not a gated action.
        let tmp = std::env::temp_dir().join(format!("kres-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let action = json!({"command": "nope"}); // no `type` field
        let allow: std::collections::BTreeSet<String> =
            ["read", "grep"].iter().map(|s| s.to_string()).collect();
        let (out, _) = dispatch_non_mcp(&tmp, &action, &allow).await;
        assert!(
            out.contains("unknown action type"),
            "expected unknown-type error, got {out}"
        );
        assert!(
            !out.contains("not in the allowed-action list"),
            "unexpected allowlist error for malformed action: {out}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn action_label_covers_each_kind() {
        assert_eq!(
            action_label(&json!({"type":"mcp","server":"semcode","tool":"find_function"})),
            "semcode/find_function"
        );
        assert_eq!(
            action_label(&json!({"type":"grep","pattern":"foo"})),
            "grep foo"
        );
        assert_eq!(
            action_label(&json!({"type":"read","file":"a.c","line":10})),
            "read a.c:10"
        );
        assert_eq!(
            action_label(&json!({"type":"git","command":"log -1"})),
            "git log -1"
        );
    }
}
