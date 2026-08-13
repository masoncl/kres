# Workflows

kres ships four workflows — `/review`, `/fix`, `/triage` and
`/validate`. This page explains what they are for, what a run actually
does, and what you get at the end of one. Field-level contracts and the
reasoning behind each rule live in
[workflow-internals.md](workflow-internals.md).

## What a workflow is

A workflow is a fixed pipeline with a name. It is an ordered set of
steps: some ask a model a question, some run a command such as `git
commit` or `make`. Everything between the steps — what evidence gets
fetched, whether an answer is usable, which step runs next, when the run
is finished — is decided by kres, not by a model.

That is the whole point of the design. A single long agent conversation
decides for itself when it is done, and it is the same agent that
decided the work was good. In a workflow, each step is asked one scoped
question, has to answer it in a form kres can check, and does not choose
what happens next. When a review says the patch is wrong, it is kres
that routes back to the patch writer; when a validation cannot prove a
bug is reachable, it is kres that refuses to let the run call it real.

The practical consequences:

- **Model steps decide, command steps do.** Commits, builds, patch
  publication and finding-status updates run without a model, and model
  steps are not permitted to run mutating git at all. What landed in
  your tree was put there deterministically.
- **A step's answer has to be usable.** If it is malformed, or it
  claims something the run has not established, the step is asked
  again with the specific problem, or the run routes somewhere that can
  fix it.
- **A run can be resumed.** Progress is recorded as it happens, so an
  interrupted run picks up where it stopped rather than redoing work
  that already had an effect.

## The four workflows

| command | you point it at | what it does | what you get |
|---|---|---|---|
| `/review <target>` | a source file, a directory, a commit, or a range | several reviewers hunt different bug classes in parallel, then the run keeps going on the leads they raise | findings, accumulated across turns |
| `/fix <target>` | a finding directory, or just a description of a bug | research, write the patch, write the changelog, commit, build, review, refine, publish | a real git commit in your tree, plus a patch file next to the finding |
| `/triage <finding-dir>` | one exported finding | one pass: is this a defect, how bad, and why | `summary.md` plus synchronized status and severity in the finding |
| `/validate <finding-dir>` | one exported finding | the hostile version of triage: try to prove the finding wrong | a verdict two different models tried to break, and rewritten finding artifacts |

Each is available two ways — as a slash command in the REPL, or as
`kres --prompt "fix: <target>"` from a script. Both entry points run the
same pipeline; there is no separate "batch mode" behaviour to learn.

### `/review` — find bugs you did not know to look for

A review turn gathers source once and then asks several reviewers the
same question from different angles: memory and lifetime, bounds and
arithmetic, races, general correctness, and — for a commit or range —
whether the commit message and comments are actually true. None of them
sees what the others concluded, which is what stops them converging on
the first plausible answer. A consolidator merges the results and drops
duplicates.

A turn rarely finishes the job, and it is not supposed to. Reviewers
that need more evidence say so, and those requests become the next
turn's work: kres deduplicates them against what has already been
covered, ranks what is left, and dispatches the next batch. The run
continues until nothing is left to chase or you hit the turn cap you
set.

For a named source file, kres does some homework before the first
reviewer runs: it builds one diff covering the last six months of change
to that file, has a model rate every function in it for risk, and hands
that ranking to the planner. The first turn is therefore aimed at the
parts of the file that recently changed, rather than starting at line 1.

### `/fix` — from a bug report to a commit

`/fix` is the longest pipeline, and the only one that changes your
source tree. Given a finding directory or a plain description, it runs:

```text
  research ──── is this bug real at the current HEAD, and what would fix it?
     │
     ├── not real, with proof ─────> mark the finding invalid, stop
     ├── cannot tell ─────────────> mark it unconfirmed, stop
     └── real
           │
           ▼
     write the patch ─> write the commit message ─> commit ─> build
           ▲                                                    │
           │                                                    ▼
           └──────────── refine ◄──── review (several reviewers,
                                              in parallel)
                                                    │
                                              clean │
                                                    ▼
                                                 publish
```

