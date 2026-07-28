# Planning and Goal-Setting Audit

This document records how kres chooses goals, constructs plans, updates them,
and decides that work is complete. It is a baseline for moving planning and
goal setting to the most capable configured slow model. It describes the
current implementation; it does not propose the migration itself.

## Workflow inventory

Kres has five shipped top-level execution workflows. Only four are JSON files;
the generic task pipeline is implemented by the REPL session and agent
pipeline.

| Workflow | Normal entry points | Execution engine | Current planning mechanism |
| --- | --- | --- | --- |
| Generic task pipeline | ordinary `--prompt`, REPL input | task/todo loop | main-model `define_goal` + `define_plan` |
| Review | `/review TARGET`, `--prompt 'review: TARGET'` | task/todo loop configured by `review.json` | main-model `define_goal` + `define_plan` |
| Fix series | `/fix TARGET`, `--prompt 'fix: TARGET'`, workflow runner | JSON workflow executor | static 16-step DAG plus fast-model `research.fix_plan` |
| Validation | `/validate DIR [WORKSPACE]`, `--prompt 'validate: ...'`, workflow runner | JSON workflow executor | static two-step DAG |
| Triage | `/triage DIR`, `--prompt 'triage: DIR'`, workflow runner | JSON workflow executor | static one-step DAG |

Operator-defined JSON workflows use the same executor as fix, validation, and
triage. They have a statically authored step graph unless their own prompts
produce a workflow-specific structured plan like fix's `fix_plan`.

Summary rendering, exporting, `kres test`, and `kres turn` are commands, not
multi-step workflows, and are outside this audit.

## Two unrelated meanings of “plan”

There are currently two planning systems. They are not interchangeable.

### Session plan

`kres_core::Plan` is used by the task/todo loop. It contains the original
prompt, derived goal, classified mode, creation time, and 3–12 `PlanStep`
records. Steps are linked to todo items through `step_id` and `todo_ids` and
are persisted in `session.json`.

The session plan is visible to the main fetcher, fast agent, slow agent, todo
agent, and goal judge. It is created only for an operator-submitted prompt.
Pipeline-generated followup tasks inherit it rather than creating a new plan.

### Workflow plan

A JSON workflow's `steps`, `depends_on`, `run_if`, eval policy, and completion
expressions form a static executable plan. The workflow executor does not call
`define_goal`, `define_plan`, `check_goal`, or the todo agent. Completion is
decided deterministically from step state and the workflow's expressions.

Fix has a third, workflow-local layer: `research.fix_plan`. That is an ordered
array of independently committable fix todos generated at runtime. Rust runs
the static fix workflow once per todo. It is not a `kres_core::Plan`, is not
shown by `/plan`, and is not judged by `check_goal`.

## Task/todo goal lifecycle

The generic and review workflows share this lifecycle.

1. `Session::submit_prompt_inner` receives an operator prompt.
2. `define_goal` runs on `GoalClient`, which reuses the configured **main**
   model and client with the dedicated `prompts/goal.txt` system prompt.
3. The same response both defines a completion criterion and classifies the
   task as `audit`, `generic`, or `coding`.
4. `define_plan` runs as a second independent call on that same main model.
   It receives the original prompt, derived goal, mode, and any existing
   session plan.
5. The task runs. Followup tasks inherit the cached session goal and plan.
6. After a task is reaped, the configured **todo** model deduplicates
   followups, links todos to plan steps, and may rewrite the plan.
7. `check_goal` runs on the main model after each completed non-coding task.
   It receives the operator prompt, derived goal, accumulated analyses,
   recorded findings, and current plan.
8. A met goal normally drains pending work to the deferred list. Lensed review
   is an exception: concrete review followups keep the next turn alive.

`GoalClient` exists only when a main-agent configuration was successfully
loaded. Without it there is no derived goal, mode defaults to generic (except
commands such as `/review` that force a mode), no new session plan is created,
and no goal checks run. Goal or plan parse/call failure is non-fatal. For that
prompt, `define_goal` failure skips new planning and goal checks while any old
session plan remains installed; `define_plan` failure likewise leaves the old
plan in place. A failed or unparseable `check_goal` currently fails open by
returning `met=true`.

The goal calls use the main model's limits but cap output at 8,000 tokens. They
do not use prompt caching because each call is treated as one-shot.

## Session plan creation and mutation

