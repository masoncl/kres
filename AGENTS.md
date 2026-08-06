# kres — Kernel Code Analysis Tool

## Architecture

kres is a multi-agent kernel code analysis REPL. Three agents collaborate:

- **Fast agent** (configurable model): Scopes work, identifies needed source code, builds a structured brief for the slow agent. Runs in task threads.
- **Slow agent** (configurable model): Deep analysis with all context pre-gathered. Thorough findings with file:line citations. Runs in task threads.
- **Main agent** (configurable model): Data retrieval only. Fetches code via semcode MCP, grep, read, git. Runs in service threads spawned per-task.

## Flow

```
User prompt → Task created → Task thread starts
  → Fast agent [round 1]: requests data via followups
  → Service thread: main agent gathers data (semcode/grep/read/git)
  → Fast agent [round 2+]: verifies, requests more or sets ready_for_slow
  → Slow agent: deep analysis with all gathered context
  → Task completes → followups sent through inference for dedup → new todos
```

## Key Design Decisions

### Compatibility Policy
- kres is new enough that there is no backwards-compatibility burden.
- Do not keep duplicate command paths, compatibility aliases, deprecated
  prompt formats, or renamed-file shims unless the current workflow
  actively needs them.
- Workflow-owned commands have exactly one implementation mechanism:
  the JSON workflow under `configs/workflows/` (or the operator's
  `~/.kres/workflows/<name>.json` override). `/fix`, `/review`,
  `/triage`, and `/validate` may be started from the REPL or from CLI
  `--prompt`; both entry points must derive behavior from the same JSON
  workflow.
- `/fix`, `/triage`, and `/validate` use the workflow executor directly.
  `/review` uses `review.json` to define its prompt contract and lenses, then
  runs through the REPL task/todo loop so followups become prioritized
  next-turn todos. Do not add a second review engine or a markdown
  prompt fallback.
- Do not reintroduce markdown prompt-template fallbacks, special REPL
  task paths, or command-specific alternate engines for workflow-owned
  commands. If workflow behavior is wrong, fix the JSON workflow and
  shared workflow runner/executor. Delete stale templates and tests
  instead of preserving alternate behavior.
- `/review` specifically must remain one JSON workflow with the old
  forward-progress semantics: each turn runs parallel slow-agent
  lenses, emits Findings plus typed followups, dedups those followups
  through the todo agent and ranks the runnable ones through the
  prioritization agent into the next review task, and continues until
  the turn cap or followup exhaustion. Do not replace
  this with workflow-local "fetch followups and repeat the same step"
  logic.
- Do not weaken the golden review prompt contract: every lens is
  exhaustive for its bug class, does not stop after the first issue, and
  emits typed followups when more source, callers, history, or API
  context is needed to be confident. A clean lens means confident, not
  merely "nothing proved from the first gathered context."
- For commit/range reviews, audit the semantic contract changed by the
  diff, not just the edited lines. If a patch changes representation,
  helper family, callback/dispatch, allocation, lifetime, locking,
  ordering, or accounting behavior, trace unchanged readers, writers,
  callers, callees, callbacks, setup/registration sites, and shared
  helpers that still rely on the old contract. Do this generically by
  following the changed contract; do not hardcode subsystem-specific
  rules. Missing unchanged paths are followups, not a clean review.
- Prompt/workflow fixes must be bug-agnostic. Do not add guidance that
  names a specific missed regression, subsystem, file, function, helper,
  or one-off mechanism because a recent run failed to find it. Generalize
  to the review invariant instead: changed contracts, unchanged users,
  concrete trigger paths, strong suspects, and typed followups for
  missing evidence. Regression tests may check that generic invariants
  are present, but prompts themselves must stay reusable across all bug
  classes.
- Negative coverage claims must be earned. Agents, consolidators, todo
  updates, and goal checks must not accept phrases like "no remaining
  users", "all callers updated", "old path unreachable", "only reader",
  or "only writer" unless the run has concrete source/search/callgraph
  or history evidence for that claim. Missing proof becomes a typed
  followup, not a clean result.
