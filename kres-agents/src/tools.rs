//! Internal tool implementations for the main-agent data path.
//!
//! Four low-dependency tools: `read` (file range), `grep` (regex
//! over a path), `git` (readonly whitelisted commands), and `bash`
//! (arbitrary shell command, scoped to the workspace, mainly used by
//! the coding flow to compile and run generated source). MCP tools
//! route through a separate adapter in kres-repl; keeping those
//! out of kres-agents avoids a transitive kres-mcp dependency here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AgentError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadArgs {
    /// Filename. Accept both `file` and `path` keys — the Python
    /// (bugs.md#L2) would silently no-op when the main
    /// agent sent the alternative name.
    #[serde(alias = "path")]
    pub file: String,
    /// Starting line, 1-based.
    #[serde(default, alias = "startLine")]
    pub line: Option<u32>,
    /// Number of lines (inclusive). Accept `endLine` alias too.
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default, alias = "endLine")]
    pub end_line: Option<u32>,
}

pub fn read_file_range(workspace: &Path, args: &ReadArgs) -> Result<String, AgentError> {
    let abs = resolve_workspace(workspace, &args.file)?;
    let raw = std::fs::read_to_string(&abs)
        .map_err(|e| AgentError::Other(format!("read {}: {e}", abs.display())))?;
    let lines: Vec<&str> = raw.split_inclusive('\n').collect();
    let start = args.line.unwrap_or(1).saturating_sub(1) as usize;
    // count=0 is "read to EOF" — matches the convention.
    // Otherwise count wins over end_line; end_line is clamped to
    // `>= start` to avoid slice-out-of-order panics when the agent
    // supplies end_line < line.
    let end = match (args.count, args.end_line) {
        (Some(0), _) | (None, None) => lines.len(),
        (Some(c), _) => start.saturating_add(c as usize),
        (None, Some(e)) => (e as usize).max(start),
    };
    let end = end.min(lines.len());
    if start >= lines.len() || start >= end {
        return Ok(String::new());
    }
    Ok(lines[start..end].concat())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    /// Search root (relative to workspace).
    #[serde(default)]
    pub path: Option<String>,
    /// Max matches; protects against runaway output.
    #[serde(default)]
    pub limit: Option<u32>,
    /// File glob to filter (e.g. "*.c").
    #[serde(default)]
    pub glob: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindArgs {
    /// Root directory (relative to workspace). Defaults to ".".
    #[serde(default)]
    pub path: Option<String>,
    /// `-name` glob (e.g. `"*.c"`).
    #[serde(default)]
    pub name: Option<String>,
    /// `-type` character (`f`, `d`, `l`, ...).
    #[serde(default, alias = "file_type")]
    pub kind: Option<String>,
}

/// Thin wrapper around the `find(1)` binary, matching 's
/// `find` dispatch. Output is capped at
/// 20 000 chars with the same `... (truncated at 20000 chars)` tail.
pub async fn find(workspace: &Path, args: &FindArgs) -> Result<String, AgentError> {
    let root = match &args.path {
        Some(p) => resolve_workspace(workspace, p)?,
        None => workspace.to_path_buf(),
    };
    let mut cmd = tokio::process::Command::new("find");
    cmd.arg(&root);
    if let Some(n) = &args.name {
        cmd.arg("-name").arg(n);
    }
    if let Some(t) = &args.kind {
        cmd.arg("-type").arg(t);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| AgentError::Other("find timed out".into()))?
        .map_err(|e| AgentError::Other(format!("find spawn: {e}")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.stderr.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&err);
    }
    Ok(truncate_output(&text, TOOL_OUTPUT_CAP_GREP_FIND))
}

pub async fn grep(workspace: &Path, args: &GrepArgs) -> Result<String, AgentError> {
    let root = if let Some(p) = &args.path {
        resolve_workspace(workspace, p)?
    } else {
        workspace.to_path_buf()
    };
    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--no-messages")
        .arg("-n")
        .arg("-i")
        .arg("-e")
        .arg(&args.pattern);
    if let Some(g) = &args.glob {
        cmd.arg("-g").arg(g);
    }
    cmd.arg(&root);
    let limit = args.limit.unwrap_or(500);
    cmd.arg("--max-count").arg(limit.to_string());
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| AgentError::Other("grep timed out".into()))?
        .map_err(|e| AgentError::Other(format!("grep spawn: {e}")))?;
    // Keep stderr separate from stdout so the fast agent can
    // distinguish grep matches from error messages. Format matches
    // the convention used by other tool outputs in the pipeline.
    let stdout_text = String::from_utf8_lossy(&out.stdout).to_string();
    let combined = if out.stderr.is_empty() {
        stdout_text
    } else {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        if stdout_text.is_empty() {
            format!("[stderr]\n{err}")
        } else {
            format!("{stdout_text}\n[stderr]\n{err}")
        }
    };
    Ok(truncate_output(&combined, TOOL_OUTPUT_CAP_GREP_FIND))
}

