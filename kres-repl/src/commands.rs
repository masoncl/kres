//! REPL command parsing and dispatch.

/// Parsed form of a user input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/help` — print command list.
    Help,
    /// `/tasks` — list active tasks.
    Tasks,
    /// `/findings` — summarise the current findings list.
    Findings,
    /// `/stop` — cancel every running task.
    Stop,
    /// `/clear` — cancel tasks AND reset the in-memory findings +
    /// todo + accumulated-analysis state. After /clear the next
    /// prompt starts with no conversational context carried
    /// forward.
    Clear,
    /// `/compact` — replace the accumulated-analysis ledger with a
    /// short fast-agent-produced summary. Keeps conversational
    /// continuity (subsequent prompts still see a brief of what was
    /// done) while dropping the bulk that was bloating the
    /// preamble attached to every new prompt.
    Compact,
    /// `/cost` — print accumulated token usage.
    Cost,
    /// `/todo [--clear]` — show or clear the current todo list.
    Todo { clear: bool },
    /// `/plan` — show the current plan (step id, status, title)
    /// if one was produced by `define_plan` when the prompt was
    /// submitted. Prints a reminder when no plan exists.
    Plan,
    /// `/resume [PATH]` — load plan + todo + deferred + turn
    /// counter from a persisted `session.json`. When PATH is
    /// omitted, reads `<results>/session.json.prev` if present
    /// (the backup kres writes at startup when you did not pass
    /// `--resume`) then falls back to `<results>/session.json`.
    /// Overwrites the current in-memory state, so run it before
    /// submitting prompts.
    Resume { path: Option<String> },
    /// `/followup` — list items deferred by goal-met or --turns cap.
    Followup,
    /// `/summary [filename]` — render the run's report.md +
    /// findings.json into a plain-text summary. Filename defaults to
    /// `summary.txt`, placed in the results directory when one was
    /// configured (else the current working directory).
    Summary { filename: Option<String> },
    /// `/summary-markdown [filename]` — same as /summary but
    /// selects the `summary-markdown` slash-command template for
    /// the system prompt and defaults the filename to `summary.md`.
    SummaryMarkdown { filename: Option<String> },
    /// `/review <target>` — submit a prompt equivalent to
    /// `--prompt "review: <target>"`. Composes the `review`
    /// slash-command template (disk override at
    /// ~/.kres/commands/review.md wins over the embedded copy)
    /// with the trailing target text and queues it as a new task.
    Review { target: String },
    /// `/extract [--dir DIR] [--report F] [--todo F] [--findings F]`
    /// — copy session artifacts to operator-chosen destinations.
    Extract {
        dir: Option<String>,
        report: Option<String>,
        todo: Option<String>,
        findings: Option<String>,
    },
    /// `/done N` — remove the N'th pending todo item.
    Done { index: usize },
    /// `/report <path>` — write a findings report to a markdown file.
    Report { path: String },
    /// `/load <path>` — submit a file's contents as the next prompt.
    Load { path: String },
    /// `/edit` — open $EDITOR on a scratch file and submit what's typed.
    Edit,
    /// `/reply <text>` — prepend the last task's analysis to new text.
    Reply { text: String },
    /// `/next` — dispatch the first pending todo item as a prompt.
    Next,
    /// `/continue` — dispatch every unblocked pending todo item.
    Continue,
    /// `/quit` or `/exit` — leave the REPL.
    Quit,
    /// Any non-slash input (or a slash we don't recognise) submitted
    /// verbatim as the next prompt.
    Prompt(String),
    /// Unknown `/slash` — surface to the user rather than treat as a
    /// prompt so a typo doesn't become an unintended API call
    /// (bugs.md#M8 adapted).
    Unknown(String),
    /// Blank line — REPL skips.
    Noop,
}

