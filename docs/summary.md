# Summary output — `/summary`, `--summary`, `summary.txt`/`summary.md`

After each task, kres appends the slow agent's narrative to
`<results>/report.md` and applies the task's findings delta to the
jsondb-backed `<results>/findings.json`. The canonical file is
rewritten atomically in place (tmp + fsync + rename); there are no
per-turn history snapshots.

A plain-text summary is produced explicitly by `/summary` or standalone via
`kres --summary --results <dir>`. Turn-cap and idle exits do not render one
automatically. The markdown variant is
`/summary-markdown` / `kres --summary-markdown --results <dir>`,
which writes `summary.md`. Both commands require configured fast and slow
agents because validation runs before rendering. That run:

- exports every canonical finding under `<results>/summary-validation/` and
  runs the existing `validate` workflow against the active source workspace;
  any validation failure aborts summary generation;
- renders only validation-produced narratives and structured verdicts, so
  stale pre-validation details cannot reintroduce contradicted claims;
- reads `<results>/prompt.md` for the original question and does not parse
  `report.md`;
- uses the fast agent to render with the embedded `summary` slash-command
  template as its system prompt (override at
  `~/.kres/commands/summary.md`; `--summary-markdown` picks the
  `summary-markdown` variant at
  `~/.kres/commands/summary-markdown.md`);
- filters findings validation marked Invalid or Fixed, sorts validated
  severities high to low, and runs a final render/combine pass; if none remain,
  writes a deterministic no-findings message;
- orders sections by `bug-severity` (`high` → `medium` → `low` →
  `latent` → `unknown`), one section per bug headed by
  `Subject:`, `bug-severity:`, `bug-impact:` lines;
- writes `<results>/summary.txt` (or `summary.md` with
  `--summary-markdown`); falls back to the cwd when `--results`
  was absent.

`--template PATH` overrides the shipped summariser prompt for one
run without rebuilding.
