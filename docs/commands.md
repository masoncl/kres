# Slash-command templates

`review` / `summary` / `summary-markdown` are embedded
slash-command templates. Each has an `.md` body bundled in the
kres binary via `kres_agents::user_commands`, and operators can
override or add commands by dropping a file at
`~/.kres/commands/<name>.md`.

| Command            | CLI                                                                                      | REPL                           |
|--------------------|------------------------------------------------------------------------------------------|--------------------------------|
| `review`           | `kres --prompt 'review: fs/btrfs/ctree.c'` or `kres --prompt '/review fs/btrfs/ctree.c'` | `/review fs/btrfs/ctree.c`     |
| `summary`          | `kres --summary --results DIR`                                                           | `/summary [filename]`          |
| `summary-markdown` | `kres --summary --markdown --results DIR`                                                | `/summary-markdown [filename]` |

The three shipped templates play two different roles:

- `review` — a task prompt. CLI and REPL invocations prepend
  the operator's target to the template body via
  `user_commands::compose` and submit the result as a new task
  (see [review-template.md](review-template.md)).
- `summary` — a system prompt. `/summary` and `kres --summary`
  feed the template body to the fast agent alongside the run's
  `report.md` + `findings.json` to render `bug-report.txt`
  (`kres-repl/src/summary.rs`). No target composition.
- `summary-markdown` — identical path; selected by `--markdown`
  and writes `bug-report.md`.

Adding your own: drop `~/.kres/commands/audit.md` and run
`kres --prompt 'audit: net/...'` or `/audit net/...`. No rebuild
needed — the disk override path is consulted on every invocation.

Load order (identical for every command):

1. `~/.kres/commands/<name>.md` on disk (operator override).
2. Embedded body in `kres_agents::user_commands` (shipped three).
3. Legacy `~/.kres/prompts/<name>-template.md` — back-compat for
   custom templates from before this refactor.
4. No match → treat `"name: extra"` as a verbatim prompt.

`setup.sh` still copies operator-authored
`configs/prompts/<word>-template.md` to `~/.kres/prompts/` for
any `<word>` that isn't one of the embedded names, so
pre-refactor custom templates keep working through the legacy
fallback.
