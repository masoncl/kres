//! MCP-backed DataFetcher that delegates to a WorkspaceFetcher for
//! tool kinds an MCP server doesn't handle.
//!
//! Routes followups based on `kind`:
//! - `source` → MCP `find_function`, falling back to local grep if the
//!   server returns empty, indexing/unavailable text, or errors.
//! - `type` → MCP `find_type`, falling back to grep+read if the server
//!   returns empty, indexing/unavailable text, or errors.
//! - `callers` → MCP `find_callers`, falling back to local grep when
//!   the callgraph is unavailable.
//! - `callees` → MCP `find_calls`, falling back to local grep when
//!   the callgraph is unavailable.
//! - `file` → MCP `find_files` if the server offers it; otherwise falls
//!   back to the inner fetcher's `search` for the pattern.
//! - Everything else → inner fetcher.
//!
//! The MCP client is wrapped in a `Mutex` so sequential tool-call
//! semantics are preserved (bugs.md#M10's timeout lives inside kres-mcp).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use kres_mcp::McpClient;

use crate::{
    error::AgentError,
    fetcher::WorkspaceFetcher,
    followup::Followup,
    pipeline::{DataFetcher, FetchResult},
    tools::{truncate_output, TOOL_OUTPUT_CAP_MCP},
};

/// Optional per-server hints for which MCP method handles which
/// followup kind. The defaults match the semcode server used in the
#[derive(Debug, Clone)]
pub struct McpMethodMap {
    pub find_function: &'static str,
    pub find_type: &'static str,
    pub find_callers: &'static str,
    pub find_calls: &'static str,
    pub lore_search: &'static str,
}

impl Default for McpMethodMap {
    fn default() -> Self {
        Self {
            find_function: "find_function",
            find_type: "find_type",
            find_callers: "find_callers",
            find_calls: "find_calls",
            lore_search: "lore_search",
        }
    }
}

/// Window for lore body searches — the kres workflow's `lore`
/// followup kind asks for recent emails relevant to the current bug.
/// 30 days back from the call is enough to catch v1/v2 patch posts
/// without flooding the model with stale threads.
const LORE_SINCE_DAYS: i64 = 30;

/// Build the JSON arguments for a semcode `lore_search` call. The field
/// names must match the tool's real schema (`body_patterns`,
/// `since_date`); a mismatch silently returns an unfiltered or empty
/// result, so this helper is unit-tested.
fn lore_call_args(query: &str, since: &str) -> Value {
    json!({"body_patterns": [query], "since_date": since})
}

pub struct McpFetcher {
    pub client: Arc<Mutex<McpClient>>,
    pub methods: McpMethodMap,
    pub inner: Arc<WorkspaceFetcher>,
}

impl McpFetcher {
    pub fn new(client: McpClient, inner: Arc<WorkspaceFetcher>) -> Arc<Self> {
        Arc::new(Self {
            client: Arc::new(Mutex::new(client)),
            methods: McpMethodMap::default(),
            inner,
        })
    }

    /// Build an `McpFetcher` from an already-shared client handle —
    /// used when the caller (main.rs) has spawned a pool of MCP
    /// servers as `Arc<Mutex<McpClient>>` and wants a specific one
    /// to back the rule-based source/type/callers/callees path.
    pub fn from_shared(client: Arc<Mutex<McpClient>>, inner: Arc<WorkspaceFetcher>) -> Arc<Self> {
        Arc::new(Self {
            client,
            methods: McpMethodMap::default(),
            inner,
        })
    }
}

