# kres

Kernel code RESearch agent — an LLM-driven multi-agent REPL for reviewing and
fixing bugs in large source trees. The Linux kernel is the primary target.

## kres introduction

kres splits the job of reviewing code across a number of cooperating agents,
all of which iterate generating additional research todos.  While kres can
review discrete things such as individual commits or functions, the main focus
is longer reviews on whole files, where each iteration adds to and prioritizes
a large TODO list of additional research until our turn limit is exhausted.

Potential findings are documented along the way, which can then get exported and
validated in separate passes.

kres can also automatically generate fixes for the bugs it finds.  The patch
generation workflow validates the bug, and iterates on fixes until review
lenses and compile steps pass.

See [docs/agents.md](docs/agents.md) for the task flow and
[docs/workflow.md](docs/workflow.md) for the review and fix workflows.

## Quick start

1. **Build**:

   ```
   cargo build --release
   ```

2. **Populate `~/.kres/`** from shipped configs:

   kres is really meant to drive requests through the provider APIs.  This gives
   us more control over context, caching, and the workflows themselves.  But,
   if you only have CLI access to claude or codex, kres can simulate API access
   through the rust-code-agent-sdks crate.  This basically uses a set of
   claude/codex sessions and sends requests through them:

   ```
   ./setup.sh --provider claude
   ```

   If you're using API access:

   ```
   ./setup.sh --provider anthropic --api-key "$ANTHROPIC_API_KEY"
   ```

   Select exactly one provider: `anthropic`, `openai`, `claude`, or `codex`.
   Anthropic and OpenAI require a literal `--api-key`; Claude and Codex use
   their CLI authentication.

   If you have semcode and/or the kernel review prompts installed, `setup.sh`
   will try to autodetect them, but there are flags to help with that if needed.

   The generated `~/.kres/settings.json` can be edited afterwards.

3. **Run a review** from a kernel tree:

   ```
   cd linux
   kres --results review --prompt 'review: fs/btrfs/ctree.c' --turns 10
   kres --summary-markdown --results review
   # review/summary.md now has your results
   ```

   `--prompt 'review: X'` invokes the embedded review workflow —
   a parallel lensed audit over the target. `--results DIR`
   keeps the run's artifacts under `DIR/` (findings.json,
   report.md, summary.txt).  `--turns 10` stops after 10 rounds.

   If you're doing more in-depth reviews of whole files, 50 turns is a good starting point.

   Summary generation first runs the validation workflow for every finding,
   with up to 20 findings validating in parallel, then renders only the
   validated results. Both fast and slow models must be configured; validation
   artifacts are kept under
   `review/summary-validation/` in this example.

## Combining and comparing multiple models

You can add a second model for reviews. In `~/.kres/settings.json`:

```json
{
  "models": {
    "slow": "opus",
    "slow_secondary": "gpt"
  }
}
```

The primary slow model runs every active lens. The secondary model runs only
the `general` lens during `/review`, or the broader `maintainer` lens while
`/fix` iterates on a patch.

For a one-off run, pass both models with `--slow opus,gpt` (equivalent to
repeating `--slow`). Any explicit `--slow` selection replaces the configured
primary and secondary pair. Add `--compare` to run every active lens with
every selected model and write a model comparison under the results
directory.

See [docs/configuration.md](docs/configuration.md) for selector and alias
resolution, and [docs/cli.md](docs/cli.md) for precedence details.

## Exporting findings

You can export results into either text or markdown:

- [docs/summary.md](docs/summary.md) — `/summary`,
  `kres --summary`, and the summary output format.

But these scans can produce a lot of results, and churning through a giant
text file isn't the easiest way to walk them.  It's much better to use:

```
kres --export DIR --results <results-dir>
```

After the export, DIR has each of the findings exploded into a subdirectory
with metadata and tracking details so you can run validation scripts and generate
fixes.

- [docs/exporting.md](docs/exporting.md) — `kres --export DIR`

## Validating the exported findings

Since we can't let kres run forever, many of the findings are going to be incomplete
research.  Some will also be hallucinations or other mistakes from the research
agents.

After we've exported the findings, we can run the validation workflow:

```
kres --prompt 'validate: <finding dir> <linux source>'
```

This needs to be run on each individual finding in the export, and scans can
create a large number of findings.  There's a helper script `scripts/validate-all.py`.  Example run:

```
cd ~/src/linux
# semcode-index isn't required, but it does help
semcode-index

# do the initial kres scan
kres --results kres-scan --turns 50 --prompt 'review: fs/btrfs/ctree.c'

# export our results into ~/src/kernel-bugs
kres --export ~/src/kernel-bugs --results kres-scan

cd ~/src/kernel-bugs
validate-all.py --workspace ~/src/linux -n 20

# list the bugs that are still active
./findings-index.py --search status:active
```

The validate-all.py run will churn through each of the findings and run 20
parallel kres workers to validate the runs.  If you do future exports into the
same directory, the validate-all.py script skips anything you've already validated.

It's usually easiest to ask your favorite agent to read findings-index.py and
suggest which bugs are most important.

## Fixing bugs

Continuing our example, pretend you want to make a patch for `~/src/kernel-bugs/findings/big_bad_mm_bug`

```
cd ~/src/linux
# make defconfig, or whatever else needed for the kernel to compile
kres --results fix --prompt 'fix: ~/src/kernel-bugs/findings/big_bad_mm_bug'
```

At the end of the fix run, you'll either have details about why the bug wasn't
worth fixing, a failure message, or (hopefully) commits in ~/src/linux fixing the
bug.  The corresponding finding directory will also have the patch file exported
there.

## Further reading

- [docs/generating-fixes.md](docs/generating-fixes.md) — safety
  process kres uses before editing, committing, reviewing, and
  publishing generated kernel fixes.
- [docs/agents.md](docs/agents.md) — fast / slow / todo / etc
  agent flow and how follow-up tasks drive larger reviews.
- [docs/workflow.md](docs/workflow.md) — the workflow-backed
  `/review`, `/validate`, and `/fix` flows.
- [docs/coding-tasks.md](docs/coding-tasks.md) — reproducer and
  fix generation (`code_output`, `code_edits`, `bash` verify).
- [docs/turns-and-follow.md](docs/turns-and-follow.md) — when
  kres decides a non-interactive run is done.
- [docs/action-allowlist.md](docs/action-allowlist.md) — which
  non-MCP tools kres can dispatch and how to change
  that.
- [docs/configuration.md](docs/configuration.md) — `~/.kres/`
  layout, model selection, system-prompt overrides, semcode MCP
  integration, and kernel review-prompts setup.
- [docs/commands.md](docs/commands.md) — command dispatch paths
  and operator-authored prompt-template additions.
- [docs/cli.md](docs/cli.md) — every CLI flag and REPL command.
- [docs/development.md](docs/development.md) — workspace layout,
  build / test / lint, pre-commit hook.
