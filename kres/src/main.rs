//! kres — kernel code RESearch agent.
//!
//! `kres test` and `kres turn` are small one-shot tools around the
//! Anthropic API; the REPL (the default subcommand) is the main entry
//! point.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

mod turn;

/// Emit a startup-banner line in dimmed ("dark white") style so the
/// metadata block visually settles below the eye level of the
/// agent-traffic lines that follow. Wraps `kres_core::async_eprintln!`.
macro_rules! banner {
    ($($arg:tt)*) => {{
        use owo_colors::OwoColorize;
        kres_core::async_eprintln!("{}", format!($($arg)*).dimmed());
    }};
}

/// kres entry point. The REPL is the default; specifying `test` or
/// `turn` runs the sub-tool instead.
#[derive(Parser, Debug)]
#[command(version, about = "Kernel code research agent", long_about = None)]
struct Cli {
    /// Sub-tool (omit for the default interactive REPL).
    #[command(subcommand)]
    cmd: Option<Command>,

    /// REPL flags (in scope when no subcommand is given).
    #[command(flatten)]
    repl: ReplArgs,

    /// `RUST_LOG`-style filter (e.g. `kres=debug`).
    #[arg(long, global = true)]
    log: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Hello-world smoke test against the API.
    Test(TestArgs),
    /// One-shot large-context turn: JSON/stdin → streamed response file.
    Turn(TurnArgs),
}

#[derive(Args, Debug)]
struct ReplArgs {
    /// Fast code agent config (context gathering). Defaults to
    /// ~/.kres/fast-code-agent.json.
    #[arg(long)]
    fast_agent: Option<PathBuf>,
    /// Slow agent tag — picks ~/.kres/slow-code-agent-<tag>.json
    /// (or the shipped configs/ default). When omitted the file
    /// resolver falls back to "sonnet" but the slow model id from
    /// settings.json is left alone. When passed AND the tag is a
    /// known shorthand (sonnet/opus) the matching model id ALSO
    /// overrides settings.models.slow — pass --slow-model to
    /// override the model independently.
    #[arg(long)]
    slow: Option<String>,
    /// Explicit slow-agent config path (overrides --slow).
    #[arg(long)]
    slow_agent: Option<PathBuf>,
    /// Override the fast-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    fast_model: Option<String>,
    /// Override the slow-agent model id. Beats settings.json and
    /// beats the tag-derived default from --slow.
    #[arg(long, value_name = "ID")]
    slow_model: Option<String>,
    /// Override the main-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    main_model: Option<String>,
    /// Override the todo-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    todo_model: Option<String>,
    /// Main agent config JSON file. Defaults to
    /// ~/.kres/main-agent.json.
    #[arg(long)]
    main_agent: Option<PathBuf>,
    /// Todo-list maintenance agent config. A tools-disabled variant
    /// used for update_todo calls so the main agent's tool-dispatch
    /// system prompt doesn't cause it to emit <actions> tags or
    /// hallucinate research. Defaults to ~/.kres/todo-agent.json.
    #[arg(long)]
    todo_agent: Option<PathBuf>,
    /// MCP servers config JSON file. Defaults to ~/.kres/mcp.json.
    /// Currently accepted for CLI parity with ; MCP plumbing
    /// lives in kres-mcp and will consume this path when wired in
    /// the data-fetcher.
    #[arg(long)]
    mcp_config: Option<PathBuf>,
    /// Stop after N completed task runs (a "run" is a task that
    /// went through the slow agent successfully). 0 = unlimited,
    /// the default.
    #[arg(long, default_value_t = 0, value_name = "N")]
    turns: u32,
    /// When `--turns 0` (unlimited), add a secondary stop on
    /// stagnation: if 3 consecutive analysis-producing runs fail to
    /// grow the findings list, exit even if the goal agent has not
    /// declared completion. Without `--follow`, `--turns 0` trusts
    /// the goal agent and keeps running until the goal is met (the
    /// goal-met handler drains the todo list). When no goal agent is
    /// configured, `--turns 0` without `--follow` stops as soon as
    /// the active batch finishes and defers any leftover followups
    /// to /followup. Ignored when `--turns N > 0` — the run-count
    /// cap still wins there.
    #[arg(long, default_value_t = false)]
    follow: bool,
    /// Resume from a prior `session.json` in the results dir.
    /// When false (default), kres ignores any existing session.json
    /// and starts clean — even when `--results DIR` points at a
    /// directory that has one. Pass `--resume` to explicitly load
    /// the persisted plan + todo + deferred + counter state. This
    /// is off by default because an accidentally-shared results
    /// dir between runs would otherwise bleed prior state into a
    /// new session. When a session.json exists but `--resume` is
    /// absent, kres prints a hint pointing at the file.
    #[arg(long, default_value_t = false)]
    resume: bool,
    /// Directory for all three artifact files (findings.json,
    /// report.md, todo.md). Defaults to ~/.kres/sessions/<session-id>/.
    /// Per-file flags (--findings/--report/--todo) still override.
    #[arg(long, value_name = "DIR")]
    results: Option<PathBuf>,
    /// JSON file tracking actionable bug findings across tasks.
    /// See docs/findings-json-format.md. If the file exists, its
    /// findings are loaded; it is rewritten after every task.
    /// Defaults to <results>/findings.json. Accepts `--finding`
    /// (singular) too.
    #[arg(long, alias = "finding", value_name = "FILE")]
    findings: Option<PathBuf>,
    /// Markdown report file (appended after each task). Defaults
    /// to <results>/report.md.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,
    /// Markdown todo file (updated with next steps). Defaults to
    /// <results>/todo.md.
    #[arg(long, value_name = "FILE")]
    todo: Option<PathBuf>,
    /// Initial prompt. Three forms:
    ///
    ///   1. `--prompt /path/to/file.md` — read the file verbatim.
    ///      `[kind] name[: reason]` lines become session-wide
    ///      slow-agent lenses, the rest is submitted as the opening
    ///      prompt.
    ///   2. `--prompt "word: extra details"` — look for
    ///      `~/.kres/prompts/<word>-template.md`. If it exists, the
    ///      extra details are prepended to the template contents and
    ///      that combined text becomes the prompt. e.g.
    ///      `--prompt "review: all interfaces in kernel/futex/*.c"`.
    ///   3. `--prompt "<anything else>"` — submitted verbatim as the
    ///      opening prompt.
    #[arg(long, value_name = "PROMPT")]
    prompt: Option<String>,
    /// Workspace for local tools (read/grep/git).
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Directory of skill `*.md` files. When given, auto-loaded
    /// skills are attached to every fast-agent prompt. Defaults to
    /// ~/.kres/skills/.
    #[arg(long)]
    skills: Option<PathBuf>,
    /// Max fast↔main gather rounds before forcing slow (bugs.md#M5).
    #[arg(long, default_value_t = 5)]
    gather_turns: u8,
    /// Grace period (ms) for `/stop` / Ctrl-C before aborting tasks.
    #[arg(long, default_value_t = 5_000)]
    stop_grace_ms: u64,
    /// Plain stdio mode: skip the persistent status-line scroll
    /// region and the DECSTBM fuss. Useful when the terminal is a
    /// pipe, a dumb tty, or something that doesn't handle scroll
    /// regions (mosh, some tmux configs).
    #[arg(long, default_value_t = false)]
    stdio: bool,

