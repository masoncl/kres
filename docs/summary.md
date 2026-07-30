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
which writes `summary.md`. That run:

- reads `<results>/prompt.md` (saved on first submit so later summaries know
  the original question) and `<results>/findings.json`; it does not parse
  `report.md`. Per-task narrative comes from `findings[].details` and the
  top-level `task_prose` ledger;
- uses the fast agent for task condensation, then renders with the embedded
  `summary` slash-command template as its system prompt (override at
  `~/.kres/commands/summary.md`; `--summary-markdown` picks the
  `summary-markdown` variant at
  `~/.kres/commands/summary-markdown.md`);
- filters invalidated findings, sorts stored severities high to low, groups
  findings and narrative by task, condenses task batches that fit the fast
  model's input limit, and runs a final render/combine pass;
- orders sections by `bug-severity` (`high` → `medium` → `low` →
  `latent` → `unknown`), one section per bug headed by
  `Subject:`, `bug-severity:`, `bug-impact:` lines;
- writes `<results>/summary.txt` (or `summary.md` with
  `--summary-markdown`); falls back to the cwd when `--results`
  was absent.

`--template PATH` overrides the shipped summariser prompt for one
run without rebuilding.
