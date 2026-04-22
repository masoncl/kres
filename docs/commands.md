# Slash-command templates

`review` / `summary` / `summary-markdown` are embedded
slash-command templates. Each has an `.md` body bundled in the
kres binary via `kres_agents::user_commands`, and an operator
can override or add commands by dropping a file at
`~/.kres/commands/<name>.md`.

Invocation paths (all three commands available in both places,
plus arbitrary operator commands dropped under
`~/.kres/commands/<name>.md` are invocable the same way):

| Command            | CLI                                                                                      | REPL                           |
|--------------------|------------------------------------------------------------------------------------------|--------------------------------|
| `review`           | `kres --prompt 'review: fs/btrfs/ctree.c'` or `kres --prompt '/review fs/btrfs/ctree.c'` | `/review fs/btrfs/ctree.c`     |
| `summary`          | `kres --summary --results DIR`                                                           | `/summary [filename]`          |
| `summary-markdown` | `kres --summary --markdown --results DIR`                                                | `/summary-markdown [filename]` |

The `review:` and `/review` CLI forms compose the template body
with the trailing target; the `/review` REPL form does the same
composition through `user_commands::compose` and submits the
result as a new task.

The shipped three:

- `review` — the parallel-lens review template (see
  [review-template.md](review-template.md)). Invocation prepends
  the operator's target to the template body.
- `summary` — the plain-text bug-report system prompt that
  `/summary` and `kres --summary` pass to the fast agent.
- `summary-markdown` — the markdown-output variant selected by
  `--markdown`.

Adding your own: drop `~/.kres/commands/audit.md` and run
`kres --prompt 'audit: net/...'` or `kres --prompt '/audit
net/...'`. No rebuild needed — the disk override path is
consulted on every invocation.

Load order (identical for every command):

1. `~/.kres/commands/<name>.md` on disk (operator override).
2. Embedded body in `kres_agents::user_commands` (for the three
   shipped commands).
3. Fallback to the legacy `~/.kres/prompts/<name>-template.md`
   lookup when neither of the above hit — preserves existing
   custom templates from before this refactor.
4. Nothing matched → treat `"name: extra"` as a verbatim prompt.

Files that setup.sh still copies to `~/.kres/prompts/`: any
operator-authored `<word>-template.md` the user drops into
`configs/prompts/` that isn't shadowed by an embedded command
of the same root name. The shipped `review-template.md`,
`bug-summary.md`, and `bug-summary-markdown.md` are NOT copied
(they're embedded); `configs/prompts/<word>-template.md` for
any other `<word>` is copied verbatim so custom templates from
before the refactor keep working via the legacy
`~/.kres/prompts/<word>-template.md` fallback path.
