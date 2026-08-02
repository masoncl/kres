//! kres — kernel code RESearch agent.
//!
//! `kres test` and `kres turn` are small one-shot tools around the
//! Anthropic API; the REPL (the default subcommand) is the main entry
//! point.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use kres_agents::AgentKind;

mod turn;

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
    /// Validate a workflow JSON file against the embedded schema +
    /// cross-field invariants. Prints "ok: <n> steps" on success or
    /// the first batch of validation errors and exits non-zero.
    ValidateWorkflow(ValidateWorkflowArgs),
    /// Run a workflow JSON file end-to-end. Loads model configs from
    /// `--kres-dir/models` (default `~/.kres/models`), resolves workflow inputs,
    /// drives the executor against the kres-llm client, and prints
    /// the per-step trace as the run progresses. Final exit status:
    /// 0 on Success / TerminalSuccess, non-zero on Failure /
    /// IterationCap.
    RunWorkflow(Box<RunWorkflowArgs>),
}

#[derive(Args, Debug)]
struct ValidateWorkflowArgs {
    /// Path to a workflow JSON (e.g. configs/workflows/fix.json).
    path: PathBuf,
}

#[derive(Args, Debug)]
struct RunWorkflowArgs {
    /// Path to a workflow JSON (e.g. configs/workflows/fix.json).
    path: PathBuf,
    /// Workflow input as KEY=VALUE. Repeatable. Values are parsed
    /// as JSON when possible (numbers, booleans, strings); plain
    /// strings can be passed unquoted (e.g. `target=/abs/path`).
    /// Bare KEY (no `=`) sets the input to `true`.
    #[arg(long, value_name = "KEY=VAL")]
    input: Vec<String>,
    /// Workspace for git/make post-actions. Defaults to cwd.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Directory holding settings.json, models/, skills/, workflows/,
    /// and mcp.json. Defaults to ~/.kres/.
    #[arg(long, value_name = "DIR")]
    kres_dir: Option<PathBuf>,
    /// Slow model selector. Use a model id when unique, or
    /// provider.json:model-id to disambiguate. sonnet/opus are aliases.
    #[arg(long, value_delimiter = ',', conflicts_with = "slow_model")]
    slow: Vec<String>,
    /// Run every workflow review lens with every --slow model.
    #[arg(long, default_value_t = false)]
    compare: bool,
    /// Override the fast-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    fast_model: Option<String>,
    /// Override the slow-agent model id. Beats settings.json.
    /// Mutually exclusive with --slow.
    #[arg(long, value_name = "ID", conflicts_with = "slow")]
    slow_model: Option<String>,
    /// Override the classifier-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    classifier_model: Option<String>,
    /// Directory of skill .md files. Defaults to <kres-dir>/skills/.
    /// Skill files named in workflow.skills are loaded eagerly and
    /// prepended to every step prompt.
    #[arg(long, value_name = "DIR")]
    skills_dir: Option<PathBuf>,
    /// Directory for code.jsonl / main.jsonl turn logs. When set,
    /// every LLM call's user + assistant messages are appended.
    /// Defaults to .kres/logs/<uuid>/ in the workspace, matching
    /// the REPL's layout.
    #[arg(long, value_name = "DIR")]
    logs: Option<PathBuf>,
    /// Persist a snapshot of workflow state to <DIR>/workflow-<id>.json
    /// after every step settles, so a killed run can pick up where it
    /// left off. Pair with --resume to load.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Resume from workflow-<id>.json instead of starting clean. Uses
    /// --state-dir, then --results, then the workspace state directory.
    /// Inputs from the snapshot override command-line --input values.
    #[arg(long, default_value_t = false)]
    resume: bool,
    /// Cap on total step executions before the run aborts. Mostly a
    /// safety net for authoring bugs; the FIX flow's own per-step
    /// max_attempts and on_exhausted handlers should fire first.
    #[arg(long, default_value_t = 200)]
    iteration_cap: usize,
    /// Directory for run artefacts: findings.json (every Finding
    /// produced by the run) + report.md (markdown roll-up). Same
    /// flag as the REPL's --results so /review and /fix from the
    /// CLI's `--prompt` short-circuit honour it. When omitted, no
    /// artefacts are written.
    #[arg(long, value_name = "DIR")]
    results: Option<PathBuf>,
    /// Path to mcp.json (the same file the REPL consumes). When
    /// present, the first MCP server in the registry is spawned and
    /// wraps the workspace fetcher so `source` / `callers` /
    /// `callees` followups have a real backend. Defaults to
    /// <kres-dir>/mcp.json.
    #[arg(long, value_name = "FILE")]
    mcp_config: Option<PathBuf>,
    /// Override the exact value used after `Assisted-by:` in
    /// fix-workflow commit messages. Defaults to
    /// `kres:<resolved-slow-model-id>`.
    #[arg(long, value_name = "TEXT")]
    assisted_by: Option<String>,
}

#[derive(Args, Debug)]
struct ReplArgs {
    /// Explicit fast provider config path. The selected model still comes
    /// from --fast-model or settings.json.
    #[arg(long)]
    fast_agent: Option<PathBuf>,
    /// Slow model selector. Use a model id when unique, or
    /// provider.json:model-id to disambiguate. sonnet/opus are aliases.
    /// Mutually exclusive with --slow-model.
    #[arg(long, value_delimiter = ',', conflicts_with = "slow_model")]
    slow: Vec<String>,
    /// Run every review lens with every --slow model and write comparison.json.
    /// Without this flag, additional slow models run only the general lens.
    #[arg(long, default_value_t = false)]
    compare: bool,
    /// Explicit slow-agent config path (overrides --slow).
    #[arg(long)]
    slow_agent: Option<PathBuf>,
    /// Override the fast-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    fast_model: Option<String>,
    /// Override the slow-agent model id. Beats settings.json.
    /// Mutually exclusive with --slow.
    #[arg(long, value_name = "ID", conflicts_with = "slow")]
    slow_model: Option<String>,
    /// Override the main-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    main_model: Option<String>,
    /// Override the todo-agent model id. Beats settings.json.
    #[arg(long, value_name = "ID")]
    todo_model: Option<String>,
    /// Override the classifier-agent model id. Beats settings.json.
    /// Workflow-owned prompts such as `triage:` use this when they
    /// short-circuit into the workflow executor.
    #[arg(long, value_name = "ID")]
    classifier_model: Option<String>,
    /// Override the exact value used after `Assisted-by:` in
    /// fix-workflow commit messages. Defaults to
    /// `kres:<resolved-slow-model-id>`.
    #[arg(long, value_name = "TEXT")]
    assisted_by: Option<String>,
    /// Explicit main-agent provider config path.
    #[arg(long)]
    main_agent: Option<PathBuf>,
    /// Explicit todo-agent provider config path.
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
    /// Exit the REPL once the work-stop condition fires, instead of
    /// staying open waiting for further operator input. Same exit
    /// path as the existing piped-stdout case (auto-renders summary
    /// before teardown). Useful for batch-style invocations on a
    /// real TTY where the operator wants the kres process to end as
    /// soon as `--turns N` exhausts or `--turns 0` hits goal-met /
    /// no-progress / no-goal-batch-finished.
    #[arg(long, default_value_t = false)]
    one: bool,
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
    ///   2. `--prompt "word: extra details"` — look for a non-workflow
    ///      slash-command template named `word`, first under
    ///      `~/.kres/commands/word.md` and then in the embedded
    ///      user_commands table. If found, the extra details are
    ///      prepended to that template. Workflow-owned names such as
    ///      `review`, `triage`, `validate`, and `fix` dispatch through
    ///      JSON workflows.
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
    /// regions (mosh, some tmux configs). Also the mode to pick when
    /// redirecting output to a file — `--tui` is ignored when
    /// `--stdio` is set.
    #[arg(long, default_value_t = false)]
    stdio: bool,
    /// Force the ratatui TUI on even when stdout isn't a TTY.
    /// Useful for debugging the TUI rendering path from inside
    /// `script` or other wrappers. Without `--tui` and without
    /// `--no-tui`, the TUI is used automatically on a TTY.
    /// `--stdio` takes precedence when both are set.
    #[arg(long, default_value_t = false)]
    tui: bool,
    /// Force the rustyline-based prompt line — the pre-TUI default.
    /// Left in as an escape hatch while the TUI shakes out; pass
    /// this if the ratatui path misbehaves in your terminal. Wins
    /// over `--tui` when both are set; `--stdio` wins over both.
    #[arg(long, default_value_t = false)]
    no_tui: bool,

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

    /// Export every finding from `findings.json` as a per-finding
    /// folder under DIR. Each entry becomes `DIR/<tag>/` with a
    /// `meta.yaml` (id, severity, workspace git sha/subject, cross
    /// references) and a `FINDING.md` carrying the full body
    /// (summary, mechanism, reproducer, impact, fix sketch, open
    /// questions, per-task analysis). Inputs honour --results /
    /// --findings the same way --summary does. Exits without
    /// starting the REPL.
    #[arg(long, value_name = "DIR")]
    export: Option<PathBuf>,

    /// Walk every `<tag>/metadata.yaml` under DIR (the output of a
    /// prior `--export`) and write `DIR/INDEX.md` — a single
    /// markdown index sorted by severity (high → low) and then by
    /// the `date` field (oldest first, so long-standing bugs stay
    /// visible at the top of each severity band). Exits without
    /// starting the REPL; no findings.json is consulted.
    #[arg(long, value_name = "DIR")]
    export_index: Option<PathBuf>,

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
    /// Path to model/agent config JSON.
    config: PathBuf,
    /// Override the model id.
    #[arg(long)]
    model: Option<String>,
    /// Prompt to send.
    #[arg(short, long, default_value = "Say hello in one sentence.")]
    prompt: String,
}