pub fn parse_command(line: &str) -> Command {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Noop;
    }
    if let Some(cmd) = trimmed.strip_prefix('/') {
        let (head, rest) = match cmd.split_once(' ') {
            Some((h, r)) => (h, r.trim()),
            None => (cmd, ""),
        };
        return match head {
            "help" | "?" => Command::Help,
            "tasks" | "task" => Command::Tasks,
            "findings" => Command::Findings,
            "stop" => Command::Stop,
            "clear" => Command::Clear,
            "compact" => Command::Compact,
            "cost" => Command::Cost,
            "todo" => Command::Todo {
                clear: rest.split_whitespace().any(|tok| tok == "--clear"),
            },
            "plan" => Command::Plan,
            "resume" => Command::Resume {
                path: {
                    let t = rest.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                },
            },
            "followup" | "followups" | "deferred" => Command::Followup,
            "summary" => Command::Summary {
                filename: rest.split_whitespace().next().map(|s| s.to_string()),
            },
            "summary-markdown" => Command::SummaryMarkdown {
                filename: rest.split_whitespace().next().map(|s| s.to_string()),
            },
            "review" => {
                let target = rest.trim().to_string();
                if target.is_empty() {
                    Command::Unknown(
                        "review (expected: /review <target>, e.g. /review fs/btrfs/ctree.c)".into(),
                    )
                } else {
                    Command::Review { target }
                }
            }
            "extract" => Command::Extract {
                dir: flag_value(rest, "--dir").map(|s| s.to_string()),
                report: flag_value(rest, "--report").map(|s| s.to_string()),
                todo: flag_value(rest, "--todo").map(|s| s.to_string()),
                findings: flag_value(rest, "--findings").map(|s| s.to_string()),
            },
            "done" => match rest.split_whitespace().next().and_then(|s| s.parse().ok()) {
                Some(n) => Command::Done { index: n },
                None => Command::Unknown("done (expected a number)".into()),
            },
            "report" => Command::Report {
                path: rest.to_string(),
            },
            "load" => Command::Load {
                path: rest.to_string(),
            },
            "edit" => Command::Edit,
            "reply" => Command::Reply {
                text: rest.to_string(),
            },
            "next" => Command::Next,
            "continue" => Command::Continue,
            "quit" | "exit" | "bye" | "q" => Command::Quit,
            other => Command::Unknown(other.to_string()),
        };
    }
    Command::Prompt(trimmed.to_string())
}

