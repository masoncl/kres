# kres

Kernel code RESearch agent — an LLM-driven multi-agent REPL for
reviewing, auditing, and finding bugs in C source trees (the kernel
is the primary target).

# NEWS

April 22: Agent system prompts and slash-command templates are
now embedded in the kres binary — rebuilding kres refreshes
them. `setup.sh` no longer copies `*.system.md`, `bug-summary*.md`,
or `review-template.md` anywhere. Two new override directories:

- `~/.kres/system-prompts/<name>.system.md` — operator override
  for an agent system prompt.
- `~/.kres/commands/<name>.md` — operator override for a slash-
  command template (`/review`, `/summary`, `/summary-markdown`),
  and the same lookup that backs `--prompt "word: extra"` and
  the new `--prompt "/word extra"` form.

Stale files left under `~/.kres/prompts/` from earlier installs
are ignored and safe to delete. See "System prompts" and
"Slash-command templates" below.

April 21: There's new support for writing patches, more details below.

## Quick start

1. Build:
   ```
   cargo build --release
   ```

2. Populate `~/.kres/` from this repo's shipped configs by running
   `setup.sh`:
   ```
   ./setup.sh --fast-key $FAST_API_KEY --slow-key $SLOW_API_KEY
   ```
   Each `--fast-key` / `--slow-key` argument accepts either a literal
   API key string or a path to an existing key file (contents trimmed
   and used verbatim). Running `setup.sh --help` lists the full set
   of options.

   The script copies `configs/*.json`, `configs/prompts/`, and
   `skills/` into `~/.kres/`, substitutes `@FAST_KEY@` / `@SLOW_KEY@`
   placeholders in the installed agent configs with the keys you
   passed, and installs `mcp.json` only when `semcode-mcp` is found
   on your `PATH` (or you pass `--semcode PATH`). It also installs
   the kernel skill if it can find a review-prompts tree — pass
   `--review-prompts /path/to/review-prompts` if you want that on
   from the start.

   Model selection lives in `~/.kres/settings.json`, one key per agent
   role (`fast`, `slow`, `main`, `todo`). `setup.sh` writes that file
   from its own flags:
     - `--slow MODEL` sets the slow-agent model (default
       `claude-opus-4-7`).
     - `--model MODEL` sets the fast / main / todo model (default
       `claude-sonnet-4-6`).
   The shipped agent configs do not hardcode a model; `settings.json`
   is the single source of truth. An operator who adds `"model": …`
   back to a specific agent config will override `settings.json` for
   just that agent (see the precedence note below).

   Running `--slow` and `--model` against the same model id is fine
   and often what you want if you only have one model's credentials.
   The difference between "fast" and "slow" work is driven by the
   per-agent system prompts shipped under `configs/prompts/` and the
   amount of context each agent receives, not by the model choice —
   so pointing both at the same id still produces the full
   fast/main/slow pipeline, each agent thinking as hard or as lightly
   as its prompt asks. Using two different models is an optimisation
   for cost or latency, not a correctness requirement.

   `--overwrite` is required to replace any file that already exists
   under `~/.kres/`; without it `setup.sh` is idempotent and reports
   each skipped file.

3. Run a review from a kernel tree:
   ```
   cd linux
   kres --results review --prompt 'review: fs/btrfs/ctree.c' --turns 2
   ```

The `--prompt 'review: fs/btrfs/ctree.c'` form is a two-part
prompt: the token `review` names the slash-command template
embedded in the kres binary (source:
`configs/prompts/review-template.md`), and the rest of the
string is the specific target. kres splices the target onto the
front of the template body to produce a full prompt covering
object lifetime, memory safety, bounds checks, races, and
general bugs in the named code.

Two equivalent forms — pick whichever reads better:

```
kres --prompt 'review: fs/btrfs/ctree.c'
kres --prompt '/review fs/btrfs/ctree.c'
```

Both resolve via `kres_agents::user_commands::lookup("review")`,
which prefers `~/.kres/commands/review.md` on disk (the operator
override path) and falls back to the embedded copy. Drop a file
at `~/.kres/commands/<name>.md` to add a new command; use the
same `--prompt "name: extra"` or `--prompt "/name extra"` form
to invoke it.