- semcode is an accelerator, not an authority. It can be unavailable
  while indexing and can miss macros, global symbols, generated-looking
  constructs, or complex definitions. Any failed, empty, unparseable, or
  "not found" semcode result must fall back to local `grep`/`read`
  evidence before research, review, todo, or goal logic treats source as
  unavailable or a symbol as absent. Never mark a finding unconfirmed,
  invalid, clean, or fully covered based only on semcode absence.
- Local fallback must preserve the complete grep match list for the requesting
  agent, because Rust cannot know which match is semantically relevant.
  Do not add per-file or shared output caps that hide candidates. Do not expand broad grep fallback results into full
  source reads for every hit; return the matches and require targeted
  `read` followups for the specific file:line ranges that need full
  context.
- Build and shell output is the one exception, and it is not a cap.
  Above `TOOL_OUTPUT_INLINE_MAX` the complete output is written to
  `<workspace>/.kres/tool-output/` and the tool result carries the
  head, the tail, the exact byte count, and that path. Nothing is
  discarded and the omission is explicit, so a targeted `read`
  recovers any region. Do not extend this to grep, source, or any
  evidence a review reasons over, and do not turn it into a silent
  truncation by dropping the pointer or the byte count.
- Prior findings are sent to agents in full, every time. Do not add
  relevance routing, anchor heuristics, source-body-free manifests, or
  any other scheme that decides which findings a review is allowed to
  see: a finding that looks unrelated by filename is exactly what a
  cross-file contract review must catch. Cost is handled by placing
  `previous_findings` in the shared cached lens prefix, not by sending
  less. If the accumulated set ever approaches model capability, add a
  semantic partitioner over findings — do not hide them.
- Rust must not infer semantic workflow state from free-form AI prose.
  Do not add substring/regex classifiers over model `analysis`,
  `invalid_evidence`, defect text, commit-message prose, or other natural
  language output to decide cleanliness, routing, invalidation, retry,
  completion, or followup creation. Use explicit JSON fields, typed
  arrays, enums, booleans, and workflow expressions. If a legacy/free-form
  output must be interpreted, add a targeted inference/judge step that
  returns structured JSON and make Rust consume only that structure.
  Prose may be preserved for humans and logs, but it must not be a hidden
  control channel.

### Lints
- The pre-commit hook runs `cargo clippy --workspace --all-targets
  -- -D warnings`. Do not silence a clippy diagnostic with
  `#[allow(clippy::...)]` (or any other lint-suppression attribute)
  to make the hook pass. Fix the underlying issue instead:
  `clippy::too_many_arguments` → bundle related args into a struct
  or split the function; `clippy::while_let_loop` → rewrite as the
  suggested form; `clippy::needless_clone` → drop the clone; and so
  on. Suppressions are only acceptable when there is a concrete
  reason the lint is wrong for that call site, in which case a
  one-line comment above the attribute must say why. A `#[allow]`
  with no explanation is treated the same as an unfixed warning.
- This rule applies even when an existing `#[allow]` is already in
  the file. Pre-existing suppressions are technical debt, not a
  precedent.

### Async REPL
- Input runs in a separate thread (readline → queue)
- Main loop: 100ms poll cycle checking input queue + servicing tasks
- `async_print()` clears readline line before printing to avoid garbled output
- All background output (task status, results) uses `async_print`

### Ratatui TUI
- Read ratatui's widget implementation before changing rendering or
  scrolling behavior. Do not infer viewport behavior from terminal
  intuition.
- `Paragraph::wrap(Wrap { .. })` reflows logical input lines through
  ratatui's `WordWrapper`; `Paragraph::scroll((y, x))` is applied
  after wrapping, so `y` is a rendered-row offset, not a stored-line
  index.
- For scrollback panes that wrap text, do not pre-slice the last N
  logical lines and then hand that to `Paragraph`; older wrapped lines
  can consume the viewport and clip newer output. Build the paragraph
  from the full retained scrollback, compute `Paragraph::line_count(width)`,
  and use `Paragraph::scroll` to follow the rendered tail.
- `Paragraph::line_count` is behind ratatui's
  `unstable-rendered-line-info` feature. Keep that feature enabled
  rather than copying or approximating ratatui's private wrapping logic.
