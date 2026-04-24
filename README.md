# kres

Kernel code RESearch agent — an LLM-driven multi-agent REPL for
reviewing, auditing, and finding bugs in C source trees. The
Linux kernel is the primary target; any large C codebase with
source-level tooling works too.

## kres introduction

kres splits the job of reviewing code across a number of cooperating agents:

- **fast** scopes the work, picks the code to look at, and emits
  a structured brief for deeper analysis.
- **main** fetches that code via MCP tools, grep, read, git —
  treating code navigation as a first-class tool-call surface
  rather than free-form text manipulation.
- **slow** runs the deep analysis with a prepared context and
  previous findings in hand, so the expensive model's tokens go
  to bug-hunting rather than chasing files.
- **todo** dedups follow-up questions, reprioritises, and keeps a
  running list across turns so a single prompt can drive 30+
  tasks without losing coverage.
- **merger** folds each task's findings into a cumulative,
  deduplicated bug list; old findings get `invalidated` when a
  later one supersedes them.

The results of every turn are used to reprioritize the todo list, and identify
additional context needed for the next round.

See [docs/agents.md](docs/agents.md) for the task flow and
[docs/review-template.md](docs/review-template.md) for the
parallel-lens review.

## Quick start

1. **Build**:

   ```
   cargo build --release
   ```

2. **Populate `~/.kres/`** from shipped configs:

   ```
   ./setup.sh --fast-key $FAST_API_KEY --slow-key $SLOW_API_KEY
   ```

   Each key arg accepts a literal API key or a path to a key
   file. `setup.sh --help` lists every option — model picks
   (`--slow`, `--model`), `--semcode PATH`,
   `--review-prompts PATH`, `--overwrite`, and more. The shipped
   defaults use `claude-opus-4-7` for the slow agent and
   `claude-sonnet-4-6` for the fast / main / todo roles;
   `~/.kres/settings.json` is the single source of truth for
   model selection.

3. **Run a review** from a kernel tree:

   ```
   cd linux
   kres --results review --prompt 'review: fs/btrfs/ctree.c' --turns 2
   kres --summary-markdown --results review
   # review/summary.md now has your results
   ```

   `--prompt 'review: X'` invokes the embedded review template —
   a five-lens parallel audit over the target. `--results DIR`
   keeps the run's artifacts under `DIR/` (findings.json,
   report.md, summary.txt). `--turns 2` stops after two
   completed tasks; see
   [docs/turns-and-follow.md](docs/turns-and-follow.md) for the
   other stop modes.

Two optional integrations are worth wiring up while you're
here: semcode-mcp for whole-program code navigation and the
kernel `review-prompts` repo for subsystem knowledge. Both are
configured via `setup.sh` flags — see
[docs/configuration.md](docs/configuration.md) for details.

## Exporting findings

You can export results into either text or markdown:

- [docs/summary.md](docs/summary.md) — `/summary`,
  `kres --summary`, and the summary output format.

But these scans can produce a lot of results, and churning through a giant
text file isn't the easiest way to walk them.  You can also dump them into
a one-dir-per-finding format:

- [docs/exporting.md](docs/exporting.md) — `kres --export DIR`:

## Further reading

- [docs/agents.md](docs/agents.md) — fast / main / slow / todo /
  merger flow and how follow-up tasks drive larger reviews.
- [docs/review-template.md](docs/review-template.md) — the
  parallel-lens review flow behind `--prompt "review:"`.
- [docs/coding-tasks.md](docs/coding-tasks.md) — reproducer and
  fix generation (`code_output`, `code_edits`, `bash` verify).
- [docs/turns-and-follow.md](docs/turns-and-follow.md) — when
  kres decides a non-interactive run is done.
- [docs/action-allowlist.md](docs/action-allowlist.md) — which
  non-MCP tools the main agent can dispatch and how to change
  that.
- [docs/configuration.md](docs/configuration.md) — `~/.kres/`
  layout, model selection, system-prompt overrides, semcode MCP
  integration, and kernel review-prompts setup.
- [docs/commands.md](docs/commands.md) — slash-command templates
  (`/review`, `/summary`, operator-authored additions).
- [docs/cli.md](docs/cli.md) — every CLI flag and REPL command.
- [docs/development.md](docs/development.md) — workspace layout,
  build / test / lint, pre-commit hook.
