//! Workspace-backed DataFetcher.
//!
//! Translates followups into local tool calls. No MCP here — the
//! kres-repl crate supplies a decorated fetcher that delegates MCP
//! followups to kres-mcp and non-MCP ones to this type.
//!
//! Followup types routed locally:
//! - `survey` — fallback file-scoped definition matches when semcode's
//!   Tree-sitter `file_survey` is unavailable.
//! - `read` — name = "file.c:100+50" or "file.c"; delegates to tools::read_file_range.
//! - `search` / `grep` — name = regex; `path` = search root.
//! - `source` — fallback grep for a symbol, plus bounded source reads
//!   only when the match set is small, when no MCP-backed semcode fetcher
//!   is configured or semcode is unavailable.
//! - `callers` / `callees` — fallback grep for a symbol use when
//!   no MCP-backed callgraph is available.
//! - `type` — fallback grep for a type name when no MCP-backed
//!   semcode fetcher is configured.
//! - `git` — name = command string.
//! - `make` — name = make arguments; dispatched as `make` argv
//!   without a shell, with a 300s timeout.
//! - `meson` — name = meson arguments; dispatched as `meson` argv
//!   without a shell, with a 300s timeout.
//! - `bash` — name = shell command; dispatched to tools::bash_run
//!   with default timeout and workspace-root cwd. Mainly used by the
//!   coding flow to compile and run emitted source.
//! - `question` — no-op (answered by the LLM, not by data fetch).
//!
//! Types preferably routed through MCP when configured: `source`, `type`,
//! `callers`, `callees`, `file` (semcode / find). This fetcher still handles
//! source/callgraph requests with local grep so MCP indexing failures do not
//! strand research.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::{
    error::AgentError,
    followup::Followup,
    pipeline::{DataFetcher, FetchResult},
    tools::{
        bash_run, cargo_run, find, git, grep, make_run, meson_run, read_file_range, BashArgs,
        FindArgs, GitArgs, GrepArgs, ReadArgs,
    },
};