    /// Render a summary from a prior run's report.md +
    /// findings.json and exit without starting the REPL. Uses the
    /// fast agent with the embedded `summary` template as the
    /// system prompt. Single-shot when the inputs fit
    /// `max_input_tokens`; on overflow, splits findings into chunks,
    /// renders one partial summary per chunk, then runs a combine
    /// pass to merge them. Pairs with --report, --findings, and
    /// --results (or their defaults) to locate the inputs. The
    /// output filename is `summary.txt`, placed in the results
    /// directory when --results was supplied, otherwise in the
    /// current working directory.
    #[arg(long, default_value_t = false)]
    summary: bool,

    /// Markdown variant of --summary. Selects the
    /// `summary-markdown` template and writes `summary.md` instead
    /// of `summary.txt`. Mutually useful with --template FILE, in
    /// which case the explicit template wins over the variant
    /// picker but the filename still defaults to `summary.md`.
    #[arg(long, default_value_t = false)]
    summary_markdown: bool,

    /// Override the summary template path for --summary /
    /// --summary-markdown. Accepted by `/summary` too. When
    /// omitted, kres reads `~/.kres/commands/summary.md` (or
    /// `summary-markdown.md` for the markdown variant — the
    /// operator-override path, empty by default) and falls back to
    /// the compiled-in copy bundled in the binary (see
    /// `kres-agents/src/user_commands.rs`).
    #[arg(long, value_name = "FILE")]
    template: Option<PathBuf>,

    /// Allow one additional non-MCP action type for this session.
    /// Repeatable (`--allow bash --allow git`) or comma-separated
    /// (`--allow bash,git`). Adds to whatever `actions.allowed`
    /// resolved to from settings.json. The default allowlist is
    /// grep/find/read/git/edit — `bash` is OFF by default because
    /// operators report it becoming an escape hatch for things the
    /// typed tools already cover. Example: `--allow bash` enables
    /// the bash tool for compile+run in coding flows. The special
    /// value `--allow all` enables every action type the dispatcher
    /// knows (including bash).
    #[arg(long, value_name = "ACTION", value_delimiter = ',')]
    allow: Vec<String>,
}

#[derive(Parser, Debug)]
struct TestArgs {
    /// Path to API-key file (model is auto-selected from filename).
    key_file: PathBuf,
    /// Override the model id.
    #[arg(long)]
    model: Option<String>,
    /// Prompt to send.
    #[arg(short, long, default_value = "Say hello in one sentence.")]
    prompt: String,
}

#[derive(Parser, Debug)]
struct TurnArgs {
    /// Path to API-key file (model is auto-selected from filename).
    key_file: PathBuf,
    /// JSON input file (stdin is used if omitted).
    #[arg(short, long)]
    input: Option<PathBuf>,
    /// Output file for the response.
    #[arg(short, long)]
    output: PathBuf,
    /// Override model id.
    #[arg(long)]
    model: Option<String>,
    /// Override max_tokens.
    #[arg(long)]
    max_tokens: Option<u32>,
    /// Inline system prompt (overrides JSON).
    #[arg(short, long)]
    system: Option<String>,
    /// Read the system prompt from a file (overrides JSON, not --system).
    #[arg(long)]
    system_file: Option<PathBuf>,
    /// Thinking budget in tokens. 0 disables. Default: safe 1/4 of
    /// max_tokens capped at 32000 (bugs.md#R2).
    #[arg(long)]
    thinking_budget: Option<u32>,
    /// Temperature. Only honoured when thinking is disabled.
    #[arg(long)]
    temperature: Option<f32>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = match cli.cmd {
        Some(Command::Test(args)) => rt.block_on(run_test(args)),
        Some(Command::Turn(args)) => rt.block_on(turn::run_turn(args)),
        None => rt.block_on(run_repl(cli.repl)),
    };

    // The REPL's stdin reader lives on a `tokio::task::spawn_blocking`
    // thread that's blocked inside `rustyline::readline()` — a
    // `read(2)` syscall on a tty can't be interrupted from userspace.
    // Dropping the runtime normally waits for all blocking tasks to
    // finish, which hangs forever until the user types another line.
    //
    // Every kres side-effect that must reach disk (TurnLogger, the
    // FindingsStore's tmp-file+rename writes, report.md append) is
    // either fsync'd on each write or synchronously flushed before the
    // REPL loop returns. A direct `exit()` therefore loses no data and
    // avoids the drop-waits-for-readline deadlock. `shutdown_timeout`
    // with a short grace would work too but still blocks for the
    // grace period on every clean exit, which is visible to the
    // operator.
    rt.shutdown_background();
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            kres_core::async_eprintln!("error: {e:?}");
            std::process::exit(1);
        }
    }
}