#[async_trait]
impl DataFetcher for McpFetcher {
    async fn fetch(
        &self,
        followups: &[Followup],
        plan: Option<&kres_core::Plan>,
    ) -> Result<FetchResult, AgentError> {
        let mut out = FetchResult::default();
        let mut passthrough: Vec<Followup> = Vec::new();

        for fu in followups {
            match fu.kind.as_str() {
                "source" => {
                    match self
                        .try_call_mcp_text("source", self.methods.find_function, &fu.name)
                        .await
                    {
                        Ok(text) => {
                            crate::symbol::append_context(
                                &mut out.context,
                                json!({
                                    "source": format!("mcp:source:{}", fu.name),
                                    "content": text,
                                    "note": "raw semcode source output; agents must choose the relevant result when semcode returns multiple candidates",
                                }),
                            );
                            // Parse the semcode output into a
                            // structured symbol when possible; if the
                            // parse fails (server returned an error
                            // blob or unexpected shape) keep the raw
                            // text as a context entry so the slow
                            // agent can still read it.
                            if let Some(sym) = crate::symbol::parse_semcode_symbol(
                                &text,
                                self.methods.find_function,
                            ) {
                                crate::symbol::append_symbol(&mut out.symbols, sym);
                            } else {
                                passthrough.push(fu.clone());
                            }
                        }
                        Err(_) => {
                            // Fall back to grep so a dead or empty MCP
                            // server doesn't strand the agent.
                            crate::symbol::append_context(
                                &mut out.context,
                                json!({
                                    "source": format!("mcp:source:{}", fu.name),
                                    "error": "semcode source lookup failed; local fallback requested",
                                    "tool": self.methods.find_function,
                                }),
                            );
                            passthrough.push(fu.clone());
                        }
                    }
                }
                "type" => {
                    match self
                        .try_call_mcp_text("type", self.methods.find_type, &fu.name)
                        .await
                    {
                        Ok(text) => {
                            crate::symbol::append_context(
                                &mut out.context,
                                json!({
                                    "source": format!("mcp:type:{}", fu.name),
                                    "content": text,
                                    "note": "raw semcode type output; agents must choose the relevant result when semcode returns multiple candidates",
                                }),
                            );
                            if let Some(sym) =
                                crate::symbol::parse_semcode_symbol(&text, self.methods.find_type)
                            {
                                crate::symbol::append_symbol(&mut out.symbols, sym);
                            } else {
                                passthrough.push(Followup {
                                    kind: "type".into(),
                                    name: fu.name.clone(),
                                    reason: fu.reason.clone(),
                                    path: fu.path.clone(),
                                    nice_to_have: fu.nice_to_have,
                                });
                            }
                        }
                        Err(_) => {
                            crate::symbol::append_context(
                                &mut out.context,
                                json!({
                                    "source": format!("mcp:type:{}", fu.name),
                                    "error": "semcode type lookup failed; local fallback requested",
                                    "tool": self.methods.find_type,
                                }),
                            );
                            passthrough.push(Followup {
                                kind: "type".into(),
                                name: fu.name.clone(),
                                reason: fu.reason.clone(),
                                path: fu.path.clone(),
                                nice_to_have: fu.nice_to_have,
                            });
                        }
                    }
                }
                "callers" => {
                    match self
                        .try_call_mcp_result("callers", self.methods.find_callers, &fu.name)
                        .await
                    {
                        Ok(v) if !mcp_result_unavailable(&v) => out.context.push(v),
                        Ok(v) => {
                            out.context.push(v);
                            passthrough.push(fu.clone());
                        }
                        Err(err_ctx) => {
                            out.context.push(err_ctx);
                            passthrough.push(fu.clone());
                        }
                    }
                }
                "callees" => {
                    match self
                        .try_call_mcp_result("callees", self.methods.find_calls, &fu.name)
                        .await
                    {
                        Ok(v) if !mcp_result_unavailable(&v) => out.context.push(v),
                        Ok(v) => {
                            out.context.push(v);
                            passthrough.push(fu.clone());
                        }
                        Err(err_ctx) => {
                            out.context.push(err_ctx);
                            passthrough.push(fu.clone());
                        }
                    }
                }
                "lore" => {
                    let since = (chrono::Utc::now() - chrono::Duration::days(LORE_SINCE_DAYS))
                        .format("%Y-%m-%d")
                        .to_string();
                    // No local fallback exists for lore (we can't grep the
                    // mailing-list archive offline), so unavailable / error
                    // envelopes are surfaced to the agent as-is rather than
                    // passed through to the inner fetcher.
                    match self
                        .try_call_mcp_lore(self.methods.lore_search, &fu.name, &since)
                        .await
                    {
                        Ok(v) => out.context.push(v),
                        Err(err_ctx) => out.context.push(err_ctx),
                    }
                }
                _ => passthrough.push(fu.clone()),
            }
        }

        if !passthrough.is_empty() {
            let inner_out = self.inner.fetch(&passthrough, plan).await?;
            out.symbols.extend(inner_out.symbols);
            out.context.extend(inner_out.context);
        }
        Ok(out)
    }
}

impl McpFetcher {
    /// Call an MCP tool and return the raw (already-capped) text —
    /// used by the `source` path so the caller can parse it into a
    /// semcode symbol. Returns Err(error_text) on failure so the
    /// caller can decide whether to fall back.
    async fn try_call_mcp_text(
        &self,
        label: &str,
        tool: &str,
        name: &str,
    ) -> Result<String, String> {
        let args = json!({"name": name});
        let mut guard = self.client.lock().await;
        let server = guard.server_name().to_string();
        match guard.call_tool(tool, &args).await {
            Ok(text) => Ok(truncate_output(&text, TOOL_OUTPUT_CAP_MCP)),
            Err(e) => {
                tracing::warn!(
                    target: "kres_agents",
                    server = %server,
                    tool,
                    name,
                    label,
                    "mcp call failed: {e}"
                );
                Err(e.to_string())
            }
        }
    }

