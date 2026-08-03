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
- renders each bug with the shared kernel problem-description rules from
  `configs/prompts/kernel-problem-description.md` and the descriptor catalog
  from `configs/prompts/commit-log-descriptors.md`: a source-area subject,
  non-prose descriptors wherever possible, and only minimal supporting prose,
  without proposing a fix; code used as bug evidence is copied verbatim with
  filename:function context and pseudocode is forbidden; the text variant
  emits raw problem-description blocks while the Markdown variant uses each
  subject as a `##` heading;
- writes `<results>/summary.txt` (or `summary.md` with
  `--summary-markdown`); falls back to the cwd when `--results`
  was absent.

`--template PATH` overrides the shipped summariser prompt for one
run without rebuilding. The shared kernel problem-description rules and
descriptor catalog are still prepended to that output-specific override.