/// Path to `~/.kres/` — the per-user config dir. Returns None when
/// $HOME is unset. Used to locate the default names for agent JSON
/// files, the skills directory, findings base, and mcp.json.
/// Resolve the --prompt CLI argument into (source-description, body).
///
/// Recognised forms:
///   1. Path to an existing file → `(path.display(), file-contents)`.
///   2. `"word: extra"` or `"/word extra"` naming a slash-command
///      template (embedded default plus optional override at
///      `~/.kres/commands/<word>.md`) → `(source-label, extra +
///      "\n\n" + command-body)`. Both forms are equivalent:
///      `--prompt "review: fs/btrfs/ctree.c"` and
///      `--prompt "/review fs/btrfs/ctree.c"` produce the same
///      composed prompt.
///   3. Legacy `~/.kres/prompts/<word>-template.md` lookup — kept
///      as a back-compat fallback so operators with custom
///      `<word>-template.md` files from before the slash-command
///      refactor keep working without edits. The new location
///      `~/.kres/commands/<word>.md` is preferred.
///   4. Anything else → `("<inline>", raw)`.
fn resolve_prompt_arg(raw: &str) -> Result<(String, String)> {
    // Form 1: existing file path wins outright, including when the
    // name happens to contain a colon.
    let as_path = std::path::Path::new(raw);
    if as_path.exists() && as_path.is_file() {
        let body = std::fs::read_to_string(as_path)
            .with_context(|| format!("reading prompt file {}", as_path.display()))?;
        return Ok((as_path.display().to_string(), body));
    }

    // Form 2: try to extract a command name and the trailing extra
    // text from either "word: extra" or "/word extra". In both
    // cases the name must be a single bare word (alphanumerics,
    // dash, underscore) so free-form questions that happen to
    // contain colons or start with a slash don't false-match.
    let named: Option<(&str, &str)> = if let Some(after_slash) = raw.strip_prefix('/') {
        // `/word extra` — split on the first whitespace run.
        let (head, rest) = match after_slash.split_once(char::is_whitespace) {
            Some((h, r)) => (h, r.trim()),
            None => (after_slash, ""),
        };
        Some((head, rest))
    } else if let Some((head, rest)) = raw.split_once(':') {
        Some((head.trim(), rest.trim()))
    } else {
        None
    };
    if let Some((head, rest)) = named {
        // Preferred: ~/.kres/commands/<word>.md via user_commands
        // (disk-first + embedded fallback + name-validation). The
        // validation inside compose covers the same character set
        // we'd enforce here, so there's no need to pre-filter.
        if let Some((src, composed)) = kres_agents::user_commands::compose(head, rest) {
            return Ok((src, composed));
        }
        // Legacy: ~/.kres/prompts/<word>-template.md. Kept for
        // operators whose custom templates predate the slash-
        // command refactor. New templates should go under
        // ~/.kres/commands/<word>.md.
        let is_word = !head.is_empty()
            && head
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if is_word {
            if let Some(dir) = kres_dir() {
                let tmpl = dir.join("prompts").join(format!("{}-template.md", head));
                if tmpl.exists() {
                    let body = std::fs::read_to_string(&tmpl)
                        .with_context(|| format!("reading template {}", tmpl.display()))?;
                    let composed = if rest.is_empty() {
                        body
                    } else {
                        format!("{rest}\n\n{body}")
                    };
                    return Ok((tmpl.display().to_string(), composed));
                }
            }
        }
    }
    // Form 4: inline prompt text.
    Ok(("<inline>".to_string(), raw.to_string()))
}

fn kres_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".kres"))
}

/// Map `--slow <tag>` to a concrete model id when the tag is a
/// known shorthand. Keeps the flag useful as a model selector on
/// top of its historical role as a max_tokens variant picker.
/// Returns None for unknown tags so settings.json stays in charge.
fn slow_tag_to_model_id(tag: &str) -> Option<&'static str> {
    match tag {
        "sonnet" => Some("claude-sonnet-4-6"),
        "opus" => Some("claude-opus-4-7"),
        _ => None,
    }
}

/// Resolve an optional CLI path:
/// - If the caller passed `--foo /abs/path`, use it verbatim.
/// - Otherwise look in `~/.kres/<default_name>`. Return the path only
///   when it exists on disk; absent files collapse to `None` so the
///   caller's "not configured" branch fires instead of a noisy error.
fn resolve_default(cli: Option<&PathBuf>, default_name: &str) -> Option<PathBuf> {
    if let Some(p) = cli {
        return Some(p.clone());
    }
    let fallback = kres_dir()?.join(default_name);
    if fallback.exists() {
        Some(fallback)
    } else {
        None
    }
}