/// Allowed readonly git subcommands. Mirrors
/// verbatim — the review scope was already
/// reviewing local history via `reflog`, and `shortlog`/`cat-file`/
/// `name-rev`/`rev-list` are load-bearing for kernel patch archaeology
/// (who touched what, resolve a blob sha, etc.). `for-each-ref` and
/// `status` aren't in but are useful diagnostics the agent
/// occasionally asks for; keeping them doesn't broaden the readonly
/// surface.
pub const GIT_ALLOWED: &[&str] = &[
    "log",
    "show",
    "diff",
    "blame",
    "annotate",
    "shortlog",
    "describe",
    "tag",
    "branch",
    "rev-parse",
    "rev-list",
    "name-rev",
    "cat-file",
    "ls-tree",
    "ls-files",
    "grep",
    "reflog",
    "status",
    "for-each-ref",
];

/// Per-tool output caps. grep/find truncate at 20k chars and MCP at
/// 50k chars with a `… (truncated at Nk chars)` tail. That stops one
/// runaway
/// `find /` from blowing the slow agent's input budget in a single
/// round.
pub const TOOL_OUTPUT_CAP_GREP_FIND: usize = 20_000;
pub const TOOL_OUTPUT_CAP_MCP: usize = 50_000;

/// Truncate `s` to `cap` chars (byte-indexed: works for ASCII-heavy
/// tool output, which is what grep/find/MCP return), appending
/// "\n… (truncated at Ncc chars)" so the agent can see the clip.
pub fn truncate_output(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    // Clip at cap bytes, then back up to the next char boundary so we
    // don't split a multi-byte character mid-sequence.
    let mut cut = cap.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 48);
    out.push_str(&s[..cut]);
    out.push_str(&format!("\n... (truncated at {} chars)", cap));
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitArgs {
    /// The full command string after the `git` binary (e.g. `"log --oneline -5"`).
    /// Accept `cmd` alias too (bugs.md#L2 — some agent outputs use that key).
    #[serde(alias = "cmd")]
    pub command: String,
}

/// Bash tool: execute an arbitrary shell command with the workspace
/// as cwd. Primarily used by the coding flow so the slow agent can
/// ask the main agent to `cc reproducer.c && ./a.out`. Kept simple
/// on purpose — full shell capability, no allowlist — because the
/// coding flow produces an open-ended set of compile/run commands
/// (make, cc, python, cargo, go, kselftest Makefile, bpftrace, …)
/// and an allowlist would either be too narrow to be useful or too
/// broad to be safer than no list at all. The operator's trust
/// boundary is kres's own `--workspace` directory and the model they
/// pointed at it.
///
/// Safeguards:
/// - Default 60s timeout (BASH_DEFAULT_TIMEOUT_SECS), capped at
///   BASH_MAX_TIMEOUT_SECS. On timeout the bash child is dropped and
///   SIGKILL'd via tokio's `kill_on_drop(true)`.
/// - Output (stdout + stderr + exit code) is captured and capped at
///   TOOL_OUTPUT_CAP_BASH chars — same envelope size as grep/find.
/// - cwd defaults to the workspace root; a relative `cwd` is
///   resolved via resolve_workspace so `..` traversal is rejected.
///   Absolute cwd paths are also rejected.
/// - The command is passed to `bash -c` verbatim. No attempt is
///   made to parse / filter / allowlist.
///
/// Known gap: kill_on_drop sends SIGKILL to the bash process, not to
/// its descendants. `bash -c "make -j8"` on timeout kills bash but
/// the make+cc grandchildren are reparented to init and keep
/// running until they finish or crash. A cleaner fix would use
/// setsid + killpg; deferred until a real user hits it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashArgs {
    /// The shell command to run, passed verbatim to `bash -c`.
    pub command: String,
    /// Optional per-invocation timeout in seconds. Defaults to 60,
    /// clamped to 600.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Optional workspace-relative cwd. When `Some`, the command
    /// runs from `<workspace>/<cwd>`. Absolute paths and `..`
    /// traversal are rejected by resolve_workspace.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Max output captured from a bash command. Same cap as grep/find