#[derive(Parser, Debug)]
struct TurnArgs {
    /// Path to model/agent config JSON.
    config: PathBuf,
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
        Some(Command::ValidateWorkflow(args)) => run_validate_workflow(args),
        Some(Command::RunWorkflow(args)) => rt.block_on(run_workflow(*args)),
        None => {
            // Workflow prompt invocations use their workflow-owned
            // path. Fix/triage/validate run through the workflow executor.
            // Review is special because its workflow-owned semantics
            // are the REPL task/todo loop: one lensed review turn
            // emits followups, the reaper sends them through the todo
            // agent, and --turns controls how many fresh review tasks
            // run.
            if let Some(short_circuit) = workflow_short_circuit_from_repl_args(&cli.repl) {
                validate_workflow_short_circuit_model_flags(&cli.repl)?;
                rt.block_on(run_workflow(short_circuit))
            } else {
                rt.block_on(run_repl(cli.repl))
            }
        }
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
/// $HOME is unset. Used to locate model configs, non-agent config
/// files, the skills directory, findings base, and mcp.json.
/// Resolve the --prompt CLI argument into (source-description, body).
///
/// Recognised forms:
///   1. Path to an existing file → `(path.display(), file-contents)`.
///   2. `"word: extra"` or `"/word extra"` naming a slash-command
///      template that is not workflow-owned. `fix`, `triage`, and
///      `validate` are
///      handled before this function by
///      `workflow_short_circuit_from_repl_args`; `review` is handled
///      by `kres_repl::review_prompt_file_from_prompt` so it enters
///      the task/todo loop. Summary commands use `--summary` /
///      `/summary`.
///   3. Anything else → `("<inline>", raw)`.
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
        // (disk-first + embedded default + name-validation). The
        // validation inside compose covers the same character set
        // we'd enforce here, so there's no need to pre-filter.
        if matches!(head, "fix" | "review" | "triage" | "validate") {
            return Err(anyhow::anyhow!(
                "`{head}` is workflow-only; use `/{head} <target>` or `--prompt '{head}: <target>'`"
            ));
        }
        if matches!(head, "summary" | "summary-markdown") {
            return Err(anyhow::anyhow!(
                "`{head}` is a report-rendering command; use `--{head}` or `/{head}`"
            ));
        }
        if let Some((src, composed)) = kres_agents::user_commands::compose(head, rest) {
            return Ok((src, composed));
        }
    }
    // Form 4: inline prompt text.
    Ok(("<inline>".to_string(), raw.to_string()))
}

fn kres_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".kres"))
}

fn kres_config_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![home.join(".kres")]
}

/// Map `--slow <selector>` to a concrete model id when the selector is a
/// known shorthand. Other selectors are model ids or provider:model pairs.
fn slow_tag_to_model_id(tag: &str) -> Option<&'static str> {
    match tag.to_ascii_lowercase().as_str() {
        "sonnet" => Some("claude-sonnet-5"),
        "opus" => Some("claude-opus-4-8"),
        _ => None,
    }
}

fn assisted_by_from_model_id(model_id: &str) -> String {
    format!("kres:{model_id}")
}

fn default_assisted_by_for_slow_agent(
    slow_agent: Option<&PathBuf>,
    settings: &kres_repl::Settings,
) -> String {
    let cfg_model = slow_agent
        .and_then(|path| kres_agents::AgentConfig::load_for_role(path, AgentKind::Slow).ok())
        .and_then(|cfg| cfg.model);
    let model = kres_repl::pick_model(cfg_model.as_deref(), kres_repl::ModelRole::Slow, settings);
    assisted_by_from_model_id(&model.id)
}

fn resolved_assisted_by(
    override_value: Option<&String>,
    slow_agent: Option<&PathBuf>,
    settings: &kres_repl::Settings,
) -> String {
    override_value
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_assisted_by_for_slow_agent(slow_agent, settings))
}

fn resolved_agent_model_label(
    agent_path: Option<&PathBuf>,
    role: kres_repl::ModelRole,
    settings: &kres_repl::Settings,
) -> String {
    let agent_kind = agent_kind_for_model_role(role);
    let cfg_model = agent_path
        .and_then(|path| kres_agents::AgentConfig::load_for_role(path, agent_kind).ok())
        .and_then(|cfg| cfg.model);
    kres_repl::pick_model(cfg_model.as_deref(), role, settings).id
}

fn resolved_model_config_hint(
    role: kres_repl::ModelRole,
    settings: &kres_repl::Settings,
) -> String {
    match settings.model_for(role) {
        Some(model_id) => {
            let expected = kres_config_dirs()
                .into_iter()
                .map(|dir| dir.join("models/*.json").display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("model {model_id:?} (expected {expected})")
        }
        None => "no model configured in settings.json".to_string(),
    }
}

fn validate_prompt_agent_configs(
    args: &ReplArgs,
    fast_agent: Option<&PathBuf>,
    slow_agent: Option<&PathBuf>,
    settings: &kres_repl::Settings,
) -> Result<()> {
    if args.prompt.is_none() || args.resume {
        return Ok(());
    }

    let mut missing = Vec::new();
    if fast_agent.is_none() {
        missing.push(format!(
            "fast: {}",
            resolved_model_config_hint(kres_repl::ModelRole::Fast, settings)
        ));
    }
    if slow_agent.is_none() {
        missing.push(format!(
            "slow: {}",
            resolved_model_config_hint(kres_repl::ModelRole::Slow, settings)
        ));
    }
    if missing.is_empty() {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "--prompt requires fast and slow agent configs; missing {}. \
         Pass --fast-agent/--slow-agent or configure provider files under ~/.kres/models/.",
        missing.join("; ")
    ))
}

