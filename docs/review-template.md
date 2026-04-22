# Review template — the `/review` parallel-lens flow

`--prompt 'review: fs/btrfs/ctree.c'` splices the target onto
the front of the embedded review template
(`configs/prompts/review-template.md`), producing a prompt that
covers object lifetime, memory, bounds, races, and general
bugs. Two equivalent invocations:

```
kres --prompt 'review: fs/btrfs/ctree.c'
kres --prompt '/review fs/btrfs/ctree.c'
```

Both resolve through `kres_agents::user_commands::lookup`: the
on-disk override `~/.kres/commands/review.md` wins over the
embedded copy. Drop `~/.kres/commands/<name>.md` to add a new
command and invoke it via `"name: extra"` or `"/name extra"` —
see [commands.md](commands.md).

The split is anchored at the start of the prompt. Free-form
text that contains `review:` or `/review` mid-string is
submitted verbatim.

## Parallel lenses

Each markdown todo bullet in the template is a **lens**:

```
- [ ] **[investigate]** object lifetime: #lifetime
- [ ] **[investigate]** memory allocations: #memory
- [ ] **[investigate]** bounds checks ... #bounds
- [ ] **[investigate]** races: #races
- [ ] **[investigate]** general: #general
```

`kres_agents::prompt_file::parse` turns each bullet into a
`LensSpec` and installs them as session-wide lenses. Every task
fans out one slow-agent call per lens over the same gathered
symbols and context; a consolidator dedupes the findings across
lenses before the merger folds them into the cumulative list
(`kres-core/src/lens.rs`). That parallelism is the point — each
angle gets a focused call with full context, and overlap is
resolved at consolidation time.

Indented sub-bullets under a lens bullet fold into its `reason`
field as extra guidance for that lens's slow-agent call (see the
`object lifetime` and `memory allocations` bullets in the shipped
template).

To change the lens set, drop a customised copy at
`~/.kres/commands/review.md`. Dropping `~/.kres/commands/<word>.md`
adds a `/<word>` slash-command invocable via
`--prompt "<word>: target"` or `--prompt "/<word> target"`.

`--results <dir>` keeps the run's artifacts (`findings.json`
plus `findings-N.json` history, `report.md`, `summary.txt`)
in `<dir>/`; without it kres picks
`~/.kres/sessions/<timestamp>/`.
