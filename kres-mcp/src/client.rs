//! High-level MCP client.
//!
//! Responsibilities:
//! - Own the `Transport` (the child process) and a monotonically-
//!   increasing request-id counter.
//! - Serialize requests to `jsonrpc: "2.0"` lines.
//! - Read responses line-by-line, match on `id` to pending requests.
//! - Enforce a per-call timeout (bugs.md#M10).
//!
//! Concurrency model: requests are serialized — we hold an
//! `&mut self` across the write+wait loop. For the workload this
//! matches how already calls MCP (one call at a time per agent).
//! If later phases need pipelined requests, we can hoist a channel in
//! front of the writer.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::{
    config::ServerConfig,
    error::McpError,
    message::{JsonRpcError, Notification, Request, Response, ResponseResult},
    transport::Transport,
};

/// Default per-request timeout. Matches the bugs.md#M10 recommendation
/// of "default 30s, surface a wedge as an error".
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct McpClient {
    transport: Transport,
    next_id: i64,
    default_timeout: Duration,
    /// Cached `tools/list` response populated by `spawn` once the
    /// `initialize` handshake completes. Empty on servers that declined
    /// the handshake or returned no tools.
    tools: Vec<Value>,
}

impl McpClient {
    /// Spawn a server and wrap it in a client.
    ///
    /// Performs the MCP handshake (`initialize` → `notifications/
    /// initialized` → `tools/list`) so later `call_tool` calls are
    /// unambiguous and the cached tool descriptions are ready for
    /// `tools()`. A failing handshake returns an Err rather than
    /// silently leaving the client half-initialized.
    pub async fn spawn(
        server_name: &str,
        cfg: &ServerConfig,
        log_dir: &Path,
    ) -> Result<Self, McpError> {
        let transport = Transport::spawn(server_name, cfg, log_dir).await?;
        let mut client = Self {
            transport,
            next_id: 1,
            default_timeout: DEFAULT_TIMEOUT,
            tools: Vec::new(),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Spawn without running the handshake. Exposed for tests and for
    /// operators pointing kres at a non-MCP server for diagnostics
    /// (e.g. an echo loopback). Production paths should use `spawn`.
    pub async fn spawn_raw(
        server_name: &str,
        cfg: &ServerConfig,
        log_dir: &Path,
    ) -> Result<Self, McpError> {
        let transport = Transport::spawn(server_name, cfg, log_dir).await?;
        Ok(Self {
            transport,
            next_id: 1,
            default_timeout: DEFAULT_TIMEOUT,
            tools: Vec::new(),
        })
    }

    /// Return the cached `tools/list` entries (JSON objects with `name`
    /// / `description` / `inputSchema` at minimum).
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// Perform the MCP `initialize` → `notifications/initialized` →
    /// `tools/list` sequence. Mirrors + `_list_tools`
    ///A server that can't
    /// complete this returns an error — later `tools/call` calls on a
    /// half-initialized server frequently wedge, so failing loud here
    /// keeps diagnostics useful.
    async fn handshake(&mut self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "kres", "version": env!("CARGO_PKG_VERSION")},
        });
        self.call("initialize", Some(&params)).await?;
        self.send_notification("notifications/initialized", None)
            .await?;
        let listed = self.call("tools/list", None).await?;
        if let Some(arr) = listed.get("tools").and_then(|v| v.as_array()) {
            self.tools = arr.clone();
        }
        Ok(())
    }

    /// Send a JSON-RPC notification (no `id`, no response). Used for
    /// the `notifications/initialized` step in the MCP handshake.
    pub async fn send_notification(
        &mut self,
        method: &str,
        params: Option<&Value>,
    ) -> Result<(), McpError> {
        let note = Notification::new(method, params);
        let line = serde_json::to_string(&note).map_err(|source| McpError::Json {
            server: self.transport.server_name.clone(),
            source,
        })?;
        self.transport.write_line(&line).await
    }

    /// High-level `tools/call`. Joins every `type:"text"` block from
    /// the server's `content` array into one string, matching 's
    /// `call_tool` at. Callers that want the
    /// raw JSON envelope should use `call("tools/call", ...)` directly.
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<String, McpError> {
        let params = json!({"name": tool_name, "arguments": arguments});
        let result = self.call("tools/call", Some(&params)).await?;
        let mut parts: Vec<String> = Vec::new();
        if let Some(arr) = result.get("content").and_then(|c| c.as_array()) {
            for block in arr {
                let t = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if t == "text" {
                    if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(s.to_string());
                    }
                }
            }
        }
        Ok(parts.join("\n"))
    }

    pub fn set_default_timeout(&mut self, d: Duration) {
        self.default_timeout = d;
    }

    pub fn stderr_log_path(&self) -> &Path {
        &self.transport.stderr_log_path
    }

    pub fn server_name(&self) -> &str {
        &self.transport.server_name
    }

    /// JSON-RPC call with the default timeout.
    pub async fn call(&mut self, method: &str, params: Option<&Value>) -> Result<Value, McpError> {
        let d = self.default_timeout;
        self.call_with_timeout(method, params, d).await
    }

    /// JSON-RPC call with an explicit timeout.
    pub async fn call_with_timeout(
        &mut self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id, method, params);
        let line = serde_json::to_string(&req).map_err(|source| McpError::Json {
            server: self.transport.server_name.clone(),
            source,
        })?;
        self.transport.write_line(&line).await?;

        let server = self.transport.server_name.clone();
        let result = tokio::time::timeout(timeout, self.read_matching(id)).await;
        match result {
            Ok(r) => r,
            Err(_) => Err(McpError::Timeout {
                server,
                id,
                millis: timeout.as_millis() as u64,
            }),
        }
    }

    /// Read lines until the response with `id == wanted_id` arrives.
    /// Non-matching ids (notifications, or stale ids from earlier
    /// aborted calls) are dropped with a warning. A response whose
    /// `id` is null AND which carries an `error` payload is treated
    /// as belonging to the current request — some servers wrongly
    /// emit null-id errors when they fail to parse our request,
    /// and dropping them would cause a 30s timeout with no useful
    /// diagnostic (covered by the review of the MCP client).
    async fn read_matching(&mut self, wanted_id: i64) -> Result<Value, McpError> {
        loop {
            let line = match self.transport.read_line().await? {
                Some(l) => l,
                None => {
                    return Err(McpError::StdoutClosed {
                        server: self.transport.server_name.clone(),
                        id: wanted_id,
                    })
                }
            };
            if line.is_empty() {
                continue;
            }
            // JSON-RPC 2.0 allows the server to interleave notifications
            // (no `id`, no `result`, no `error` — just `method`/`params`)
            // with responses. semcode-mcp emits
            // `notifications/message` log lines before the real
            // response; parsing those as `Response` fails because
            // ResponseResult is untagged over {result}|{error}. Inspect
            // the line as a generic Value first and skip notifications.
            let raw: Value = serde_json::from_str(&line).map_err(|source| McpError::Json {
                server: self.transport.server_name.clone(),
                source,
            })?;
            if raw.get("method").is_some()
                && raw.get("result").is_none()
                && raw.get("error").is_none()
            {
                tracing::debug!(
                    target: "kres_mcp",
                    server = %self.transport.server_name,
                    method = raw.get("method").and_then(|v| v.as_str()).unwrap_or(""),
                    "dropping jsonrpc notification"
                );
                continue;
            }
            let resp: Response = serde_json::from_value(raw).map_err(|source| McpError::Json {
                server: self.transport.server_name.clone(),
                source,
            })?;
            let resp_id = match resp.id.as_ref() {
                None => None,
                Some(v) => {
                    // Accept integer ids natively and numeric-string
                    // ids as a tolerance for servers that stringify.
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                }
            };
            let is_null_id_error = matches!(
                (&resp.id, &resp.result),
                (
                    None | Some(serde_json::Value::Null),
                    crate::message::ResponseResult::Err { .. }
                )
            );
            if resp_id != Some(wanted_id) && !is_null_id_error {
                tracing::debug!(
                    target: "kres_mcp",
                    server = %self.transport.server_name,
                    got_id = ?resp_id,
                    wanted_id,
                    "dropping unmatched response"
                );
                continue;
            }
            return decode_result(&self.transport.server_name, resp);
        }
    }

    /// Graceful shutdown: close stdin so the server exits cleanly,
    /// then wait up to `grace` for the process, then await the
    /// stderr drainer so late tracebacks reach disk, then reap.
    pub async fn shutdown(mut self, grace: Duration) -> Result<(), McpError> {
        drop(self.transport.stdin);
        let wait = tokio::time::timeout(grace, self.transport.child.wait()).await;
        if wait.is_err() {
            let _ = self.transport.child.kill().await;
            // Reap regardless of whether kill succeeded; leaving the
            // Child in tokio's keeper causes zombies until drop.
            let _ = self.transport.child.wait().await;
        }
        // Give the drainer up to 1s to flush a final line. The
        // ChildStderr it reads from is closed when the child exits,
        // so this typically returns immediately.
        if let Some(drainer) = self.transport.stderr_drainer.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), drainer).await;
        }
        Ok(())
    }
}