Research is a gate, not a formality: it has to prove the bug exists at
your workspace HEAD, not merely that the finding says so, and a run that
cannot prove it stops rather than patching speculatively.

After the first commit, the run is a loop. The reviewers judge the whole
patch every round — it is amended in place, never stacked — and when
they disagree with each other, a reconciliation step resolves the
contradiction into one instruction set instead of handing the patch
author two incompatible demands. It also keeps a list of objectives:
what must become true of the patch, tracked across rounds even as the
review rewords the complaint, so a concern cannot be raised ten times
and answered zero. The loop ends when the review comes back clean, when
the run judges the remaining objections answerable and publishes with
them recorded, or when it hits the round cap with a working patch left
in your tree.

One bug report can need more than one commit. `/fix` plans that up
front, runs the whole pipeline once per commit, and finishes with an
assessment of whether the series actually fixed the reported bug.

### `/triage` — classify one finding

One pass over an exported finding. It reads the finding, gathers
whatever source it needs, and writes `summary.md` with a status, a
severity and the reasoning, updating the finding's metadata to match.
The status vocabulary and the decision tree behind it are shared with
`/validate`, so the two cannot drift.

This is faster than /validate, but often incomplete.  /validate is much more
effective.

### `/validate` — try to prove the finding wrong

`/validate` exists because a finding that reads convincingly is not the
same as a bug. It is deliberately hostile to the thing it is handed, and
a run is not over when it reaches a verdict — it is over when that
verdict has survived two attempts to break it.

```text
  claims ────── is each individual statement in the finding true?
     │          (checked against your source, not against the finding)
     ▼
  conjunction ─ can the surviving preconditions all hold on ONE
     │          execution?  (this is where most false positives die)
     ▼
  verdict ───── close the remaining questions, decide status and
     │          severity, rewrite the finding's files
     │
     ├── anything other than "real and reachable" ────────> done
     │
     └── "real and reachable"
              │
              ├─> a refuter on the main model      ─┐
              └─> a refuter on a DIFFERENT model    ├─> broke it?
                                                    │   back to the verdict
                                                    └─> survived? done
```

The middle step is the one that earns its keep. Every claim in a finding
can be individually correct while the conjunction is empty: one entry
says a path cannot run when a feature is active, another says the code
that reads the state only exists when that same feature is compiled in
and enabled. Nothing checks that until something asks whether all the
preconditions can hold at once.

The refuters are asked to break the finding, not to assess it, and the
second one runs on a different model — agreement between two model
families is worth more than one model re-reading its own reasoning.
Either of them succeeding sends the run back to reconsider the verdict.

`/summary` runs `/validate` over every finding in the store before
rendering, which is how a summary ends up containing only findings that
survived.

## How a step runs

Every model step is three phases, not one:

1. **Gather.** A fast model says what it needs — the source of a
   function, a range of a file, the callers of a symbol, a bounded git
   query, a grep. It does not get to run anything.
2. **Fetch.** kres runs those requests itself and collects the results.
   No model touches a tool.
3. **Answer.** The step's own model — fast for cheap classification,
   slow for the reasoning that matters — answers the actual question
   with everything gathered attached.

The split is what makes the expensive model's budget go on thinking
rather than shopping. It also means the evidence belongs to kres rather
than to a conversation: the same gathered source can be shared by every
reviewer in a parallel fan-out, reused by a later step that depends on
it, and cached between calls. Evidence is dropped only when the bytes it
came from actually change.

Each step is allowed a specific set of request kinds, and some are
allowed none at all — a step whose job is to adjudicate what other steps
already found is given everything and told to fetch nothing, because
what it is missing is not evidence.