const SOURCE_FALLBACK_CONTEXT_BEFORE: u32 = 20;
const SOURCE_FALLBACK_READ_LINES: u32 = 120;
const SOURCE_FALLBACK_AUTO_READ_MAX_TARGETS: usize = 25;

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
                "survey" => {
                    let args = GrepArgs {
                        pattern: r"^[[:space:]]*(struct|union|enum|typedef)[[:space:]]|^[A-Za-z_][A-Za-z0-9_[:space:]_*]*[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\(".into(),
                        path: Some(fu.name.clone()),
                        glob: None,
                    };
                    match grep(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("fallback:survey:{}", fu.name),
                            "content": content,
                            "note": "semcode file_survey was unavailable; this is a file-scoped local definition-match inventory, not a complete Tree-sitter survey",
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("fallback:survey:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
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
                "type" => {
                    let args = GrepArgs {
                        pattern: type_definition_pattern(&fu.name),
                        path: fu.path.clone(),
                        glob: None,
                    };
                    match grep(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("type:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("type:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "source" => fetch_source_fallback(&self.workspace, fu, &mut out).await,
                "callers" | "callees" => {
                    let args = symbol_grep_args(&fu.name, fu.path.clone(), Some("*.[chS]"));
                    match grep(&self.workspace, &args).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("fallback:{}:{}", fu.kind, fu.name),
                            "content": content,
                            "note": "semcode callgraph lookup was unavailable or not configured; this is a local ripgrep fallback for symbol references",
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("fallback:{}:{}", fu.kind, fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "file" | "find" => {
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
                "make" => {
                    match make_run(&self.workspace, &fu.name, Some(300)).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("make:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("make:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "meson" => {
                    match meson_run(&self.workspace, &fu.name, Some(300)).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("meson:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("meson:{}", fu.name),
                            "error": e.to_string(),
                        })),
                    }
                }
                "cargo" => {
                    match cargo_run(&self.workspace, &fu.name, Some(300)).await {
                        Ok(content) => out.context.push(json!({
                            "source": format!("cargo:{}", fu.name),
                            "content": content,
                        })),
                        Err(e) => out.context.push(json!({
                            "source": format!("cargo:{}", fu.name),
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

/// Escape regex metacharacters so a type name like `foo::bar<T>` can
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

fn type_definition_pattern(name: &str) -> String {
    let name = regex_escape_word(name);
    format!(
        r"^\s*(struct|union|enum)\s+{name}\b\s*(\{{|$)|^\s*typedef\b.*\b{name}\b|\}}\s*{name}\s*;"
    )
}

fn symbol_reference_pattern(name: &str) -> String {
    let name = regex_escape_word(name);
    format!(r"\b{name}\b")
}

fn symbol_grep_args(name: &str, path: Option<String>, glob: Option<&str>) -> GrepArgs {
    GrepArgs {
        pattern: symbol_reference_pattern(name),
        path,
        glob: glob.map(str::to_string),
    }
}

async fn fetch_source_fallback(workspace: &Path, fu: &Followup, out: &mut FetchResult) {
    let args = symbol_grep_args(&fu.name, fu.path.clone(), Some("*.[chS]"));
    match grep(workspace, &args).await {
        Ok(content) => {
            let read_targets = source_fallback_read_targets(&content);
            let auto_read = read_targets.len() <= SOURCE_FALLBACK_AUTO_READ_MAX_TARGETS;
            let note = if auto_read {
                format!(
                    "semcode source lookup was unavailable or not configured; this is a local ripgrep fallback. Every grep match is listed below. Because there are {} parseable match(es), bounded read ranges follow.",
                    read_targets.len()
                )
            } else {
                format!(
                    "semcode source lookup was unavailable or not configured; this is a local ripgrep fallback. Every grep match is listed below. There are {} parseable matches, so bounded reads were not expanded automatically; request targeted read followups for the specific file:line ranges needed.",
                    read_targets.len()
                )
            };
            out.context.push(json!({
                "source": format!("fallback:source:{}", fu.name),
                "content": content,
                "note": note,
            }));
            if !auto_read {
                out.context.push(json!({
                    "source": format!("fallback:source-read-skipped:{}", fu.name),
                    "content": format!(
                        "{} parseable grep matches for `{}` were listed in fallback:source:{}. Automatic {}-line source reads are skipped for broad fallback searches so the same match list is not immediately expanded into many unrelated full bodies. Request explicit read followups such as `path/to/file.c:123+80` for the matches that need full context.",
                        read_targets.len(),
                        fu.name,
                        fu.name,
                        SOURCE_FALLBACK_READ_LINES
                    ),
                    "note": "Broad local source fallback: grep match lines are provided above; detailed source must be requested with targeted read followups.",
                }));
                return;
            }
            for (file, line) in read_targets {
                let start = line.saturating_sub(SOURCE_FALLBACK_CONTEXT_BEFORE).max(1);
                let args = ReadArgs {
                    file: file.clone(),
                    line: Some(start),
                    count: Some(SOURCE_FALLBACK_READ_LINES),
                    end_line: None,
                };
                match read_file_range(workspace, &args) {
                    Ok(read_content) => out.context.push(json!({
                        "source": format!(
                            "fallback:source-read:{}:{}+{}",
                            file, start, SOURCE_FALLBACK_READ_LINES
                        ),
                        "content": read_content,
                        "note": format!(
                            "bounded read around local source fallback match for {} at {}:{}",
                            fu.name, file, line
                        ),
                    })),
                    Err(e) => out.context.push(json!({
                        "source": format!("fallback:source-read:{}:{}", file, line),
                        "error": e.to_string(),
                    })),
                }
            }
        }
        Err(e) => out.context.push(json!({
            "source": format!("fallback:source:{}", fu.name),
            "error": e.to_string(),
        })),
    }
}

fn source_fallback_read_targets(grep_output: &str) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for line in grep_output.lines() {
        if let Some((file, line_no, _text)) = parse_grep_match_line(line) {
            let start = line_no
                .saturating_sub(SOURCE_FALLBACK_CONTEXT_BEFORE)
                .max(1);
            if out.iter().any(|(seen_file, seen_line)| {
                seen_file == file
                    && seen_line
                        .saturating_sub(SOURCE_FALLBACK_CONTEXT_BEFORE)
                        .max(1)
                        == start
            }) {
                continue;
            }
            out.push((file.to_string(), line_no));
        }
    }
    out
}

fn parse_grep_match_line(line: &str) -> Option<(&str, u32, &str)> {
    for (idx, _) in line.match_indices(':') {
        let rest = &line[idx + 1..];
        let Some((line_no, text)) = rest.split_once(':') else {
            continue;
        };
        if line_no.is_empty() || !line_no.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let file = &line[..idx];
        if file.is_empty() {
            continue;
        }
        return Some((file, line_no.parse().ok()?, text));
    }
    None
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
                    nice_to_have: false,
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
    async fn survey_fallback_returns_scoped_definition_matches() {
        let dir = tmpdir("survey-fallback");
        let mut f = std::fs::File::create(dir.join("large.c")).unwrap();
        f.write_all(
            b"struct demo { int value; };\n\
              static int helper(struct demo *demo)\n\
              {\n\
                  return demo->value;\n\
              }\n",
        )
        .unwrap();
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "survey".into(),
                    name: "large.c".into(),
                    reason: "build inventory".into(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        let item = &r.context[0];
        assert_eq!(
            item.get("source").and_then(|v| v.as_str()),
            Some("fallback:survey:large.c")
        );
        let content = item.get("content").and_then(|v| v.as_str()).unwrap();
        assert!(content.contains("struct demo"));
        assert!(content.contains("static int helper"));
        assert!(!content.contains("return demo->value"));
        assert!(item
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("not a complete Tree-sitter survey"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn type_followup_fallback_searches_definitions_not_references() {
        let dir = tmpdir("type");
        let mut f = std::fs::File::create(dir.join("types.h")).unwrap();
        f.write_all(
            b"void use_bio(struct bio *bio);\n\
              int bio_count;\n\
              struct bio *member;\n\
              struct bio {\n\
                  unsigned int bi_opf;\n\
              };\n\
              typedef unsigned long sector_t;\n",
        )
        .unwrap();
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "type".into(),
                    name: "bio".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        let content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert!(content.contains("struct bio {"));
        assert!(!content.contains("void use_bio"));
        assert!(!content.contains("bio_count"));
        assert!(!content.contains("struct bio *member"));

        let r = f
            .fetch(
                &[Followup {
                    kind: "type".into(),
                    name: "sector_t".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        let content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert!(content.contains("typedef unsigned long sector_t;"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn source_followup_falls_back_to_local_symbol_search_and_read() {
        let dir = tmpdir("source-fallback");
        std::fs::create_dir_all(dir.join("mm")).unwrap();
        let mut src = std::fs::File::create(dir.join("mm/cma.c")).unwrap();
        src.write_all(
            b"static void helper(void) {}\n\
              bool cma_release(void)\n\
              {\n\
                  helper();\n\
                  return true;\n\
              }\n",
        )
        .unwrap();
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "source".into(),
                    name: "cma_release".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 2);
        assert_eq!(
            r.context[0].get("source").and_then(|v| v.as_str()),
            Some("fallback:source:cma_release")
        );
        let content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert!(content.contains("mm/cma.c"));
        assert!(content.contains("cma_release"));
        assert!(r.context[1]
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap()
            .starts_with("fallback:source-read:"));
        let content = r.context[1].get("content").unwrap().as_str().unwrap();
        assert!(content.contains("bool cma_release(void)"));
        assert!(content.contains("helper();"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn broad_source_fallback_lists_matches_without_auto_reading_every_hit() {
        let dir = tmpdir("source-fallback-broad");
        std::fs::create_dir_all(dir.join("mm")).unwrap();
        for idx in 0..=SOURCE_FALLBACK_AUTO_READ_MAX_TARGETS {
            let mut src = std::fs::File::create(dir.join(format!("mm/file{idx}.c"))).unwrap();
            writeln!(src, "void use_{idx}(struct page *page)").unwrap();
            writeln!(src, "{{").unwrap();
            writeln!(src, "\tstruct folio *folio = page_folio(page);").unwrap();
            writeln!(src, "\t(void)folio;").unwrap();
            writeln!(src, "}}").unwrap();
        }
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "source".into(),
                    name: "page_folio".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 2);
        assert_eq!(
            r.context[0].get("source").and_then(|v| v.as_str()),
            Some("fallback:source:page_folio")
        );
        let grep_content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert!(grep_content.contains("file0.c"));
        assert!(grep_content.contains(&format!("file{}.c", SOURCE_FALLBACK_AUTO_READ_MAX_TARGETS)));
        assert!(r.context[0]
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("request targeted read followups"));
        assert_eq!(
            r.context[1].get("source").and_then(|v| v.as_str()),
            Some("fallback:source-read-skipped:page_folio")
        );
        assert!(r.context[1]
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("Automatic 120-line source reads are skipped"));
        assert!(!r.context.iter().any(|item| item
            .get("source")
            .and_then(|v| v.as_str())
            .is_some_and(|source| source.starts_with("fallback:source-read:"))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn source_fallback_does_not_apply_per_file_grep_cap() {
        let dir = tmpdir("source-fallback-no-per-file-cap");
        std::fs::create_dir_all(dir.join("mm")).unwrap();
        let mut src = std::fs::File::create(dir.join("mm/many.c")).unwrap();
        for idx in 0..505 {
            writeln!(src, "void use_{idx}(struct page *page)").unwrap();
            writeln!(src, "{{").unwrap();
            writeln!(src, "\tstruct folio *folio = page_folio(page);").unwrap();
            writeln!(src, "\t(void)folio;").unwrap();
            writeln!(src, "}}").unwrap();
        }
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "source".into(),
                    name: "page_folio".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        let grep_content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert_eq!(grep_content.matches("page_folio(page)").count(), 505);
        assert_eq!(
            r.context[1].get("source").and_then(|v| v.as_str()),
            Some("fallback:source-read-skipped:page_folio")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn source_fallback_targets_preserve_grep_order_and_all_ranges() {
        let grep_output = "\
/tmp/tree/mm/demo.c:4:bool cma_release(void);
/tmp/tree/mm/demo.c:140:bool cma_release(void)
/tmp/tree/include/linux/demo.h:9:bool cma_release(void);
";
        let targets = source_fallback_read_targets(grep_output);
        assert_eq!(
            targets,
            vec![
                ("/tmp/tree/mm/demo.c".to_string(), 4),
                ("/tmp/tree/mm/demo.c".to_string(), 140),
                ("/tmp/tree/include/linux/demo.h".to_string(), 9),
            ]
        );
    }

    #[test]
    fn source_fallback_targets_do_not_rank_macro_definitions() {
        let grep_output = "\
/tmp/tree/mm/demo.c:20:if (VM_BUG_ON_FOLIO(folio))
/tmp/tree/include/linux/mmdebug.h:10:#define VM_BUG_ON_FOLIO(folio) do { } while (0)
";
        let targets = source_fallback_read_targets(grep_output);
        assert_eq!(
            targets,
            vec![
                ("/tmp/tree/mm/demo.c".to_string(), 20),
                ("/tmp/tree/include/linux/mmdebug.h".to_string(), 10),
            ]
        );
    }

    #[tokio::test]
    async fn unhandled_followup_kind_produces_explanatory_error() {
        let dir = tmpdir("unk");
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "widget".into(),
                    name: "some_func".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 1);
        let err = r.context[0].get("error").unwrap().as_str().unwrap();
        assert!(err.contains("widget"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fetches_make_followup() {
        let dir = tmpdir("make");
        let mut f = std::fs::File::create(dir.join("Makefile")).unwrap();
        f.write_all(b"check:\n\t@printf make-ok\n").unwrap();
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "make".into(),
                    name: "check".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 1);
        let content = r.context[0].get("content").unwrap().as_str().unwrap();
        assert!(content.contains("make-ok"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn make_followup_does_not_shell_out() {
        let dir = tmpdir("make-noshell");
        let mut f = std::fs::File::create(dir.join("Makefile")).unwrap();
        f.write_all(b"check:\n\t@printf make-ok\n").unwrap();
        let f = WorkspaceFetcher::new(&dir);
        let r = f
            .fetch(
                &[Followup {
                    kind: "make".into(),
                    name: "check; touch owned".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.context.len(), 1);
        assert!(
            r.context[0].get("error").is_some(),
            "invalid make target should fail, not execute a shell"
        );
        assert!(!dir.join("owned").exists());
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