Legacy compatibility: `--prompt "word: extra"` still falls back
to `~/.kres/prompts/<word>-template.md` when no matching
`~/.kres/commands/<word>.md` exists and the name isn't one of
the embedded commands — operators with custom `<word>-template.md`
files from before the refactor keep working.

Note: the template is invoked because the prompt starts with
`review:` (or `/review`). A bare `review` or `review ` without
the separator is submitted verbatim.

### Parallel lenses inside `review-template.md`

The shipped template is more than a prose prompt — each of its
markdown todo bullets is a **lens**:

```
- [ ] **[investigate]** object lifetime: #lifetime
- [ ] **[investigate]** memory allocations: #memory
- [ ] **[investigate]** bounds checks ... #bounds
- [ ] **[investigate]** races: #races
- [ ] **[investigate]** general: #general
```
(`configs/prompts/review-template.md`)

`kres_agents::parse_prompt_file`
(`kres-agents/src/prompt_file.rs:28-98`) turns each bullet into a
`LensSpec` (id, kind, name, reason) and installs them as
**session-wide lenses**. For every task, kres then fans out one
slow-agent call per lens over the *same* gathered symbols and
source sections — five parallel analyses in the case of the shipped
template — and runs a consolidator pass that dedupes the findings
across lenses before the merger folds them into the cumulative
list (`kres-core/src/lens.rs:1-7`).

That parallelism is what makes a single `review:` run productive:
instead of the slow agent juggling lifetime + memory + bounds +
races + general bugs in one response, each angle gets its own
focused call with the full context, and overlap between findings
is resolved at consolidation time. Indented sub-bullets under a
lens bullet fold into its `reason` field and become extra guidance
the slow agent sees on that specific lens (see the sub-bullets
under `object lifetime` and `memory allocations` in the template).

To add or remove angles for your own reviews, drop a customised
copy of the review template at `~/.kres/commands/review.md` — it
takes precedence over the embedded copy at load time. Dropping a
new `<word>.md` alongside it (e.g. `~/.kres/commands/audit.md`)
adds a `/audit` slash-command you can invoke via
`--prompt "audit: target"` or `--prompt "/audit target"`.

`--results review` tells kres where to keep the run's artifacts:
`findings.json` (plus `findings-N.json` history snapshots), the
running narrative `report.md`, and the rendered `bug-report.txt`
when `/summary` fires. Without `--results`, kres picks
`~/.kres/sessions/<timestamp>/` automatically.

## `--turns` and `--follow`: stopping the run

`--turns` controls when kres decides a non-interactive run is "done".
A "completed task" throughout this section means a unit that ran all
the way through fast → main → slow and produced a non-empty analysis
(`kres-core/src/task.rs:309-311`).

- **`--turns N` (N ≥ 1)** — stop after N completed tasks. Useful for
  a single focused question (`--turns 1`) or a time-boxed review
  (`--turns 5` etc.). The REPL exits as soon as the Nth task
  finishes, regardless of what the goal agent or the followup queue
  look like. `--follow` has no effect in this mode; the run-count
  cap wins.

- **`--turns 0` (the default)** — no run-count cap. kres trusts the
  goal agent: after every task the goal agent checks the accumulated
  analysis against the per-task goal; when it declares the goal met,
  its handler drains the todo list and the reaper exits on the next
  tick (nothing is active, nothing is pending). Until then kres
  keeps dispatching the followup tasks the goal check spawns.

  - Add `--follow` to layer a cost cap on top: if 3 consecutive
    analysis-producing runs fail to grow the findings list, exit
    even if the goal agent is still saying "not met". Use this when
    you want a hard ceiling on how long kres will keep pulling on
    threads.

  (`kres-repl/src/session.rs` — see the `turns_limit == 0` branch in
  the reaper for the exact predicates. If you run without a
  `main-agent.json`, no goal agent is wired up and kres falls back
  to "stop when the active batch finishes"; `--follow` switches that
  fallback to "drain the todo list with the 3-run stagnation cap".)

On any `--turns` exit path — run-count cap, goal-met drain, or
stagnation cap — kres

1. cancels any in-flight work,
2. runs `/summary` automatically, producing `bug-report.txt`
   (`bug-report.md` with `--markdown`) in the results directory, or
   in the current working directory when `--results` was not given,
   and
