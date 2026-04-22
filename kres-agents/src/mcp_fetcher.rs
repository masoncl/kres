//! MCP-backed DataFetcher that delegates to a WorkspaceFetcher for
//! tool kinds an MCP server doesn't handle.
//!
//! Routes followups based on `kind`:
//! - `source` → MCP `find_function`, falling back to grep+read if the
//!   server returns empty or errors.
//! - `callers` → MCP `find_callers`.
//! - `callees` → MCP `find_calls`.
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
}

impl Default for McpMethodMap {
    fn default() -> Self {
        Self {
            find_function: "find_function",
            find_type: "find_type",
            find_callers: "find_callers",
            find_calls: "find_calls",
        }
    }
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
    /// to back the rule-based source/callers/callees path.
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
                                crate::symbol::append_context(
                                    &mut out.context,
                                    json!({
                                        "source": format!("mcp:source:{}", fu.name),
                                        "content": text,
                                    }),
                                );
                            }
                        }
                        Err(_) => {
                            // Fall back to grep so a dead or empty MCP
                            // server doesn't strand the agent.
                            passthrough.push(Followup {
                                kind: "search".into(),
                                name: format!(r"\b{}\b", regex_escape_word(&fu.name)),
                                reason: fu.reason.clone(),
                                path: fu.path.clone(),
                            });
                        }
                    }
                }
                "callers" => {
                    match self
                        .try_call_mcp_result("callers", self.methods.find_callers, &fu.name)
                        .await
                    {
                        Ok(v) => out.context.push(v),
                        Err(err_ctx) => out.context.push(err_ctx),
                    }
                }
                "callees" => {
                    match self
                        .try_call_mcp_result("callees", self.methods.find_calls, &fu.name)
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
    #[allow(dead_code)]
    async fn try_call_mcp(&self, label: &str, tool: &str, name: &str) -> Option<Value> {
        self.try_call_mcp_result(label, tool, name).await.ok()
    }

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
}

/// Escape regex metacharacters so a symbol name like `foo::bar<T>` can
/// be passed to ripgrep without surprises.
fn regex_escape_word(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\'
            | '/' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_escapes_common_chars() {
        assert_eq!(regex_escape_word("foo::bar"), "foo::bar");
        assert_eq!(regex_escape_word("a(b)"), "a\\(b\\)");
        assert_eq!(regex_escape_word("x.y"), "x\\.y");
        assert_eq!(regex_escape_word("a|b"), "a\\|b");
        assert_eq!(regex_escape_word("c[d]"), "c\\[d\\]");
    }

    #[test]
    fn method_map_defaults_match_semcode() {
        let m = McpMethodMap::default();
        assert_eq!(m.find_function, "find_function");
        assert_eq!(m.find_callers, "find_callers");
        assert_eq!(m.find_calls, "find_calls");
    }
}