    /// Call a tool via the standard MCP `tools/call` request (the
    /// flow at). The server's `content` array
    /// is concatenated into one text string and wrapped into a kres
    /// symbol/context envelope. On error, returns an error stub that
    /// the slow agent can still read ("we tried X but it failed" —
    /// so absent data isn't confused with "no callers at all").
    async fn try_call_mcp_result(
        &self,
        label: &str,
        tool: &str,
        name: &str,
    ) -> Result<Value, Value> {
        let args = json!({"name": name});
        let mut guard = self.client.lock().await;
        let server = guard.server_name().to_string();
        match guard.call_tool(tool, &args).await {
            Ok(text) => Ok(json!({
                "source": format!("mcp:{}:{}", label, name),
                "result": truncate_output(&text, TOOL_OUTPUT_CAP_MCP),
            })),
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    target: "kres_agents",
                    server = %server,
                    tool,
                    name,
                    "mcp call failed: {msg}"
                );
                Err(json!({
                    "source": format!("mcp:{}:{}", label, name),
                    "error": msg,
                    "server": server,
                    "tool": tool,
                }))
            }
        }
    }

    /// Body-search the kernel mailing-list archive (`lore`) via the
    /// semcode MCP tool. The agent provides the search query in
    /// `name`; we add a `since_date` so results stay focused on
    /// recent posts (default 30 days back). Result text is wrapped
    /// in the standard `{source, result}` context envelope so the
    /// slow agent sees it alongside any other gathered evidence.
    async fn try_call_mcp_lore(
        &self,
        tool: &str,
        query: &str,
        since: &str,
    ) -> Result<Value, Value> {
        let args = lore_call_args(query, since);
        let mut guard = self.client.lock().await;
        let server = guard.server_name().to_string();
        match guard.call_tool(tool, &args).await {
            Ok(text) => Ok(json!({
                "source": format!("mcp:lore:{}", query),
                "since": since,
                "result": truncate_output(&text, TOOL_OUTPUT_CAP_MCP),
            })),
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    target: "kres_agents",
                    server = %server,
                    tool,
                    query,
                    since,
                    "lore call failed: {msg}"
                );
                Err(json!({
                    "source": format!("mcp:lore:{}", query),
                    "since": since,
                    "error": msg,
                    "server": server,
                    "tool": tool,
                }))
            }
        }
    }
}

pub(crate) fn mcp_result_unavailable(v: &Value) -> bool {
    v.get("error")
        .and_then(Value::as_str)
        .map(mcp_text_unavailable)
        .unwrap_or(false)
        || v.get("result")
            .and_then(Value::as_str)
            .map(mcp_text_unavailable)
            .unwrap_or(false)
}

pub(crate) fn mcp_text_unavailable(text: &str) -> bool {
    let s = text.trim();
    if s.is_empty() {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    lower.contains("database is currently being indexed")
        || lower.contains("please wait for indexing")
        || lower.contains("indexing_status")
        || lower.contains("failed to get statistics for index")
        || lower.contains("page_lookup.lance not found")
        || lower.contains("cannot open index")
        || lower.contains("mcp call failed")
        || lower.contains("not found")
        || lower.contains("no function")
        || lower.contains("no type")
        || lower.contains("no callers")
        || lower.contains("no callees")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_map_defaults_match_semcode() {
        let m = McpMethodMap::default();
        assert_eq!(m.find_function, "find_function");
        assert_eq!(m.find_type, "find_type");
        assert_eq!(m.find_callers, "find_callers");
        assert_eq!(m.find_calls, "find_calls");
        assert_eq!(m.lore_search, "lore_search");
    }

    #[test]
    fn lore_since_window_is_about_thirty_days() {
        // Sanity check the constant the lore branch uses. If the window
        // moves, downstream prompts that say "last 30 days" need updating.
        assert_eq!(LORE_SINCE_DAYS, 30);
    }

    #[test]
    fn lore_call_args_match_semcode_lore_search_schema() {
        // The semcode `lore_search` tool takes `body_patterns: string[]`
        // and `since_date: string`. Sending the wrong field names is
        // silent: the tool ignores them and returns an unfiltered result.
        // Pin the arg shape so a rename of either field fails loudly.
        let args = lore_call_args("psp_dev_unregister", "2026-04-14");
        assert_eq!(
            args,
            json!({
                "body_patterns": ["psp_dev_unregister"],
                "since_date": "2026-04-14",
            })
        );
    }

    #[test]
    fn detects_semcode_indexing_as_unavailable() {
        assert!(mcp_text_unavailable(
            "Database is currently being indexed (Analyzing files). Please wait for indexing to complete."
        ));
        assert!(mcp_text_unavailable(
            "LanceError(IO): Object at location .semcode.db/functions.lance/_indices/x/page_lookup.lance not found"
        ));
        assert!(mcp_text_unavailable(""));
        assert!(!mcp_text_unavailable(
            "Function: cma_release\nFile: mm/cma.c\nbool cma_release(...)"
        ));
    }
}