3. exits.

Remaining pending or blocked todo items are moved to the "deferred"
list; `/followup` shows them if you re-enter the REPL later, and
`/continue` will dispatch them.

## Flow of work between the agents

A task goes through three agents, all configured from
`~/.kres/`:

- **fast** (`fast-code-agent.json`): scopes the task, figures out
  what source kres needs to look at, and emits a structured brief.
  When it's ready it returns a list of "followups" — concrete fetch
  requests (grep, file read, semcode symbol/callchain, git log)
  that the main agent should run.

- **main** (`main-agent.json`): the data fetcher. It takes the
  fast agent's followups and dispatches them to local tools and to
  any MCP servers configured in `mcp.json` (semcode in particular).
  The output is funnelled back into the fast agent for another
  round. This fast↔main loop runs until the fast agent says
  `ready_for_slow`, or the `--gather-turns` cap is reached.

- **slow** (`slow-code-agent-<tag>.json`, default `sonnet`): the
  deep analyser. It receives the gathered symbols and file sections,
  the cumulative findings from earlier tasks, and the task brief,
  then produces a new analysis and any new findings. Slow-agent
  output is cheap prose plus structured findings records.

- **todo** (`todo-agent.json`): after the slow agent returns, the
  todo agent dedups its followup suggestions against the existing
  pending/done todo list and emits an updated list. This is what
  drives larger reviews — see below.

- **merger**: a non-agent fast-client call that merges the new
  task's findings into the cumulative findings list. Duplicates get
  folded; old findings that a new one supersedes get marked
  `invalidated`.

All inference happens over the Anthropic streaming API. Every
round-trip is logged to `.kres/logs/<session-uuid>/` so you can
inspect what each agent saw and replied.

## Building up a larger review

A single `--prompt 'review: fs/btrfs/ctree.c'` call seeds exactly
one task. That task's slow-agent response usually contains followup
suggestions like "investigate memory lifetime of the path argument"
or "check callers of btrfs_search_slot". The todo agent turns those
into todo items.

From there:

- **`/next`** runs the first pending todo item as its own task.
- **`/continue`** dispatches every pending todo item.
- **auto-continue**: when there are pending todos and no active
  tasks, kres launches `/continue` automatically after 5 seconds of
  idle. You can override the idle by typing anything, including
  `/stop`.

Each task feeds back into the same pipeline: fast → main → slow →
merger, plus the todo agent deduping any new followups against the
existing list. The goal agent (a special mode of the main-agent
model) periodically checks whether the original prompt has been
satisfied; if yes, work stops even if followups remain.

A full review of a substantial source file usually takes between 5
and 50 task runs, depending on how branchy the code is and how
aggressive the slow agent is about producing followup questions.
`--turns` caps that; `/quit` lets you bail out early and resume
later.

## Summary output

After each task, kres appends the slow agent's narrative to
`<results>/report.md` and rewrites `<results>/findings.json` with
the cumulative merged list (the prior turn's canonical file is
copied to `findings-N.json` first, so you have the history).

At the end of a run you get a plain-text bug report via `/summary`
(or automatically on `--turns` exit, or separately with
`kres --summary --results <dir>`). That run:

- Picks up `<results>/prompt.md` (saved on the first submit so
  subsequent `/summary` or `--summary` invocations know the original
  question), `<results>/report.md`, and `<results>/findings.json`.
- Uses the fast agent with the `summary` slash-command template
  (embedded in the kres binary; overridable at
  `~/.kres/commands/summary.md`) as a dedicated system prompt.
  `--markdown` selects the `summary-markdown` variant instead.
- Orders the resulting sections by `bug-severity` — `high` →
  `medium` → `low` → `latent` → `unknown` — with one section per
  bug, each led by `Subject:`, `bug-severity:`, and `bug-impact:`
  lines.
- Writes the result to `<results>/bug-report.txt` (or
  `bug-report.txt` in the current working directory if you did not
  pass `--results`).

You can point `--template PATH` at a custom file to override the
shipped summariser prompt without rebuilding.

## Coding tasks: reproducers and in-place fixes