/// Return the value following a named flag (order-independent).
/// `"--dir /tmp --report r.md"` with `"--dir"` → `Some("/tmp")`.
/// `"--dir /tmp"` with `"--report"` → `None`.
fn flag_value<'a>(s: &'a str, flag: &str) -> Option<&'a str> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == flag {
            return toks.get(i + 1).copied();
        }
        // Support `--flag=value` inline form too.
        if let Some(rest) = toks[i].strip_prefix(flag) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_help() {
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(parse_command("/?"), Command::Help);
    }

    #[test]
    fn parses_tasks() {
        assert_eq!(parse_command("/tasks"), Command::Tasks);
        assert_eq!(parse_command("/task"), Command::Tasks);
    }

    #[test]
    fn parses_stop() {
        assert_eq!(parse_command("/stop"), Command::Stop);
    }

    #[test]
    fn parses_findings() {
        assert_eq!(parse_command("/findings"), Command::Findings);
    }

    #[test]
    fn parses_clear() {
        assert_eq!(parse_command("/clear"), Command::Clear);
    }

    #[test]
    fn parses_compact() {
        assert_eq!(parse_command("/compact"), Command::Compact);
    }

    #[test]
    fn parses_cost() {
        assert_eq!(parse_command("/cost"), Command::Cost);
    }

    #[test]
    fn parses_todo() {
        assert_eq!(parse_command("/todo"), Command::Todo { clear: false });
        assert_eq!(
            parse_command("/todo --clear"),
            Command::Todo { clear: true }
        );
    }

    #[test]
    fn parses_plan() {
        assert_eq!(parse_command("/plan"), Command::Plan);
    }

    #[test]
    fn parses_resume_without_path() {
        assert_eq!(parse_command("/resume"), Command::Resume { path: None });
    }

    #[test]
    fn parses_resume_with_path() {
        assert_eq!(
            parse_command("/resume /tmp/foo.json"),
            Command::Resume {
                path: Some("/tmp/foo.json".into())
            }
        );
    }

    #[test]
    fn parses_followup_and_deferred() {
        assert_eq!(parse_command("/followup"), Command::Followup);
        assert_eq!(parse_command("/deferred"), Command::Followup);
    }

    #[test]
    fn parses_summary_markdown() {
        assert_eq!(
            parse_command("/summary-markdown"),
            Command::SummaryMarkdown { filename: None }
        );
        assert_eq!(
            parse_command("/summary-markdown report.md"),
            Command::SummaryMarkdown {
                filename: Some("report.md".into())
            }
        );
    }

    #[test]
    fn parses_review_with_target() {
        match parse_command("/review fs/btrfs/ctree.c") {
            Command::Review { target } => {
                assert_eq!(target, "fs/btrfs/ctree.c");
            }
            other => panic!("expected Review, got {other:?}"),
        }
    }

    #[test]
    fn review_without_target_is_unknown() {
        match parse_command("/review") {
            Command::Unknown(s) => {
                assert!(s.starts_with("review"), "got {s}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parses_summary() {
        assert_eq!(
            parse_command("/summary"),
            Command::Summary { filename: None }
        );
        assert_eq!(
            parse_command("/summary report.txt"),
            Command::Summary {
                filename: Some("report.txt".to_string())
            }
        );
    }

    #[test]
    fn parses_done_with_index() {
        assert_eq!(parse_command("/done 3"), Command::Done { index: 3 });
    }

    #[test]
    fn parses_done_rejects_non_numeric() {
        match parse_command("/done abc") {
            Command::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parses_extract_flags_independent_of_order() {
        let c =
            parse_command("/extract --dir /tmp/out --report r.md --todo t.md --findings f.json");
        match c {
            Command::Extract {
                dir,
                report,
                todo,
                findings,
            } => {
                assert_eq!(dir.as_deref(), Some("/tmp/out"));
                assert_eq!(report.as_deref(), Some("r.md"));
                assert_eq!(todo.as_deref(), Some("t.md"));
                assert_eq!(findings.as_deref(), Some("f.json"));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
        // Reorder: --report first.
        let c = parse_command("/extract --report out.md --dir /a");
        match c {
            Command::Extract { dir, report, .. } => {
                assert_eq!(dir.as_deref(), Some("/a"));
                assert_eq!(report.as_deref(), Some("out.md"));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn parses_extract_inline_equals() {
        match parse_command("/extract --dir=/tmp/x --report=r.md") {
            Command::Extract { dir, report, .. } => {
                assert_eq!(dir.as_deref(), Some("/tmp/x"));
                assert_eq!(report.as_deref(), Some("r.md"));
            }
            other => panic!("expected Extract, got {other:?}"),
        }
    }

    #[test]
    fn parses_report_with_path() {
        match parse_command("/report ./findings.md") {
            Command::Report { path } => assert_eq!(path, "./findings.md"),
            other => panic!("expected Report, got {other:?}"),
        }
    }

    #[test]
    fn parses_report_with_no_path() {
        match parse_command("/report") {
            Command::Report { path } => assert_eq!(path, ""),
            other => panic!("expected Report, got {other:?}"),
        }
    }

    #[test]
    fn parses_load_with_path() {
        match parse_command("/load /tmp/prompt.md") {
            Command::Load { path } => assert_eq!(path, "/tmp/prompt.md"),
            other => panic!("expected Load, got {other:?}"),
        }
    }

    #[test]
    fn parses_edit() {
        assert_eq!(parse_command("/edit"), Command::Edit);
    }

    #[test]
    fn parses_reply_with_text() {
        match parse_command("/reply more context") {
            Command::Reply { text } => assert_eq!(text, "more context"),
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn parses_next() {
        assert_eq!(parse_command("/next"), Command::Next);
    }

    #[test]
    fn parses_continue() {
        assert_eq!(parse_command("/continue"), Command::Continue);
    }

    #[test]
    fn parses_quit() {
        assert_eq!(parse_command("/quit"), Command::Quit);
        assert_eq!(parse_command("/exit"), Command::Quit);
        assert_eq!(parse_command("/bye"), Command::Quit);
        assert_eq!(parse_command("/q"), Command::Quit);
    }

    #[test]
    fn unknown_slash_not_treated_as_prompt() {
        // bugs.md#M8 protection: a typo'd `/xyz` surfaces as Unknown
        // rather than being sent to the LLM as a prompt.
        match parse_command("/xyz") {
            Command::Unknown(s) => assert_eq!(s, "xyz"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn prompt_passthrough() {
        match parse_command("find all bugs please") {
            Command::Prompt(s) => assert_eq!(s, "find all bugs please"),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn blank_is_noop() {
        assert_eq!(parse_command(""), Command::Noop);
        assert_eq!(parse_command("   \t  "), Command::Noop);
    }

    #[test]
    fn trims_leading_whitespace_for_commands() {
        assert_eq!(parse_command("   /help  "), Command::Help);
    }
}
