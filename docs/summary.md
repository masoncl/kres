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
which writes `summary.md`. Both commands require a configured fast agent.
Standalone `--summary` and `--summary-markdown` use the fast model for both
validation roles by default; `--slow NAME` selects a different slow validation
model for that run. That run:

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
- renders each bug as a candidate kernel commit message using the conventions
  in `configs/prompts/commit-kernel-template.md`: a subsystem-prefixed,
  imperative subject followed by a short causal changelog and supported fix;
  the text variant emits raw commit-message blocks while the Markdown variant
  uses each proposed subject as a `##` heading;
- writes `<results>/summary.txt` (or `summary.md` with
  `--summary-markdown`); falls back to the cwd when `--results`
  was absent.

`--template PATH` overrides the shipped summariser prompt for one
run without rebuilding.
