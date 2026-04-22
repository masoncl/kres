//! Workspace-backed DataFetcher.
//!
//! Translates followups into local tool calls. No MCP here — the
//! kres-repl crate supplies a decorated fetcher that delegates MCP
//! followups to kres-mcp and non-MCP ones to this type.
//!
//! Followup types routed locally:
//! - `read` — name = "file.c:100+50" or "file.c"; delegates to tools::read_file_range.
//! - `search` / `grep` — name = regex; `path` = search root.
//! - `git` — name = command string.
//! - `bash` — name = shell command; dispatched to tools::bash_run
//!   with default timeout and workspace-root cwd. Mainly used by the
//!   coding flow to compile and run emitted source.
//! - `question` — no-op (answered by the LLM, not by data fetch).
//!
//! Types routed through a plugin in Phase 8: `source`, `callers`,
//! `callees`, `file` (semcode / find).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::{
    error::AgentError,
    followup::Followup,
    pipeline::{DataFetcher, FetchResult},
    tools::{
        bash_run, find, git, grep, read_file_range, BashArgs, FindArgs, GitArgs, GrepArgs,
        ReadArgs,
    },
};

#[derive(Debug, Clone)]
pub struct WorkspaceFetcher {
    pub workspace: PathBuf,
}

impl WorkspaceFetcher {
    pub fn new(workspace: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            workspace: workspace.into(),
        })
    }
}

#[async_trait]
impl DataFetcher for WorkspaceFetcher {
    async fn fetch(
        &self,
        followups: &[Followup],
        _plan: Option<&kres_core::Plan>,
    ) -> Result<FetchResult, AgentError> {
        let mut out = FetchResult::default();
        for fu in followups {
            match fu.kind.as_str() {
                "read" => match parse_read_spec(&fu.name) {
                    Ok(args) => match read_file_range(&self.workspace, &args) {
                        Ok(content) => out.context.push(json!({
                            "source": format!("read:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("read:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    },
                    Err(e) => out.context.push(json!({
                        "source": format!("read:{}", fu.name),
                        "error": e.to_string(),
                    })),
                },
                "search" | "grep" => {
                    let args = GrepArgs {
                        pattern: fu.name.clone(),
                        path: fu.path.clone(),
                        limit: Some(500),
                        glob: None,
                    };
                    match grep(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("search:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("search:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "find" => {
                    // `find` accepts a single `name` value for
                    // `-name` and an optional `path`.
                    let args = FindArgs {
                        name: Some(fu.name.clone()),
                        path: fu.path.clone(),
                        kind: None,
                    };
                    match find(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("find:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("find:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "git" => {
                    let args = GitArgs {
                        command: fu.name.clone(),
                    };
                    match git(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("git:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("git:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "bash" => {
                    // `name` carries the command string (same shape
                    // the main-agent `<actions>` branch accepts).
                    // timeout_secs and cwd aren't currently plumbed
                    // through Followup; default to 60s / workspace
                    // root. If an operator needs either, they should
                    // run with a main-agent configured (the richer
                    // LLM-driven dispatch path that can emit full
                    // args).
                    let args = BashArgs {
                        command: fu.name.clone(),
                        timeout_secs: None,
                        cwd: None,
                    };
                    match bash_run(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("bash:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("bash:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "question" => {}
                _ => out.context.push(json!({
                    "source": format!("{}:{}", fu.kind, fu.name),
                    "error": format!("follow-up kind `{}` not handled by WorkspaceFetcher", fu.kind),
                })),
            }
        }
        Ok(out)
    }
}

/// Parse a `"file.c:100+50"` or `"file.c"` spec into ReadArgs.
pub fn parse_read_spec(spec: &str) -> Result<ReadArgs, AgentError> {
    // Find the LAST ':' so Windows paths / colons in names behave.
    let (file, rest) = match spec.rsplit_once(':') {
        Some((f, r)) if !r.is_empty() && r.chars().all(|c| c.is_ascii_digit() || c == '+') => {
            (f, Some(r))
        }
        _ => (spec, None),
    };
    let (line, count) = match rest {
        None => (None, None),
        Some(range) => match range.split_once('+') {
            Some((start, len)) => {
                let s: u32 = start
                    .parse()
                    .map_err(|_| AgentError::Other(format!("bad start line in {spec:?}")))?;
                let c: u32 = len
                    .parse()
                    .map_err(|_| AgentError::Other(format!("bad count in {spec:?}")))?;
                (Some(s), Some(c))
            }
            None => {
                let s: u32 = range
                    .parse()
                    .map_err(|_| AgentError::Other(format!("bad line in {spec:?}")))?;
                (Some(s), None)
            }
        },
    };
    Ok(ReadArgs {
        file: file.to_string(),
        line,
        count,
        end_line: None,
    })
}

#[allow(dead_code)]
fn _ensure_path_compiles(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kres-fetcher-{}-{}", nonce, std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn fetches_read_followup() {
        let dir = tmpdir("read");
        let mut f = std::fs::File::create(dir.join("a.c")).unwrap();
        f.write_all(b"1\n2\n3\n4\n5\n").unwrap();
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "read".into(),
                    name: "a.c:2+2".into(),
                    reason: String::new(),
                    path: None,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 1);
        let content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert_eq!(content, "2\n3\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unhandled_followup_kind_produces_explanatory_error() {
        let dir = tmpdir("unk");
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "source".into(),
                    name: "some_func".into(),
                    reason: String::new(),
                    path: None,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 1);
        let err = r.context[0].get("error").unwrap().as_str().unwrap();
        assert!(err.contains("source"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_read_spec_full() {
        let a = parse_read_spec("foo.c:100+50").unwrap();
        assert_eq!(a.file, "foo.c");
        assert_eq!(a.line, Some(100));
        assert_eq!(a.count, Some(50));
    }

    #[test]
    fn parse_read_spec_just_line() {
        let a = parse_read_spec("foo.c:100").unwrap();
        assert_eq!(a.file, "foo.c");
        assert_eq!(a.line, Some(100));
        assert_eq!(a.count, None);
    }

    #[test]
    fn parse_read_spec_no_range() {
        let a = parse_read_spec("foo.c").unwrap();
        assert_eq!(a.file, "foo.c");
        assert_eq!(a.line, None);
        assert_eq!(a.count, None);
    }

    #[test]
    fn parse_read_spec_keeps_colons_in_non_numeric_tail() {
        let a = parse_read_spec("foo/bar:baz.c").unwrap();
        assert_eq!(a.file, "foo/bar:baz.c");
        assert_eq!(a.line, None);
    }
}