The initial planner is blind to source unless source text happened to be in
the operator prompt. `define_plan` cannot call tools and runs before the first
fast/main gathering cycle. Its audit prompt explicitly asks for file, symbol,
subsystem, or code-path decomposition rather than duplicating the automatic
correctness lenses.

There are four possible plan writers:

1. An embedded `PLAN:` JSON block in a prompt. This bypasses `define_plan` but
   still uses the derived goal and mode for plan metadata.
2. The main-model `define_plan` call on each operator-submitted prompt.
3. The first slow synthesis for that prompt, if it returns a `plan` rewrite.
4. The todo model after each completed task, including goal-not-met updates.

The third writer has an important exception: lensed audit runs discard all
slow-lens plan rewrites because merging competing rewrites would churn step
IDs. Consequently normal `/review` planning is initially main-owned and later
todo-owned; the advanced slow review models cannot revise the global plan.
Generic and coding tasks use one slow synthesis, so their first slow call can
rewrite the plan after seeing gathered source.

Rewrites contain only `steps`. Rust preserves the plan's original prompt,
goal, mode, and creation time, normalizes missing or duplicate IDs, and carries
forward workflow-authored step context for surviving IDs. Replacing a plan
clears todo `step_id` values that refer to removed steps.

Plan status is a todo rollup, not an independent judgment. The todo agent links
work to steps. `sync_plan_from_todo` marks a step complete when its linked todos
are terminal. An initial step with no linked todo does not become complete just
because the first slow analysis covered it; this makes plan quality and todo
linkage jointly load-bearing.

## Generic task pipeline

Generic input is not backed by a JSON workflow. `define_goal` selects one of
three modes:

- `generic`: fast/main gathering followed by one slow analysis call.
- `coding`: fast/main gathering followed by one slow coding call that may emit
  file writes or edits.
- `audit`: the review-style lensed path when the free-form prompt explicitly
  asks for a correctness or defect audit.

The global goal and plan are both authored by the main model before gathering.
The first single slow synthesis may rewrite the plan for generic and coding
work. Subsequent changes come from the todo model. `check_goal` uses the main
model and is skipped for coding results.

Therefore generic work already gives the slow model one source-informed chance
to correct the plan, but the initial goal, mode, decomposition, later plan
maintenance, and completion decisions remain on main/todo models.

## Review workflow

`configs/workflows/review.json` owns review prose, lenses, lens conditions, and
consolidation rules. Its normal entry points intentionally do not execute that
file as a one-shot workflow DAG. Rust converts its single `investigate` step
into a prompt plus session lenses, forces audit mode, and submits it to the
task/todo loop.

The global goal and plan are therefore created by the main model exactly as in
the generic pipeline. Each task then performs shared gathering, parallel slow
lenses, and consolidation. Typed followups go through the todo model and become
the next review tasks.

The slow lenses receive the plan but cannot rewrite it. The consolidator also
does not author the plan. Only the main model's initial plan and todo-model
rewrites shape the review sweep. `check_goal` is main-model owned, although
remaining typed review followups override an early met judgment and keep the
review moving.

`kres run-workflow workflow-id:review` can execute the JSON step directly, but
that is not the supported `/review` or `--prompt 'review: ...'` behavior. The
supported path is the task/todo loop so followups produce prioritized future
turns.

## Fix-series workflow

`configs/workflows/fix.json` is a statically authored 16-step graph containing
research, status recording, lore search, patch writing, commit-message writing,
commit, build, compile triage, review, orchestration, result recording, and
publication. Its `completion` expressions are the workflow's explicit success
and failure criteria. It does not use the session goal judge or session plan.

Before running the full graph, `run_fix_series_driver` clones the workflow and
runs only `research`, `invalidate`, and `unconfirm` as a planning/status pass.
When research confirms the finding, `research.fix_plan` must contain one or
more typed todos with stable ID, title, scope, affected files and symbols, fix
contract, rationale, and dependencies.

The research step is declared `agent: fast`. In the production AgentRunner
path it still gets normal fast/main gathering, but its final synthesis also
uses the **fast model**. Thus the runtime fix decomposition and fix contracts
are currently authored by the fast model, not the slow model.

Rust validates the generated plan, rejects duplicate or empty IDs and forward
dependencies, then executes the complete static workflow once per todo in
dependency order. Per-todo research may revise the current todo, preserving its
ID, when the bug remains proven but the proposed fix contract is incomplete.
Those revisions are again generated by the fast-model research step and are
bounded to three attempts.