/// so the slow agent's input budget can't be blown by a runaway
/// build log.
pub const TOOL_OUTPUT_CAP_BASH: usize = 20_000;

/// Default timeout for `bash`. Enough for most compile-and-run
/// cycles without letting a stuck process stall the main agent for
/// minutes.
pub const BASH_DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const BASH_MAX_TIMEOUT_SECS: u64 = 600;

pub async fn bash_run(workspace: &Path, args: &BashArgs) -> Result<String, AgentError> {
    if args.command.trim().is_empty() {
        return Err(AgentError::Other("bash: empty command".into()));
    }
    let run_cwd = match &args.cwd {
        Some(p) => resolve_workspace(workspace, p)?,
        None => workspace.to_path_buf(),
    };
    let timeout = Duration::from_secs(
        args.timeout_secs
            .unwrap_or(BASH_DEFAULT_TIMEOUT_SECS)
            .min(BASH_MAX_TIMEOUT_SECS)
            .max(1),
    );
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c").arg(&args.command);
    cmd.current_dir(&run_cwd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // kill_on_drop so the timeout branch reaps the child instead of
    // leaking it until shell exit.
    cmd.kill_on_drop(true);
    let child_fut = cmd.output();
    let out_result = tokio::time::timeout(timeout, child_fut).await;
    let out = match out_result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(AgentError::Other(format!("bash spawn: {e}"))),
        Err(_) => {
            return Ok(truncate_output(
                &format!(
                    "[error] bash timed out after {}s; cwd={} cmd={}",
                    timeout.as_secs(),
                    run_cwd.display(),
                    args.command
                ),
                TOOL_OUTPUT_CAP_BASH,
            ));
        }
    };
    let stdout_text = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&out.stderr).to_string();
    let code_line = match out.status.code() {
        Some(c) => format!("[exit {c}]"),
        None => "[exit ?]".to_string(),
    };
    let mut body = String::new();
    body.push_str(&code_line);
    body.push('\n');
    if !stdout_text.is_empty() {
        body.push_str("[stdout]\n");
        body.push_str(&stdout_text);
        if !stdout_text.ends_with('\n') {
            body.push('\n');
        }
    }
    if !stderr_text.is_empty() {
        body.push_str("[stderr]\n");
        body.push_str(&stderr_text);
        if !stderr_text.ends_with('\n') {
            body.push('\n');
        }
    }
    Ok(truncate_output(&body, TOOL_OUTPUT_CAP_BASH))
}

pub async fn git(workspace: &Path, args: &GitArgs) -> Result<String, AgentError> {
    let parts = shell_split(&args.command)
        .ok_or_else(|| AgentError::Other(format!("unparseable git command: {}", args.command)))?;
    let Some(first) = parts.first() else {
        return Err(AgentError::Other("empty git command".into()));
    };
    if !GIT_ALLOWED.contains(&first.as_str()) {
        return Err(AgentError::Other(format!(
            "git subcommand `{first}` not in allowlist ({:?})",
            GIT_ALLOWED
        )));
    }
    for arg in &parts[1..] {
        if let Some(reason) = reject_risky_git_flag(arg) {
            return Err(AgentError::Other(format!(
                "git flag `{arg}` rejected: {reason}"
            )));
        }
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(workspace);
    for a in &parts {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| AgentError::Other("git timed out".into()))?
        .map_err(|e| AgentError::Other(format!("git spawn: {e}")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.stderr.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        text.push_str(&err);
    }
    Ok(text)
}

/// Reject git flags that turn a readonly query into a side-effecting
/// or code-executing operation. Returns Some(reason) when the flag is
/// rejected.
///
/// The allowlist of subcommands is already readonly, but some flags
/// allow writing files (`--output=PATH`), overriding config that can
/// execute arbitrary commands (`-c core.pager=/tmp/x`, `--pager=...`),
/// or launching external processes (`--exec=...`, `--upload-pack=...`).
fn reject_risky_git_flag(arg: &str) -> Option<&'static str> {
    // Strip trailing `=value` if present for name-only checks.
    let name = arg.split('=').next().unwrap_or(arg);
    match name {
        "-o" | "--output" => Some("writes to a file"),
        "-c" | "--config" => Some("lets callers override core.pager / alias"),
        "--pager" => Some("runs an external pager"),
        "--exec" => Some("runs an external program"),
        "--upload-pack" | "--receive-pack" => Some("specifies a remote helper to run"),
        "-P" | "--paginate" => Some("forces a pager"),
        "--help" | "-h" => Some("can invoke the man pager"),
        _ => None,
    }
}

/// Super-simple shell split: honours single and double quotes but no
/// backslash escapes. Good enough for the readonly git surface.
fn shell_split(s: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in s.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if in_single || in_double {
        return None;
    }
    if !current.is_empty() {
        out.push(current);
    }
    Some(out)
}

/// Resolve a user-supplied path against the workspace, rejecting any
/// path that escapes the workspace via `..` components after joining.
/// Absolute paths are accepted only when they are already inside the
/// workspace (so an agent supplying `/home/user/kernel/foo.c` with a
/// matching workspace still works, but `/etc/passwd` is rejected).
///
/// Returns `AgentError::Other` with a descriptive message on refusal,
/// matching how other tool errors surface in the fetcher.
fn resolve_workspace(workspace: &Path, rel: &str) -> Result<PathBuf, AgentError> {
    let p = Path::new(rel);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    };
    // Canonicalize to dissolve `..` / symlinks. Canonicalize fails
    // when the target doesn't exist, so fall back to a textual check
    // against the canonical workspace.
    let ws_canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    match joined.canonicalize() {
        Ok(c) => {
            if c.starts_with(&ws_canon) {
                Ok(c)
            } else {
                Err(AgentError::Other(format!(
                    "path {} escapes workspace {}",
                    c.display(),
                    ws_canon.display()
                )))
            }
        }
        Err(_) => {
            // Target doesn't exist (or parent missing) — still
            // refuse paths that contain `..` traversal after
            // normalising lexically.
            let normalised = normalise_lexical(&joined);
            if normalised.starts_with(&ws_canon) || normalised.starts_with(workspace) {
                Ok(normalised)
            } else {
                Err(AgentError::Other(format!(
                    "path {} escapes workspace {}",
                    normalised.display(),
                    ws_canon.display()
                )))
            }
        }
    }
}

