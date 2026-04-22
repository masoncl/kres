# Summary output — `/summary`, `--summary`, `bug-report.txt`

After each task, kres appends the slow agent's narrative to
`<results>/report.md` and rewrites `<results>/findings.json` with
the cumulative merged list (the previous canonical file is copied
to `findings-N.json` first, preserving history).

A plain-text bug report is produced by `/summary` (or
automatically on `--turns` exit, or standalone via
`kres --summary --results <dir>`). That run:

- reads `<results>/prompt.md` (saved on first submit so later
  summaries know the original question), `<results>/report.md`,
  and `<results>/findings.json`;
- calls the fast agent with the embedded `summary` slash-command
  template as its system prompt (override at
  `~/.kres/commands/summary.md`; `--markdown` picks the
  `summary-markdown` variant);
- orders sections by `bug-severity` (`high` → `medium` → `low` →
  `latent` → `unknown`), one section per bug headed by
  `Subject:`, `bug-severity:`, `bug-impact:` lines;
- writes `<results>/bug-report.txt` (or `bug-report.txt` in cwd
  when `--results` was absent).

`--template PATH` overrides the shipped summariser prompt for one
run without rebuilding.
