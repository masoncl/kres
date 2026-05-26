---
name: kres
description: Navigate a kres export tree of Linux-kernel bug findings. Covers per-finding layout (metadata.yaml, FINDING.md, summary.md, fix-state diff files), the findings-index.py search syntax, and the kres runtime directories (~/.kres config, <cwd>/.kres logs). Load when (a) a directory contains `findings/`, `INDEX.md`, and `findings-index.py`; (b) the cwd matches `*/kres/linux.<tag>` or `*/working/kres/linux.<tag>` — these are linux worktrees mirroring a finding at `~/local/kernel-bugs/findings/<tag>/`; or (c) the user mentions a kres finding, the kernel-bugs tree, or asks to reproduce / verify / debug / read a finding.
invocation_policy: manual
---

# kres findings tree

A kres export tree records the output of one or more kernel-bug audit
runs as a browsable directory of findings. When you encounter such a
tree (i.e. you see a `findings/` subdirectory next to an `INDEX.md`,
`index.html`, and `findings-index.py`), **read the tree's `README.md`
first**. The README documents the local conventions — column order
of `INDEX.md`, what subset indexes are produced, what `index-config.yaml`
contains, and any operator overrides. Everything below is the
common shape the README expands on.

## Tree layout

```
<export-root>/
├── README.md              local conventions, index column shape
├── INDEX.md               markdown table of every finding
├── index.html             same index in a browser with filters
├── index-config.yaml      (optional) operator-defined subset indexes
├── findings-index.py      regenerate indexes / search the tree
└── findings/
    └── <tag>/
        ├── metadata.yaml           structured fields
        ├── FINDING.md              long-form analysis
        ├── summary.md              triage write-up (optional)
        ├── auto-generated-fix*.diff           (if a fix was published)
        ├── invalidated-auto-generated-fix*.diff
        │                            (a previously-published fix that
        │                             a later run invalidated)
        ├── invalidation.md         (when the whole finding is invalid)
        ├── partial-invalidation.md (when one todo of a series is invalid)
        └── repro/                  (optional reproducer artifacts)
```

`<tag>` is the finding's `id` sanitised for a filename. Names are
stable: re-running the audit against the same bug reuses the tag.

Some subset-index files (`INDEX-priority.md`, `INDEX-mm.md`, etc.) may
appear at the root. Their selection criteria are documented in the
local `README.md` plus `index-config.yaml`.

## Per-finding files

### `findings/<tag>/metadata.yaml`

The single source of structured truth for the finding. Top-level
keys:

- `id`, `title`, `severity` (`high` / `medium` / `low`), `status`
  (`active` / `invalidated` / `unconfirmed`).
- `filename`, `subsystem`, `date`.
- `git.sha` / `git.subject`: workspace HEAD at analysis time.
- `first_seen_task` / `last_updated_task`: task IDs from the audit
  that produced or last touched this finding.
- `related_finding_ids`: cross-references into the tree. A bug
  deferred to another finding cites the related id here.
- `relevant_symbols`: list of `{name, filename, line}`. Every
  function, type, or macro the analysis cites lives here. The
  `function:` search clause matches against this list.
- `relevant_file_sections`: list of `{filename, line_start,
  line_end}`. The `file:` search clause matches against this list
  plus the top-level `filename:`.
- `open_questions`: optional list of strings. Each entry is tagged
  `[UNVERIFIED]`, `[RESOLVED]`, or `[PARTIALLY RESOLVED]` and
  records audit-time unresolved hypotheses.
- `bugs`: optional list of `{id, description}` entries that
  enumerate the distinct mechanisms a multi-bug finding describes.
  Joined against `results[].bug` and against the bug ids the fix
  workflow's research step emits.
- `results`: optional list of `{bug, outcome, evidence}` entries
  written by a fix workflow. `outcome` is one of `fixed`,
  `invalidated`, `deferred`, or `unresolved`; `evidence` is prose
  citing a file:line, commit ref, or sibling finding. The block is
  replaced wholesale per fix run.
- `auto_generated_fixes`: list of `.diff` filenames in the same
  directory, one per published commit in the fix series. New
  publishes append; never reordered.
- `invalidated_auto_generated_fixes`: same shape but for diff
  files that a later run determined were wrong. The diff files
  themselves are renamed with an `invalidated-` prefix when this
  block exists.

A finding can be in only one fix state at a time: either
`auto_generated_fixes:` is present (active fix), or
`invalidated_auto_generated_fixes:` is present (the prior fix was
invalidated), never both. A subsequent re-fix clears the
invalidated block and writes a fresh active one.

### `findings/<tag>/FINDING.md`

The slow agent's long-form analysis. Sections typically include
summary, mechanism walkthrough, reproducer sketch, impact, fix
sketch, open questions, per-task analysis, and excerpts of the
cited symbols / file sections. Treat as read-only authoritative
evidence. The first line `**Status:** active|invalidated|unconfirmed`
mirrors `metadata.status:` and is updated by the workflow when
status changes.