fn decode_result(server: &str, resp: Response) -> Result<Value, McpError> {
    match resp.result {
        ResponseResult::Ok { result } => Ok(result),
        ResponseResult::Err { error } => Err(McpError::Rpc {
            server: server.to_string(),
            code: error.code,
            message: error.message,
        }),
    }
}

#[allow(dead_code)]
fn reexport_jsonrpc_error() -> JsonRpcError {
    JsonRpcError {
        code: 0,
        message: String::new(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn tmp_dir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kres-mcp-client-{}-{}", nonce, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Minimal "echo-back a valid JSON-RPC response for whatever comes in".
    /// Implemented as a tiny awk inside `sh -c`. Stops when stdin closes.
    fn jsonrpc_echo_cfg() -> ServerConfig {
        // For every input line, reply with
        //   {"jsonrpc":"2.0","id":<same id>,"result":{"echo":<method>}}
        let script = r#"
while IFS= read -r line; do
    method=$(printf '%s' "$line" | python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); print(d.get("method",""))')
    id=$(printf '%s' "$line" | python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); print(d.get("id",0))')
    printf '{"jsonrpc":"2.0","id":%s,"result":{"echo":"%s"}}\n' "$id" "$method"
done
"#;
        ServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn call_returns_result() {
        let dir = tmp_dir("echo");
        let mut c = McpClient::spawn_raw("echo", &jsonrpc_echo_cfg(), &dir)
            .await
            .unwrap();
        let v = c.call("my_method", None).await.unwrap();
        assert_eq!(v.get("echo").unwrap().as_str(), Some("my_method"));
        c.shutdown(Duration::from_secs(2)).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn call_with_params() {
        let dir = tmp_dir("echo-p");
        let mut c = McpClient::spawn_raw("echo", &jsonrpc_echo_cfg(), &dir)
            .await
            .unwrap();
        let params = serde_json::json!({"a": 1});
        let v = c.call("ping", Some(&params)).await.unwrap();
        assert_eq!(v.get("echo").unwrap().as_str(), Some("ping"));
        c.shutdown(Duration::from_secs(2)).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn timeout_on_wedged_server() {
        // Child that reads stdin but never writes to stdout.
        // A `while read` in the shell keeps stdout open (it inherits
        // the pipe) without producing any output — the wedge case we
        // actually want to simulate.
        let dir = tmp_dir("wedge");
        let cfg = ServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "while IFS= read -r line; do : ; done".into()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let mut c = McpClient::spawn_raw("wedge", &cfg, &dir).await.unwrap();
        let res = c
            .call_with_timeout("anything", None, Duration::from_millis(150))
            .await;
        match res {
            Err(McpError::Timeout { millis, .. }) => assert_eq!(millis, 150),
            Err(other) => panic!("expected Timeout, got {other}"),
            Ok(v) => panic!("expected Timeout, got Ok({v:?})"),
        }
        c.shutdown(Duration::from_secs(1)).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Server that responds to `tools/call` with two text blocks plus
    /// a non-text block — verifies call_tool joins only the text parts.
    fn two_text_blocks_cfg() -> ServerConfig {
        let script = r#"
while IFS= read -r line; do
    id=$(printf '%s' "$line" | python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); print(d.get("id",0))')
    printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"first"},{"type":"image","data":"..."},{"type":"text","text":"second"}]}}\n' "$id"
done
"#;
        ServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn call_tool_joins_text_parts() {
        let dir = tmp_dir("two-text");
        let mut c = McpClient::spawn_raw("t", &two_text_blocks_cfg(), &dir)
            .await
            .unwrap();
        let s = c
            .call_tool("whatever", &serde_json::json!({}))
            .await
            .unwrap();
        // Two text blocks joined with "\n"; the image block is dropped.
        assert_eq!(s, "first\nsecond");
        c.shutdown(Duration::from_secs(2)).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Server that interleaves a JSON-RPC notification line BEFORE the
    /// real response. Mirrors semcode-mcp's behaviour of emitting
    /// `notifications/message` log lines on stdout during a call.
    fn notification_then_response_cfg() -> ServerConfig {
        let script = r#"
while IFS= read -r line; do
    id=$(printf '%s' "$line" | python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); print(d.get("id",0))')
    printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","message":"working..."}}\n'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
done
"#;
        ServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn notification_before_response_is_skipped() {
        let dir = tmp_dir("notify");
        let mut c = McpClient::spawn_raw("notify", &notification_then_response_cfg(), &dir)
            .await
            .unwrap();
        let v = c.call("anything", None).await.unwrap();
        assert_eq!(v.get("ok").and_then(|v| v.as_bool()), Some(true));
        c.shutdown(Duration::from_secs(2)).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stdout_close_is_reported() {
        // Child that exits immediately, closing stdout before we
        // ever send a request.
        let dir = tmp_dir("shortlived");
        let cfg = ServerConfig {
            command: "true".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let mut c = McpClient::spawn_raw("short", &cfg, &dir).await.unwrap();
        // Give the child a moment to exit before we try to send.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let e = c.call("whatever", None).await.unwrap_err();
        // Accept either StdoutClosed OR an I/O error — both mean the
        // pipe is dead. What MUST NOT happen is an infinite block.
        match e {
            McpError::StdoutClosed { .. } | McpError::Io(_) => {}
            other => panic!("unexpected: {other}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