Not every prompt is a review. Ask kres `--prompt 'write a
reproducer for the UAF in net/sched/cls_bpf.c'` or `--prompt 'fix
the missing frag-free in bnxt_xdp_redirect'` and the goal agent
classifies the task as **coding mode** instead of analysis. Coding
mode swaps out the review pipeline's lens fan-out and findings
consolidator for a single slow-agent call whose job is to produce
source code. Two output channels:

- **`code_output`** — a list of `{path, content, purpose}`
  records. Each entry is a full file body that the reaper writes
  under `<workspace>/code/<path>` via tmp + rename. Use this for
  fresh artifacts (reproducers, test harnesses, trigger programs,
  scratch fixes that rewrite a whole file).

- **`code_edits`** — a list of `{file_path, old_string,
  new_string, replace_all}` records, same shape as Claude Code's
  Edit primitive. The reaper applies each edit in order via
  `kres_agents::tools::edit_file`: `old_string` must appear
  exactly once in the current file contents (unless
  `replace_all: true`), and the file is rewritten atomically via
  tmp + rename (`kres-agents/src/tools.rs`). This is the
  preferred channel for surgical one-line fixes — the
  `old_string` anchor forces the slow agent to quote bytes from
  the real file rather than reconstruct them from summary-level
  descriptions. Each edit's result (replacement count for
  success, verbatim error message for failure) is folded into
  the task's analysis trailer under `Edits applied (N/M[, K
  FAILED]):` so the next slow-agent turn can see which edits
  landed and correct any that didn't.

The slow-code prompt (`configs/prompts/slow-code-agent-coding.system.md`)
enforces two rules that matter in practice: the verbatim current
contents of the file being fixed must be in the gathered symbols
or context before any edit is emitted (a `read` followup is
requested and waited on otherwise — the slow agent is explicitly
told not to fix from memory), and a multi-edit batch applies in
emission order with each `old_string` matching the file state
AFTER prior edits in the same batch have landed.

**Verification via `bash`** — the slow agent can emit a `bash`
followup (e.g. `cc -o repro repro.c && ./repro`, `make -C test`)
to build and run what it just wrote. The main agent executes it
from the workspace root, captures `[exit N]` + stdout + stderr,
and feeds the result back. This is the one flow where `bash` is
genuinely useful — but it is OFF by default (see "Action
allowlist" below) and must be explicitly enabled for the session.

On a coding run you typically invoke kres with:

```
kres --prompt 'write a reproducer for the stack OOB in x_tables' \
     --allow bash \
     --results repro-run
```

Artifacts land in `<results>/code/<path>` (for `code_output`) and
in-place under `<workspace>` (for `code_edits`). The ordinary
`report.md` + `findings.json` ledger continues to accumulate
narrative; coding tasks skip the findings-merger path since their
output is source files, not bug records.

## Action allowlist

The main agent's non-MCP tools are gated by a session-wide
allowlist. Defaults: `grep`, `find`, `read`, `git`, `edit`.
`bash` is **OFF by default** because operators report it being
reached for as a general escape hatch for things the typed tools
already cover (`bash sed` for range reads, `bash find` for
filename locates). An action whose `type` isn't in the allowlist
is rejected at dispatch time with a message naming the allowed
set and pointing at the two ways to fix it.

**Three precedence levels:**

1. `--allow ACTION` CLI flags — additive on top of whatever the
   files resolved to. Repeatable (`--allow bash --allow git`) or
   comma-separated (`--allow bash,git`). The special value
   `--allow all` enables every action type the dispatcher knows.
2. Per-project `<cwd>/.kres/settings.json` — overrides global
   values field-by-field; an explicit allowlist replaces rather
   than unions with the global one.
3. Global `~/.kres/settings.json` — the default resting place
   for a per-user policy.

**Example — enable bash for this session only:**

```
kres --allow bash --prompt 'reproduce the RDS UAF'
```

**Example — enable bash permanently in settings.json:**

```json
{
  "actions": {
    "allowed": ["grep", "find", "read", "git", "edit", "bash"]
  }
}
```

**Example — deny every non-MCP action (tight lockdown, leaves
only MCP tools available to the main agent):**

```json
{
  "actions": {
    "allowed": []
  }
}
```

The empty array is the explicit "lock it down" signal — kres
dispatcher enforces it and does not fall back to defaults.
A missing or absent `actions.allowed` (i.e. `null` or the key
unset) is different: it means "use the built-in default list".

**Typo detection** — tokens in `--allow` or `actions.allowed`
that aren't recognised action names produce a startup warning
with a closest-match suggestion (Levenshtein ≤ 2), e.g.
`settings: unknown action token 'bsah' (--allow) — did you mean
'bash'? known: grep, find, read, git, edit, bash, mcp`. Unknown
tokens are dropped rather than silently inserted, so a typo
never leaves a dead entry in the allowlist.

**Startup banner** — when a main-agent config is resolved, kres
prints the effective allowlist on startup and distinguishes
"bash disabled by default" from "bash disabled by explicit
allowlist in settings.json". Both point at `--allow bash` as the
fix but the wording respects the source of the decision.

MCP tools are gated separately (by mcp.json server registration,
not this allowlist) and don't enter the allowlist's dispatch
path. `--allow mcp` is a no-op and does not produce a typo
warning.

## Review prompts

kres can leverage the kernel review prompts for additional subsystem knowledge.
These live in a separate repo:

https://github.com/masoncl/review-prompts

The shipped kernel skill (`skills/kernel.md`) is a thin loader: it
references `@REVIEW_PROMPTS@/kernel/technical-patterns.md` as a
mandatory read on every slow-agent turn, plus
`@REVIEW_PROMPTS@/kernel/subsystem/subsystem.md` as an index into
per-subsystem guides. `setup.sh` substitutes `@REVIEW_PROMPTS@` with
an on-disk path at install time (see `skills/kernel.md:8`,
`skills/kernel.md:17`, `skills/kernel.md:29`).

Point `setup.sh` at your clone so the skill can resolve those files:

```
./setup.sh --fast-key $FAST_API_KEY --slow-key $SLOW_API_KEY \
           --review-prompts /path/to/review-prompts
