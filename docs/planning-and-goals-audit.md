# Planning and Goal Ownership

This document describes how kres currently creates plans, goals, and follow-up
work. It is an implementation guide, not a proposed architecture. Detailed
workflow step ordering, retries, review lenses, and fix publication semantics
belong in [workflow.md](workflow.md).

## Ownership

kres has three different kinds of plans. They are related, but they are not
interchangeable:

| Plan | Owner | Persistence | Purpose |
|------|-------|-------------|---------|
| Session plan | Planner/main model and `TaskManager` | `session.json` | Decompose an open-ended prompt into linked todo items and report progress |
| Workflow DAG | `configs/workflows/<name>.json` | Workflow run state | Define deterministic steps, dependencies, conditions, retries, and artifacts |
| Fix-series plan | Primary slow model and fix workflow | `fix-series.json` | Decompose a fix target into ordered, independently testable commits |

The workflow JSON is authoritative for `/fix`, `/review`, `/triage`, and
`/validate`. `/review` uses that JSON to define its prompt contract and lenses,
then deliberately executes through the session task/todo loop. The other three
commands use the workflow executor directly.

## Generic Session Planning

For an ordinary prompt, kres asks the main/planner model for a concrete
completion goal and a structured plan. A `Plan` contains:

- the original prompt and completion goal;
- a mode;
- ordered steps with stable ids, titles, statuses, and linked todo ids.

Each runnable todo becomes a `Task`. The fast agent gathers evidence, the
deterministic data-fetch path services typed requests, and the slow agent
performs the final analysis. Follow-ups emitted by completed tasks pass through
the todo agent, which deduplicates them against both pending and
completed work.

The goal agent evaluates the session completion goal after task completion. If
the goal is met, pending and blocked work is moved to deferred follow-ups. If it
is not met, only the missing work should remain or be added. Goal decisions and
todo updates are structured model outputs; Rust does not infer control state
from analysis prose.

`TaskManager::sync_plan_from_todo` derives plan-step status from the linked todo
rows. Completed todo history is retained so later updates do not recreate work
that has already run.

## Review Planning

The shipped review contract lives in `configs/workflows/review.json`. It
defines the review prompt and parallel slow-agent lenses. Review execution then
uses the regular task/todo machinery so typed lens follow-ups become ranked
work for later review turns.

For a whole-file review, kres performs the scan before defining the review
goal:

1. `gix` follows target renames and builds one target-file diff from immediately
   before the oldest relevant change in the six-month window to the working-tree file.
   One low-effort slow-agent call assesses that net diff; oversized diffs are
   partitioned losslessly at hunk or line boundaries, assessed in parallel, and merged. The completed assessment has a durable
   checkpoint that is reused only by explicit resume.
2. `file_survey` obtains the file's defined and referenced functions.
3. The primary slow model combines the change ratings with structural evidence,
   retains external major-risk research only when the target interacts with that function,
   and emits per-function plus whole-file risk ratings. Rust prevents the combined
   function ratings from falling below their change ratings and keeps the file rating
   at least as high as its highest function rating.
4. The review planner uses that ranked inventory to form the initial goal,
   plan, and todo list.

The scanned-file plan has at most five stages: three or four semantic groups
and a final cross-contract reconciliation stage. The last stage must reconcile
the reviewed groups against the complete ranked inventory, rather than treating
the highest-ranked functions as exhaustive coverage.

Commit and range reviews plan around changed semantic contracts, not only
edited lines. When evidence for callers, readers, writers, callbacks, setup,
history, or shared helpers is missing, lenses emit typed follow-ups. Negative
coverage claims require concrete search, source, callgraph, or history evidence.

Each review turn runs the configured lenses in parallel over the same gathered
context. Their findings and follow-ups are merged deterministically before the
todo and goal agents run. A clean lens means it is confident for its assigned
bug class; it does not mean that the first context bundle happened not to prove
a defect.

## Fix-Series Planning

The fix workflow asks the primary slow model to research the target and produce
an ordered commit series. Planning is revisioned: later research or validation
may refine the series while preserving immutable facts from the original target
record.

The workflow owns this state in `fix-series.json`. Each entry records scope,
dependencies, acceptance criteria, status, and the commit identity once
created. Individual commits run through implementation, build, triage, and
review gates. Publication is based on the exact persisted commit list after the
final series assessment, not on an inferred range or per-todo patch fragments.

Outer-series resume restores `fix-series.json`; inner workflow resume restores
the corresponding per-todo execution snapshot. Exact behavior and artifact
names are documented in [workflow.md](workflow.md).

## Static Workflows

`/triage` and `/validate` are workflow-DAG operations. Their JSON definitions
already provide the plan, so they do not need the open-ended session planner or
goal agent. Conditions and validators decide whether the workflow advances or
fails.

## Persistence and Termination

`session.json` stores the session plan, todo list, deferred list,
`completed_run_count`, and last prompt. On resume, in-progress session work is
returned to pending because its executor no longer exists. Deferred work is
also restored as pending so continuation can dispatch it.

Workflow execution state is separate from `session.json`. The workflow runner
persists completed steps and validated outputs so resumable workflows do not
repeat successful side effects.

`--turns` limits completed task runs, not model calls or workflow steps. At a
positive cap, kres stops launching work, preserves pending follow-ups, and lets
already-running tasks publish their results. With the unlimited default,
termination depends on follow-up exhaustion, the goal configuration, and
`--follow`; see [turns-and-follow.md](turns-and-follow.md).

## Current Limitations

- Generic planning starts before source gathering, so its first decomposition
  is based on the prompt and configured skills rather than code evidence.
- Review planning has a richer whole-file bootstrap than generic planning.
- Todo reconciliation and goal evaluation are separate structured calls, so
  transient model failure can delay convergence even though persisted work is
  retained.
- Planning, todo, and goal calls use configured agent roles rather than a
  separately configurable planning role.

These are implementation constraints, not alternate workflow contracts. Any
change to them should update this document and, when workflow behavior changes,
[workflow.md](workflow.md).