- Tests for scrollback visibility must render through ratatui
  (`TestBackend`/`Terminal` or equivalent) and assert on the rendered
  buffer. Tests that only validate local line-count approximations are
  not sufficient for TUI scrolling bugs.

### Task System
- Each todo item becomes a `Task` with its own thread
- Task states: `pending → inference → waiting_main → gathering → done`
- `TaskManager` handles scheduling (respects `depends_on`), servicing, reaping

### Shared Symbol Cache
- `TaskManager.symbol_cache` and `context_cache` are thread-safe (via `cache_lock`)
- Tasks seed from cache at startup — avoids re-fetching known symbols
- Source followups served from cache skip the main agent entirely
- Cache populated after service thread gathers data and when tasks are reaped

### Todo List with Completed History
- Completed items stay in the list as `status=done`
- All followup→todo additions go through `_update_todo_via_agent` (inference call)
- Main agent sees done items and won't re-add equivalent work
- `todo_lock` protects all list mutations from concurrent access
- Done items preserved even if main agent drops them from its response

The todo list is Rust-owned and the agent's reply is a set of edits
against it, not a rewrite of it (`kres-agents/src/todo_agent.rs`,
`reconcile_update`). The agent controls prose (`name`, `reason`,
`type`), completion (`newly_done`) and retirement (`retired`). It
controls nothing else:

- `todo` carries the PENDING list only. Done rows are reconstructed
  from Rust's own copy, so the agent never re-emits them.
- ORDER is not a channel. The pending list is stable storage:
  surviving rows keep their position and new rows are appended.
  Choosing what runs next belongs to the prioritization agent.
- `id`, `step_id` and `depends_on` on an existing row are restored
  from the original. Do not ask the agent to re-emit fields Rust
  overwrites; that is pure output cost.
- Coverage is write-once, at the completion that first marks a row
  Done. Later rounds cannot paraphrase settled evidence.
- Omission is not deletion. A pending row the agent neither kept,
  completed, nor retired is restored and the drop is logged.
  Deleting work requires naming it in `retired`.

Do not widen this contract back out. If the agent needs to change
something, add a typed channel for it; do not let it restate the
whole list and have Rust guess which differences were intentional.

### Prioritization Agent

Ranking is its own agent (`kres-agents/src/prioritize.rs`), split out
of the todo agent. It runs on the **slow agent** — same client, model
and token budget, derived in `Session::with_agent_runner` so the two
cannot drift apart via separate config — under the same system prompt
the session's lenses use, selected per call from `Plan.mode` via
`AgentRunner::slow_system_for_mode`. A review is `Audit`, which uses
`slow_system`, NOT `slow_coding_system`. That is not cosmetic: the
system block is part of the Anthropic cache prefix, so a mismatch
makes the shared cache block below impossible.

- Input is the ready rows only (`TaskManager::ready_pending_snapshot`):
  nothing done, running, retired, or blocked on an unfinished
  dependency. Plus the session question, the findings so far in full,
  the skills, and the plan — the same material the slow agents reason
  over.
- Output is at most N ids, best first, each with a one-line rationale
  that is logged and not otherwise consumed. N is the dispatch budget:
  `BATCH_CAP` (10) for `/continue`, 1 for `/next`, further clamped by
  the `--turns` remainder.
- It cannot edit, complete, retire, merge, or invent work. Ids not in
  the ready set are dropped with a log line; duplicates and
  over-budget picks are truncated.
- It runs at DISPATCH time, not in the reap sequence — once per wave
  rather than once per reaped task — and never under the manager's
  write lock. `ready_pending_snapshot` then `claim_selected_todos` is
  the snapshot/claim split that makes that safe; rows that stop being
  ready in between are skipped.
- When it is unavailable, fails, or returns nothing usable, dispatch
  falls back to `claim_ready_todos_with_turn_limit` in storage order.
  Ranking is an optimisation and must never stall a wave.
- A wave where every ready row fits in the budget skips the call
  entirely — there is nothing to rank.
