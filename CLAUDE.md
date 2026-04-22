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
| `/todo --clear` | Clear all todo items |
| `/cost` | Token usage by agent role and model |
| `/summary [FILE]` | Fast agent renders the run's report.md + findings.json into a bug report via the embedded `summary` slash-command template. Output defaults to `bug-report.txt` in the results dir |
| `/summary-markdown [FILE]` | Same as `/summary` but uses the `summary-markdown` template and defaults the filename to `bug-report.md` |
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
    findings.json             # Cumulative findings (history in findings-N.json)
    report.md                 # Append-only narrative
    bug-report.txt            # Output of /summary or kres --summary

.kres/logs/<session-uuid>/    # Next to cwd, one dir per REPL run
  code.jsonl                  # All fast + slow agent turns
  main.jsonl                  # All main agent turns
```