async fn run_repl(args: ReplArgs) -> Result<()> {
    use kres_agents::WorkspaceFetcher;
    use kres_core::TaskManager;
    use kres_repl::{build_orchestrator, ReplConfig, Session};
    use std::sync::Arc;

    // --- Resolve agent configs -------------------------------------
    // Explicit path wins; otherwise look in ~/.kres/<default>.
    let fast_agent = resolve_default(args.fast_agent.as_ref(), "fast-code-agent.json");

    // --slow is a tag; --slow-agent is an explicit path override.
    // Resolution for --slow: prefer ~/.kres/slow-code-agent-<tag>.json,
    // then fall back to <binary-repo>/configs/slow-code-agent-<tag>.json.
    // When the operator didn't pass --slow at all, default the file
    // resolver to "sonnet" — the model id is left to settings.json
    // (see the override block below).
    let slow_tag_for_file = args.slow.as_deref().unwrap_or("sonnet");
    let slow_tag_name = format!("slow-code-agent-{}.json", slow_tag_for_file);
    let slow_agent = args
        .slow_agent
        .clone()
        .or_else(|| {
            kres_dir()
                .map(|d| d.join(&slow_tag_name))
                .filter(|p| p.exists())
        })
        .or_else(|| {
            // Shipped-config fallback: <repo>/configs/<name>.json.
            // Anchor on the crate's manifest dir so this works no matter
            // where the binary was invoked from; fall back to cwd-relative
            // for installed binaries where CARGO_MANIFEST_DIR no longer
            // points to the shipping repo.
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let candidates = [
                manifest.join("../configs").join(&slow_tag_name),
                manifest.join("configs").join(&slow_tag_name),
                PathBuf::from("configs").join(&slow_tag_name),
            ];
            candidates.into_iter().find(|p| p.exists())
        });

    let main_agent = resolve_default(args.main_agent.as_ref(), "main-agent.json");
    let todo_agent = resolve_default(args.todo_agent.as_ref(), "todo-agent.json");
    let mcp_config = resolve_default(args.mcp_config.as_ref(), "mcp.json");
    let skills_dir = resolve_default(args.skills.as_ref(), "skills");

    // Per-user settings (~/.kres/settings.json). Carries the default
    // model-id for each agent role; picked up by every agent-
    // construction site via kres_repl::settings::pick_model.
    // Missing file is not an error — every field is optional.
    //
    // CLI model overrides are applied into this struct before any
    // pick_model call runs, so `--<role>-model` always wins. The
    // precedence (highest → lowest) inside pick_model stays:
    //   1. agent config's `"model"` field
    //   2. settings.models.<role>  ← CLI overrides land here
    //   3. Model::sonnet_4_6() fallback
    //
    // When --slow is passed as a known tag (sonnet/opus) we also map
    // it to a model id, so `--slow sonnet` actually switches the
    // slow model. Explicit --slow-model still beats the tag mapping.
    let mut settings = kres_repl::Settings::load_merged(&args.workspace);
    // Only map the --slow tag to a model id when the operator
    // actually passed --slow. Without this gate the clap default
    // "sonnet" would unconditionally overwrite settings.models.slow
    // every run, masking whatever the operator set in
    // ~/.kres/settings.json (user report 2026-04-21: settings.json
    // said claude-mythos-preview, banner reported claude-sonnet-4-6).
    if let Some(tag) = args.slow.as_deref() {
        if let Some(id) = slow_tag_to_model_id(tag) {
            settings.set_model(kres_repl::ModelRole::Slow, Some(id.to_string()));
        }
    }
    settings.set_model(kres_repl::ModelRole::Fast, args.fast_model.clone());
    settings.set_model(kres_repl::ModelRole::Slow, args.slow_model.clone());
    settings.set_model(kres_repl::ModelRole::Main, args.main_model.clone());
    settings.set_model(kres_repl::ModelRole::Todo, args.todo_model.clone());

    // --- Resolve artifact dir + per-file paths ---------------------
    // `--results DIR` sets the default dir for findings/report/todo.
    // Individual `--findings FILE`, `--report FILE`, `--todo FILE`
    // override their own slot. When --results is absent, the default
    // is ~/.kres/sessions/<session-id>/ (session-id is a timestamp).
    // Treat --summary and --summary-markdown as the same "standalone
    // summary" entry; the markdown flag just picks the variant
    // template and filename further down.
    let summary_mode = args.summary || args.summary_markdown;
    let markdown = args.summary_markdown;

    // In --summary mode we avoid creating a fresh session directory
    // because the operator points at an existing run's artifacts.
    let results_dir = match (args.results.clone(), summary_mode) {
        (Some(d), _) => d,
        (None, true) => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        (None, false) => {
            let base = kres_dir().unwrap_or_else(|| PathBuf::from("."));
            let session_id = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            base.join("sessions").join(session_id)
        }
    };
    let findings_base = Some(
        args.findings
            .clone()
            .unwrap_or_else(|| results_dir.join("findings.json")),
    );
    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| results_dir.join("report.md"));
    let todo_path = args
        .todo
        .clone()
        .unwrap_or_else(|| results_dir.join("todo.md"));

    // --- --summary / --summary-markdown: standalone rendering ----
    // Inputs come from --report / --findings / --results (or their
    // defaults above). Output is `summary.txt` (or `summary.md` with
    // --summary-markdown), living in the results dir when --results
    // was set and the cwd otherwise. Exits right after the file is
    // written; no REPL, no MCP, no orchestrator, no turn logger.
    if summary_mode {
        let fast_cfg_path = match fast_agent.as_ref() {
            Some(p) => p.clone(),
            None => {
                return Err(anyhow::anyhow!(
                    "--summary requires a fast agent config (pass --fast-agent or drop one in ~/.kres/fast-code-agent.json)"
                ));
            }
        };
        let findings_path = match findings_base.as_ref() {
            Some(p) if p.exists() => p.clone(),
            Some(p) => {
                return Err(anyhow::anyhow!(
                    "--summary: findings file {} does not exist",
                    p.display()
                ));
            }
            None => {
                return Err(anyhow::anyhow!(
                    "--summary: no findings path configured (pass --findings or --results)"
                ));
            }
        };
        let (fast_client, fast_model, fast_max_tokens, fast_max_input) =
            kres_repl::summary::load_fast_for_summary(&fast_cfg_path, &settings)?;
        // `results_dir` is already cwd when --results was absent (see
        // the match at the top of run_repl), so the output lands
        // alongside the inputs either way. `--summary-markdown` flips
        // the default filename to summary.md.
        let default_filename = if markdown { Some("summary.md") } else { None };
        let output_path =
            kres_repl::summary::default_output_path(Some(results_dir.as_path()), default_filename);
        // Original prompt lookup: prompt.md in the results dir wins,
        // since we only ever write it there (and only when the user
        // passed --results). Nothing to read from memory in the
        // standalone --summary path.
        let original_prompt = args.results.as_ref().and_then(|d| {
            let p = d.join("prompt.md");
            match std::fs::read_to_string(&p) {
                Ok(s) if !s.trim().is_empty() => {
                    eprintln!("--summary: prompt   = {}", p.display());
                    Some(s)
                }
                _ => None,
            }
        });
        eprintln!("--summary: findings = {}", findings_path.display());
        eprintln!("--summary: output   = {}", output_path.display());
        // Race the summary call against SIGINT so ctrl-c actually
        // aborts the HTTP request instead of hanging until the
        // streaming response completes. Without this branch the REPL
        // path installs its own ctrl-c handler but --summary has
        // none, so SIGINT just sits in the tokio signal queue.
        let summary_fut = kres_repl::summary::run_summary(kres_repl::summary::SummaryInputs {
            findings_path,
            output_path,
            template_path: args.template.clone(),
            markdown,
            original_prompt,
            client: fast_client,
            model: fast_model,
            max_tokens: fast_max_tokens,
            max_input_tokens: fast_max_input,
        });
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("--summary: ctrl-c received; aborting");
                std::process::exit(130);
            }
            r = summary_fut => r?,
        }
        return Ok(());
    }

    // --- Announce resolved paths -----------------------------------
    for (label, p) in [
        ("fast-agent", fast_agent.as_ref()),
        ("slow-agent", slow_agent.as_ref()),
        ("main-agent", main_agent.as_ref()),
        ("todo-agent", todo_agent.as_ref()),
        ("mcp-config", mcp_config.as_ref()),
        ("skills", skills_dir.as_ref()),
        ("findings", findings_base.as_ref()),
    ] {
        match p {
            Some(path) => banner!("{label}: {}", path.display()),
            None => banner!("{label}: (none)"),
        }
    }
    banner!("results: {}", results_dir.display());
    banner!("report:  {}", report_path.display());
    banner!("todo:    {}", todo_path.display());
    // Settings summary: show whichever paths settings.json would
    // fill in for each role, so the operator can confirm the
    // per-user defaults without spelunking into ~/.kres.
    match kres_repl::Settings::default_path() {
        Some(p) if p.exists() => banner!("settings: {}", p.display()),
        Some(p) => {
            banner!("settings: {} (absent; using fallbacks)", p.display())
        }
        None => banner!("settings: (no $HOME; using fallbacks)"),
    }
    for (role, label) in [
        (kres_repl::ModelRole::Fast, "fast"),
        (kres_repl::ModelRole::Slow, "slow"),
        (kres_repl::ModelRole::Main, "main"),
        (kres_repl::ModelRole::Todo, "todo"),
    ] {
        match settings.model_for(role) {
            Some(id) => banner!("  default {label} model: {id}"),
            None => banner!(
                "  default {label} model: (unset — agent config or sonnet_4_6 fallback)"
            ),
        }
    }
    if args.turns > 0 {
        banner!("--turns: stop after {} completed task run(s)", args.turns);
    }
    // report, todo are parsed for CLI parity with ; wiring their
    // downstream use is follow-on work. Keep them non-dead:
    let _ = (&report_path, &todo_path);

    let mgr = TaskManager::new();
    // session.json lives beside findings.json / report.md so an
    // interrupted run can be resumed via `--results <same dir>`.
    // Always set — even for defaulted session dirs, so crash recovery
    // works out-of-the-box; operators who don't point at the dir
    // again will simply never read it.
    let persist_path = Some(results_dir.join("session.json"));
    let cfg = ReplConfig {
        stop_grace: std::time::Duration::from_millis(args.stop_grace_ms),
        findings_base,
        turns_limit: args.turns,
        follow_followups: args.follow,
        report_path: Some(report_path.clone()),
        // Only pass the explicit --results through; a defaulted
        // ~/.kres/sessions/<ts>/ dir should not trigger prompt.md
        // persistence.
        results_dir: args.results.clone(),
        template_path: args.template.clone(),
        stdio: args.stdio,
        workspace: args.workspace.clone(),
        persist_path,
    };
    let mut session = Session::new(mgr, cfg).await;
    // Resume from a prior session.json ONLY when `--resume` was
    // passed. Without the flag, any existing session.json is left
    // untouched on disk and the REPL starts clean — this avoids
    // silently inheriting a prior session's plan/todo/deferred
    // state when the operator re-uses a results dir by accident.
    // When the flag is absent but a session.json is present, log a
    // hint so the operator knows the state is available.
    if args.resume {
        // Prefer the live session.json; fall back to
        // session.json.prev when the live file is missing. The
        // backup is what a prior run-without-`--resume` moved
        // aside, so `--resume` on the next launch should pick it
        // up rather than telling the operator there is nothing
        // to load.
        let live = results_dir.join("session.json");
        let backup = results_dir.join("session.json.prev");
        let chosen: Option<std::path::PathBuf> = if live.exists() {
            Some(live)
        } else if backup.exists() {
            banner!(
                "resume: session.json missing; loading {} instead",
                backup.display()
            );
            Some(backup)
        } else {
            None
        };
        let load_result = match chosen.as_deref() {
            Some(p) => session.resume_state_from(Some(p)).await,
            None => Ok(None),
        };
        match load_result {
            Ok(Some(state)) => {
                banner!(
                    "resume: {} todo item(s), {} deferred, turns done={}",
                    state.todo.len(),
                    state.deferred.len(),
                    state.completed_run_count
                );
                if let Some(ref prompt) = state.last_prompt {
                    let short: String = prompt.chars().take(80).collect();
                    banner!("resume: last prompt: {}", short);
                }
            }
            Ok(None) => {
                banner!(
                    "resume: no session.json or session.json.prev in {} — starting clean",
                    results_dir.display()
                );
            }
            Err(e) => {
                banner!("resume: {e}");
            }
        }
    } else {
        let session_json = results_dir.join("session.json");
        if session_json.exists() {
            // Move the prior snapshot to session.json.prev so the
            // first reaper tick that writes this session's fresh
            // state does not destroy it. `/resume` inside the REPL
            // reads this backup when the live session.json matches
            // the current in-memory state.
            let backup = results_dir.join("session.json.prev");
            match std::fs::rename(&session_json, &backup) {
                Ok(()) => banner!(
                    "note: prior session snapshot moved to {}; \
                     starting clean. Type /resume (or restart with \
                     --resume) to load it back.",
                    backup.display()
                ),
                Err(e) => banner!(
                    "note: {} exists but could not be moved aside ({e}); \
                     the first reaper tick will overwrite it. Pass \
                     --resume next time to load prior state.",
                    session_json.display()
                ),
            }
        }
    }

    // Turn logger: always on (see todo.md §2). Rooted at cwd so
    // `.kres/logs/<uuid>/` lands next to the session artifacts.
    let logger = match kres_core::log::TurnLogger::new(std::path::Path::new(".")) {
        Ok(lg) => {
            let lg = std::sync::Arc::new(lg);
            banner!("session: {}", lg.session_id());
            banner!("logs:    {}", lg.session_dir().display());
            Some(lg)
        }
        Err(e) => {
            banner!(
                "logs: could not initialise turn logger ({e}); continuing unlogged"
            );
            None
        }
    };
    if let Some(ref lg) = logger {
        session = session.with_logger(lg.clone());
    }
    let usage = Some(session.usage_tracker());
    // Compute the session's non-MCP action allowlist from settings
    // layered with CLI --allow flags. Shared Arc so every MainAgent
    // instance (currently one per kres) reads the same resolved set.
    // Emit typo warnings up-front so an operator who wrote
    // `--allow bsah` sees their mistake instead of silently keeping
    // bash disabled.
    let _ = settings.warn_unknown_action_tokens(&args.allow);
    let allowed_actions: Arc<std::collections::BTreeSet<String>> =
        Arc::new(settings.effective_allowed_actions(&args.allow));
    // Print the allowlist banner only when a main agent is going
    // to consult it. In --summary mode and any other shape where
    // there's no main agent, the allowlist is dead data and
    // printing it is just noise.
    if main_agent.is_some() {
        // The banner differentiates "bash off because default"
        // from "bash off because the operator explicitly wrote a
        // list that excludes it" — in the latter case pointing at
        // `--allow bash` still works (CLI is additive) but the
        // hint is worded to respect the deliberate choice rather
        // than nudge them to undo it.
        let bash_in_explicit_list = settings
            .actions
            .allowed
            .as_ref()
            .map(|l| l.iter().any(|s| s == "bash"))
            .unwrap_or(false);
        let bash_status = if allowed_actions.contains("bash") {
            "ENABLED".to_string()
        } else if settings.actions.allowed.is_some() && !bash_in_explicit_list {
            "disabled by explicit allowlist in settings.json".to_string()
        } else {
            "disabled by default (add to settings.json or pass --allow bash to enable)".to_string()
        };
        banner!(
            "actions: allowlist = [{}] (bash {bash_status})",
            allowed_actions
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let (Some(fc), Some(sc)) = (fast_agent.as_ref(), slow_agent.as_ref()) {
        let workspace =
            std::fs::canonicalize(&args.workspace).unwrap_or_else(|_| args.workspace.clone());
        let workspace_fetcher = WorkspaceFetcher::new(&workspace);

        // --mcp-config: load the registry and spawn every configured
        // server. We keep them all in a HashMap keyed by name so the
        // main agent (if configured) can route `mcp` actions to any
        // server the operator provisioned. Without a main agent, the
        // first-spawned server becomes the "primary" that the rule-
        // based McpFetcher consults.
        use std::collections::HashMap;
        let mut spawned_mcp: HashMap<String, Arc<tokio::sync::Mutex<kres_mcp::McpClient>>> =
            HashMap::new();
        let mut primary_name: Option<String> = None;
        if let Some(p) = mcp_config.as_ref() {
            match kres_mcp::ServerRegistry::load_from_file(p) {
                Ok(reg) if !reg.servers.is_empty() => {
                    // MCP stderr is diagnostic, not user-facing output.
                    // Drop it next to the conversation logs under
                    // .kres/logs/<session-uuid>/ so results_dir stays
                    // limited to findings.json / report.md / todo.md.
                    // Fall back to results_dir only if the turn logger
                    // failed to initialise.
                    let log_dir = logger
                        .as_ref()
                        .map(|l| l.session_dir().join("mcp-logs"))
                        .unwrap_or_else(|| results_dir.join("mcp-logs"));
                    for (name, cfg) in &reg.servers {
                        match kres_mcp::McpClient::spawn(name, cfg, &log_dir).await {
                            Ok(client) => {
                                banner!(
                                    "mcp: spawned `{name}` (log: {})",
                                    client.stderr_log_path().display()
                                );
                                if primary_name.is_none() {
                                    primary_name = Some(name.clone());
                                }
                                spawned_mcp.insert(
                                    name.clone(),
                                    Arc::new(tokio::sync::Mutex::new(client)),
                                );
                            }
                            Err(e) => banner!("mcp: spawn `{name}` failed: {e}"),
                        }
                    }
                }
                Ok(_) => banner!("mcp-config: {} has no servers", p.display()),
                Err(e) => {
                    banner!("mcp-config: load failed ({}): {e}", p.display())
                }
            }
        }

        // Fetcher selection:
        //
        //   * `--main-agent` set → build a MainAgent, which consults
        //     the LLM to decide which tool to call and routes MCP
        //     across every spawned server (§1, §13, §29).
        //   * otherwise → fall back to the rule-based path:
        //     McpFetcher(first server) wrapping WorkspaceFetcher, or
        //     plain WorkspaceFetcher when no MCP is configured.
        // `goal_client_from_main` is populated alongside the main-
        // agent-backed fetcher so the Session can run §4's
        // define_goal / check_goal loop on the same model.
        let mut goal_client_from_main: Option<Arc<kres_agents::GoalClient>> = None;
        let fetcher: Arc<dyn kres_agents::pipeline::DataFetcher> = match main_agent.as_ref() {
            Some(p) => match kres_agents::AgentConfig::load(p) {
                Ok(mc) => {
                    let model = kres_repl::pick_model(
                        mc.model.as_deref(),
                        kres_repl::ModelRole::Main,
                        &settings,
                    );
                    let client = Arc::new(kres_llm::client::Client::new(mc.key.clone())?);
                    let ma_max_tokens =
                        mc.max_tokens.unwrap_or(model.max_output_tokens).min(32_000);
                    // Deliberately NOT mc.system — the main-agent
                    // system prompt trains the model to reply
                    // `done` when no fetch actions are needed,
                    // which was shadowing the "Return JSON only"
                    // instructions in check_goal's user message
                    // (observed in session e84c7fac: reply=`done`,
                    // parse failed, assume_met() fired). GoalClient
                    // gets its own judge-mode prompt.
                    goal_client_from_main = Some(Arc::new(kres_agents::GoalClient {
                        client: client.clone(),
                        model: model.clone(),
                        system: Some(kres_agents::GOAL_INSTRUCTIONS.to_string()),
                        max_tokens: ma_max_tokens.min(8_000),
                        max_input_tokens: mc.max_input_tokens,
                        logger: logger.clone(),
                    }));
                    let ma = kres_agents::main_agent::MainAgent {
                        client,
                        model: model.clone(),
                        system: mc.system,
                        max_tokens: ma_max_tokens,
                        max_input_tokens: mc.max_input_tokens,
                        max_main_turns: kres_agents::DEFAULT_MAX_MAIN_TURNS,
                        user_query: String::new(),
                        task_brief: String::new(),
                        workspace: workspace.clone(),
                        mcp_servers: spawned_mcp.clone(),
                        logger: logger.clone(),
                        usage: usage.clone(),
                        allowed_actions: allowed_actions.clone(),
                    };
                    banner!(
                        "main-agent: LLM-driven ({}), {} MCP server(s) routed",
                        p.display(),
                        spawned_mcp.len()
                    );
                    Arc::new(ma)
                }
                Err(e) => {
                    banner!(
                        "main-agent: config load failed ({}): {e}; falling back",
                        p.display()
                    );
                    rule_based_fetcher(&spawned_mcp, &primary_name, workspace_fetcher.clone())
                }
            },
            None => rule_based_fetcher(&spawned_mcp, &primary_name, workspace_fetcher.clone()),
        };
        if let Some(gc) = goal_client_from_main {
            session = session.with_goal_client(gc);
            banner!("goal agent: ready");
        }
        // §50: hand the MCP client map to the session so it can
        // shut them down cleanly on REPL exit.
        if !spawned_mcp.is_empty() {
            let clients: Vec<_> = spawned_mcp.values().cloned().collect();
            session.register_mcp_clients(clients).await;
        }
        let skills_value = match skills_dir.as_ref() {
            Some(dir) => match kres_agents::Skills::load_dir(dir) {
                Ok(s) => {
                    let auto = s.auto_loaded();
                    banner!(
                        "skills: loaded {} total, {} auto-invoked from {}",
                        s.items.len(),
                        auto.len(),
                        dir.display()
                    );
                    Some(s.to_prompt_value(&auto))
                }
                Err(e) => {
                    banner!("skills: load failed: {e}");
                    None
                }
            },
            None => None,
        };
        let built = build_orchestrator(
            fc,
            sc,
            workspace,
            fetcher,
            skills_value,
            usage.clone(),
            args.gather_turns,
            logger.clone(),
            &settings,
        )
        .await?;
        let orc = built.orchestrator;
        let consolidator = built.consolidator;
        session = session
            .with_orchestrator(orc)
            .with_consolidator(consolidator);

        // Optional todo agent.
        if let Some(ref tc_path) = todo_agent {
            match kres_agents::AgentConfig::load(tc_path) {
                Ok(tc_cfg) => {
                    let model = kres_repl::pick_model(
                        tc_cfg.model.as_deref(),
                        kres_repl::ModelRole::Todo,
                        &settings,
                    );
                    let client = Arc::new(kres_llm::client::Client::new(tc_cfg.key.clone())?);
                    let todo_client = Arc::new(kres_agents::TodoClient {
                        client,
                        model: model.clone(),
                        system: tc_cfg.system,
                        max_tokens: tc_cfg
                            .max_tokens
                            .unwrap_or(32_000)
                            .min(model.max_output_tokens),
                        max_input_tokens: tc_cfg.max_input_tokens,
                    });
                    session = session.with_todo_client(todo_client);
                    banner!("todo agent: ready");
                }
                Err(e) => banner!("todo agent config load: {e}"),
            }
        }
        banner!("orchestrator: ready (gather_turns={})", args.gather_turns);
    } else {
        banner!(
            "orchestrator: not configured (pass --fast-agent and --slow/--slow-agent)"
        );
    }
    if let Some(ref raw_arg) = args.prompt {
        match resolve_prompt_arg(raw_arg) {
            Ok((source, body)) => {
                let pf = kres_agents::parse_prompt_file(&body);
                banner!(
                    "prompt: loaded {} lens(es) + {} chars of prose from {}",
                    pf.lenses.len(),
                    pf.prompt.len(),
                    source,
                );
                session = session.with_prompt_file(pf);
            }
            Err(e) => banner!("prompt: {e}"),
        }
    }
    session.run().await
}

/// Build the rule-based fetcher used when `--main-agent` is absent
/// (or its setup fails). Uses the first-spawned MCP server as the
/// primary for `source`/`callers`/`callees` lookups; other servers
/// stay spawned (so they can be queried via a future tool dispatcher)
/// but are not auto-routed yet.
fn rule_based_fetcher(
    spawned: &std::collections::HashMap<
        String,
        std::sync::Arc<tokio::sync::Mutex<kres_mcp::McpClient>>,
    >,
    primary_name: &Option<String>,
    workspace_fetcher: std::sync::Arc<kres_agents::WorkspaceFetcher>,
) -> std::sync::Arc<dyn kres_agents::pipeline::DataFetcher> {
    if let Some(name) = primary_name {
        if let Some(primary) = spawned.get(name) {
            return kres_agents::McpFetcher::from_shared(primary.clone(), workspace_fetcher);
        }
    }
    workspace_fetcher
}

fn init_tracing(filter: Option<&str>) {
    let env = filter
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::var("KRES_LOG").unwrap_or_else(|_| "info".into()));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run_test(args: TestArgs) -> Result<()> {
    use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

    let api_key = kres_llm::key::load_api_key(&args.key_file)
        .with_context(|| format!("loading key file {}", args.key_file.display()))?;
    let model = match args.model.as_deref() {
        Some(id) => Model::from_id(id),
        None => Model::from_key_file(&args.key_file), // bugs.md#R1
    };
    banner!("model: {}", model.id);

    let client = Client::new(api_key)?;
    // Defaults now pick the right thinking schema per model family
    // (adaptive for opus-4-7+, legacy budget for older). Cap
    // max_tokens to keep the smoke test small.
    let cfg = CallConfig::defaults_for(model.clone()).with_max_tokens(16_384);
    let messages = vec![Message {
        role: "user".into(),
        content: args.prompt,
        cache: false,
        cached_prefix: None,
    }];

    let resp = client.messages(&cfg, &messages).await?;
    println!(
        "model (actual): {}",
        resp.model.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "stop reason: {}",
        resp.stop_reason.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "usage: input={} output={}",
        resp.usage.input_tokens, resp.usage.output_tokens
    );
    for block in &resp.content {
        match block {
            kres_llm::request::ContentBlock::Thinking { thinking } => {
                println!("thinking: {}", truncate(thinking, 200));
            }
            kres_llm::request::ContentBlock::Text { text } => {
                println!("response: {text}");
            }
            kres_llm::request::ContentBlock::Other => {}
        }
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_args_parse() {
        let c =
            Cli::try_parse_from(["kres", "test", "/tmp/opus.api.key", "--prompt", "hi"]).unwrap();
        match c.cmd {
            Some(Command::Test(a)) => {
                assert_eq!(a.prompt, "hi");
                assert_eq!(a.key_file, PathBuf::from("/tmp/opus.api.key"));
            }
            _ => panic!("expected test"),
        }
    }

    #[test]
    fn turn_args_parse() {
        let c = Cli::try_parse_from([
            "kres",
            "turn",
            "/tmp/sonnet.api.key",
            "-i",
            "in.json",
            "-o",
            "out.md",
            "--thinking-budget",
            "0",
        ])
        .unwrap();
        match c.cmd {
            Some(Command::Turn(a)) => {
                assert_eq!(a.thinking_budget, Some(0));
                assert_eq!(a.output, PathBuf::from("out.md"));
            }
            _ => panic!("expected turn"),
        }
    }

    #[test]
    fn no_subcommand_means_repl() {
        let c = Cli::try_parse_from(["kres", "--prompt", "file.md", "--turns", "3"]).unwrap();
        assert!(c.cmd.is_none());
        assert_eq!(c.repl.prompt.as_deref(), Some("file.md"));
        assert_eq!(c.repl.turns, 3);
    }

    #[test]
    fn slow_tag_unset_when_not_passed() {
        // --slow is now Option<String> with no clap default, so the
        // settings.json slow model is not silently overridden when
        // the operator omits the flag (user report 2026-04-21).
        let c = Cli::try_parse_from(["kres"]).unwrap();
        assert_eq!(c.repl.slow, None);
    }

    #[test]
    fn slow_tag_passes_through_when_set() {
        let c = Cli::try_parse_from(["kres", "--slow", "opus"]).unwrap();
        assert_eq!(c.repl.slow.as_deref(), Some("opus"));
    }

    #[test]
    fn allow_flag_accepts_comma_separated() {
        // value_delimiter = ',' on the --allow arg means both
        // `--allow bash --allow git` and `--allow bash,git` parse
        // into ["bash", "git"]. Repeatable-plus-delimited is what
        // clap's conventional pattern expects, and this pins it so
        // a future refactor can't silently drop the delimiter.
        let c = Cli::try_parse_from(["kres", "--allow", "bash,git", "--allow", "edit"]).unwrap();
        assert_eq!(c.repl.allow, vec!["bash", "git", "edit"]);
    }

    #[test]
    fn allow_flag_defaults_to_empty() {
        let c = Cli::try_parse_from(["kres"]).unwrap();
        assert!(c.repl.allow.is_empty());
    }

    #[test]
    fn resolve_prompt_arg_word_colon_form_hits_user_commands() {
        // --prompt "review: target" resolves via user_commands to the
        // embedded review template with the target prepended.
        let (src, body) =
            resolve_prompt_arg("review: fs/btrfs/ctree.c").expect("review: form should resolve");
        assert!(src.contains("review"), "source label: {src}");
        assert!(
            body.starts_with("fs/btrfs/ctree.c\n\n"),
            "target must lead body: {body:?}"
        );
        assert!(body.contains("[investigate]"), "review body missing");
    }

    #[test]
    fn resolve_prompt_arg_slash_form_equivalent_to_colon_form() {
        // The whole point of the CLI slash-form: --prompt "/review X"
        // must produce the same composed prompt as --prompt "review: X".
        let (_, colon_body) = resolve_prompt_arg("review: fs/btrfs/ctree.c").unwrap();
        let (_, slash_body) = resolve_prompt_arg("/review fs/btrfs/ctree.c").unwrap();
        assert_eq!(
            colon_body, slash_body,
            "slash form and colon form must compose identically"
        );
    }

    #[test]
    fn resolve_prompt_arg_slash_unknown_command_falls_to_inline() {
        // A slash prefix with no matching command and no legacy
        // template on disk must pass through as verbatim prompt
        // text — NOT error, NOT be silently dropped.
        let (src, body) = resolve_prompt_arg("/no-such-cmd hello world").unwrap();
        assert_eq!(src, "<inline>");
        assert_eq!(body, "/no-such-cmd hello world");
    }

    #[test]
    fn resolve_prompt_arg_inline_colon_not_misparsed() {
        // A free-form question that happens to contain a colon but
        // doesn't start with a command word must stay inline — this
        // is the "question like 'when did btrfs: land?' shouldn't
        // look up a btrfs template" case.
        let (src, body) = resolve_prompt_arg("why does func() return: unusual values?").unwrap();
        assert_eq!(src, "<inline>");
        assert!(body.contains("unusual values"));
    }

    #[test]
    fn truncate_preserves_under_limit() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_trims_over_limit() {
        let out = truncate("abcdef", 3);
        assert_eq!(out, "abc...");
    }
}
