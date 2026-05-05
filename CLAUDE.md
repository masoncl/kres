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

### Async REPL
- Input runs in a separate thread (readline → queue)
- Main loop: 100ms poll cycle checking input queue + servicing tasks
- `async_print()` clears readline line before printing to avoid garbled output
- All background output (task status, results) uses `async_print`

### Task System
- Each todo item becomes a `Task` with its own thread
- Task states: `pending → inference → waiting_main → gathering → done`
- `TaskManager` handles scheduling (respects `depends_on`), servicing, reaping
- Max parallel tasks configurable via `"concurrency"` in main-agent.json

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

**`--turns N > 0`**: stop after N completed task runs. Once
`done >= turns_limit` in the reaper loop, it drains
pending/blocked todos to `/followup` and cancels root shutdown.

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
- InProgress drains: ctrl-c, the `--turns N` cap, goal-met, and
  `--turns 0` follow-stagnation all call
  `TaskManager::reset_in_progress_to_pending()` before moving items
  to the deferred list, so a task that was mid-run when the drain
  fired still ends up on `/followup` instead of being orphaned.
  `/stop` is separate: it moves `Pending|Blocked|InProgress` items
  to deferred directly via its own `matches!` pattern
  (kres-repl/src/session.rs), so a resumed REPL picks them up via
  `/continue`.

### Skills
- Loaded from `~/.kres/skills/*.md` at startup
- Skill files scanned for absolute paths in backticks — referenced files pre-loaded
- Full skill content + pre-loaded files sent to code agent as `skills` field in JSON
- Code agent can request additional files via `skill_reads` in response

## Configuration

All configs live in `~/.kres/`, installed there by `setup.sh` from
this repo's `configs/` tree:

| File | Purpose |
|------|---------|
| `fast-code-agent.json` | Fast agent: key, max_tokens, rate_limit, system prompt (model id lives in `settings.json`) |
| `slow-code-agent-<tag>.json` | Slow agent variants; `--slow <tag>` picks one (default: sonnet). Tags differ by `max_tokens`. Known tags (sonnet/opus) also imply a slow model id, overriding `settings.json` unless `--slow-model` is also passed |
| `main-agent.json` | Main agent: key, max_tokens, rate_limit, concurrency, system prompt (model id lives in `settings.json`) |
| `todo-agent.json` | Todo-list-maintenance agent (tools-disabled variant) |
| `mcp.json` | MCP server definitions (installed only when semcode-mcp is available) |
| `settings.json` | Per-user defaults (today: per-role model ids). CLI flags `--fast-model`, `--slow-model`, `--main-model`, `--todo-model` override the matching role; a known `--slow <tag>` (sonnet/opus) also overrides the slow model id unless `--slow-model` is given |
| `system-prompts/*.system.md` | Optional operator overrides for agent system prompts. Default prompts are embedded in the kres binary (`kres-agents/src/embedded_prompts.rs`); a file at `~/.kres/system-prompts/<basename>` shadows the embedded copy. Empty by default |
| `commands/<name>.md` | Optional operator overrides (or additions) for slash-command templates. Shipped commands `review`, `summary`, `summary-markdown` are embedded in the kres binary (`kres-agents/src/user_commands.rs`). A file at `~/.kres/commands/<name>.md` shadows the embedded copy; adding a new `<name>.md` creates a `/name` command invocable via `--prompt "name: extra"` or `--prompt "/name extra"`. Empty by default |
| `skills/*.md` | Domain knowledge files |

Rate limiters are shared across agents that use the same API key string.

## REPL Commands

| Command | Action |
|---------|--------|
| `/tasks` `/task` | Show active tasks and states |
| `/todo` | Show pending items (ready/blocked) + completed count |
| `/plan` | Show the current plan + per-step status (produced by `define_plan`) |
| `/resume [PATH]` | Load a persisted `session.json` (defaults to `<results>/session.json.prev` → live file). Overwrites in-memory state |
| `/todo --clear` | Clear all todo items |
| `/cost` | Token usage by agent role and model |
| `/summary [FILE]` | Fast agent renders the run's report.md + findings.json into a summary via the embedded `summary` slash-command template. Output defaults to `summary.txt` in the results dir. Auto-chunks findings when the prompt exceeds the fast agent's `max_input_tokens` and runs a combine pass to merge the partials |
| `/summary-markdown [FILE]` | Same as `/summary` but uses the `summary-markdown` template and defaults the filename to `summary.md` |
| `/review <target>` | Compose the embedded `review` slash-command template with `<target>` and submit as a new task — CLI equivalent of `--prompt 'review: <target>'` |
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
- On 429: count_tokens for exact size, auto-shrink if over max_input_tokens, retry
- `_shrink_messages` removes largest symbols/context first
- 8 retries with exponential backoff, retry-after header support

### Token Management
- `fit_payload` checks payload size before sending to slow agent
- Cheap estimate first (chars/4), exact count via `count_tokens` API if close to limit
- `max_input_tokens` config (default 900K) caps payload size

### Thread Safety
- `todo_lock` on TaskManager protects todo_list mutations
- `cache_lock` protects symbol/context cache
- `_print_lock` in `async_print` prevents output interleaving
- Task state changes via `set_state` use per-task `state_lock`
- MCP `call_tools_bulk` pipelines requests but collects responses by ID (out-of-order safe)

### Git Commands
- Readonly whitelist: log, show, diff, blame, annotate, etc.
- Uses `shlex.split()` for proper quote handling
- Unknown subcommands rejected with error listing allowed ones

## File Layout

```
~/.kres/                      # Populated by setup.sh
  fast-code-agent.json        # Fast agent config (with inline API key)
  slow-code-agent-sonnet.json # Default slow agent
  slow-code-agent-opus.json   # Alternative slow agent (--slow opus)
  main-agent.json             # Main agent config
  todo-agent.json             # Todo-list-maintenance agent config
  mcp.json                    # MCP server registry (semcode, …)
  settings.json               # Per-user defaults (model ids per role)
  prompts/                    # System prompts + bug-summary.md
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
line is a `LogEntry` with fields: `role`, `content`, and optionally
`usage` (token counts) and `thinking` (slow agent reasoning).

### code.jsonl

Alternating user/assistant records for the fast+slow agent pipeline.

**User records** (`role: "user"`): the `content` field is a JSON
string containing the prompt assembled by the pipeline. Key fields:

| Field | Description |
|-------|-------------|
| `question` | The task prompt (e.g. `COMPILE TRIAGE ONLY ...` or the original user prompt) |
| `plan` | Current plan with `steps[]`, each having `title` and `status` (`pending`/`done`/`skipped`) |
| `skills` | Loaded skill file contents |
| `symbols` | Source code gathered by the main agent |
| `context` | Additional context (prior analysis, tool results) |
| `previously_fetched` | Manifest of data gathered in earlier rounds |

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

**Todo agent responses** — JSON with a `todo` key containing the
full todo list. Each item has:

| Field | Description |
|-------|-------------|
| `id` | Stable identifier (e.g. `research-done`, `compile-verify`) |
| `name` | Human-readable description |
| `status` | `pending`, `done`, `blocked`, `skipped` |
| `reason` | Why the item was created or completed |
| `depends_on` | List of item ids that must complete first |

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
   transitions — this is where `compile-verify: pending → done`
   (or not) gets recorded.
4. Token usage is on every assistant record in the `usage` field:
   `{input, output, cache_creation, cache_read}`.