/// Lexical path normalisation — collapse `..` components without
/// touching the filesystem. Used when `canonicalize` fails because
/// the target is missing.
fn normalise_lexical(p: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut absolute = false;
    for comp in p.components() {
        match comp {
            std::path::Component::RootDir => {
                absolute = true;
                out.clear();
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Pop unless we would ascend past root.
                out.pop();
            }
            std::path::Component::Normal(seg) => {
                out.push(seg.to_os_string());
            }
            std::path::Component::Prefix(pref) => {
                out.clear();
                out.push(pref.as_os_str().to_os_string());
            }
        }
    }
    let mut pb = PathBuf::new();
    if absolute {
        pb.push("/");
    }
    for seg in out {
        pb.push(seg);
    }
    pb
}

/// Generic "followup -> tool args" translator: extracts fields by
/// accepting both canonical and alias names.
pub fn coerce_args<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, AgentError> {
    serde_json::from_value(v.clone()).map_err(AgentError::from)
}

/// A tiny in-memory record of a tool invocation, for logging.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub tool: String,
    pub args: BTreeMap<String, Value>,
    pub output_bytes: usize,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kres-tools-{}-{}", nonce, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn bash_captures_stdout_stderr_and_exit() {
        let dir = tmpdir("bash1");
        let args = BashArgs {
            command: "echo out; echo err 1>&2; exit 7".into(),
            timeout_secs: Some(5),
            cwd: None,
        };
        let got = bash_run(&dir, &args).await.unwrap();
        assert!(got.starts_with("[exit 7]"), "got {got}");
        assert!(got.contains("[stdout]\nout"), "got {got}");
        assert!(got.contains("[stderr]\nerr"), "got {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bash_timeout_surfaces_error() {
        let dir = tmpdir("bash2");
        let args = BashArgs {
            command: "sleep 5".into(),
            timeout_secs: Some(1),
            cwd: None,
        };
        let got = bash_run(&dir, &args).await.unwrap();
        assert!(got.contains("bash timed out"), "got {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bash_runs_in_workspace_cwd() {
        let dir = tmpdir("bash3");
        let args = BashArgs {
            command: "pwd".into(),
            timeout_secs: Some(5),
            cwd: None,
        };
        let got = bash_run(&dir, &args).await.unwrap();
        // canonicalize-free compare: the tmpdir may resolve to a
        // realpath under /tmp or /var/tmp depending on platform, so
        // just look for the trailing basename.
        let basename = dir.file_name().unwrap().to_str().unwrap();
        assert!(got.contains(basename), "got {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bash_rejects_traversal_cwd() {
        let dir = tmpdir("bash4");
        let args = BashArgs {
            command: "true".into(),
            timeout_secs: Some(5),
            cwd: Some("../escape".into()),
        };
        let res = bash_run(&dir, &args).await;
        assert!(res.is_err(), "expected rejection, got {res:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bash_empty_command_errors() {
        let dir = tmpdir("bash5");
        let args = BashArgs {
            command: "   ".into(),
            timeout_secs: None,
            cwd: None,
        };
        let res = bash_run(&dir, &args).await;
        assert!(res.is_err(), "expected rejection for empty command");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_whole_file_when_no_range() {
        let dir = tmpdir("read1");
        let path = dir.join("f.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"a\nb\nc\n").unwrap();
        let args = ReadArgs {
            file: "f.txt".into(),
            line: None,
            count: None,
            end_line: None,
        };
        let got = read_file_range(&dir, &args).unwrap();
        assert_eq!(got, "a\nb\nc\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_range_uses_line_and_count() {
        let dir = tmpdir("read2");
        let path = dir.join("f.txt");
        std::fs::write(&path, "1\n2\n3\n4\n5\n").unwrap();
        let args = ReadArgs {
            file: "f.txt".into(),
            line: Some(2),
            count: Some(2),
            end_line: None,
        };
        let got = read_file_range(&dir, &args).unwrap();
        assert_eq!(got, "2\n3\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_accepts_path_alias() {
        // bugs.md#L2 — alias "path" must resolve to the same field.
        let dir = tmpdir("read3");
        std::fs::write(dir.join("x.txt"), "hello").unwrap();
        let v = serde_json::json!({"path": "x.txt"});
        let args: ReadArgs = serde_json::from_value(v).unwrap();
        let got = read_file_range(&dir, &args).unwrap();
        assert_eq!(got, "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_accepts_startline_endline_aliases() {
        let dir = tmpdir("read4");
        std::fs::write(dir.join("f.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let v = serde_json::json!({"path": "f.txt", "startLine": 2, "endLine": 4});
        let args: ReadArgs = serde_json::from_value(v).unwrap();
        let got = read_file_range(&dir, &args).unwrap();
        assert_eq!(got, "2\n3\n4\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_rejects_disallowed_subcommand() {
        let v = serde_json::json!({"command": "push origin main"});
        let args: GitArgs = serde_json::from_value(v).unwrap();
        // Use workspace = /tmp; we only test allowlist validation
        // before cmd spawn.
        let err =
            futures::executor::block_on(git(std::path::Path::new("/tmp"), &args)).unwrap_err();
        match err {
            AgentError::Other(m) => assert!(m.contains("not in allowlist"), "{m}"),
            _ => panic!("wrong err"),
        }
    }

    #[test]
    fn git_rejects_risky_flags() {
        for cmd in [
            "log --output=/tmp/x",
            "log -o /tmp/x",
            "log --pager=cat",
            "log -c core.pager=/tmp/x",
            "log --exec=/bin/sh",
            "log -h",
            "log --upload-pack=/bin/sh",
        ] {
            let v = serde_json::json!({"command": cmd});
            let args: GitArgs = serde_json::from_value(v).unwrap();
            let err =
                futures::executor::block_on(git(std::path::Path::new("/tmp"), &args)).unwrap_err();
            match err {
                AgentError::Other(m) => assert!(m.contains("rejected"), "{cmd}: {m}"),
                _ => panic!("wrong err for {cmd}"),
            }
        }
    }

    #[test]
    fn git_accepts_cmd_alias() {
        // bugs.md#L2 — alias `cmd` must map to `command`.
        let v = serde_json::json!({"cmd": "log --oneline -1"});
        let args: GitArgs = serde_json::from_value(v).unwrap();
        assert_eq!(args.command, "log --oneline -1");
    }

    #[test]
    fn shell_split_basic() {
        assert_eq!(
            shell_split("log --oneline -5").unwrap(),
            vec!["log", "--oneline", "-5"]
        );
    }

    #[test]
    fn shell_split_quotes() {
        assert_eq!(
            shell_split("log --grep=\"one two\"").unwrap(),
            vec!["log", "--grep=one two"]
        );
        assert_eq!(
            shell_split("log -S 'apple banana'").unwrap(),
            vec!["log", "-S", "apple banana"]
        );
    }

    #[test]
    fn shell_split_unbalanced_quote_errors() {
        assert!(shell_split("log 'oops").is_none());
    }
}