- Its ONE cached block is the lens session head
  (`prompt::session_cache_head`, `{common_skills, previous_findings}`),
  byte-identical to what the wave's lens fan-out sends and read by it
  seconds later. Findings must be passed already redacted with
  `redact_findings_for_agent`, and skills as the common
  (task-independent) half, or the head diverges and buys an extra
  write of the largest payload in the request instead of a share.
  Route both callers through the one constructor; do not add a second.
- There is deliberately no prioritize-specific cached block. One was
  measured and removed: calls 943s and 783s apart against a 300s
  ephemeral TTL meant the entry expired every time — 21,886 tokens of
  cache creation per call against zero reads. Do not add a cached
  prefix to a call whose own cadence exceeds the TTL.

Do not put ranking language back into the todo agent prompt. Two
agents ordering one list is how the list stops being stable storage.

### Goal System
- Before processing, main agent defines a concrete completion goal
- After slow agent finishes, main agent checks if goal is met
- Goal met → suppress followups → no new todos → work stops
- Goal not met → only missing items become followups
- Auto-progress checks goal after each completed task for early exit
- Deferred items (identified but not started when goal met) saved via `/followup`

### Session Termination (`--turns`)

`--turns N` controls when the REPL exits. The counter is
`completed_run_count`, incremented in `TaskManager::finish_ok`
(kres-core task.rs) when a reaped task produced non-empty
`analysis`, `code_output`, or `code_edits`:

```rust
let produced = !entry.analysis.is_empty()
    || !entry.code_output.is_empty()
    || !entry.code_edits.is_empty();
if produced {
    g.completed_run_count = g.completed_run_count.saturating_add(1);
}
```

One completed task = one fast+slow agent cycle for a single todo
item.

**`--turns N > 0`**: stop launching new work after N completed
task runs. Once `done >= turns_limit` in the reaper loop, it
drains pending/blocked todos to `/followup` and latches the turns
cap so auto-continue cannot start more work. Already-running tasks
are allowed to finish and be reaped before root shutdown is
cancelled, because `completed_run_count` can reach the cap while
parallel tasks still have findings to merge into `findings.json`.
The cap is checked before continuation-only LLM calls: the reaper
still publishes the completed task's findings/report/state, records
any emitted followups locally, and drains them to `/followup`, but it
must not call the todo or goal agents after the cap has fired.

**`--turns 0` (default, unlimited)**: stop condition is computed
in the reaper's `else` branch (`// --turns 0 (unlimited)`):

```rust
let should_stop = if follow_followups {
    followups_drained || no_progress
} else if goal_configured {
    followups_drained
} else {
    no_goal_batch_stop
};
```

Where `followups_drained = active == 0 && pending_or_blocked == 0`,
`no_progress = no_new_findings_streak >= 3`, and
`no_goal_batch_stop = !goal_configured && !follow_followups && active == 0`.

So:
- With `--follow`: stop on drained OR 3-run stagnation streak.
- With goal agent (no `--follow`): stop **only** when drained.
  The stagnation streak is ignored.
- Without goal agent or `--follow`: stop when `active == 0`
  (batch finished), defers leftover followups.

`exit_on_idle` (set in main.rs `ReplConfig` init) controls whether
the REPL exits on stop or stays open for more input. True when
stdout is not a TTY (piped/batch) or `--one` is passed:
`args.one || !std::io::IsTerminal::is_terminal(&std::io::stdout())`.

**Goal-met drain**: when `check_goal` returns `check.met == true`,
the reaper calls `drain_pending_blocked()`, moving all
pending/blocked items to deferred. This makes
`pending_or_blocked == 0` so `followups_drained` fires on the
next reaper tick. Exception: when `--follow` and `--turns N > 0`
are both set, the goal-met handler immediately pulls deferred
items back into the todo list as Pending so auto-continue
dispatches them and the session keeps working until the turns cap
is reached.

**Implication**: under `--turns 0` with a goal agent, a todo item
stuck at `Pending` does not block termination if the goal agent
declares "met" (the drain clears it). But if the goal agent keeps
saying "not met" while a todo is stuck `Pending`, the session
cannot self-terminate — `pending_or_blocked > 0` forever and the
stagnation watchdog only fires with `--follow`.