When an answer comes back unusable — malformed, or missing something the
step was asked for — kres re-asks with the exact problem rather than
guessing at what was meant, and carries over the evidence the failed
attempt had asked for so the retry starts better informed than the
original. Nothing is ever silently trimmed to make a request fit: if the
gathered evidence is too large for the model, the request is abandoned
whole and the step re-gathers, told what it overshot.

## What happens between steps

**Order.** A step becomes eligible when the steps it depends on are
done, and conditions decide whether it actually runs. Skipping is normal
— a clean build skips compile triage, a non-reachable verdict skips the
refuters.

**Parallelism is explicit.** Only steps that declare it fan out, and
they do it over one shared pile of evidence: one gather, N questions,
one merge. This is how `/review` and the `/fix` review round work. In
`/review`, one failed reviewer does not waste the others: their results
are kept and the bug class the failed one covered is re-queued, rather
than being quietly counted as reviewed.

**Gates.** After a step answers, kres checks the answer before letting
the run continue. Some checks are trivial (is this field set), some are
real invariants that no prompt can be trusted with (a validation may not
call a bug reachable while a load-bearing question is still open), and
some ask a second model to judge. A failed check re-runs the step, or
routes the run back to whichever step can fix the problem.

**Loops and caps.** Routing backwards is normal — a defect sends the run
back to the patch author, a refutation sends it back to the verdict.
Every loop has a cap, and hitting one ends the run in a defined state
rather than spinning.

**Side effects happen last.** A step's file writes are staged while its
answer is being checked and only applied once it passes. If the run is
interrupted after a step's effect was recorded but before it was
applied, resuming replays that effect without calling the model again.

## What a run leaves behind

Every run owns a directory: `--results DIR` if you gave one, otherwise a
fresh `~/.kres/sessions/<timestamp>-<pid>/`. Two runs never share one,
so you can run fifty validations in parallel without them overwriting
each other's records.

In it:

- **`report.md`** — the human narrative, plus a trace of the run: which
  steps ran, which were skipped, which checks passed or failed, where it
  branched back, what the build did, whether it published. This is
  usually enough to audit a run without opening a log.
- **`findings.json`** — the canonical findings, for `/review`.
- **`session.json`** — plan, to-do list, deferred work and counters;
  what `--resume` reads.
- For `/fix` and the finding-oriented workflows, the finding directory
  itself is rewritten in place: `summary.md`, `metadata.yaml`,
  `FINDING.md`, and any generated patch.

Full transcripts land under `<cwd>/.kres/logs/<session>/` — every model
call in `code.jsonl` and `main.jsonl`, and every progress line kres
printed in `console.jsonl`, which is what lets you reconstruct why the
scheduler did what it did.

## Tuning a run

- **Models per role.** Fast, slow and main are configured separately in
  `~/.kres/settings.json` or overridden on the command line. Configuring
  a second slow model enables the second-opinion steps — the different-
  model refuter in `/validate`, and a supplemental reviewer elsewhere.
- **`--turns N`** caps how much work a session launches. Already-running
  work is allowed to finish and publish; leftover leads are saved rather
  than dropped.
- **`--max-parallel N`** (default 10) is how many tasks run at once.
- **`--results DIR`** puts the run's artifacts somewhere you chose.
- **`--resume`** re-enters a previous run from its saved state.

To change what a workflow actually does, drop your own copy at
`~/.kres/workflows/<name>.json`; it shadows the built-in one. The
shipped workflows keep their prompts inline in that JSON precisely so
they can be read and edited without touching Rust.

## Going deeper

- [workflow-internals.md](workflow-internals.md) — step-by-step
  contracts, output fields, evals, retry and branch semantics, and the
  measured evidence behind each rule.
- [findings-json-format.md](findings-json-format.md) — the finding
  record itself.
- [planning-and-goals-audit.md](planning-and-goals-audit.md) — who owns
  goals, plans and completion decisions across the workflows.
