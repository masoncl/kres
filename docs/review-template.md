# Review template — the `/review` parallel-lens flow

`--prompt 'review: fs/btrfs/ctree.c'` is a two-part prompt: the
token `review` names the slash-command template embedded in the
kres binary (source: `configs/prompts/review-template.md`), and
the rest of the string is the specific target. kres splices the
target onto the front of the template body to produce a full
prompt covering object lifetime, memory safety, bounds checks,
races, and general bugs in the named code.

Two equivalent forms — pick whichever reads better:

```
kres --prompt 'review: fs/btrfs/ctree.c'
kres --prompt '/review fs/btrfs/ctree.c'
```

Both resolve via `kres_agents::user_commands::lookup("review")`,
which prefers `~/.kres/commands/review.md` on disk (the operator
override path) and falls back to the embedded copy. Drop a file
at `~/.kres/commands/<name>.md` to add a new command; use the
same `--prompt "name: extra"` or `--prompt "/name extra"` form
to invoke it. See [docs/commands.md](commands.md).

Legacy compatibility: `--prompt "word: extra"` still falls back
to `~/.kres/prompts/<word>-template.md` when no matching
`~/.kres/commands/<word>.md` exists and the name isn't one of
the embedded commands — operators with custom `<word>-template.md`
files from before the refactor keep working.

The template is invoked only when `review` appears as the
colon-terminated leading word (`"review:..."`) or as the
slash-prefixed leading word followed by whitespace
(`"/review ..."`). Free-form text that happens to contain those
character sequences elsewhere (e.g. `"what caused the review: ..."`)
is submitted verbatim — the split is anchored to the start of
the prompt.

## Parallel lenses inside `review-template.md`

The shipped template is more than a prose prompt — each of its
markdown todo bullets is a **lens**:

```
- [ ] **[investigate]** object lifetime: #lifetime
- [ ] **[investigate]** memory allocations: #memory
- [ ] **[investigate]** bounds checks ... #bounds
- [ ] **[investigate]** races: #races
- [ ] **[investigate]** general: #general
```
(`configs/prompts/review-template.md`)

`kres_agents::parse_prompt_file`
(`kres-agents/src/prompt_file.rs:28-98`) turns each bullet into a
`LensSpec` (id, kind, name, reason) and installs them as
**session-wide lenses**. For every task, kres then fans out one
slow-agent call per lens over the *same* gathered symbols and
source sections — five parallel analyses in the case of the shipped
template — and runs a consolidator pass that dedupes the findings
across lenses before the merger folds them into the cumulative
list (`kres-core/src/lens.rs:1-7`).

That parallelism is what makes a single `review:` run productive:
instead of the slow agent juggling lifetime + memory + bounds +
races + general bugs in one response, each angle gets its own
focused call with the full context, and overlap between findings
is resolved at consolidation time. Indented sub-bullets under a
lens bullet fold into its `reason` field and become extra guidance
the slow agent sees on that specific lens (see the sub-bullets
under `object lifetime` and `memory allocations` in the template).

To add or remove angles for your own reviews, drop a customised
copy of the review template at `~/.kres/commands/review.md` — it
takes precedence over the embedded copy at load time. Dropping a
new `<word>.md` alongside it (e.g. `~/.kres/commands/audit.md`)
adds a `/audit` slash-command you can invoke via
`--prompt "audit: target"` or `--prompt "/audit target"`.

`--results <dir>` tells kres where to keep the run's artifacts:
`findings.json` (plus `findings-N.json` history snapshots), the
running narrative `report.md`, and the rendered `bug-report.txt`
when `/summary` fires. Without `--results`, kres picks
`~/.kres/sessions/<timestamp>/` automatically.