**`--resume`**: loads a prior `session.json` (plan, todo list,
deferred list, completed_run_count). Deferred items are
automatically pulled into the todo list as Pending so
auto-continue can dispatch them immediately. When `--resume`
loads successfully, `--prompt` is ignored (with a stderr warning)
to prevent `define_goal` + `define_plan` from overwriting the
resumed plan.

**`--stdio` auto-detection**: `cfg.stdio` is set automatically
when stdout is not a TTY. This suppresses DECSTBM scroll-region
escape sequences that would otherwise pollute piped/redirected
output. The explicit `--stdio` flag still works as an override.

`--gather-turns` (default 5) is a **separate** cap: max fast↔main
gather rounds within a single task before forcing the slow agent.
Per-task, not per-session.

### Plan + Session Persistence
- `kres_core::Plan` holds the planner's decomposition: `prompt`, `goal`,
  `mode`, and `steps` (each with `id`, `title`, `status`, `todo_ids`
  linking to `TodoItem` rows). Lives on `TaskManager` as
  `Option<Plan>`; `sync_plan_from_todo` rolls up step status from
  linked todo statuses.
- `kres_core::SessionState` (`<results>/session.json`) is the
  resumable snapshot: plan + todo list + deferred list +
  `completed_run_count` + last prompt. Written atomically (tmp +
  fsync + rename) from the reaper tick and the various drain
  paths.
- Resume: `kres --results <dir> --resume` loads the snapshot,
  flips every `InProgress` todo/plan step back to `Pending` (its
  prior executor is gone), and seeds the manager + deferred list
  before the REPL starts. Without `--resume`, any existing
  session.json is left untouched on disk and the REPL starts
  clean; a note in the startup banner points at the file so the
  operator knows the state is recoverable.
- InProgress drains: ctrl-c, goal-met, and `--turns 0`
  follow-stagnation call
  `TaskManager::reset_in_progress_to_pending()` before moving items
  to the deferred list, so a task that was mid-run when the drain
  fired still ends up on `/followup` instead of being orphaned.
  The `--turns N` cap is different: it drains only Pending/Blocked
  items, blocks auto-continue, waits for InProgress tasks to finish
  and publish their outputs, then exits.
  `/stop` is separate: it moves `Pending|Blocked|InProgress` items
  to deferred directly via its own `matches!` pattern
  (kres-repl/src/session.rs), so a resumed REPL picks them up via
  `/continue`.

### Workflows

Detailed workflow documentation lives in `docs/workflow.md`. Treat
that file as the source of truth for `/fix`, `/review`, workflow
runner behavior, reaper actions, retry semantics, and shipped
workflow invariants. Keep this section short and update
`docs/workflow.md` when workflow behavior changes.

### Workspace and Mentioned Paths

- The configured workspace is implicitly readable and writable by
  kres tools.
- If the operator mentions an existing absolute file or directory
  outside the workspace in a prompt, kres grants session-scoped
  read/write access to that file's parent directory or to that
  directory itself.
- The same grant is used by read, edit, code_output, and workflow
  reaper paths. Restarting kres or `/clear` drops the grants.

### Skills
- Scanned from `~/.kres/skills/*.md` at startup; automatic skills are
  selected by workspace detection (currently kernel and systemd)
- Skill files scanned for absolute paths in backticks — referenced files pre-loaded
- Full skill content + pre-loaded files sent to code agent as `skills` field in JSON
- Code agent can request additional files via `skill_reads` in response

## Configuration

All configs live in `~/.kres/`, installed there by `setup.sh` from
this repo's `configs/` tree:

| File | Purpose |
|------|---------|
| `models/<model-id>.json` | Model/role config selected by `settings.json` or CLI model flags. Uses `api_key`, provider fields, max_tokens, rate_limit, thinking, and optional role sections (`fast`, `slow`, `main`, `todo`). |
| `mcp.json` | MCP server definitions (installed only when semcode-mcp is available) |
| `settings.json` | Per-user defaults for per-role model ids. Optional `models.slow_secondary` adds a supplemental review model (`general` for `/review`, `maintainer` for `/fix`). CLI flags `--fast-model`, `--slow-model`, `--main-model`, and `--todo-model` override matching roles. Any explicit `--slow` selection replaces the configured slow pair; repeat or comma-separate it to select multiple models. Without `--compare`, the first model runs all lenses and later models run only the supplemental lens. `--compare` runs every lens on every selected slow model. `--slow` and `--slow-model` are mutually exclusive. |
| `system-prompts/*.system.md` | Optional operator overrides for agent system prompts. Default prompts are embedded in the kres binary (`kres-agents/src/embedded_prompts.rs`); a file at `~/.kres/system-prompts/<basename>` shadows the embedded copy. Empty by default |
| `commands/<name>.md` | Optional operator overrides (or additions) for non-workflow slash-command templates. Summary rendering reads `summary` / `summary-markdown` templates through `kres-agents/src/user_commands.rs`; workflow-owned names (`fix`, `review`, `triage`, `validate`) are reserved and cannot be resurrected as prompt templates. |
| `workflows/<name>.json` | Optional operator overrides for shipped workflows such as `fix`, `review`, `triage`, and `validate`. Disk overrides shadow embedded workflow JSON. |
| `skills/*.md` | Domain knowledge files |

Rate limiters are shared across agents that use the same API key string.

Agent config files may set a per-call thinking override:
`"thinking": {"type": "adaptive", "effort": "medium"}`,
`"thinking": {"type": "enabled", "budget_tokens": 32000}`, or
`"thinking": {"type": "disabled"}`. When omitted, kres uses the
model-aware default. Use this for models whose API contract requires a
specific thinking request shape instead of hardcoding private model
names in source.

`--assisted-by TEXT` controls the exact commit-message trailer value
used by the fix workflow after `Assisted-by:`. When omitted, kres derives
`kres:<slow-model-id>` from the resolved slow agent model.

## REPL Commands

| Command | Action |
|---------|--------|
| `/tasks` `/task` | Show active tasks and states |
| `/todo` | Show pending items (ready/blocked) + completed count |
| `/plan` | Show the current plan + per-step status (produced by `define_plan`) |
| `/resume [PATH]` | Load a persisted `session.json` (defaults to `<results>/session.json.prev` → live file). Overwrites in-memory state |
| `/todo --clear` | Clear all todo items |
| `/cost` | Token usage by agent role and model |
| `/summary [FILE]` | Run the existing `validate` workflow for every finding, then have the fast agent render only validated summaries through the embedded `summary` template. Output defaults to `summary.txt`; validation artifacts live under `<results>/summary-validation/`. Auto-chunks oversized render inputs and combines the partials |
| `/summary-markdown [FILE]` | Same as `/summary` but uses the `summary-markdown` template and defaults the filename to `summary.md` |
| `/review <target>` | Run the embedded `review` workflow for `<target>` — CLI equivalent of `--prompt 'review: <target>'`. The shipped workflow defines the review prompt contract and lenses; execution uses the REPL task/todo loop so followups become next-turn review todos. This is workflow-only; no markdown prompt fallback exists |
| `/triage <finding-dir>` | Run the embedded `triage` workflow for a kres-exported finding directory. The workflow includes the golden triage template, preserves followups, and validates that `summary.md` was actually written. This is workflow-only; no alternate prompt path exists |
| `/validate <finding-dir> [source-workspace]` | Run the embedded `validate` workflow for a kres-exported finding directory against source workspace (default `.`). It validates finding claims with the fast coding agent, verifies reachability/non-latency with the slow coding agent, and writes `summary.md` plus severity updates like `/triage`. This is workflow-only; no alternate prompt path exists |
| `/fix <target>` | Run the embedded `fix` workflow for `<target>` — CLI equivalent of `--prompt 'fix: <target>'`. `fix` is workflow-only; no slash-command template or alternate prompt path is used. Drives the research → write-patch → write-commit-message → commit → build → triage/review → publish pipeline (see [docs/workflow.md](docs/workflow.md)) |
| `/report <file>` | Write all findings to markdown file |
| `/followup` | Show deferred items (identified but skipped when goal met) |
| `/next` | Run next todo item |
| `/continue` | Resume interrupted work or continue todo processing |
| `/done N` | Remove todo item N |
| `/reply <text>` | Prepend last response to new prompt |
| `/load <file>` | Inline file contents into prompt |
| `/edit` | Open $EDITOR for prompt (also ctrl-g) |
| `/clear` | Reset all state |
| `/quit` `/exit` | Exit |