fn agent_kind_for_model_role(role: kres_repl::ModelRole) -> AgentKind {
    match role {
        kres_repl::ModelRole::Fast => AgentKind::Fast,
        kres_repl::ModelRole::Slow => AgentKind::Slow,
        kres_repl::ModelRole::Main => AgentKind::Main,
        kres_repl::ModelRole::Todo => AgentKind::Todo,
        kres_repl::ModelRole::Classifier => AgentKind::Classifier,
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
    for dir in kres_config_dirs() {
        let fallback = dir.join(default_name);
        if fallback.exists() {
            return Some(fallback);
        }
    }
    None
}

fn qualified_model_path(path: &Path, model_id: &str) -> PathBuf {
    PathBuf::from(format!("{}:{model_id}", path.display()))
}

fn resolve_agent_for_model_in_dirs(
    cli: Option<&PathBuf>,
    model_id: Option<&str>,
    dirs: &[PathBuf],
) -> Result<Option<PathBuf>> {
    if let Some(p) = cli {
        if p.to_string_lossy().contains(".json:") {
            return Ok(Some(p.clone()));
        }
        return Ok(Some(match model_id {
            Some(selector) => qualified_model_path(
                p,
                selector
                    .split_once(':')
                    .map_or(selector, |(_, model)| model),
            ),
            None => p.clone(),
        }));
    }
    let Some(selector) = model_id else {
        return Ok(None);
    };
    let (provider_name, requested_model) = match selector.split_once(':') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => (
            Some(provider.strip_suffix(".json").unwrap_or(provider)),
            model,
        ),
        _ => (None, selector),
    };
    let mut matches = Vec::new();
    for dir in dirs {
        let models_dir = dir.join("models");
        let entries = match std::fs::read_dir(&models_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", models_dir.display())),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(provider) = provider_name {
                if path.file_stem().and_then(|s| s.to_str()) != Some(provider) {
                    continue;
                }
            }
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading model provider {}", path.display()))?;
            let value: serde_json::Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing model provider {}", path.display()))?;
            if value
                .get("models")
                .and_then(|v| v.as_object())
                .is_some_and(|models| models.contains_key(requested_model))
            {
                matches.push(qualified_model_path(&path, requested_model));
            }
        }
    }
    matches.sort();
    matches.dedup();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(anyhow::anyhow!(
            "model {requested_model:?} is provided by multiple configs: {}. Select one as <provider>.json:{requested_model}",
            matches
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn resolve_agent_for_model(
    cli: Option<&PathBuf>,
    model_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    resolve_agent_for_model_in_dirs(cli, model_id, &kres_config_dirs())
}

fn resolve_classifier_agent_for_model_in_dirs(
    classifier_model_id: Option<&str>,
    fast_agent: Option<&PathBuf>,
    dirs: &[PathBuf],
) -> Result<Option<PathBuf>> {
    match classifier_model_id {
        Some(model_id) => resolve_agent_for_model_in_dirs(None, Some(model_id), dirs),
        None => Ok(fast_agent.cloned()),
    }
}

fn resolve_classifier_agent_for_model(
    classifier_model_id: Option<&str>,
    fast_agent: Option<&PathBuf>,
) -> Result<Option<PathBuf>> {
    resolve_classifier_agent_for_model_in_dirs(classifier_model_id, fast_agent, &kres_config_dirs())
}

fn resolve_slow_selector_in_dirs(
    selector: &str,
    settings: &kres_repl::Settings,
    dirs: &[PathBuf],
) -> Result<(PathBuf, Option<String>)> {
    let configured = settings.resolve_model_selector(selector);
    let selector = if configured == selector {
        slow_tag_to_model_id(selector).unwrap_or(selector)
    } else {
        configured
    };
    if let Some(path) = resolve_agent_for_model_in_dirs(None, Some(selector), dirs)? {
        return Ok((path, Some(selector.to_string())));
    }
    Err(anyhow::anyhow!(
        "--slow {selector:?} did not match any model JSON under {}",
        if dirs.is_empty() {
            "<no kres config dirs>".to_string()
        } else {
            dirs.iter()
                .map(|p| p.join("models").display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        }
    ))
}

fn append_configured_secondary_slow(
    specs: &mut Vec<(PathBuf, Option<String>)>,
    settings: &kres_repl::Settings,
    dirs: &[PathBuf],
) -> Result<()> {
    let Some(selector) = settings.secondary_slow_model() else {
        return Ok(());
    };
    specs.push(resolve_slow_selector_in_dirs(selector, settings, dirs)?);
    Ok(())
}

fn apply_slow_model_override_from_spec(
    settings: &mut kres_repl::Settings,
    spec: &(PathBuf, Option<String>),
) {
    if let Some(model_id) = spec.1.as_ref() {
        settings.set_model(kres_repl::ModelRole::Slow, Some(model_id.clone()));
    }
}

fn load_settings_for_kres_dir(kres_dir: &Path, workspace: &Path) -> kres_repl::Settings {
    let global = kres_dir.join("settings.json");
    let project = workspace.join(".kres").join("settings.json");
    kres_repl::Settings::load_merged_with_paths(Some(&global), &project)
}

fn apply_workflow_model_overrides(settings: &mut kres_repl::Settings, args: &RunWorkflowArgs) {
    settings.set_model(kres_repl::ModelRole::Fast, args.fast_model.clone());
    settings.set_model(kres_repl::ModelRole::Slow, args.slow_model.clone());
    settings.set_model(
        kres_repl::ModelRole::Classifier,
        args.classifier_model.clone(),
    );
}

fn review_comparison_path(
    results_dir: &Path,
    slow_model_count: usize,
    compare: bool,
) -> Option<PathBuf> {
    (compare && slow_model_count > 1).then(|| results_dir.join("comparison.json"))
}

async fn run_repl(args: ReplArgs) -> Result<()> {
    use kres_agents::WorkspaceFetcher;
    use kres_core::TaskManager;
    use kres_repl::{build_agent_runner, ReplConfig, Session};
    use std::sync::Arc;

    // Per-user settings (~/.kres/settings.json). Carries the default
    // model-id for each agent role. CLI model overrides are applied
    // before config path resolution so model-file fallback can find
    // provider JSON files under ~/.kres/models/.
    let mut settings = kres_repl::Settings::load_merged(&args.workspace);
    settings.set_model(kres_repl::ModelRole::Fast, args.fast_model.clone());
    settings.set_model(kres_repl::ModelRole::Slow, args.slow_model.clone());
    settings.set_model(kres_repl::ModelRole::Main, args.main_model.clone());
    settings.set_model(kres_repl::ModelRole::Todo, args.todo_model.clone());
    settings.set_model(
        kres_repl::ModelRole::Classifier,
        args.classifier_model.clone(),
    );

    // --- Resolve role configs --------------------------------------
    // Explicit path wins; otherwise discover a provider containing the model.
    let fast_agent = resolve_agent_for_model(
        args.fast_agent.as_ref(),
        settings.model_for(kres_repl::ModelRole::Fast),
    )?;

    let mut slow_agent_specs: Vec<(PathBuf, Option<String>)> =
        if let Some(p) = args.slow_agent.clone() {
            let selector = args
                .slow_model
                .as_deref()
                .or_else(|| settings.model_for(kres_repl::ModelRole::Slow));
            vec![(
                resolve_agent_for_model(Some(&p), selector)?.expect("explicit path resolves"),
                selector.map(ToOwned::to_owned),
            )]
        } else if args.slow.is_empty() {
            resolve_agent_for_model(None, settings.model_for(kres_repl::ModelRole::Slow))?
                .map(|p| vec![(p, args.slow_model.clone())])
                .unwrap_or_default()
        } else {
            let dirs = kres_config_dirs();
            args.slow
                .iter()
                .map(|selector| resolve_slow_selector_in_dirs(selector, &settings, &dirs))
                .collect::<Result<Vec<_>>>()?
        };
    if args.slow.is_empty() {
        append_configured_secondary_slow(&mut slow_agent_specs, &settings, &kres_config_dirs())?;
    }
    if let Some(spec) = slow_agent_specs.first() {
        apply_slow_model_override_from_spec(&mut settings, spec);
    }
    let slow_agent = slow_agent_specs.first().map(|(p, _)| p.clone());

    let main_agent = resolve_agent_for_model(
        args.main_agent.as_ref(),
        settings.model_for(kres_repl::ModelRole::Main),
    )?;
    let todo_agent = resolve_agent_for_model(
        args.todo_agent.as_ref(),
        settings.model_for(kres_repl::ModelRole::Todo),
    )?;
    let classifier_agent = resolve_classifier_agent_for_model(
        settings.model_for(kres_repl::ModelRole::Classifier),
        fast_agent.as_ref(),
    )?;
    let mcp_config = resolve_default(args.mcp_config.as_ref(), "mcp.json");
    let skills_dir = resolve_default(args.skills.as_ref(), "skills");
    let assisted_by =
        resolved_assisted_by(args.assisted_by.as_ref(), slow_agent.as_ref(), &settings);

    // --- Resolve artifact dir + per-file paths ---------------------
    // `--results DIR` sets the default dir for findings/report/todo.
    // Individual `--findings FILE`, `--report FILE`, `--todo FILE`
    // override their own slot. When --results is absent, the default
    // is ~/.kres/sessions/<session-id>/. The session-id is a UTC
    // timestamp + pid: bulk-launching parallel kres processes (e.g.
    // a triage-all wrapper) used to collide on the timestamp alone
    // because chrono's seconds-resolution string was identical for
    // every process started in the same second.
    // Treat --summary and --summary-markdown as the same "standalone
    // summary" entry; the markdown flag just picks the variant
    // template and filename further down.
    let summary_mode = args.summary || args.summary_markdown;
    let markdown = args.summary_markdown;
    let export_mode = args.export.is_some();
    let export_index_mode = args.export_index.is_some();

    // In --summary / --export mode we avoid creating a fresh session
    // directory because the operator points at an existing run's
    // artifacts.
    let standalone = summary_mode || export_mode || export_index_mode;
    let results_dir = match (args.results.clone(), standalone) {
        (Some(d), _) => d,
        (None, true) => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        (None, false) => {
            let base = kres_dir().unwrap_or_else(|| PathBuf::from("."));
            let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let session_id = format!("{ts}-{}", std::process::id());
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
    // written; no REPL, no MCP, no AgentRunner, no turn logger.
    if summary_mode {
        let fast_cfg_path = match fast_agent.as_ref() {
            Some(p) => p.clone(),
            None => {
                return Err(anyhow::anyhow!(
                    "--summary requires a fast agent config (pass --fast-agent or configure ~/.kres/models/<fast-model>.json)"
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
        let summary_agent = kres_repl::summary::load_fast_for_summary(&fast_cfg_path, &settings)?;
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
            client: summary_agent.client,
            model: summary_agent.model,
            max_tokens: summary_agent.max_tokens,
            max_input_tokens: summary_agent.max_input_tokens,
            thinking: summary_agent.thinking,
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

    // --- --export DIR: per-finding folder tree -------------------
    // Iterates findings.json (honouring --results / --findings),
    // writes DIR/<tag>/meta.yaml + DIR/<tag>/FINDING.md for every
    // finding, then exits. No REPL, no MCP, no AgentRunner.
    if let Some(ref export_dir) = args.export {
        let findings_path = match findings_base.as_ref() {
            Some(p) if p.exists() => p.clone(),
            Some(p) => {
                return Err(anyhow::anyhow!(
                    "--export: findings file {} does not exist",
                    p.display()
                ));
            }
            None => {
                return Err(anyhow::anyhow!(
                    "--export: no findings path configured (pass --findings or --results)"
                ));
            }
        };
        eprintln!("--export: findings = {}", findings_path.display());
        eprintln!("--export: output   = {}", export_dir.display());
        kres_repl::run_export(kres_repl::ExportInputs {
            findings_path,
            output_dir: export_dir.clone(),
            workspace: args.workspace.clone(),
        })
        .await?;
        return Ok(());
    }

    // --- --export-index DIR: walk a prior --export dir ------------
    // Reads every <tag>/metadata.yaml under DIR, sorts by severity
    // then date, writes DIR/INDEX.md, and exits.
    if let Some(ref index_dir) = args.export_index {
        eprintln!("--export-index: dir    = {}", index_dir.display());
        let out = kres_repl::run_export_index(index_dir)?;
        eprintln!("--export-index: wrote  = {}", out.display());
        return Ok(());
    }

    validate_prompt_agent_configs(&args, fast_agent.as_ref(), slow_agent.as_ref(), &settings)?;

    // --- Announce resolved paths -----------------------------------
    // Buffer these until the REPL output sink is installed. In TUI
    // mode, printing before Session::run() installs the ratatui
    // scrollback sink leaves the lines outside the visible buffer.
    let mut startup_lines = Vec::new();
    for (label, p) in [
        ("fast-agent", fast_agent.as_ref()),
        ("slow-agent", slow_agent.as_ref()),
        ("main-agent", main_agent.as_ref()),
        ("todo-agent", todo_agent.as_ref()),
        ("classifier-agent", classifier_agent.as_ref()),
        ("mcp-config", mcp_config.as_ref()),
        ("skills", skills_dir.as_ref()),
        ("findings", findings_base.as_ref()),
    ] {
        match p {
            Some(path) => startup_lines.push(format!("{label}: {}", path.display())),
            None => startup_lines.push(format!("{label}: (none)")),
        }
    }
    startup_lines.push(format!("results: {}", results_dir.display()));
    startup_lines.push(format!("report:  {}", report_path.display()));
    startup_lines.push(format!("todo:    {}", todo_path.display()));
    // Settings summary: show whichever paths settings.json would
    // fill in for each role, so the operator can confirm the
    // per-user defaults without spelunking into ~/.kres.
    match kres_repl::Settings::default_path() {
        Some(p) if p.exists() => startup_lines.push(format!("settings: {}", p.display())),
        Some(p) => startup_lines.push(format!(
            "settings: {} (absent; using fallbacks)",
            p.display()
        )),
        None => startup_lines.push("settings: (no $HOME; using fallbacks)".to_string()),
    }
    for (role, label) in [
        (kres_repl::ModelRole::Fast, "fast"),
        (kres_repl::ModelRole::Slow, "slow"),
        (kres_repl::ModelRole::Main, "main"),
        (kres_repl::ModelRole::Todo, "todo"),
        (kres_repl::ModelRole::Classifier, "classifier"),
    ] {
        match settings.model_for(role) {
            Some(id) => startup_lines.push(format!("  default {label} model: {id}")),
            None => startup_lines.push(format!(
                "  default {label} model: (unset — agent config or sonnet_4_6 fallback)"
            )),
        }
    }
    for (role, label, path) in [
        (kres_repl::ModelRole::Fast, "fast", fast_agent.as_ref()),
        (kres_repl::ModelRole::Slow, "slow", slow_agent.as_ref()),
        (kres_repl::ModelRole::Main, "main", main_agent.as_ref()),
        (kres_repl::ModelRole::Todo, "todo", todo_agent.as_ref()),
        (
            kres_repl::ModelRole::Classifier,
            "classifier",
            classifier_agent.as_ref(),
        ),
    ] {
        startup_lines.push(format!(
            "  active {label} model: {}",
            resolved_agent_model_label(path, role, &settings)
        ));
    }
    if args.turns > 0 {
        startup_lines.push(format!(
            "--turns: stop after {} completed task run(s)",
            args.turns
        ));
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
        startup_lines,
        findings_base,
        turns_limit: args.turns,
        follow_followups: args.follow,
        report_path: Some(report_path.clone()),
        // Only pass the explicit --results through; a defaulted
        // ~/.kres/sessions/<ts>/ dir should not trigger prompt.md
        // persistence.
        results_dir: args.results.clone(),
        template_path: args.template.clone(),
        stdio: args.stdio || !std::io::IsTerminal::is_terminal(&std::io::stdout()),
        // TUI is now the default on a TTY. Precedence:
        // --stdio (plain)  >  --no-tui (rustyline)  >  --tui
        // (force on)  >  auto (TUI when stdout is a TTY).
        // Non-TTY stdout defaults to rustyline too, since ratatui
        // needs a terminal to drive; --tui overrides that.
        tui: !args.stdio
            && !args.no_tui
            && (args.tui || std::io::IsTerminal::is_terminal(&std::io::stdout())),
        workspace: args.workspace.clone(),
        mcp_config: mcp_config.clone(),
        persist_path,
        assisted_by,
        // Piped/redirected stdout has no operator on the other end,
        // so once the work-stop condition fires there is no one to
        // type the next prompt. Match the existing `--turns N` exit
        // path and quit the REPL when stdout isn't a tty.
        // `--one` forces the same exit path the piped-stdout case
        // takes; otherwise default to "exit when stdout has no
        // terminal on the other end".
        exit_on_idle: args.one || !std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    let mut session = Session::new(mgr, cfg).await;
    // Resume from a prior session.json ONLY when `--resume` was
    // passed. Without the flag, any existing session.json is left
    // untouched on disk and the REPL starts clean — this avoids
    // silently inheriting a prior session's plan/todo/deferred
    // state when the operator re-uses a results dir by accident.
    // When the flag is absent but a session.json is present, log a
    // hint so the operator knows the state is available.
    let mut resumed_ok = false;
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
            kres_core::async_eprintln!(
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
                kres_core::async_eprintln!(
                    "resume: {} todo item(s), {} deferred, turns done={}",
                    state.todo.len(),
                    state.deferred.len(),
                    state.completed_run_count
                );
                if let Some(ref prompt) = state.last_prompt {
                    let short: String = prompt.chars().take(80).collect();
                    kres_core::async_eprintln!("resume: last prompt: {}", short);
                }
                resumed_ok = true;
            }
            Ok(None) => {
                kres_core::async_eprintln!(
                    "resume: no session.json or session.json.prev in {} — starting clean",
                    results_dir.display()
                );
            }
            Err(e) => {
                kres_core::async_eprintln!("resume: {e}");
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
                Ok(()) => kres_core::async_eprintln!(
                    "note: prior session snapshot moved to {}; \
                     starting clean. Type /resume (or restart with \
                     --resume) to load it back.",
                    backup.display()
                ),
                Err(e) => kres_core::async_eprintln!(
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
            kres_core::async_eprintln!("session: {}", lg.session_id());
            kres_core::async_eprintln!("logs:    {}", lg.session_dir().display());
            Some(lg)
        }
        Err(e) => {
            kres_core::async_eprintln!(
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
        kres_core::async_eprintln!(
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
                        let cfg = cfg.with_workspace_cwd(name, &workspace);
                        match kres_mcp::McpClient::spawn(name, &cfg, &log_dir).await {
                            Ok(client) => {
                                kres_core::async_eprintln!(
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
                            Err(e) => kres_core::async_eprintln!("mcp: spawn `{name}` failed: {e}"),
                        }
                    }
                }
                Ok(_) => kres_core::async_eprintln!("mcp-config: {} has no servers", p.display()),
                Err(e) => {
                    kres_core::async_eprintln!("mcp-config: load failed ({}): {e}", p.display())
                }
            }
        }

        // The fast agent is the only model that reasons about data
        // gathering. Its typed followups are executed directly by
        // MCP/WorkspaceFetcher; a second LLM must not reinterpret or
        // silently repair them. The configured main model remains
        // available to the legacy non-review goal loop.
        let mut goal_client_from_main: Option<Arc<kres_agents::GoalClient>> = None;
        if let Some(p) = main_agent.as_ref() {
            match kres_agents::AgentConfig::load_for_role(p, AgentKind::Main) {
                Ok(mc) => {
                    let model = kres_repl::pick_model(
                        mc.model.as_deref(),
                        kres_repl::ModelRole::Main,
                        &settings,
                    );
                    let client = Arc::new(mc.client_builder()?.build()?);
                    let ma_max_tokens = mc.max_tokens.unwrap_or(model.max_output_tokens);
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
                        max_tokens: ma_max_tokens,
                        max_input_tokens: mc.max_input_tokens,
                        thinking: mc
                            .thinking
                            .as_ref()
                            .map(|thinking| thinking.to_budget(ma_max_tokens)),
                        logger: logger.clone(),
                        usage: usage.clone(),
                    }));
                    kres_core::async_eprintln!("goal agent: configured from {}", p.display());
                }
                Err(e) => {
                    kres_core::async_eprintln!(
                        "goal agent: config load failed ({}): {e}",
                        p.display()
                    );
                }
            }
        }
        let fetcher: Arc<dyn kres_agents::pipeline::DataFetcher> =
            rule_based_fetcher(&spawned_mcp, &primary_name, workspace_fetcher.clone());
        kres_core::async_eprintln!(
            "gather service: deterministic, {} MCP server(s)",
            spawned_mcp.len()
        );
        if let Some(gc) = goal_client_from_main {
            session = session.with_goal_client(gc);
            kres_core::async_eprintln!("goal agent: ready");
        }
        // §50: hand the MCP client map to the session so it can
        // shut them down cleanly on REPL exit.
        if !spawned_mcp.is_empty() {
            let clients: Vec<_> = spawned_mcp.values().cloned().collect();
            session.register_mcp_clients(clients).await;
        }
        let workspace_profile = kres_agents::detect_workspace(&workspace);
        kres_core::async_eprintln!(
            "workspace: detected {} tree, build={}",
            workspace_profile.kind.as_str(),
            workspace_profile.build_system.as_str()
        );
        let skills_value = match skills_dir.as_ref() {
            Some(dir) => {
                match kres_agents::Skills::load_auto_for_workspace(dir, &workspace_profile) {
                    Ok((s, warnings)) => {
                        for w in &warnings {
                            kres_core::async_eprintln!("skills: {w}");
                        }
                        let auto = s.auto_loaded();
                        kres_core::async_eprintln!(
                            "skills: loaded {} workspace skill(s), {} auto-invoked from {}",
                            s.items.len(),
                            auto.len(),
                            dir.display()
                        );
                        Some(s.to_prompt_value(&auto))
                    }
                    Err(e) => {
                        kres_core::async_eprintln!("skills: load failed: {e}");
                        None
                    }
                }
            }
            None => None,
        };
        let built = build_agent_runner(
            fc,
            sc,
            workspace,
            fetcher,
            &settings,
            kres_repl::AgentRunnerBuildOptions {
                extra_slow_cfgs: slow_agent_specs.iter().skip(1).cloned().collect(),
                compare_slow_models: args.compare,
                skills: skills_value,
                usage: usage.clone(),
                gather_turns: args.gather_turns,
                logger: logger.clone(),
                comparison_path: review_comparison_path(
                    &results_dir,
                    slow_agent_specs.len(),
                    args.compare,
                ),
            },
        )
        .await?;
        let orc = built.agent_runner;
        let consolidator = built.consolidator;
        session = session
            .with_agent_runner(orc)
            .with_consolidator(consolidator)
            .with_review_planner(built.review_goal_client, built.review_todo_client);
        kres_core::async_eprintln!("review planner: primary slow model");

        // Optional workflow classifier agent.
        if let Some(ref classifier_path) = classifier_agent {
            match kres_agents::AgentConfig::load_for_role(classifier_path, AgentKind::Classifier) {
                Ok(classifier_cfg) => {
                    let model = kres_repl::pick_model(
                        classifier_cfg.model.as_deref(),
                        kres_repl::ModelRole::Classifier,
                        &settings,
                    );
                    let client = Arc::new(classifier_cfg.client_builder()?.build()?);
                    let max_tokens = classifier_cfg.max_tokens.unwrap_or(model.max_output_tokens);
                    let thinking = classifier_cfg
                        .thinking
                        .as_ref()
                        .map(|thinking| thinking.to_budget(max_tokens));
                    let env = kres_agents::workflow_runner::AgentEnv::new_with_config(
                        client,
                        &model.id,
                        max_tokens,
                        classifier_cfg.system,
                        thinking,
                    );
                    session = session.with_workflow_classifier(env);
                    kres_core::async_eprintln!("classifier agent: ready");
                }
                Err(e) => {
                    kres_core::async_eprintln!("classifier agent config load: {e}");
                }
            }
        }

        // Optional todo agent.
        if let Some(ref tc_path) = todo_agent {
            match kres_agents::AgentConfig::load_for_role(tc_path, AgentKind::Todo) {
                Ok(tc_cfg) => {
                    let model = kres_repl::pick_model(
                        tc_cfg.model.as_deref(),
                        kres_repl::ModelRole::Todo,
                        &settings,
                    );
                    let client = Arc::new(tc_cfg.client_builder()?.build()?);
                    let max_tokens = tc_cfg.max_tokens.unwrap_or(model.max_output_tokens);
                    let todo_client = Arc::new(kres_agents::TodoClient {
                        client,
                        model: model.clone(),
                        system: tc_cfg.system,
                        max_tokens,
                        max_input_tokens: tc_cfg.max_input_tokens,
                        thinking: tc_cfg
                            .thinking
                            .as_ref()
                            .map(|thinking| thinking.to_budget(max_tokens)),
                        usage: usage.clone(),
                    });
                    session = session.with_todo_client(todo_client);
                    kres_core::async_eprintln!("todo agent: ready");
                }
                Err(e) => kres_core::async_eprintln!("todo agent config load: {e}"),
            }
        }
        kres_core::async_eprintln!("AgentRunner: ready (gather_turns={})", args.gather_turns);
    } else {
        kres_core::async_eprintln!(
            "AgentRunner: not configured (pass --fast-agent/--slow-agent or configure matching ~/.kres/models/*.json files)"
        );
    }
    if let Some(ref raw_arg) = args.prompt {
        if resumed_ok {
            kres_core::async_eprintln!(
                "resume: ignoring --prompt because --resume loaded prior state; \
                 use /continue to dispatch pending items"
            );
        } else {
            let resolved_kres_dir = kres_dir();
            match kres_repl::review_prompt_file_from_prompt(raw_arg, resolved_kres_dir.as_deref()) {
                Ok(Some(cfg)) => {
                    kres_core::async_eprintln!(
                        "prompt: loaded {} lens(es) + {} chars of prose from {}",
                        cfg.prompt_file.lenses.len(),
                        cfg.prompt_file.prompt.len(),
                        cfg.source,
                    );
                    session = session.with_review_prompt_config(cfg);
                }
                Ok(None) => match resolve_prompt_arg(raw_arg) {
                    Ok((source, body)) => {
                        let pf = kres_agents::parse_prompt_file(&body);
                        kres_core::async_eprintln!(
                            "prompt: loaded {} lens(es) + {} chars of prose from {}",
                            pf.lenses.len(),
                            pf.prompt.len(),
                            source,
                        );
                        session = session.with_prompt_file(pf);
                    }
                    Err(e) => kres_core::async_eprintln!("prompt: {e}"),
                },
                Err(e) => kres_core::async_eprintln!("prompt: {e}"),
            }
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

/// Validate a workflow JSON file against the embedded JSON Schema
/// and cross-field invariants. Prints `ok: <n> steps` on success;
/// returns the first batch of errors otherwise. Synchronous — no
/// network or I/O beyond reading the file.
/// If `--prompt` looks like a workflow invocation
/// (`<id>: <target>` or `/<id> <target>`) AND `<id>` resolves to an
/// embedded or operator-overridden workflow, build a `RunWorkflowArgs`
/// for executor-owned workflows. `review` deliberately returns None
/// here because its JSON-owned execution path is the REPL task/todo
/// loop, not the one-shot workflow executor.
fn workflow_short_circuit_from_repl_args(repl: &ReplArgs) -> Option<RunWorkflowArgs> {
    use kres_agents::workflow::lookup_workflow;
    let raw = repl.prompt.as_ref()?;
    let (id, rest) = kres_repl::workflow_prompt_invocation(raw)?;
    if id == "review" {
        return None;
    }
    // Resolve via the workflow registry — disk override first, then
    // embedded. If neither has it, the caller handles the prompt as a
    // normal slash-command template or inline text.
    let resolved_kres_dir: Option<PathBuf> = kres_dir();
    let override_dir: Option<PathBuf> = resolved_kres_dir.as_ref().map(|d| d.join("workflows"));
    let wf = match lookup_workflow(override_dir.as_deref(), id) {
        Ok(w) => w,
        Err(_) => return None,
    };
    let (mut input, workspace) = if id == "validate" {
        validate_prompt_inputs(rest, &repl.workspace)
    } else {
        let chosen_input_key = kres_repl::target_input_key(&wf);
        (
            vec![format!("{chosen_input_key}={rest}")],
            repl.workspace.clone(),
        )
    };
    // Path to the workflow file: the lookup is by id, but
    // RunWorkflowArgs takes a path. Use a sentinel — the runner re-
    // looks up via id when the path doesn't exist (handled in
    // run_workflow below).
    let stub_path: PathBuf = format!("workflow-id:{id}").into();
    // The REPL's --workspace and --results carry through into the
    // workflow run so `kres --results may6 --prompt '/review HEAD'`
    // writes findings.json + report.md to may6/. Earlier the
    // short-circuit silently dropped both flags.
    let results = repl.results.clone();
    grant_prompt_path_mentions(&workspace, raw);
    if rest != raw {
        grant_prompt_path_mentions(&workspace, rest);
    }
    Some(RunWorkflowArgs {
        path: stub_path,
        input: std::mem::take(&mut input),
        workspace,
        kres_dir: resolved_kres_dir,
        slow: repl.slow.clone(),
        compare: repl.compare,
        fast_model: repl.fast_model.clone(),
        slow_model: repl.slow_model.clone(),
        classifier_model: repl.classifier_model.clone(),
        skills_dir: None,
        logs: None,
        state_dir: None,
        resume: false,
        results,
        iteration_cap: if repl.turns > 0 {
            repl.turns as usize
        } else {
            200
        },
        mcp_config: None,
        assisted_by: repl.assisted_by.clone(),
    })
}

fn validate_prompt_inputs(rest: &str, default_workspace: &Path) -> (Vec<String>, PathBuf) {
    let mut parts = rest.split_whitespace();
    let finding = parts.next().unwrap_or_default();
    let workspace = resolve_validate_source_workspace(default_workspace, parts.next());
    (
        vec![
            format!("target={finding}"),
            format!("source_workspace={}", workspace.display()),
        ],
        workspace,
    )
}

fn resolve_validate_source_workspace(default_workspace: &Path, workspace: Option<&str>) -> PathBuf {
    let raw = workspace
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let expanded = if raw == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    let resolved = if raw == "." {
        default_workspace.to_path_buf()
    } else if expanded.is_absolute() {
        expanded
    } else {
        default_workspace.join(expanded)
    };
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

fn validate_workflow_short_circuit_model_flags(repl: &ReplArgs) -> Result<()> {
    let mut unsupported = Vec::new();
    if repl.main_model.is_some() {
        unsupported.push("--main-model");
    }
    if repl.todo_model.is_some() {
        unsupported.push("--todo-model");
    }
    if unsupported.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{} {} not supported for workflow executor prompts; use --fast-model/--slow-model or run the REPL task loop",
        unsupported.join(" and "),
        if unsupported.len() == 1 { "is" } else { "are" }
    )
}

fn grant_prompt_path_mentions(workspace: &std::path::Path, prompt: &str) {
    let store = kres_core::consent::get_or_install();
    let added = kres_core::consent::grant_paths_from_text(&store, workspace, prompt);
    if added.is_empty() {
        return;
    }

    let label: Vec<String> = added.iter().map(|g| g.dir.display().to_string()).collect();
    eprintln!(
        "consent: granted access to {} dir(s) named in --prompt: {}",
        added.len(),
        truncate(&label.join(", "), 200)
    );

    let wide: Vec<String> = added
        .iter()
        .filter(|g| g.suspicious)
        .map(|g| g.dir.display().to_string())
        .collect();
    if !wide.is_empty() {
        eprintln!(
            "consent: WARNING wide grant(s) for top-level system dir(s): {} — narrow the path in --prompt or restart kres if accidental",
            truncate(&wide.join(", "), 200)
        );
    }
}

fn run_validate_workflow(args: ValidateWorkflowArgs) -> Result<()> {
    let wf = kres_agents::workflow::load_workflow(&args.path)?;
    println!("ok: workflow '{}' — {} step(s)", wf.id, wf.steps.len());
    Ok(())
}

/// Run a workflow end-to-end against the kres-llm client. Builds an
/// `LlmDriver` with whichever model configs are present in
/// `--kres-dir/models`, applies derive rules to the input map, then drives
/// the executor. Trace is printed line-by-line as events happen.
async fn run_workflow(args: RunWorkflowArgs) -> Result<()> {
    use std::sync::Arc;

    use kres_agents::config::AgentConfig;
    use kres_agents::workflow_runner::{derive_inputs, parse_input_kvs, AgentEnv, LlmDriver};

    let kres_dir = args
        .kres_dir
        .clone()
        .or_else(|| dirs::home_dir().map(|h| h.join(".kres")))
        .ok_or_else(|| anyhow::anyhow!("could not resolve kres-dir (set --kres-dir)"))?;
    let workflow = kres_repl::load_workflow_path_or_id(&args.path, Some(&kres_dir))?;
    let mut settings = load_settings_for_kres_dir(&kres_dir, &args.workspace);
    apply_workflow_model_overrides(&mut settings, &args);
    let config_dirs = std::slice::from_ref(&kres_dir);
    let fast_model_cfg = resolve_agent_for_model_in_dirs(
        None,
        settings.model_for(kres_repl::ModelRole::Fast),
        config_dirs,
    )?;
    let classifier_model_cfg = resolve_agent_for_model_in_dirs(
        None,
        settings.model_for(kres_repl::ModelRole::Classifier),
        config_dirs,
    )?;
    let mut slow_agent_specs = if !args.slow.is_empty() {
        args.slow
            .iter()
            .map(|selector| {
                resolve_slow_selector_in_dirs(selector, &settings, std::slice::from_ref(&kres_dir))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        resolve_agent_for_model_in_dirs(
            None,
            settings.model_for(kres_repl::ModelRole::Slow),
            config_dirs,
        )?
        .map(|path| vec![(path, args.slow_model.clone())])
        .unwrap_or_default()
    };
    if args.slow.is_empty() {
        append_configured_secondary_slow(&mut slow_agent_specs, &settings, config_dirs)?;
    }
    if let Some(spec) = slow_agent_specs.first() {
        apply_slow_model_override_from_spec(&mut settings, spec);
    }
    let fast_path = fast_model_cfg.unwrap_or_else(|| kres_dir.join("models/__missing_fast__.json"));
    let slow_path = slow_agent_specs
        .first()
        .map(|(path, _)| path.clone())
        .unwrap_or_else(|| kres_dir.join("models/__missing_slow__.json"));
    let classifier_path = classifier_model_cfg.unwrap_or_else(|| fast_path.clone());

    let mut inputs_raw = parse_input_kvs(&args.input)?;
    if workflow.inputs.contains_key("assisted_by") {
        if let Some(value) = args
            .assisted_by
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            inputs_raw.insert(
                "assisted_by".into(),
                serde_json::Value::String(value.into()),
            );
        } else if !inputs_raw.contains_key("assisted_by") {
            inputs_raw.insert(
                "assisted_by".into(),
                serde_json::Value::String(default_assisted_by_for_slow_agent(
                    Some(&slow_path),
                    &settings,
                )),
            );
        }
    }
    kres_repl::apply_results_artifact_dir(&workflow, &mut inputs_raw, args.results.as_deref());
    let inputs = derive_inputs(&workflow, inputs_raw);

    let usage = Arc::new(kres_core::UsageTracker::new());
    let mut driver =
        LlmDriver::new(args.workspace.clone(), workflow.clone()).with_usage(usage.clone());

    // JSONL turn logging. TurnLogger::new appends `.kres/logs/<uuid>` itself, so
    // pass a base directory (the workspace by default), matching the REPL.
    let logs_base = args.logs.clone().unwrap_or_else(|| args.workspace.clone());
    if let Err(e) = std::fs::create_dir_all(&logs_base) {
        eprintln!(
            "warning: could not create logs dir {}: {e}",
            logs_base.display()
        );
    }
    let logger: Option<Arc<kres_core::log::TurnLogger>> =
        match kres_core::log::TurnLogger::new(&logs_base) {
            Ok(lg) => {
                eprintln!("logs: {}", lg.session_dir().display());
                Some(Arc::new(lg))
            }
            Err(e) => {
                eprintln!("warning: could not init turn logger: {e}");
                None
            }
        };

    // Keep the AgentEnv fallback path for workflows that do not need
    // followup gathering, but use the same settings model selection as the REPL.
    if AgentConfig::backing_path(&fast_path).exists() {
        let cfg = AgentConfig::load_for_role(&fast_path, AgentKind::Fast)?;
        let client = Arc::new(cfg.client_builder()?.build()?);
        let model =
            kres_repl::pick_model(cfg.model.as_deref(), kres_repl::ModelRole::Fast, &settings);
        let max_tokens = cfg.max_tokens.unwrap_or(model.max_output_tokens);
        let thinking = cfg
            .thinking
            .as_ref()
            .map(|thinking| thinking.to_budget(max_tokens));
        driver = driver.with_fast(AgentEnv::new_with_config(
            client,
            &model.id,
            max_tokens,
            cfg.system.clone(),
            thinking,
        ));
    }
    if AgentConfig::backing_path(&slow_path).exists() {
        let cfg = AgentConfig::load_for_role(&slow_path, AgentKind::Slow)?;
        let client = Arc::new(cfg.client_builder()?.build()?);
        let model =
            kres_repl::pick_model(cfg.model.as_deref(), kres_repl::ModelRole::Slow, &settings);
        let max_tokens = cfg.max_tokens.unwrap_or(model.max_output_tokens);
        let thinking = cfg
            .thinking
            .as_ref()
            .map(|thinking| thinking.to_budget(max_tokens));
        let slow_env = AgentEnv::new_with_config(
            client.clone(),
            &model.id,
            max_tokens,
            cfg.system.clone(),
            thinking,
        );
        driver = driver.with_slow(slow_env);
        let code_env =
            AgentEnv::new_with_config(client, &model.id, max_tokens, cfg.system, thinking);
        driver = driver.with_code(code_env);
    }
    if AgentConfig::backing_path(&classifier_path).exists() {
        let cfg = AgentConfig::load_for_role(&classifier_path, AgentKind::Classifier)?;
        let client = Arc::new(cfg.client_builder()?.build()?);
        let model = kres_repl::pick_model(
            cfg.model.as_deref(),
            kres_repl::ModelRole::Classifier,
            &settings,
        );
        let max_tokens = cfg.max_tokens.unwrap_or(model.max_output_tokens);
        let thinking = cfg
            .thinking
            .as_ref()
            .map(|thinking| thinking.to_budget(max_tokens));
        driver = driver.with_classifier(AgentEnv::new_with_config(
            client,
            &model.id,
            max_tokens,
            cfg.system.clone(),
            thinking,
        ));
    }

    if driver.fast.is_none() && driver.slow.is_none() {
        return Err(anyhow::anyhow!(
            "no model configs found in {}/models — workflow can't run without at least one role wired",
            kres_dir.display()
        ));
    }

    // Build a full AgentRunner when both fast and slow agents are wired.
    // This reuses the same builder the REPL uses, so model selection,
    // prompt loading, rate-limit sharing, gather-turn handling, and lens
    // consolidation setup stay in one place.
    if AgentConfig::backing_path(&fast_path).exists()
        && AgentConfig::backing_path(&slow_path).exists()
    {
        use kres_agents::WorkspaceFetcher;
        let workspace_fetcher = WorkspaceFetcher::new(args.workspace.clone());
        let mcp_path = args
            .mcp_config
            .clone()
            .unwrap_or_else(|| kres_dir.join("mcp.json"));
        let fetcher: Arc<dyn kres_agents::pipeline::DataFetcher> = if mcp_path.exists() {
            match kres_mcp::ServerRegistry::load_from_file(&mcp_path) {
                Ok(reg) => match reg.servers.iter().next() {
                    Some((name, server_cfg)) => {
                        // MCP wants its own log dir; reuse the
                        // logger's session dir if we built one,
                        // else fall back to the logs_base.
                        let mcp_log_dir = logger
                            .as_ref()
                            .map(|lg| lg.session_dir().to_path_buf())
                            .unwrap_or_else(|| logs_base.clone());
                        let server_cfg = server_cfg.with_workspace_cwd(name, &args.workspace);
                        match kres_mcp::McpClient::spawn(name, &server_cfg, &mcp_log_dir).await {
                            Ok(mcp) => {
                                eprintln!(
                                    "mcp: spawned '{name}' from {} ({} tool(s) advertised)",
                                    mcp_path.display(),
                                    mcp.tools().len()
                                );
                                kres_agents::McpFetcher::new(mcp, workspace_fetcher.clone())
                            }
                            Err(e) => {
                                eprintln!(
                                    "mcp: spawn '{name}' failed ({e}); using workspace-only fetcher"
                                );
                                workspace_fetcher.clone()
                            }
                        }
                    }
                    None => workspace_fetcher.clone(),
                },
                Err(e) => {
                    eprintln!(
                        "mcp: read {} failed ({e}); using workspace-only fetcher",
                        mcp_path.display()
                    );
                    workspace_fetcher.clone()
                }
            }
        } else {
            workspace_fetcher.clone()
        };

        let built = kres_repl::build_agent_runner(
            &fast_path,
            &slow_path,
            args.workspace.clone(),
            fetcher,
            &settings,
            kres_repl::AgentRunnerBuildOptions {
                extra_slow_cfgs: slow_agent_specs.iter().skip(1).cloned().collect(),
                compare_slow_models: args.compare,
                usage: Some(usage.clone()),
                gather_turns: 5,
                logger: logger.clone(),
                comparison_path: args.results.as_ref().and_then(|results| {
                    review_comparison_path(results, slow_agent_specs.len(), args.compare)
                }),
                ..Default::default()
            },
        )
        .await?;
        driver = driver
            .with_agent_runner(built.agent_runner)
            .with_consolidator(built.consolidator);
        eprintln!(
            "AgentRunner: wired via REPL builder (WorkspaceFetcher in {}; lens fan-out shares gather via run_with_lenses)",
            args.workspace.display()
        );
    } else {
        eprintln!(
            "AgentRunner: not wired (need both fast+slow model configs); falling back to single-shot LLM calls — followups WILL NOT be gathered"
        );
    }

    // Skills loading: respects --skills-dir, otherwise <kres-dir>/skills.
    let skills_dir = args
        .skills_dir
        .clone()
        .unwrap_or_else(|| kres_dir.join("skills"));
    let (driver_with_skills, skill_warnings) = driver.with_skills_dir(&skills_dir)?;
    driver = driver_with_skills;
    for w in &skill_warnings {
        eprintln!("warning: {w}");
    }

    // Hand the same logger to the LlmDriver so the AgentEnv-fallback
    // path (used by tests) also writes to code.jsonl.
    if let Some(lg) = logger.as_ref() {
        driver = driver.with_logger(lg.clone());
    }

    eprintln!(
        "running workflow '{}' from {} ({} steps, iteration cap {})",
        workflow.id,
        args.path.display(),
        workflow.steps.len(),
        args.iteration_cap
    );

    let run = kres_repl::run_workflow_driver(
        &workflow,
        &mut driver,
        inputs,
        kres_repl::WorkflowRunOptions {
            iteration_cap: args.iteration_cap,
            state_dir: args.state_dir.clone(),
            resume: args.resume,
            results_dir: args.results.clone(),
            observer: None,
        },
    )
    .await?;
    print!("{}", run.trace.pretty());
    for p in run.written_artifacts {
        eprintln!("wrote {}", p.display());
    }
    if let Some(summary) = kres_core::format_usage_summary(
        &usage,
        "final usage before exit",
        Some("final usage before exit: no API usage recorded"),
    ) {
        eprintln!("{summary}");
    }
    kres_repl::workflow_status_result(&run.trace.status)
}

async fn run_test(args: TestArgs) -> Result<()> {
    use kres_agents::AgentConfig;
    use kres_llm::{config::CallConfig, request::Message, Model};

    let agent_cfg = AgentConfig::load(&args.config)
        .with_context(|| format!("loading model config {}", args.config.display()))?;
    let model = match args.model.as_deref() {
        Some(id) => Model::from_id(id),
        None => agent_cfg
            .model
            .as_deref()
            .map(Model::from_id)
            .unwrap_or_else(Model::sonnet_4_6),
    };
    kres_core::async_eprintln!("model: {}", model.id);

    let client = agent_cfg.client_builder()?.build()?;
    let max_tokens = agent_cfg.max_tokens.unwrap_or(model.max_output_tokens);
    let mut cfg = CallConfig::defaults_for(model.clone()).with_max_tokens(max_tokens);
    if let Some(thinking) = agent_cfg.thinking.as_ref() {
        cfg = cfg.with_thinking(thinking.to_budget(max_tokens));
    }
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
        let c = Cli::try_parse_from(["kres", "test", "/tmp/model.json", "--prompt", "hi"]).unwrap();
        match c.cmd {
            Some(Command::Test(a)) => {
                assert_eq!(a.prompt, "hi");
                assert_eq!(a.config, PathBuf::from("/tmp/model.json"));
            }
            _ => panic!("expected test"),
        }
    }

    #[test]
    fn turn_args_parse() {
        let c = Cli::try_parse_from([
            "kres",
            "turn",
            "/tmp/model.json",
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
                assert_eq!(a.config, PathBuf::from("/tmp/model.json"));
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
    fn workflow_prompt_short_circuit_grants_prompt_path_mentions() {
        let base = std::env::temp_dir().join(format!(
            "kres-prompt-consent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = base.join("workspace");
        let bug_dir = base.join("bugs");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&bug_dir).unwrap();
        let bug_file = bug_dir.join("psp-uaf-assoc-get.bug");
        std::fs::write(&bug_file, "bug prose\n").unwrap();

        let store = kres_core::consent::get_or_install();
        store.clear();
        let prompt = format!("fix: {}", bug_file.display());
        let c = Cli::try_parse_from([
            "kres",
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            &prompt,
        ])
        .unwrap();

        let args = workflow_short_circuit_from_repl_args(&c.repl)
            .expect("fix prompt should short-circuit");
        assert_eq!(args.workspace, workspace);
        assert!(
            store.is_allowed(&bug_file.canonicalize().unwrap()),
            "--prompt path mentions should grant the containing directory before workflow launch"
        );

        store.clear();
        let prompt = format!("fix:{}", bug_file.display());
        let c = Cli::try_parse_from([
            "kres",
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            &prompt,
        ])
        .unwrap();

        let args = workflow_short_circuit_from_repl_args(&c.repl)
            .expect("fix prompt without a post-colon space should short-circuit");
        assert_eq!(args.workspace, workspace);
        assert!(
            store.is_allowed(&bug_file.canonicalize().unwrap()),
            "--prompt workflow rest should grant paths even when written as fix:/path"
        );

        store.clear();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn workflow_prompt_short_circuit_preserves_fast_slow_model_overrides() {
        let c = Cli::try_parse_from([
            "kres",
            "--prompt",
            "fix: /tmp/finding",
            "--fast-model",
            "fast-test",
            "--slow-model",
            "slow-test",
            "--classifier-model",
            "classifier-test",
        ])
        .unwrap();

        let args = workflow_short_circuit_from_repl_args(&c.repl)
            .expect("fix prompt should short-circuit");
        assert_eq!(args.fast_model.as_deref(), Some("fast-test"));
        assert_eq!(args.slow_model.as_deref(), Some("slow-test"));
        assert_eq!(args.classifier_model.as_deref(), Some("classifier-test"));
        validate_workflow_short_circuit_model_flags(&c.repl).unwrap();
    }

    #[test]
    fn workflow_prompt_short_circuit_preserves_multiple_slow_models() {
        let c = Cli::try_parse_from([
            "kres",
            "--prompt",
            "fix: /tmp/finding",
            "--slow",
            "opus,gpt",
        ])
        .unwrap();
        let args = workflow_short_circuit_from_repl_args(&c.repl).unwrap();
        assert_eq!(args.slow, vec!["opus", "gpt"]);
        assert!(!args.compare);
    }

    #[test]
    fn validate_prompt_short_circuit_sets_source_workspace() {
        let base = std::env::temp_dir().join(format!(
            "kres-validate-prompt-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = base.join("workspace");
        let source = base.join("linux");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&source).unwrap();

        let c = Cli::try_parse_from([
            "kres",
            "--workspace",
            workspace.to_str().unwrap(),
            "--prompt",
            &format!("validate: /tmp/finding {}", source.display()),
        ])
        .unwrap();

        let args = workflow_short_circuit_from_repl_args(&c.repl)
            .expect("validate prompt should short-circuit");
        assert_eq!(args.workspace, source);
        assert!(args.input.contains(&"target=/tmp/finding".to_string()));
        assert!(args
            .input
            .contains(&format!("source_workspace={}", source.display())));

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn validate_prompt_defaults_source_workspace() {
        let c = Cli::try_parse_from([
            "kres",
            "--workspace",
            "/tmp/source",
            "--prompt",
            "validate: /tmp/finding",
        ])
        .unwrap();

        let args = workflow_short_circuit_from_repl_args(&c.repl)
            .expect("validate prompt should short-circuit");
        assert_eq!(args.workspace, PathBuf::from("/tmp/source"));
        assert!(args
            .input
            .contains(&"source_workspace=/tmp/source".to_string()));
    }

    #[test]
    fn workflow_prompt_short_circuit_rejects_unused_main_todo_model_overrides() {
        let c = Cli::try_parse_from([
            "kres",
            "--prompt",
            "fix: /tmp/finding",
            "--main-model",
            "main-test",
            "--todo-model",
            "todo-test",
        ])
        .unwrap();

        assert!(workflow_short_circuit_from_repl_args(&c.repl).is_some());
        assert!(
            validate_workflow_short_circuit_model_flags(&c.repl).is_err(),
            "workflow executor prompt must not silently accept unused main/todo model overrides"
        );
    }

    #[test]
    fn one_flag_defaults_off_and_parses() {
        let bare = Cli::try_parse_from(["kres"]).unwrap();
        assert!(!bare.repl.one, "--one must default off");
        let with = Cli::try_parse_from(["kres", "--one"]).unwrap();
        assert!(with.repl.one, "--one must parse as a bare bool flag");
    }

    #[test]
    fn prompt_requires_resolved_fast_and_slow_agent_configs() {
        let c = Cli::try_parse_from(["kres", "--prompt", "review: security/security.c"]).unwrap();
        let mut settings = kres_repl::Settings::default();
        settings.set_model(
            kres_repl::ModelRole::Fast,
            Some("configured-fast".to_string()),
        );
        settings.set_model(
            kres_repl::ModelRole::Slow,
            Some("configured-slow".to_string()),
        );

        let fast_agent = PathBuf::from("/tmp/fast.json");
        let err =
            validate_prompt_agent_configs(&c.repl, Some(&fast_agent), None, &settings).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("--prompt requires fast and slow agent configs"));
        assert!(rendered.contains("slow: model \"configured-slow\""));
    }

    #[test]
    fn slow_tag_unset_when_not_passed() {
        // --slow has no clap default, so the
        // settings.json slow model is not silently overridden when
        // the operator omits the flag (user report 2026-04-21).
        let c = Cli::try_parse_from(["kres"]).unwrap();
        assert!(c.repl.slow.is_empty());
    }

    #[test]
    fn run_workflow_slow_tag_unset_when_not_passed() {
        let c = Cli::try_parse_from(["kres", "run-workflow", "workflow-id:fix"]).unwrap();
        match c.cmd {
            Some(Command::RunWorkflow(args)) => assert!(args.slow.is_empty()),
            other => panic!("expected run-workflow command, got {other:?}"),
        }
    }

    #[test]
    fn run_workflow_model_overrides_parse() {
        let c = Cli::try_parse_from([
            "kres",
            "run-workflow",
            "workflow-id:fix",
            "--fast-model",
            "fast-test",
            "--slow-model",
            "slow-test",
            "--classifier-model",
            "classifier-test",
        ])
        .unwrap();
        match c.cmd {
            Some(Command::RunWorkflow(args)) => {
                assert_eq!(args.fast_model.as_deref(), Some("fast-test"));
                assert_eq!(args.slow_model.as_deref(), Some("slow-test"));
                assert_eq!(args.classifier_model.as_deref(), Some("classifier-test"));
            }
            other => panic!("expected run-workflow command, got {other:?}"),
        }
    }

    #[test]
    fn classifier_agent_resolution_only_falls_back_when_unconfigured() {
        let base = std::env::temp_dir().join(format!(
            "kres-classifier-resolution-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let fast = models.join("fast.json");
        let classifier = models.join("classifier.json");
        std::fs::write(&fast, r#"{"models":{"fast-model":{}}}"#).unwrap();
        std::fs::write(&classifier, r#"{"models":{"classifier-model":{}}}"#).unwrap();
        let dirs = vec![base.clone()];

        assert_eq!(
            resolve_classifier_agent_for_model_in_dirs(
                Some("classifier-model"),
                Some(&fast),
                &dirs
            )
            .unwrap(),
            Some(qualified_model_path(&classifier, "classifier-model"))
        );
        assert_eq!(
            resolve_classifier_agent_for_model_in_dirs(Some("missing"), Some(&fast), &dirs)
                .unwrap(),
            None,
            "configured classifier model must not silently fall back to fast"
        );
        assert_eq!(
            resolve_classifier_agent_for_model_in_dirs(None, Some(&fast), &dirs).unwrap(),
            Some(fast),
            "fast fallback is only for old settings without classifier"
        );

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn run_workflow_does_not_accept_unwired_main_todo_model_overrides() {
        assert!(Cli::try_parse_from([
            "kres",
            "run-workflow",
            "workflow-id:fix",
            "--main-model",
            "main-test",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "kres",
            "run-workflow",
            "workflow-id:fix",
            "--todo-model",
            "todo-test",
        ])
        .is_err());
    }

    #[test]
    fn slow_tag_conflicts_with_slow_model() {
        assert!(
            Cli::try_parse_from(["kres", "--slow", "opus", "--slow-model", "gpt-5.5"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "kres",
            "run-workflow",
            "workflow-id:fix",
            "--slow",
            "opus",
            "--slow-model",
            "gpt-5.5",
        ])
        .is_err());
    }

    #[test]
    fn slow_tag_passes_through_when_set() {
        let c = Cli::try_parse_from(["kres", "--slow", "opus"]).unwrap();
        assert_eq!(c.repl.slow, vec!["opus"]);
    }

    #[test]
    fn slow_tag_can_repeat_for_comparison() {
        let c = Cli::try_parse_from(["kres", "--slow", "sonnet", "--slow", "opus"]).unwrap();
        assert_eq!(c.repl.slow, vec!["sonnet", "opus"]);
    }

    #[test]
    fn comparison_artifact_requires_multiple_slow_models() {
        let results = Path::new("results");
        assert_eq!(review_comparison_path(results, 0, true), None);
        assert_eq!(review_comparison_path(results, 1, true), None);
        assert_eq!(review_comparison_path(results, 2, false), None);
        assert_eq!(
            review_comparison_path(results, 2, true),
            Some(results.join("comparison.json"))
        );
    }

    #[test]
    fn comparison_mode_is_explicit() {
        let normal = Cli::try_parse_from(["kres", "--slow", "opus,gpt"]).unwrap();
        assert!(!normal.repl.compare);
        let comparison = Cli::try_parse_from(["kres", "--slow", "opus,gpt", "--compare"]).unwrap();
        assert!(comparison.repl.compare);
    }

    #[test]
    fn slow_selector_resolves_unique_model() {
        let base = std::env::temp_dir().join(format!(
            "kres-slow-selector-exact-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let provider = models.join("provider.json");
        std::fs::write(&provider, r#"{"models":{"foo":{}}}"#).unwrap();

        let (found, selected) = resolve_slow_selector_in_dirs(
            "foo",
            &kres_repl::Settings::default(),
            std::slice::from_ref(&base),
        )
        .unwrap();
        assert_eq!(found, qualified_model_path(&provider, "foo"));
        assert_eq!(selected.as_deref(), Some("foo"));

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn configured_secondary_slow_resolves_to_an_additional_spec() {
        let base = std::env::temp_dir().join(format!(
            "kres-secondary-slow-selector-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let provider = models.join("provider.json");
        std::fs::write(&provider, r#"{"models":{"primary":{},"secondary":{}}}"#).unwrap();
        let settings: kres_repl::Settings =
            serde_json::from_str(r#"{"models":{"slow":"primary","slow_secondary":"secondary"}}"#)
                .unwrap();
        let mut specs = vec![(
            qualified_model_path(&provider, "primary"),
            Some("primary".into()),
        )];

        append_configured_secondary_slow(&mut specs, &settings, std::slice::from_ref(&base))
            .unwrap();

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[1].0, qualified_model_path(&provider, "secondary"));
        assert_eq!(specs[1].1.as_deref(), Some("secondary"));
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn qualified_selector_disambiguates_duplicate_model() {
        let base = std::env::temp_dir().join(format!(
            "kres-slow-selector-substring-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let first = models.join("first.json");
        let second = models.join("second.json");
        std::fs::write(&first, r#"{"models":{"foo":{}}}"#).unwrap();
        std::fs::write(&second, r#"{"models":{"foo":{}}}"#).unwrap();

        let (found, model_override) = resolve_slow_selector_in_dirs(
            "second.json:foo",
            &kres_repl::Settings::default(),
            std::slice::from_ref(&base),
        )
        .unwrap();
        assert_eq!(found, qualified_model_path(&second, "foo"));
        assert_eq!(model_override.as_deref(), Some("second.json:foo"));

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn plain_selector_rejects_duplicate_model() {
        let base = std::env::temp_dir().join(format!(
            "kres-slow-selector-ambiguous-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("first.json"), r#"{"models":{"foo":{}}}"#).unwrap();
        std::fs::write(models.join("second.json"), r#"{"models":{"foo":{}}}"#).unwrap();

        let err = resolve_slow_selector_in_dirs(
            "foo",
            &kres_repl::Settings::default(),
            std::slice::from_ref(&base),
        )
        .expect_err("ambiguous selector must fail");
        assert!(err.to_string().contains("ambiguous"));

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn slow_selector_known_alias_resolves_to_shipped_model_id() {
        let base = std::env::temp_dir().join(format!(
            "kres-slow-selector-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let provider = models.join("anthropic.json");
        std::fs::write(&provider, r#"{"models":{"claude-sonnet-5":{}}}"#).unwrap();

        let (found, model_override) = resolve_slow_selector_in_dirs(
            "sonnet",
            &kres_repl::Settings::default(),
            std::slice::from_ref(&base),
        )
        .unwrap();
        assert_eq!(found, qualified_model_path(&provider, "claude-sonnet-5"));
        assert_eq!(model_override.as_deref(), Some("claude-sonnet-5"));

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn configured_slow_alias_overrides_shipped_alias() {
        let base = std::env::temp_dir().join(format!(
            "kres-slow-selector-configured-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let models = base.join("models");
        std::fs::create_dir_all(&models).unwrap();
        let provider = models.join("provider.json");
        std::fs::write(&provider, r#"{"models":{"claude-sonnet-5":{}}}"#).unwrap();
        let settings: kres_repl::Settings =
            serde_json::from_str(r#"{"model_aliases":{"sonnet":"claude-sonnet-5"}}"#).unwrap();

        let (found, model_override) =
            resolve_slow_selector_in_dirs("sonnet", &settings, std::slice::from_ref(&base))
                .unwrap();
        assert_eq!(found, qualified_model_path(&provider, "claude-sonnet-5"));
        assert_eq!(model_override.as_deref(), Some("claude-sonnet-5"));

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn slow_selector_alias_override_updates_primary_slow_settings() {
        let spec = (
            PathBuf::from("/tmp/claude-sonnet-4-6.json"),
            Some("claude-sonnet-4-6".to_string()),
        );
        let mut settings = kres_repl::Settings::default();
        settings.set_model(
            kres_repl::ModelRole::Slow,
            Some("claude-opus-4-7".to_string()),
        );

        apply_slow_model_override_from_spec(&mut settings, &spec);

        assert_eq!(
            settings.model_for(kres_repl::ModelRole::Slow),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn assisted_by_flag_parses_for_repl_and_workflow() {
        let c = Cli::try_parse_from(["kres", "--assisted-by", "custom tool"]).unwrap();
        assert_eq!(c.repl.assisted_by.as_deref(), Some("custom tool"));

        let c = Cli::try_parse_from([
            "kres",
            "run-workflow",
            "workflow-id:fix",
            "--assisted-by",
            "custom tool",
        ])
        .unwrap();
        match c.cmd {
            Some(Command::RunWorkflow(args)) => {
                assert_eq!(args.assisted_by.as_deref(), Some("custom tool"));
            }
            other => panic!("expected run-workflow command, got {other:?}"),
        }
    }

    #[test]
    fn assisted_by_defaults_to_slow_model() {
        let mut settings = kres_repl::Settings::default();
        settings.set_model(
            kres_repl::ModelRole::Slow,
            Some("claude-test-model".to_string()),
        );
        assert_eq!(
            resolved_assisted_by(None, None, &settings),
            "kres:claude-test-model"
        );
        assert_eq!(
            resolved_assisted_by(Some(&"operator value".to_string()), None, &settings),
            "operator value"
        );
    }

    #[test]
    fn run_workflow_settings_load_from_kres_dir_then_project_overlay() {
        let base = std::env::temp_dir().join(format!(
            "kres-run-workflow-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let kres_dir = base.join("kres-dir");
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&kres_dir).unwrap();
        std::fs::create_dir_all(workspace.join(".kres")).unwrap();
        std::fs::write(
            kres_dir.join("settings.json"),
            r#"{"models":{"fast":"fast-global","slow":"slow-global","main":"main-global","todo":"todo-global","classifier":"classifier-global"}}"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join(".kres/settings.json"),
            r#"{"models":{"main":"main-project"}}"#,
        )
        .unwrap();

        let settings = load_settings_for_kres_dir(&kres_dir, &workspace);
        assert_eq!(
            settings.model_for(kres_repl::ModelRole::Fast),
            Some("fast-global")
        );
        assert_eq!(
            settings.model_for(kres_repl::ModelRole::Slow),
            Some("slow-global")
        );
        assert_eq!(
            settings.model_for(kres_repl::ModelRole::Main),
            Some("main-project")
        );
        assert_eq!(
            settings.model_for(kres_repl::ModelRole::Todo),
            Some("todo-global")
        );
        assert_eq!(
            settings.model_for(kres_repl::ModelRole::Classifier),
            Some("classifier-global")
        );

        std::fs::remove_dir_all(base).ok();
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
    fn review_prompt_uses_task_loop_prompt_file() {
        let c = Cli::try_parse_from([
            "kres",
            "--results",
            "may6",
            "--turns",
            "20",
            "--prompt",
            "/review HEAD",
        ])
        .unwrap();
        assert!(
            workflow_short_circuit_from_repl_args(&c.repl).is_none(),
            "batch review must enter the REPL task/todo loop, not one-shot run-workflow"
        );
        let cfg =
            kres_repl::review_prompt_file_from_prompt(c.repl.prompt.as_deref().unwrap(), None)
                .expect("review prompt conversion")
                .expect("review prompt file");
        assert_eq!(cfg.prompt_file.lenses.len(), 5);
        assert_eq!(cfg.prompt_file.lenses[0].id, "memory-lifetime");
        assert!(cfg.prompt_file.lenses.iter().any(|l| l.id == "assertions"));
        assert!(cfg.prompt_file.prompt.contains("TARGET: HEAD"));
        assert!(cfg.prompt_file.prompt.contains("full Finding records"));
        assert!(cfg.prompt_file.prompt.contains("target diff/stat"));
        assert!(cfg.prompt_file.prompt.contains("Do not enumerate"));
        assert!(!cfg.prompt_file.prompt.contains("Knot Resolver"));
        assert!(cfg
            .consolidate_rules
            .as_deref()
            .unwrap_or_default()
            .contains("Merge the per-lens outputs"));
    }

    #[test]
    fn resolve_prompt_arg_review_is_workflow_only() {
        let err = resolve_prompt_arg("review: fs/btrfs/ctree.c")
            .expect_err("review must not compose as a template");
        assert!(
            err.to_string().contains("workflow-only"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_prompt_arg_summary_is_not_a_prompt_template() {
        let err = resolve_prompt_arg("summary: out.txt")
            .expect_err("summary must not compose as a prompt template");
        assert!(
            err.to_string().contains("report-rendering"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_prompt_arg_slash_unknown_command_falls_to_inline() {
        // A slash prefix with no matching command must pass through
        // as verbatim prompt text — NOT error, NOT be silently
        // dropped.
        let (src, body) = resolve_prompt_arg("/no-such-cmd hello world").unwrap();
        assert_eq!(src, "<inline>");
        assert_eq!(body, "/no-such-cmd hello world");
    }

    #[test]
    fn resolve_prompt_arg_fix_is_workflow_only() {
        let err = resolve_prompt_arg("fix: /tmp/finding").expect_err("fix must not compose");
        assert!(
            err.to_string().contains("workflow-only"),
            "unexpected error: {err}"
        );
        let err = resolve_prompt_arg("/fix /tmp/finding").expect_err("fix must not compose");
        assert!(
            err.to_string().contains("workflow-only"),
            "unexpected error: {err}"
        );
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
