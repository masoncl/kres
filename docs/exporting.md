# Exporting findings

`kres --export DIR` takes the findings from one run and writes a
per-finding folder tree you can share, review, or paste into tickets
without having to grep through `findings.json` by hand.

## Invocation

```
kres --results <run-dir> --export <out-dir> [--workspace <repo>]
```

- `--results <run-dir>` points at a previous kres run (anywhere its
  `findings.json` lives). `--findings <file>` also works.
- `--export <out-dir>` is the target. It is created if missing; it
  is not emptied first, so a re-export on top of an old one stacks
  new `<tag>/` directories alongside any stale ones.
- `--workspace <repo>` is the source tree the findings refer to.
  kres probes `git -C <workspace>` for the HEAD sha and subject and
  records them on every exported finding. Defaults to the current
  directory.

No REPL, no MCP, no orchestrator — `--export` loads the findings
file, writes the tree, and exits.

## Per-finding layout

Each finding lands at `<out-dir>/<tag>/` with two files:

```
<out-dir>/
  <tag>/
    metadata.yaml
    FINDING.md
  <tag>/
    metadata.yaml
    FINDING.md
  ...
```

`<tag>` is the finding's `id`, sanitized so it works as a directory
name (non-alphanumeric characters collapse to `_`, runs squashed,
leading/trailing `_` trimmed). If two findings sanitize to the same
tag, the second one gets a `-2` suffix (then `-3`, …).

### `metadata.yaml`

YAML metadata rendered from
`configs/prompts/export-metadata.yaml` — a compact mustache-lite
template embedded in the kres binary. Operators can shadow it by
dropping a replacement at `~/.kres/prompts/export-metadata.yaml`.

Fields:

- `id`, `title` — finding identity.
- `severity` — `low` / `medium` / `high`.
- `status` — `active` or `invalidated`.
- `date` — RFC3339 timestamp of the first task that inserted this
  finding (`Finding.first_seen_at`). Stamped once on insert, never
  shifted by later merges. When the source findings.json predates
  the field and a record has no stamp, the export falls back to
  wall-clock now so the row still carries a date — note that a
  re-export of that legacy record will show a different timestamp
  each time, while freshly-discovered findings keep a stable one.
- `git:` — workspace HEAD `sha` and commit `subject` at export
  time.
- `introduced_by:` — `sha` (required) and `subject` (optional)
  when a task has attributed the bug to a specific commit.
  Omitted entirely until that happens.
- `first_seen_task`, `last_updated_task` — provenance stamps from
  the store.
- `related_finding_ids` — cross-references by id.
- `relevant_symbols` — `{name, filename, line}` triples.
- `relevant_file_sections` — `{filename, line_start, line_end}`.
- `open_questions` — unresolved investigation threads.

The template engine supports three forms:

- `{{var}}` — scalar, auto-quoted as a YAML double-quoted string.
- `{{!var}}` — scalar emitted raw (for enums / ints that are safe
  to inline unquoted).
- `{{#var}}...{{/var}}` — section. Renders once for each item when
  `var` is a list, once when it's a non-empty scalar, and is
  skipped when missing or empty.

See `kres-repl/src/export.rs` for the context keys populated per
finding.

### `FINDING.md`

Human-readable body rendered directly from the stored Finding:

- Header block with severity, status, `Introduced by`, first/last
  seen task, and a Related line that renders each cross-reference
  as `[`id`](../tag/FINDING.md)` so you can click through.
- `## Summary`, `## Mechanism` (when `mechanism_detail` is set),
  `## Reproducer`, `## Impact`, `## Fix sketch` (when
  `fix_sketch` is set), `## Open questions` (when any).
- `## Relevant symbols` — each entry lists `name` at
  `filename:line` with the captured definition in a fenced block.
- `## Relevant file sections` — labelled by filename and line
  range, with captured content in a fenced block.
- `## Task details` — one subsection per task that contributed
  analysis, carrying the task's verbatim `effective_analysis`
  prose.

Nothing in the export consults report.md — every field comes from
`findings.json` via `FindingsStore::snapshot()`.

## Index file

```
kres --export-index <out-dir>
```

Walks every `<tag>/metadata.yaml` under `<out-dir>` and writes
`<out-dir>/INDEX.md` — a single markdown table of every finding,
sorted by severity (`high` → `medium` → `low`) and, within each
tier, by `date` ascending so long-standing bugs sit at the top.
Entries with no `date` field sink to the bottom of their tier.
Each row links to that finding's `FINDING.md`. No `findings.json`
is consulted — the index reflects whatever is currently on disk,
so hand-edits to individual `metadata.yaml` files show up on the
next run.

## Typical flow

```
kres --results run1 --prompt 'review: fs/btrfs/ctree.c' --turns 5
kres --results run1 --export kres-bugs --workspace .
less kres-bugs/INDEX.md
less kres-bugs/<tag>/FINDING.md

# if you update severities or status, reindex
kres --export-index kres-bugs
```

From there the folders are yours to grep, diff, commit, or paste
into ticketing systems alongside the `git:` attribution.