## Gotchas

### JSON Parsing
- Code agent sometimes outputs prose before JSON — `_extract_json()` uses brace-matching fallback
- `parse_code_response()` tries: whole text → fenced blocks → brace matching
- Never replace text with fenced content unless it parses as valid JSON with `analysis` key

### Tool Field Names
- Main agent sends `"path"` but tool handler expects `"file"` — accept both
- Main agent sends `"startLine"`/`"endLine"` — accept alongside `"line"`/`"count"`
- All values coerced to int with try/except for robustness

### Rate Limiting
- Shared `RateLimiter` when agents use same API key (same workspace limit)
- On 429: count tokens to distinguish provider/model input capability from
  shared rate limiting. Never shrink or delete request content; partition only
  naturally partitionable inputs and preserve every byte.
- 8 retries with exponential backoff, retry-after header support

### Token Management
- Request construction never trims, caps, or deletes inference input.
- Cheap estimates and exact `count_tokens` calls are diagnostics or choose
  lossless partition boundaries for naturally partitionable inputs.
- `max_input_tokens` describes a provider/model capability. It is not a Kres
  request ceiling and does not authorize shortening a request.

### Thread Safety
- `todo_lock` on TaskManager protects todo_list mutations
- `cache_lock` protects symbol/context cache
- `_print_lock` in `async_print` prevents output interleaving
- Task state changes via `set_state` use per-task `state_lock`
- MCP `call_tools_bulk` pipelines requests but collects responses by ID (out-of-order safe)

### Terminal and Ctrl-C
- TUI mode enables crossterm raw mode, which clears terminal-driver
  `ISIG`; while raw mode is active, Ctrl-C is a key event, not a
  kernel-generated SIGINT.
- The TUI Ctrl-C handler sends kres itself `SIGINT` so the shared
  Tokio Ctrl-C path can cancel/persist the session.
- Any hard-exit path that calls `std::process::exit` must first call
  `tui::emergency_restore_terminal()` and `status::restore()`.
  `process::exit` skips `Drop`, so relying on `TuiGuard` there leaves
  the controlling terminal in raw mode and makes Ctrl-C stop working in
  the parent shell.

### Git Commands
- Readonly whitelist: log, show, diff, blame, annotate, etc.
- Uses `shlex.split()` for proper quote handling
- Unknown subcommands rejected with error listing allowed ones

## File Layout

```
~/.kres/                      # Populated by setup.sh
  models/
    claude-sonnet-4-6.json    # Default fast/main/todo model config
    claude-opus-4-7.json      # Default slow model config
  mcp.json                    # MCP server registry (semcode, …)
  settings.json               # Per-user defaults (model ids per role)
  system-prompts/             # Optional agent system prompt overrides
  commands/                   # Optional non-workflow command overrides
  workflows/                  # Optional workflow JSON overrides
  skills/                     # Skill files (kernel.md, …)
  sessions/<ts>/              # Per-run artifacts when --results not set
    findings.json             # jsondb-backed canonical findings (delta-applied, no history)
    report.md                 # Append-only narrative
    session.json              # Plan + todo + deferred + counters (resume state)
    summary.txt               # Output of /summary or kres --summary (summary.md with --summary-markdown)

.kres/logs/<session-uuid>/    # Next to cwd, one dir per REPL run
  code.jsonl                  # All fast + slow agent turns
  main.jsonl                  # All main agent turns
```

Note: `~/.kres/sessions/` holds per-run artifacts (findings.json,
report.md, session.json) but NOT the JSONL logs. The JSONL logs
live only in `<cwd>/.kres/logs/<uuid>/`, created by `TurnLogger::new`
(kres-core log.rs). The uuid is derived from pid + timestamp via
uuid5 so parallel kres processes don't collide.

