# Summary output — `/summary`, `--summary`, and `bug-report.txt`

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
