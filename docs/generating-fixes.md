# Generating Fixes

This document explains the safety process kres uses when it generates a
kernel fix. The intended reader already knows how to write and review
kernel patches; the point here is to make clear what kres does before it
touches the tree, how it scopes commits, and how it avoids turning an
uncertain bug report into a low-quality patch.

The executable workflow is `configs/workflows/fix.json`. The detailed
workflow runner semantics are documented in [workflow.md](workflow.md).

## Starting Point

Run kres from the source tree to patch:

```bash
kres --results /tmp/kres-fix --prompt 'fix: /absolute/path/to/finding-dir'
```

or with prose:

```bash
kres --results /tmp/kres-fix --prompt 'fix: missing cleanup after foo_register() fails'
```

For finding-directory input, kres expects the exported finding material
(`FINDING.md`, `summary.md`, `metadata.yaml`) and treats
`metadata.yaml.git.sha` as the original audit commit. It still verifies
the bug against the current workspace `HEAD` before patching.

## Safety Invariants

kres should only generate a patch after it has:

- proved the bug still exists at workspace `HEAD`;
- identified a concrete trigger path or violated API/lifetime/locking
  contract;
- scoped the exact fix contract;
- read the current source it intends to edit;
- split independent bugs into independently reviewed commits;
- built the changed object targets when possible;
- reviewed the resulting commit with parallel lenses;
- published only commits that survived build/review or had review
  objections explicitly answered with source evidence.

If kres cannot prove the bug and fix contract, it should stop as
`unconfirmed`, not patch speculatively. If source or commit evidence
disproves the finding, it should stop as `invalid`.

## Research Before Editing

The first phase is not patch generation. It is an audit.

Research reads the finding/prose, current source, relevant callers, and
enough local history to decide whether the report is actionable. The
result is structured, not inferred from prose:

- `confirmed`: the bug and fix contract are proven;
- `invalid`: source or commit evidence disproves the bug;
- `unconfirmed`: evidence is insufficient to patch.

Only `confirmed` reaches patch writing. `invalid` and `unconfirmed` stop
before edits. For finding-directory runs, kres writes status artifacts
such as `invalidation.md` or `partial-invalidation.md` when appropriate.

Research is also responsible for deciding whether the finding is one
commit or a series.

## Commit Scoping

kres represents the fix as a list of internal todos, one intended commit
per todo.

For a simple bug, the plan should contain one todo. If the finding
actually describes two independent bugs, two independently triggerable
failures, or two affected sites with different fix contracts, the plan
should contain multiple todos.

Each todo has its own:

- affected files and symbols;
- fix contract;
- patch-writing attempts;
- `Fixes:` provenance search;
- commit message;
- build result;
- review result;
- published patch file.

The coding agent sees the broader series plan, but it is instructed to
edit only the current todo. This is the main guard against one generated
commit accidentally absorbing sibling fixes.

## Patch Writing

Patch writing is done by the coding agent, but file mutation is still
structured. The agent emits `code_edits` or `code_output`; the workflow
runner applies them deterministically. The agent does not run `git
commit`.

Before editing, the prompt requires the agent to fetch the verbatim
current contents of every function it will change. Retry attempts must
inspect the current worktree and, when a previous attempt was already
committed, the full patch relative to `HEAD~1`. This avoids emitting
edits against stale source or a stale symbol index.

The patch-writing eval accepts only two successful shapes:

- a real source change; or
- a permitted dispute of a prior source review defect, with no source
  change, when the current patch already satisfies the complaint.

It should not manufacture no-op edits just to keep the workflow moving.

## Fixes Tag Provenance

`Fixes:` provenance is deliberately separate from initial research and
from commit-message writing.

After the first patch for a todo exists, kres runs `fixes-tag-search` to
look for the introducing commit. It must inspect candidate diffs; blame
alone is not enough. It sets a `Fixes:` trailer only when the introducing
commit is proven with kernel-review confidence.

The search runs at most once per todo. Later build or review correction
cycles reuse the preserved result instead of spending time and tokens
redoing the same history search. If no commit was proven, the generated
commit omits `Fixes:`.

## Commit Message And Commit

The commit-message step writes `.kres-commit-msg.tmp`. It is scoped to
the current todo's patch only.

For a series, commit N is described relative to its parent, which already
contains commits 1..N-1. The message should not describe the whole
original finding as if every series commit fixed all of it.

The deterministic `commit-fix` reaper step stages the edited files and
creates or amends the git commit. Use `--assisted-by TEXT` to control the
exact `Assisted-by:` trailer; otherwise kres derives one from the slow
model.

## Build

The build step derives object targets from the actual git diff. It adds
all changed `.c`/`.S` objects it can map and skips targets disabled by
the current Kconfig. Header-only, documentation-only, Kconfig-only, or
otherwise non-object changes can skip cleanly.

Build failures are triaged. Patch-caused failures branch back to patch
writing. Environmental or pre-existing failures should not force source
churn.

## Review And Correction

The fix review step is a lensed review over the generated commit. Lenses
run in parallel and are consolidated into typed defects.

Review defects are split by correction target:

- source, behavior, build, locking, lifetime, or API contract defects go
  back to patch writing;
- commit-message-only defects go back to commit-message writing.

kres maintains a Rust-owned review ledger. The ledger is sent to the
coding agent and review agent on later passes so repeated complaints can
be tracked as open, resolved, disputed, or superseded. A correction pass
should either change the patch/message or provide source evidence that a
review complaint is wrong.

The loop is healthy when each pass closes or sharpens a concrete review
item. It is unhealthy when it repeatedly asks the same source question,
repeats the same review complaint without new evidence, or churns the
commit message without making it more kernel-reviewable.

## Publish

After a todo passes build and review, `publish-fix` writes the generated
patch to the artifact directory when one is available.

Patch filenames are deterministic:

- `auto-generated-fix.diff`
- `auto-generated-fix-2.diff`
- `auto-generated-fix-3.diff`

The publish step also records the patch names in `metadata.yaml` under
`auto_generated_fixes:` and updates `summary.md`. A successful valid
publish removes stale `invalidation.md` and `partial-invalidation.md`.

## What To Audit Afterward

Review the git commits, not just the published diff files:

```bash
git log --oneline --decorate -5
git show --stat HEAD
git show HEAD
```

For a two-commit series:

```bash
git show HEAD~1
git show HEAD
```

Check:

- the number of commits matches the planned todos;
- each commit fixes only its todo;
- each commit is reviewable on its own;
- the `Fixes:` trailer is present only when proven;
- the message explains the bug and fix relative to the commit's parent;
- review objections were fixed or explicitly disputed with evidence;
- published `auto-generated-fix*.diff` files match the commits.

Useful run artifacts:

- `/tmp/kres-fix/report.md`: high-level workflow report;
- `/tmp/kres-fix/session.json`: resumable state;
- `.kres/logs/<session>/code.jsonl`: agent prompts and replies;
- `.kres/logs/<session>/main.jsonl`: tool-fetch and reaper activity.