```

Without a resolvable path, `setup.sh` leaves the kernel skill
uninstalled (`setup.sh:386-389`) — the agents will still run, but the
slow agent won't have the pattern catalogue or subsystem context, so
findings tend to be shallower and miss conventions that are obvious
to someone who has read the pattern files.

If a path wasn't given explicitly, `setup.sh` peeks at
`~/.claude/skills/kernel/SKILL.md` and offers the first
`review-prompts` path it finds there (`setup.sh:338-372`); pass
`--review-prompts PATH` explicitly to bypass the prompt.

## semcode

The main agent's code-navigation and seawrching can be enhanced by semcode:
server:

https://github.com/facebookexperimental/semcode

When a `semcode-mcp` binary is installed, `setup.sh` writes an
`mcp.json` that launches it as an MCP child:

```
{
  "mcpServers": {
    "semcode": { "command": "semcode-mcp" }
  }
}
```

(`configs/mcp.json`).

kres works without semcode — the main agent can already answer code
questions with `read`, `grep`, and `git` against the workspace
(`CLAUDE.md:9,16`). When semcode is available, the main agent gets a
function/type/callchain-aware index to ask instead of deriving the
same information from raw regex.

Tools semcode exposes that the main agent will call when wired up:

- Function- and type-level lookups: `find_function`, `find_type`,
  `find_callers`, `find_calls`, `find_callchain`, `grep_functions`.
- Commit- and branch-level helpers: `find_commit`,
  `compare_branches`, `diff_functions`, `list_branches`.
- Vector-indexed search: `vgrep_functions`,
  `vcommit_similar_commits`, `vlore_similar_emails`, `lore_search`.

Raw semcode symbol text is normalised back into a uniform JSON
shape by `parse_semcode_symbol` (`kres-agents/src/symbol.rs:52-59`)
before reaching the fast/slow agents.

**When it helps**: whole-program questions that read/grep can only
approximate — "who calls `btrfs_search_slot`", "what does the
definition of `struct inode` look like on this branch", "show me
every change to this function in the last 1000 commits". Without
semcode the main agent still answers those, just via more grep
round-trips with more false positives.

**Install**: either drop `semcode-mcp` on your `PATH` before running
`setup.sh` (it auto-installs `mcp.json`, `setup.sh:265-269`) or pass
`--semcode PATH/TO/semcode-mcp` explicitly (`setup.sh:41-45`).
`--semcode ""` force-skips the MCP install even when the binary is
on `PATH`. kres's `.gitignore` excludes a `/.semcode.db/` directory
at the repo root (`.gitignore:4`) — that's semcode's on-disk index
cache; consult the semcode repo for details on how it's populated
and invalidated.

## Config directory: `~/.kres/`

`kres repl` resolves every optional config path in this order:

1. explicit CLI flag (e.g. `--fast-agent /path/to/fast.json`)
2. same filename under `~/.kres/`

Default filenames looked up in `~/.kres/`:

| Flag              | Default under `~/.kres/`         |
|-------------------|----------------------------------|
| `--fast-agent`    | `fast-code-agent.json`           |
| `--slow` tag      | `slow-code-agent-<tag>.json`     |
| `--main-agent`    | `main-agent.json`                |
| `--todo-agent`    | `todo-agent.json`                |
| `--mcp-config`    | `mcp.json`                       |
| `--skills`        | `skills/`                        |
| `--findings`      | `findings.json`                  |

A missing file in `~/.kres/` is not an error — the "not configured"
branch fires as if the flag were absent.

The `history` file is always written to `~/.kres/history` regardless
of other flags; it holds readline line-edit history.

`~/.kres/settings.json` carries per-user default model ids per agent
role. `setup.sh --slow MODEL` / `--model MODEL` populate the slow
slot and the fast / main / todo slots respectively; default values
are `claude-opus-4-7` (slow) and `claude-sonnet-4-6` (the rest).

Model-id precedence at runtime (see
`kres-repl/src/settings.rs::pick_model`):
  1. The agent config's explicit `"model"` field when present.
  2. The matching `settings.models.<role>` string in
     `~/.kres/settings.json`.
  3. `Model::sonnet_4_6()` — the built-in fallback when both of the
     above are absent.

The shipped agent configs no longer set `"model"`, so in a fresh
install step 2 drives the actual choice. Reintroducing a `"model"`
line in one of the agent configs still takes effect and overrides
settings.json for that agent only.

## System prompts

Agent `*.system.md` prompts (fast / slow / slow-coding /
slow-generic / main / todo) are compiled into the kres binary
via `include_str!` (see `kres-agents/src/embedded_prompts.rs`).
`setup.sh` does NOT install them on disk. Rebuilding kres
refreshes them.

The shipped agent configs under `configs/*.json` reference
`system_file: "system-prompts/<name>.system.md"`; the path is
resolved relative to the config file's directory, so at runtime
it becomes `~/.kres/system-prompts/<name>.system.md`.

Load order used by `AgentConfig::load`:

1. **Disk override**: `~/.kres/system-prompts/<basename>`. If
   this file exists and is non-empty it is used verbatim.
2. **Embedded**: the compiled-in copy keyed by basename.
3. **Error**: neither present → config load fails with a
   message that names both paths.

To customize an agent prompt for your own install, drop the
edited file at `~/.kres/system-prompts/<basename>`. The default
install has no files there; the embedded copies do all the work.

Slash-command templates (`/review`, `/summary`,
`/summary-markdown`) live in a separate module
(`kres-agents/src/user_commands.rs`) with their own override
directory at `~/.kres/commands/` — see the next section.

Why distinct directories? Older installs populated
`~/.kres/prompts/` directly from setup.sh (both `*.system.md`
and `bug-summary*.md`). Keeping the override in the same
directory would mean those leftover files shadow the embedded
defaults and produce stale behaviour after an upgrade. Two
fresh directory names sidestep that — a fresh kres reads only
the embedded defaults until the operator deliberately drops a
file under the new paths. Stale files under `~/.kres/prompts/`
are safe to delete (the slash-command loader still reads
`<word>-template.md` from there as a back-compat fallback, but
will never find a filename matching one of the shipped embedded
commands there since setup.sh never writes those names to
`prompts/`).

## Slash-command templates

`review` / `summary` / `summary-markdown` are embedded
slash-command templates. Each has an `.md` body bundled in the
kres binary via `kres_agents::user_commands`, and an operator
can override or add commands by dropping a file at
`~/.kres/commands/<name>.md`. Invocation paths:

| Where | How to invoke `review` on `fs/btrfs/ctree.c` |
|-------|---------------------------------------------|
| CLI   | `kres --prompt 'review: fs/btrfs/ctree.c'` |
| CLI   | `kres --prompt '/review fs/btrfs/ctree.c'` (equivalent) |
| REPL  | `/summary` — synthesises the accumulated run into `bug-report.txt` |

The shipped three:

- `review` — the parallel-lens review template (see the
  "Parallel lenses" section above). Invocation prepends the
  operator's target to the template body.
- `summary` — the plain-text bug-report system prompt that
  `/summary` and `kres --summary` pass to the fast agent.
- `summary-markdown` — the markdown-output variant selected by
  `--markdown`.

Adding your own: drop `~/.kres/commands/audit.md` and run
`kres --prompt 'audit: net/...'` or `kres --prompt '/audit
net/...'`. No rebuild needed — the disk override path is
consulted on every invocation.

Load order (identical for every command):

1. `~/.kres/commands/<name>.md` on disk (operator override).
2. Embedded body in `kres_agents::user_commands` (for the three
   shipped commands).
3. Fallback to the legacy `~/.kres/prompts/<name>-template.md`
   lookup when neither of the above hit — preserves existing
   custom templates from before this refactor.
4. Nothing matched → treat `"name: extra"` as a verbatim prompt.

Files that setup.sh still copies to `~/.kres/prompts/`: any
operator-authored `<word>-template.md` the user drops into
`configs/prompts/` that isn't shadowed by an embedded command
of the same root name. The shipped `review-template.md`,
`bug-summary.md`, and `bug-summary-markdown.md` are NOT copied
(they're embedded); `configs/prompts/<word>-template.md` for
any other `<word>` is copied verbatim so custom templates from
before the refactor keep working via the legacy
`~/.kres/prompts/<word>-template.md` fallback path.

## Workspace layout

```
kres/
├── Cargo.toml                     Rust workspace manifest
├── kres/                           binary crate (`kres` command)
├── kres-core/                      Task, TaskManager, shutdown, findings
├── kres-llm/                       Anthropic streaming client + rate limiter
├── kres-mcp/                       stdio JSON-RPC client for MCP servers
├── kres-agents/                    fast / slow / main / todo / consolidator / merger
├── kres-repl/                      readline UI, commands, signal handling
├── configs/                        per-agent JSON configs (shipped defaults)
│   ├── fast-code-agent.json
│   ├── slow-code-agent-opus.json
│   ├── slow-code-agent-sonnet.json
│   ├── main-agent.json
│   ├── todo-agent.json
│   ├── settings.json
│   ├── mcp.json
│   └── prompts/                   system prompts + review templates
├── skills/                         domain-knowledge markdown fed to agents
│   └── kernel.md
├── docs/                           JSON-schema docs for agent wire formats
│   ├── findings-json-format.md
│   ├── prompt-json-format.md
│   └── response-json-format.md
├── CLAUDE.md                       project instructions for Claude Code
├── setup.sh                        bootstrap ~/.kres/ from configs/
├── .githooks/pre-commit            runs cargo fmt + clippy on every commit
└── README.md
```

Build: `cargo build --release`
Test: `cargo test --workspace`
Lint: `cargo clippy --workspace --all-targets -- -D warnings`
Format check: `cargo fmt --all --check`

## Pre-commit hook

`.githooks/pre-commit` runs `cargo fmt --check` + `cargo clippy -D
warnings` on every commit. Enable it per-clone with:

```
git config core.hooksPath .githooks
```

## Supported CLI

```
kres test <key_file> [--prompt ...] [--model ...]
kres turn <key_file> -o <output.md> [-i <input.json>] [other flags]
kres [--fast-agent ...] [--slow TAG | --slow-agent ...] [--main-agent ...]
     [--todo-agent ...] [--mcp-config ...] [--skills DIR]
     [--results DIR] [--findings PATH] [--report PATH] [--todo PATH]
     [--prompt PROMPT] [--template PATH] [--turns N]
     [--gather-turns N] [--stop-grace-ms MS] [--stdio]
     [--allow ACTION]... [--summary]
```

Interactive REPL commands: `/help`, `/tasks`, `/findings`, `/stop`,
`/clear`, `/cost`, `/todo`, `/summary [FILE]`, `/report <path>`,
`/load <path>`, `/edit`, `/reply <text>`, `/next`, `/continue`,
`/quit`.
