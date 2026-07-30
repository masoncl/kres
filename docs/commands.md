# Commands

Shipped commands have one implementation path each:

| Command | CLI | REPL |
|---|---|---|
| `review` | `kres --prompt 'review: path/to/file.c'` or `kres --prompt '/review path/to/file.c'` | `/review path/to/file.c` |
| `triage` | `kres --prompt 'triage: /abs/finding-dir'` or `kres --prompt '/triage /abs/finding-dir'` | `/triage /abs/finding-dir` |
| `validate` | `kres --prompt 'validate: /abs/finding-dir [source-workspace]'` | `/validate /abs/finding-dir [source-workspace]` |
| `fix` | `kres --prompt 'fix: TARGET'` or `kres --prompt '/fix TARGET'` | `/fix TARGET` |
| `summary` | `kres --summary --results DIR` | `/summary [filename]` |
| `summary-markdown` | `kres --summary-markdown --results DIR` | `/summary-markdown [filename]` |

`review`, `triage`, `validate`, and `fix` are workflow-owned commands. They resolve
through `configs/workflows/*.json` (or an override at
`~/.kres/workflows/<name>.json`) and never fall back to a markdown prompt
template.

`summary` and `summary-markdown` are report-rendering commands. Both CLI
and REPL call `kres-repl/src/summary.rs`; they do not run through
`--prompt "summary: ..."`. The renderer uses the embedded
`bug-summary.md` / `bug-summary-markdown.md` templates, with optional
overrides at `~/.kres/commands/summary.md` and
`~/.kres/commands/summary-markdown.md`.

Operator-authored non-workflow prompt templates can still live under
`~/.kres/commands/<name>.md` and be invoked with
`kres --prompt '<name>: target'` or `/<name> target`. Shipped workflow
command names are reserved and cannot be resurrected by dropping
`review.md`, `triage.md`, `validate.md`, or `fix.md` in that directory.