### `findings/<tag>/summary.md`

A short, human-oriented triage write-up: one-line subject, status,
subsystem note, plain-language impact, trigger requirements, fix
synopsis. Written by the embedded `triage` workflow or by hand.
Optional — a finding without `summary.md` has not been triaged.
The first line carries cross-links (`[FINDING.md](FINDING.md) |
[metadata.yaml](metadata.yaml) | [auto-generated-fix.diff](auto-generated-fix.diff)`),
which the publish path appends to as fixes land.

### `findings/<tag>/invalidation.md` and `partial-invalidation.md`

Written only when research determines the finding (or one of its
todos in a multi-commit series) is invalid. Records the reason
and the source/commit evidence that disproves the bug. Deleted
when a later run reconfirms the finding and publishes a fresh
fix.

## Searching with `findings-index.py`

`findings-index.py` does two jobs: regenerate `INDEX.md` /
`index.html` from the current state of `findings/`, and run ad-hoc
searches.

```
./findings-index.py --generate
./findings-index.py --search "<query>"
```

`--search` prints a markdown table identical in shape to
`INDEX.md`, restricted to matching rows.

### Query language

Whitespace-separated `key:value` clauses joined by implicit AND,
explicit `-a`, or `-o`. Parens group; use `[(]` / `[)]` inside a
regex value.

| key         | matches                                                  |
| ----------- | -------------------------------------------------------- |
| `severity`  | `high` / `medium` / `low` / `?`                          |
| `subsystem` | the `subsystem:` field (`—` when blank)                  |
| `status`    | `active` / `invalidated` / `unconfirmed`                 |
| `file`      | any filename cited by the finding — primary `filename:`  |
|             | plus every relevant-symbol / relevant-section entry      |
| `function`  | any symbol name under `relevant_symbols:`                |
| `regex`     | the joined row text (sev + subsys + date + status +      |
|             | id + title)                                              |
| `since`     | `YYYY-MM-DD`; row's date must be ≥ this                 |

Every value except `since:` is a case-insensitive regex. Anchor
with `^foo$` for exact matches.

### Example queries

```
./findings-index.py --search "severity:high regex:uaf"
./findings-index.py --search "subsystem:work -o subsystem:sched"
./findings-index.py --search "( severity:high -o severity:medium ) -a subsystem:mm -a since:2026-04-01"
./findings-index.py --search "file:^net/ipv4/"
./findings-index.py --search "function:^pwq_ -a severity:high"
```

Refer to the tree's own `README.md` for any clauses or keys added
beyond this baseline.

## Reading order when navigating the tree

1. Open the tree's `README.md`. It pins the exact column layout
   and any local subset indexes.
2. Use `INDEX.md` or one of the saved subset indexes (e.g.
   `INDEX-priority.md`, `INDEX-high.md`) to find candidates.
3. For a candidate, read `summary.md` if present — it's
   triage-curated and short. Fall back to `FINDING.md` for the
   raw analysis when no summary exists.
4. Cross-reference `metadata.yaml` for structured fields:
   `related_finding_ids` (sibling findings that may cover deferred
   mechanisms), `bugs` / `results` (which mechanisms were fixed
   versus invalidated versus left unresolved), and
   `auto_generated_fixes` / `invalidated_auto_generated_fixes`
   (the on-disk patches and their lifecycle state).
5. Run `findings-index.py --search` to scope wider — by
   subsystem, by touched file, by symbol, or by severity since a
   date.

Do not modify `FINDING.md` or `findings-index.py` from inside this
skill. They are authored by kres or by the operator and may be
regenerated; local edits to either are expected only when the
operator explicitly intends to customise the export tree.

## kres runtime directories

kres maintains two separate on-disk areas: a per-user
configuration directory at `~/.kres/` and a per-working-tree
logging / per-session artifact directory at `<cwd>/.kres/`. The
two are independent — config is global, logs and session state
sit next to the codebase being analysed.

### `~/.kres/` — configuration

Populated by `setup.sh` from the kres source tree. Contents:

| Path                      | Purpose                                                     |
| ------------------------- | ----------------------------------------------------------- |
| `settings.json`           | Per-user defaults: model id per agent role, results-dir,    |
|                           | other CLI defaults. CLI flags override this file.           |
| `models/<id>.json`        | One file per model. Carries `api_key`, provider, max-tokens,|
|                           | rate-limit, thinking shape, optional per-role overrides     |
|                           | (`fast`, `slow`, `main`, `todo`).                           |
| `mcp.json`                | MCP server registry (semcode, etc.).                        |
| `system-prompts/*.system.md` | Operator overrides for embedded agent system prompts     |
|                           | (fast / slow / slow-coding / slow-generic / main / todo).   |
|                           | Empty by default; a file here shadows the embedded copy.    |
| `workflows/<name>.json`   | Operator overrides for shipped workflows (`fix.json`,       |
|                           | `review.json`, `triage.json`). A file here shadows the      |
|                           | embedded copy.                                              |
| `commands/<name>.md`      | Operator overrides / additions for non-workflow slash       |
|                           | commands (e.g. summary templates). Workflow-owned names     |
|                           | (`fix`, `review`, `triage`) are reserved.                   |
| `skills/*.md`             | Domain-knowledge skill files. kres scans each `*.md` at     |
|                           | startup; skills with `invocation_policy: automatic` are     |
|                           | loaded only when workspace detection selects that knowledge  |
|                           | pack (for example kernel or systemd); others are pulled in   |
|                           | on demand.                                                  |
| `sessions/<timestamp>/`   | Per-run artifact directory used when the operator did not   |
|                           | pass `--results <dir>`. Contains `findings.json`,           |
|                           | `report.md`, optional `session.json` (resume snapshot),     |
|                           | optional `summary.txt` or `summary.md` (from `/summary`).   |
|                           | Multi-finding runs may produce `findings-N.json` siblings.  |
| `history/`                | REPL prompt history.                                        |

Rate limiters are shared across agents whose model configs name the
same API key string; that grouping lives in the `models/*.json`
files.

`~/.kres/` carries no per-codebase state. It is safe to copy
across machines once the API keys are present.

### `<cwd>/.kres/` — per-working-tree logs

Created the first time kres runs inside a directory. The
operative subdirectory is `logs/`:

```
<cwd>/.kres/
└── logs/
    └── <session-uuid>/
        ├── code.jsonl     fast + slow agent turns
        ├── main.jsonl     main agent turns (data fetches)
        └── mcp-<server>.log  one log per attached MCP server
```

`<session-uuid>` is derived deterministically from PID + start
time (uuid5), so parallel kres processes do not collide. Each
session directory accumulates throughout the run.

#### `code.jsonl`

Newline-delimited JSON. Each line is one assistant or user record
for the fast + slow agent pipeline. Fields:

- `role` — `user` or `assistant`.
- `label` — opaque phase tag. Common values: `phase=fast-gather
  task=<step> round=N`, `phase=slow task=<step>`, `phase=slow-lens
  task=<step> lens=<id> model=<id>`, `phase=fast-gather-lenses
  round=N`, `phase=review-ledger ...`.
- `content` — the prompt or response body. User records are
  the assembled prompt (JSON-encoded payload with `question`,
  `symbols`, `context`, `plan`, etc.). Assistant records are
  either structured JSON (fast agent: `analysis`, `followups`,
  `ready_for_slow`, `code_edits`, `outcomes`, ...) or raw prose
  (slow agent's analysis or lens output).
- `usage` (assistant only) — `{input, output, cache_read,
  cache_creation}` token counts.
- `thinking` (assistant only, when extended thinking is enabled)
  — extracted thinking-block text.
- `request` (user only, on slow and lens calls) — wire-relevant
  request config: `{model, max_tokens, thinking, effort,
  budget_tokens}`. `thinking` is the API's `type` value
  (`"adaptive"` or `"enabled"`); `effort` is present for adaptive
  thinking (`"low"` / `"medium"` / `"high"` / `"xhigh"`);
  `budget_tokens` is present for explicit-budget thinking.

A slow agent response that starts with `[INVALID]` is the agent's
signal that the bug was determined not real. Review-step lens
responses carry `DEFECT` markers when they fail clean.

#### `main.jsonl`

Same shape as `code.jsonl` but for the main agent. Records
alternate user/assistant. Assistant responses are either
`<actions>[...]</actions>` XML carrying data-fetch requests
(`read`, `source`, `git`, `grep`, `mcp`, `make`, `bash`) or JSON
carrying `goal`/`mode` (initial goal definition) or a `todo`
update. Plain-text "done" / "compile clean" lines appear between
action rounds.

#### Tracing a run

1. Scan `code.jsonl` user records for the `plan.steps[].status`
   progression to see which workflow steps completed.
2. Scan `code.jsonl` assistant records: JSON = fast agent (look
   at `analysis` and `followups`), raw prose = slow agent (look
   for `[INVALID]`, `DEFECT`, lens verdicts, or `outcomes[]`
   payloads from the bug-coverage lens).
3. Check `main.jsonl` for `todo` entries to see how the todo
   list moved through `pending → in_progress → done`.
4. Sum token usage by reading each assistant record's `usage`
   block. Cache effectiveness is the ratio of `cache_read` to
   `cache_read + cache_creation + input`.

The per-session log directory survives across kres invocations.
`<cwd>/.kres/` may be safely deleted to reclaim disk space; the
next run creates a fresh `logs/<uuid>/` entry. Nothing under
`<cwd>/.kres/` carries configuration — that lives in `~/.kres/`.