Later decisions are distributed across roles: code/slow writes patches and
commit messages, slow lenses review, fast consolidation and the fast
`orchestrator` choose retry routing, and deterministic reaper steps perform
mutations. These decisions refine or execute the fix plan but do not replace
the top-level series plan.

## Validation workflow

`configs/workflows/validate.json` has a static two-step plan:

1. `validate-claims` uses the fast model in coding mode to gather evidence and
   produce structured supported, contradicted, and unresolved claims.
2. `validate-reachability` uses the slow model in coding mode, inherits gathered
   data from the first step, closes material questions, writes finding
   artifacts, and emits the final structured verdict.

There is no generated goal, dynamic plan, todo list, or goal check. The JSON
prompts are the goal contract; field-check evals and terminal step state decide
completion. The slow step gets the advanced model, but the decomposition is
fixed at two broad stages and cannot adapt its plan to the finding before work
starts.

## Triage workflow

`configs/workflows/triage.json` has one slow coding step. Its embedded prompt
and output schema jointly define the goal: classify the finding, write
`summary.md`, update metadata and `FINDING.md`, and emit `triage_coding`.
A field-check eval verifies the required artifacts and structured fields.

There is no generated goal, plan, todo list, or goal check. The most advanced
slow model performs the substantive work, but no separate planning phase
exists.

## Model ownership matrix

| Decision | Generic | Review | Fix | Validation | Triage |
| --- | --- | --- | --- | --- | --- |
| Define outcome goal | main | main | static JSON completion/prompt | static JSON prompt/eval | static JSON prompt/eval |
| Build initial plan | main | main | static DAG; fast creates `fix_plan` | static DAG | static one-step DAG |
| Source-informed plan revision | first slow call, then todo | todo only | fast per-todo research | none | none |
| Decide followup priority | todo | todo | static DAG + fast orchestrator | static dependency | n/a |
| Decide completion | main goal judge | main goal judge plus followup guard | Rust expressions/evals | Rust step/eval state | Rust step/eval state |
| Deep analysis | slow | parallel slow lenses | mixed by step | slow final pass | slow |

## Findings relevant to a slow-planner migration

1. There is no single planning abstraction to switch. Session plans, JSON DAGs,
   and `fix_plan` have different schemas, lifecycles, and owners.
2. The main model currently combines goal definition with mode classification.
   Moving goal authorship must preserve or deliberately separate routing.
3. Initial session planning happens before source gathering. Changing only the
   selected model would improve reasoning quality but would not make the plan
   source-informed.
4. Review explicitly suppresses slow plan rewrites. A slow planner needs one
   authoritative rewrite point rather than accepting five competing lens plans.
5. Todo-plan linkage drives visible completion. A new planner must preserve
   stable step IDs and provide a way to account for the initial task's coverage.
6. `check_goal` fails open. Moving it to an expensive model without revisiting
   failure semantics would retain the risk of premature completion.
7. Fix planning is the clearest model mismatch: the fast model authors both the
   series decomposition and the fix contracts even when a stronger slow model
   is configured.
8. Validation and triage have no adaptive planning hook. Using slow planning
   there requires either a preflight plan artifact consumed by the static DAG
   or a general executor-level goal/plan phase.
9. Workflow-runner CLI prompts reject main/todo model overrides because those
   roles are not wired into the executor. A unified planner should have an
   explicit model-selection rule that works in both REPL and executor paths.
10. Goal, plan, todo rewrite, and completion calls are logged, but their labels
    should remain distinct when model ownership changes so cost and latency can
    be audited independently.

## Primary implementation references

- `kres-agents/src/goal.rs`: `GoalClient`, `define_goal`, `define_plan`, and
  `check_goal`.
- `kres-agents/src/prompts/goal.txt`: goal, mode, plan, and completion prompt
  contract.
- `kres-repl/src/session.rs`: session lifecycle, plan writers, todo updates,
  goal checks, and review dispatch.
- `kres-core/src/plan.rs`: session plan schema, normalization, rewrites, and
  todo linkage.
- `kres-agents/src/pipeline.rs`: first-slow rewrite behavior and review-lens
  rewrite suppression.
- `kres-repl/src/workflow.rs`: review prompt adaptation and fix-series driver.
- `kres-agents/src/workflow_runner.rs`: JSON workflow role/model routing.
- `configs/workflows/{review,fix,validate,triage}.json`: shipped executable
  workflow contracts.
