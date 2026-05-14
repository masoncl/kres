//! Child-process stdio transport for MCP servers.
//!
//! Design:
//! - Spawn the server as a child with piped stdin / stdout / stderr.
//! - Hand back writer (stdin) + line-reader (stdout) to the client.
//! - Drain stderr to a log file in a dedicated task so server tracebacks
//!   land on disk instead of `/dev/null` (bugs.md#M9).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config::ServerConfig;
use crate::error::McpError;

/// Handle to a spawned MCP server process.
pub struct Transport {
    pub server_name: String,
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
    /// Path to the stderr log file that the drainer task is appending
    /// to. Exposed so the operator can find it.
    pub stderr_log_path: PathBuf,
    /// Handle to the background drainer task. `shutdown` awaits it
    /// with a bounded grace so a final traceback isn't lost in the
    /// BufWriter when the child exits. None when stderr couldn't be
    /// taken (rare).
    pub stderr_drainer: Option<tokio::task::JoinHandle<()>>,
}

impl Transport {
    /// Spawn a server described by `cfg` with name `server_name`,
    /// redirecting stderr to `<log_dir>/mcp-<server_name>.log`.
    pub async fn spawn(
        server_name: &str,
        cfg: &ServerConfig,
        log_dir: &Path,
    ) -> Result<Self, McpError> {
        tokio::fs::create_dir_all(log_dir).await?;
        let stderr_log_path = log_dir.join(format!("mcp-{server_name}.log"));

        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        if let Some(ref cwd) = cfg.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd.spawn().map_err(|source| McpError::Spawn {
            server: server_name.to_string(),
            source,
        })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::StdinClosed {
            server: server_name.to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::StdoutClosed {
            server: server_name.to_string(),
            id: -1,
        })?;
        let stderr = child.stderr.take();

        // Drain stderr to disk, one line per incoming line, timestamps
        // in front. Doesn't block requests — runs on its own task.
        // The JoinHandle is retained so `shutdown` can await a final
        // flush (otherwise a late traceback can be lost in the
        // BufWriter when the child exits).
        let stderr_drainer = stderr.map(|stderr| {
            let log_path = stderr_log_path.clone();
            let server_name_for_task = server_name.to_string();
            tokio::spawn(async move {
                if let Err(e) = drain_stderr(stderr, &log_path).await {
                    tracing::warn!(
                        target: "kres_mcp",
                        server_name = %server_name_for_task,
                        error = %e,
                        "stderr drainer exited"
                    );
                }
            })
        });

        Ok(Self {
            server_name: server_name.to_string(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_log_path,
            stderr_drainer,
        })
    }

    /// Write one JSON-RPC message (a single line) into stdin. The
    /// caller serializes; this method just appends `\n` and flushes.
    pub async fn write_line(&mut self, line: &str) -> Result<(), McpError> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read one line from stdout. Returns `None` on EOF.
    pub async fn read_line(&mut self) -> Result<Option<String>, McpError> {
        let mut buf = String::new();
        let n = self.stdout.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        // strip a single trailing newline
        if buf.ends_with('\n') {
            buf.pop();
            if buf.ends_with('\r') {
                buf.pop();
            }
        }
        Ok(Some(buf))
    }
}

async fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    log_path: &Path,
) -> Result<(), McpError> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let ts = chrono::Utc::now().to_rfc3339();
        writer.write_all(format!("{ts} ").as_bytes()).await?;
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
    }
    // best-effort file close
    let _ = writer.into_inner();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp_dir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kres-mcp-test-{}-{}", nonce, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn spawn_echo_child_and_read_line() {
        // Use `cat` as a trivial stdio echo server. stdin → stdout is
        // exactly what JSON-RPC uses.
        let dir = tmp_dir("echo");
        let cfg = ServerConfig {
            command: "cat".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let mut t = Transport::spawn("echo", &cfg, &dir).await.unwrap();
        t.write_line("{\"hello\":1}").await.unwrap();
        let l = t.read_line().await.unwrap();
        assert_eq!(l.as_deref(), Some("{\"hello\":1}"));
        let _ = t.child.kill().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stderr_drained_to_log_file() {
        // Run a shell that writes to stderr and exits. Verify the log
        // file contains what stderr wrote.
        let dir = tmp_dir("stderr");
        let cfg = ServerConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "echo 'hello stderr' 1>&2; sleep 0.2".into()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let t = Transport::spawn("stderrsvr", &cfg, &dir).await.unwrap();
        let log = t.stderr_log_path.clone();
        // Wait for the drainer to flush after the child exits.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let contents = tokio::fs::read_to_string(&log).await.unwrap();
        assert!(
            contents.contains("hello stderr"),
            "log file contents: {contents:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_errors() {
        let dir = tmp_dir("nosuch");
        let cfg = ServerConfig {
            command: "/definitely/not/a/real/binary-xyz".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        // `unwrap_err()` on Result<Transport, _> would demand
        // Debug on Transport; pattern-match instead.
        match Transport::spawn("nope", &cfg, &dir).await {
            Ok(_) => panic!("unexpected spawn success"),
            Err(McpError::Spawn { server, .. }) => assert_eq!(server, "nope"),
            Err(other) => panic!("wrong variant: {other}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