## Reading JSONL Log Files

Both `code.jsonl` and `main.jsonl` are newline-delimited JSON. Each
line is a `LogEntry` with an RFC 3339 UTC `timestamp`, `role`, `content`,
and optionally `usage` (token counts) and `thinking` (slow agent reasoning).
Subtract matching user and assistant timestamps to measure a model call;
overlapping intervals show concurrent calls.

### code.jsonl

Alternating user/assistant records for the fast+slow agent pipeline.

**User records** (`role: "user"`): the `content` field is a JSON
string containing the newest prompt turn assembled by the pipeline. Multi-turn
fast-gather calls also carry `request_content`, an ordered JSON representation of
the complete model-visible conversation; `context_stats` accounts for that complete
request. Key prompt fields:

| Field | Description |
|-------|-------------|
| `question` | The task prompt (e.g. `COMPILE TRIAGE ONLY ...` or the original user prompt) |
| `plan` | Current plan with `steps[]`, each having `title` and `status` (`pending`/`done`/`skipped`) |
| `skills` | Loaded skill file contents |
| `symbols` | Source code gathered by the main agent |
| `context` | Additional context (prior analysis, tool results) |

**Assistant records** (`role: "assistant"`): `content` is either
structured JSON (fast agent) or raw prose (slow agent).

Fast agent JSON — keys:

| Field | Description |
|-------|-------------|
| `analysis` | Free-text narrative of what the agent found/decided |
| `followups` | Array of `{type, name, reason}` — data requests or actions. Types: `read`, `source`, `git`, `make`, `search`, `publish-fix`, `bash`, `callers`, `question` |
| `ready_for_slow` | `true` = fast agent is done gathering, hand off to slow agent |
| `skill_reads` | Additional skill files to load |
| `code_edits` | Array of `{path, old_string, new_string}` surgical edits (coding mode) |
| `code_output` | Array of `{path, content}` file writes (coding mode) |

Slow agent raw text — not valid JSON. This is the deep analysis
or review output. Starts with `[INVALID]` when the bug is
determined to be not real. In review steps, may contain `DEFECT`
markers.

### main.jsonl

Alternating user/assistant records for the main agent, todo agent,
and goal agent.

**Main agent assistant responses**: either `<actions>[...]</actions>`
XML containing data-fetch requests (`read`, `source`, `git`, `grep`,
`mcp`, `make`, `bash`), or JSON with `goal`/`mode` (initial goal
definition) or `todo` (todo-list updates).

**Todo agent responses** — JSON edits against the Rust-owned list.
Logs from before this contract carry the full list under `todo`
instead; both shapes parse.

| Field | Description |
|-------|-------------|
| `todo` | Pending rows only, order-insensitive. An unchanged row is just `{"id":"..."}`; `name`/`reason`/`type` appear only when being edited, and an absent field means unchanged. A row absent from `current_todo` is new and carries `name` (required) plus `type`, `reason`, `depends_on`, `step_id` |
| `newly_done` | `[{id, coverage}]` — completions. `coverage` is written once, at this completion |
| `retired` | `[{id, reason}]` — pending rows deliberately abandoned. Logged, not stored |
| `plan` | Optional `{steps:[...]}` rewrite |

A pending row that appears in `current_todo` but in none of the three
arrays is restored by `reconcile_update` and logged as "dropped … live
item(s) without retiring them".

**Plain text responses** from the main agent (e.g. `"done"`,
`"compile clean — ..."`) appear between action rounds when the
agent reports results or concludes a service cycle.

### Tracing a session through logs

To understand what a session did:

1. Scan `code.jsonl` user records for the `plan.steps[].status`
   progression — this shows which steps completed vs stuck.
2. Scan `code.jsonl` assistant records: JSON = fast agent
   (look at `analysis` + `followups`), raw text = slow agent
   (look for `[INVALID]`, `DEFECT`, or review verdicts).
3. Check `main.jsonl` for `todo` entries to see todo item status
   transitions — this is where `compile-triage: pending → done`
   (or not) gets recorded.
4. Token usage is on every assistant record in the `usage` field:
   `{input, output, cache_creation, cache_read}`.
