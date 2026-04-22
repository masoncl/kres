# Summary output — `/summary`, `--summary`, `summary.txt`/`summary.md`

After each task, kres appends the slow agent's narrative to
`<results>/report.md` and rewrites `<results>/findings.json` with
the cumulative merged list (the previous canonical file is copied
to `findings-N.json` first, preserving history).

A plain-text summary is produced by `/summary` (or automatically
on `--turns` exit, or standalone via
`kres --summary --results <dir>`). The markdown variant is
`/summary-markdown` / `kres --summary-markdown --results <dir>`,
which writes `summary.md`. That run:

- reads `<results>/prompt.md` (saved on first submit so later
  summaries know the original question), `<results>/report.md`,
  and `<results>/findings.json`;
- calls the fast agent with the embedded `summary` slash-command
  template as its system prompt (override at
  `~/.kres/commands/summary.md`; `--summary-markdown` picks the
  `summary-markdown` variant at
  `~/.kres/commands/summary-markdown.md`);
- if the assembled prompt exceeds the fast agent's
  `max_input_tokens`, splits the findings into chunks that each
  fit, renders one partial summary per chunk, and then runs a
  final combine call that merges the partials into one report;
- orders sections by `bug-severity` (`high` → `medium` → `low` →
  `latent` → `unknown`), one section per bug headed by
  `Subject:`, `bug-severity:`, `bug-impact:` lines;
- writes `<results>/summary.txt` (or `summary.md` with
  `--summary-markdown`); falls back to the cwd when `--results`
  was absent.

`--template PATH` overrides the shipped summariser prompt for one
run without rebuilding.
