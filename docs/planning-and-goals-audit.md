# Planning and Goal-Setting Audit

This document records the current implementation of goal selection, planning,
todo reconciliation, and completion across every kres workflow. Review has
begun the migration to slow-model planning; the other workflows retain their
existing ownership. The final review section distinguishes implemented state
from remaining work.

Current as of 2026-07-30, including the review-planning, staged-dependency, and
todo-reconciliation implementation.

## Workflow inventory

Kres has five shipped top-level execution workflows. Only four are JSON files;
the generic task pipeline is implemented by the REPL session and agent
pipeline.

| Workflow | Normal entry points | Execution engine | Current planning mechanism |
| --- | --- | --- | --- |
| Generic task pipeline | ordinary `--prompt`, REPL input | task/todo loop | main-model `define_goal` + `define_plan` |
| Review | `/review TARGET`, `--prompt 'review: TARGET'` | task/todo loop configured by `review.json` | primary-slow `define_goal` + staged `define_plan`; primary-slow todo/goal review |
| Fix series | `/fix TARGET`, `--prompt 'fix: TARGET'`, workflow runner | JSON workflow executor plus outer series driver | static 19-step DAG plus primary-slow `research.fix_plan`, revisioned outer state, and final series assessment |
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
may carry `depends_on` step IDs. Plans and linked todos are persisted in
`session.json`. Review's primary-slow prompt policy currently asks for at most
six staged steps; generic plan prompting retains the broader historical range.

The session plan is visible to the deterministic fetcher, fast agent, slow
agent, todo agent, and goal judge. It is created only for an
operator-submitted prompt. Pipeline-generated followup tasks inherit it rather
than creating a new plan.

### Workflow plan

A JSON workflow's `steps`, `depends_on`, `run_if`, eval policy, and completion
expressions form a static executable plan. The workflow executor does not call
`define_goal`, `define_plan`, `check_goal`, or the todo agent. Completion is
decided deterministically from step state and the workflow's expressions.

Fix has a third, workflow-local layer: `research.fix_plan`. That is an ordered
array of independently committable fix todos generated at runtime by the
primary slow model. Rust runs the static fix workflow once per todo and stores
the authoritative tracked plan, statuses, per-todo revision counts, and plan
revision in `fix-series.json`. It is not a `kres_core::Plan` and is not shown by
`/plan`. After every todo completes, a final primary-slow `series-assessment`
checks the complete commit sequence against the original target.

Planning and execution verification are mode-scoped responsibilities within
the shared JSON `research` contract. In planning mode, research owns the series
decomposition. In per-todo mode, it verifies the selected todo and may submit a
revision-checked mutation of current or pending work. Completed work is
immutable, structural revisions are bounded, and stale revisions are rejected.
Both modes use slow synthesis after normal fast gathering and deterministic
fetches.

The outer fix-series snapshot is atomic and resumable. An interrupted
`InProgress` todo resets to `Pending`, while the matching inner workflow
snapshot resumes its already-settled commit/build/review steps instead of
restarting the todo against a partially modified tree.
Snapshot directories include the stable todo identity, outer plan revision,
and todo revision so a revised or split todo cannot resume an older shape's
inner workflow state.
If interruption occurs before outer state exists, resume continues the planning
snapshot. Before dispatching pending work, the driver validates HEAD against the
completed prefix, with a narrow allowance for a matching inner snapshot that
already owns the next commit.

The outer state stores immutable `original_bugs` separately from mutable,
commit-oriented todos. Existing artifact `metadata.bugs` is authoritative;
prose targets use the planning model's typed `bug_inventory`. Reconciliation
rewrites artifact metadata from that immutable inventory, so structural
revisions cannot erase bug coverage. The outer snapshot records the pre-series Git HEAD,
every completed todo's commit SHA read from the repository after successful
execution, and its typed review outcomes. Before final assessment, Rust proves
that those commits form an exact first-parent chain from the recorded base to
the current HEAD. Per-todo runs do not publish finding results or patch files.
After the final assessment passes, JSON-defined reaper steps record the final
outcomes and publish the exact persisted commit list. A builtin evaluator
requires final outcomes to cover every authoritative todo exactly once with a
valid disposition and non-empty evidence before completion or plan revision is
accepted.

The patch-cycle `orchestrator` also uses the primary slow model. It retains the
existing typed branch contract and executor reset semantics, but now shares the
same model quality tier as initial planning and final completion rather than
delegating correctness routing to the fast model.

## Task/todo goal lifecycle

Generic and review prompts share the session machinery, but no longer share
model ownership. The following lifecycle describes generic/coding prompts;
review differences are documented in the review section.

1. `Session::submit_prompt_inner` receives an operator prompt.
2. `define_goal` runs on `GoalClient`, which reuses the configured **main**
   model and client with the dedicated `kres-agents/src/prompts/goal.txt`
   system prompt.
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

The generic `GoalClient` exists only when a main-agent configuration was
successfully loaded. Without it there is no derived goal, mode defaults to
generic (except commands such as `/review` that force a mode), no new session plan is created,
and no goal checks run. Goal or plan parse/call failure is non-fatal. For that
prompt, `define_goal` failure skips new planning and goal checks while any old
session plan remains installed; `define_plan` failure likewise leaves the old
plan in place. A failed or unparseable `check_goal` currently fails open by
returning `met=true`.

The goal calls use the main model's limits but cap output at 8,000 tokens. They
do not use prompt caching because each call is treated as one-shot.

Review attaches separate goal and todo clients backed by the primary selected
slow model. Those clients use the same structured calls, but they do not fall
back to the generic main/todo clients. Gathering is no longer main-model owned
in either path: the fast agent emits typed requests and deterministic MCP/local
fetchers execute them.

## Session plan creation and mutation

The initial planner is blind to source unless source text happened to be in
the operator prompt. `define_plan` cannot call tools and runs before the first
fast gathering cycle. Its audit prompt explicitly asks for file, symbol,
subsystem, or code-path decomposition rather than duplicating the automatic
correctness lenses.

For generic/coding work there are four possible plan writers:

1. An embedded `PLAN:` JSON block in a prompt. This bypasses `define_plan` but
   still uses the derived goal and mode for plan metadata.
2. The main-model `define_plan` call on each operator-submitted generic/coding
   prompt.
3. The first slow synthesis for that prompt, if it returns a `plan` rewrite.
4. The todo model after each completed task, including goal-not-met updates.

Lensed audit runs discard slow-lens plan rewrites because merging competing
rewrites would churn step IDs. Review instead has one primary-slow initial plan
writer and one primary-slow todo reconciliation writer. Generic and coding
tasks use one slow synthesis, so their first slow call can rewrite the plan
after seeing gathered source.

Rewrites contain only `steps`. Rust preserves the plan's original prompt,
goal, mode, and creation time, normalizes missing or duplicate IDs, and carries
forward workflow-authored step context and `depends_on` edges for surviving
IDs. Replacing a plan clears todo `step_id` values that refer to removed steps.

Plan status is a todo rollup, not an independent judgment. The todo agent links
work to steps. `sync_plan_from_todo` marks a step complete when its linked todos
are terminal. An initial step with no linked todo does not become complete just
because the first slow analysis covered it; this makes plan quality and todo
linkage jointly load-bearing.

## Generic task pipeline

Generic input is not backed by a JSON workflow. `define_goal` selects one of
three modes:

- `generic`: fast gathering plus deterministic tool execution followed by one
  slow analysis call.
- `coding`: fast gathering plus deterministic tool execution followed by one
  slow coding call that may emit file writes or edits.
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

Review goal, initial plan, post-task todo reconciliation, and `check_goal` use
dedicated clients backed by the primary selected slow model. Comparison slow
models remain analysis variants and do not author competing plans. There is no
fallback from these review clients to the configured generic main/todo models.

For named source files, bootstrap performs one file survey and one non-lensed
slow ranking call before `define_goal` or `define_plan`. Both planning calls see
the ranked function inventory. The initial slow plan is then converted into
stable linked todos before review work starts. `PlanStep.depends_on` becomes
scheduler-enforced todo dependencies. The planner prompt requires three or four
source-informed semantic path/contract groups and one final cross-contract
completeness step depending on those groups, with at most five steps total.
Rust preserves and schedules the returned dependency graph, but does not yet
strictly validate this review-specific shape.
This replaces the nine-way cold-cache fanout observed in `kres-sol4`: the
orientation task populates shared caches before bounded parallel work starts.

After the initial scan, each semantic task performs fast gathering,
deterministic MCP/local tool execution, parallel slow lenses, and consolidation.
The slow lenses receive the plan but cannot rewrite it. Typed followups go
through the primary-slow todo client and become future review tasks. The
primary-slow goal client then checks completion;
concrete review followups still override an early met judgment.

Todo identity and execution state are deterministic rather than model-owned.
On successful reap Rust marks the dispatched stable todo ID done, preserves its
step/dependency links, keeps running siblings in progress, restores omitted
plan-linked work, canonicalizes exact-name rewrites to existing IDs, and drops
title-derived duplicates. Errored tasks remain retryable. This closes the
`kres-sol4` failure where completed work was reopened and 9 todos expanded to
18 and then 20 rewritten rows.

Fast gathering validates the response before dispatch. It rejects raw prose,
non-array `followups`, malformed entries, unsupported request types, empty
names/reasons, empty supplied paths, and empty skill reads. The same fast model
gets one schema-repair attempt; a second invalid response fails the task.
Supported deterministic kinds are `source`, `type`, `callers`, `callees`,
`search`, `grep`, `read`, `file`, `find`, `git`, `make`, `meson`, `cargo`,
`bash`, `lore`, and `question`. Semcode failures and unparseable results fall
back to local search/read evidence rather than establishing absence.

Review persistence still uses the normal `SessionState`: plan, linked todo
state, deferred work, completed count, and last prompt. In-progress work resets
to pending on resume. Calls have the generic `define_goal`, `define_plan`,
`todo_update`, and `check_goal` stream labels, but are not identified or charged
as a distinct review-planning role.

The current review planning policy is not JSON-owned. `build_session` appends
review-specific policy text to the embedded goal and todo system prompts in
`kres-repl/src/session.rs`; `review.json` owns the review task prompt, lenses,
and consolidation contract but does not define the goal/plan/todo policy. This
is an implementation gap relative to the JSON-ownership rule and the target
architecture below, not a second review execution engine.

`kres run-workflow workflow-id:review` can execute the JSON step directly, but
that is not the supported `/review` or `--prompt 'review: ...'` behavior. The
supported path is the task/todo loop so followups produce prioritized future
turns.

## Review planning target and migration status

The implementation now has a primary-slow review planner, stable linked initial
todos, staged dependencies, deterministic gathering, strict gather repair, and
deterministic todo identity reconciliation. The remainder of this section is
the target architecture, not a description of completed code.

Implemented pieces:

- primary selected slow model owns review goal/plan/todo/goal-check calls;
- initial plan steps seed linked `review-<step-id>` todos;
- named-file bootstrap scans inform the initial goal and bounded semantic plan;
- fast followups execute directly through MCP/local fetchers;
- malformed gather envelopes and bad requests are rejected and retried once;
- semcode failures fall back to local grep/read evidence;
- successful task completion and concurrent in-progress state are reconciled
  deterministically by stable todo ID; and
- plan/todo/dependency state persists through the existing `SessionState`.

Still outstanding:

- generalized slow-directed discovery for non-file or ambiguous targets;
- one atomic revisioned planning update replacing todo-then-goal calls;
- typed dispositions for every incoming followup;
- a dedicated planning selector, usage role, log labels, and evidence manifest;
- moving the review goal/plan/todo policy and schemas out of `session.rs` and
  into `review.json` (or a shared workflow schema referenced by it);
- strict Rust validation/retry of the staged plan shape; and
- stale-revision protection for concurrent planner updates.

The completed target has one authoritative **planning agent**.
It is a dedicated, non-lensed role using the most capable selected slow model,
not a sixth review lens and not the consolidator. For review sessions it owns
four pieces of structured state as one unit:

- the completion goal;
- the coverage plan;
- the todo graph that executes the plan;
- the structured decision to continue or stop.

The current migration has replaced review's main-model `define_goal`,
main-model `define_plan`, todo-model plan/todo rewrite, and main-model
`check_goal` ownership with primary-slow clients, but still uses separate goal
and todo calls. The target merges those calls into one revisioned state update.
Review gathering has already stopped using the main LLM as an
interpreter between the fast agent and tools. The fast agent is the sole
gathering reasoner; a deterministic service executes its validated typed
requests through MCP and local tools. Slow lenses, the consolidator,
finding-merger, and scheduler remain otherwise unchanged.

This remains the existing JSON-owned review workflow. The planning prompt,
schemas, and policy must live in `review.json` (or a shared workflow schema
referenced by it), while Rust provides the generic execution and validation
mechanism. It must not become a second review engine or a workflow-local loop
that repeats `investigate` internally. Review tasks and followups continue
through the task/todo scheduler.

### Desired review flow

For a vague prompt such as `review:mm/filemap.c`, the target flow is:

```text
review.json prompt contract + operator target
  -> slow planning agent: define draft goal and requested discovery
  -> fast gather agent: operationalize the discovery requests
  -> deterministic tool service: fetch source, symbols, callers, grep, and history
  -> slow planning agent: create evidence-based goal + plan + initial todos
  -> scheduler: run ready todos (respecting depends_on and parallel cap)
       -> fast gather + deterministic tool execution for that todo
       -> parallel slow review lenses
       -> consolidate findings + typed followups
  -> slow planning agent: reconcile result + followups + current todos + plan
       -> atomically update goal/plan/todos and continue-or-stop decision
  -> scheduler: run the next ready todos
```

The two planning calls are deliberately separate. The first slow call does
not pretend to know the source tree from a vague target; it specifies what it
must learn. The second slow call plans from actual evidence. Subsequent
planning calls review the plan against completed and pending todos rather than
starting from the original prompt again.

### Phase 1: slow-directed planning discovery

Add a workflow-defined `planning.discovery` contract to `review.json`. Its
single slow-agent response should be structured, for example:

```json
{
  "goal_draft": "Audit every reachable correctness path involving mm/filemap.c",
  "scope": {
    "target_kind": "file",
    "target": "mm/filemap.c"
  },
  "discovery": [
    {"type": "read", "name": "mm/filemap.c:1+200", "reason": "inventory file sections"},
    {"type": "grep", "name": "top-level definitions in mm/filemap.c", "reason": "partition coverage"}
  ],
  "discovery_brief": "Inventory implementation sections and external contract boundaries before decomposing the audit."
}
```

`discovery` uses the existing typed `Followup` vocabulary. The planning agent
does not run tools. Rust passes the original prompt, discovery brief, and
requested evidence to a planning-gather entry point on `AgentRunner`. The fast
agent may translate broad requests into concrete reads/searches and may ask
for immediately necessary supporting context; the deterministic service
executes them using the normal semcode-with-local-fallback rules. Invalid fast
output is rejected and retried against the typed response contract; another
model does not repair or reinterpret it.

Planning gather must be separately bounded from per-task `--gather-turns`.
It should return `symbols`, `context`, and a `previously_fetched` evidence
manifest. It must not produce Findings or declare review coverage. Failed,
empty, or missing semcode results use the same local grep/read fallback as
ordinary review gathering.

The bootstrap gather should be broad enough to reveal the target's real
structure but cheaper than a review turn. For a file target, useful evidence
usually includes the file outline/top-level definitions, major section
boundaries, directly referenced public types, and registration/caller edges.
For a commit/range target it starts from diff/stat and the changed semantic
contracts. The workflow prompt states these generic requirements; it must not
hardcode kernel files or a recently missed bug.

### Phase 2: slow-authored initial state

After planning discovery, call the same planning agent with the original
prompt, workflow review invariants, draft goal, gathered evidence, and evidence
manifest. It returns one validated initialization envelope:

```json
{
  "goal": "evidence-backed completion criterion",
  "plan": {
    "steps": [
      {
        "id": "audit-filemap-fault-path",
        "title": "Audit filemap fault and map-pages paths",
        "description": "Trace lookup, locking, reference transfer, and unchanged callers.",
        "evidence_requests": []
      }
    ]
  },
  "todo": [
    {
      "id": "review-filemap-fault-path",
      "type": "question",
      "name": "Review filemap fault and map-pages paths",
      "reason": "Execute the audit-filemap-fault-path coverage step",
      "status": "pending",
      "depends_on": [],
      "step_id": "audit-filemap-fault-path"
    }
  ],
  "decision": {"status": "continue", "reason": "initial review work is pending"}
}
```

The initial plan must be a coverage partition derived from gathered source,
not a restatement of memory-lifetime/bounds/races/general lenses. Every initial
todo is linked to a valid plan step before any review task starts. The current
implementation already provides that linkage from its source-blind initial
plan; this phase retains it while replacing initialization with an
evidence-backed atomic envelope.

The planner may reuse planning-gather evidence as task cache seeds, but a todo
still requests targeted source when the evidence is insufficient. Planning
evidence is orientation, not proof that a review step is complete.

Rust validates this envelope before installing it:

- goal is non-empty;
- plan IDs are unique semantic slugs;
- todo IDs are unique;
- every `step_id` resolves to a plan step;
- every `depends_on` resolves to a todo and is acyclic;
- new todos begin pending unless concrete completed evidence is supplied;
- at least one pending todo exists when decision is `continue`;
- decision cannot be `met` while pending/blocked review work or unresolved
  typed evidence requests remain.

An invalid or unavailable initial planner must retry and then fail the review
startup explicitly. It must not silently fall back to the old main planner or
start a whole-file review with an invented empty plan.

### Phase 3: planner review after each completed task

Replace review's `update_todo` followed by `check_goal` with one planning-agent
reconciliation call. Its input is:

- original operator prompt and immutable review workflow contract;
- current goal and plan;
- current todo list including completed history;
- the just-completed todo and its `step_id`;
- consolidated task analysis and typed Findings;
- typed followups emitted by the lenses/consolidator;
- compact evidence/citation manifest for coverage claims;
- turn count, active task IDs, and configured turn cap.

It returns an atomic `PlanningUpdate`:

```json
{
  "base_revision": 7,
  "goal": "unchanged or explicitly revised goal",
  "plan": {"steps": []},
  "todo": [],
  "followup_dispositions": [
    {
      "key": "source:filemap_map_pmd",
      "disposition": "queued",
      "todo_id": "review-filemap-map-pmd",
      "evidence": ""
    }
  ],
  "decision": {
    "status": "continue",
    "reason": "fault-path coverage is complete; writeback coverage remains",
    "missing": []
  }
}
```

The planner receives and returns the full plan/todo state initially. This is
less clever than a prose-derived delta and makes Rust validation deterministic.
`base_revision` prevents a late response from overwriting state updated by
another concurrently completed task. Planner reconciliation calls are
serialized through one session writer; each call reloads the newest state
before dispatch. A stale response is retried against the new revision.

The planner may deduplicate followups, add or reprioritize todos, repair
dependencies, link todos to steps, split or merge plan steps, and refine the
goal when source evidence proves the initial scope wrong. It must preserve IDs
when intent survives. It must use a new ID when meaning changes, so completed
coverage is never silently reinterpreted.

Every newly emitted lens followup must have one typed disposition: `queued`
with the surviving todo ID, `duplicate` with the equivalent todo ID,
`satisfied` with concrete evidence, or `deferred` with a structured reason.
Rust validates that every followup is accounted for; it does not accept a prose
claim that the planner “handled” the remainder. Completed todo history cannot
disappear, and an in-progress todo cannot be removed, renamed, or moved to a
different plan step by a concurrent reconciliation.

The original operator prompt remains immutable ground truth. A revised goal
may clarify or expand the evidence-backed scope, but cannot narrow an explicit
operator requirement merely to make completion easier.

Plan status remains mechanically derived from linked todo states. The planner
does not mark a plan step done through prose. It marks or preserves the
supporting todos as terminal, and `sync_plan_from_todo` performs the rollup.
Negative coverage claims require citations/search/callgraph/history evidence;
without that evidence the planner creates a typed followup rather than closing
the step.

### Goal and completion ownership

For review, the planning agent becomes the only semantic completion judge.
The separate main-model `check_goal` call is removed from this path. Its
structured decision enum should be:

- `continue`: there is ready, blocked, or newly discovered work;
- `met`: every applicable plan step has evidence-backed terminal coverage and
  no unresolved concrete suspicion or typed followup remains;
- `turn_cap`: Rust has latched the configured cap; preserve new followups as
  deferred without another planning call;
- `blocked`: no ready work exists but explicitly named external/operator
  evidence is required.

Only the model returns `continue`, `met`, or `blocked`; Rust owns `turn_cap`.
Rust consumes these typed fields and never infers completion from analysis
prose. Planner failure must not mean `met`: retain the current state, retry
within a small budget, then surface a session error or leave work resumable.
Rust also rejects `met` while another review task is active, any todo is
pending/blocked/in-progress, or any incoming followup lacks a valid disposition.

The existing review forward-progress rule remains: a planner cannot return
`met` while typed lens followups remain unrepresented. If it intentionally
drops a followup as duplicate or already satisfied, the returned todo/plan
state must identify the surviving item or completed evidence that subsumes it.

### Model selection

Introduce an explicit planning model selection rule rather than borrowing the
main or todo role:

1. `--planning-model` / `settings.models.planning`, when configured;
2. otherwise the primary selected slow model;
3. no fallback to fast/main/todo after planner initialization begins.

When review comparison mode selects multiple slow models, exactly one model is
the planning authority. Default to the primary slow selector and log that
choice. Do not fan planning out across models and attempt to merge competing
plans. The review lenses may still run against every selected slow model.

The planning agent gets a dedicated system prompt and token budget. It should
not inherit the review-lens prompt, because it emits state rather than Findings,
and it should not inherit the current todo prompt, because it also owns goal and
coverage semantics.

### Persistence and observability

Extend `SessionState` with a versioned planning state containing goal, plan,
todo list, planner revision, planner model, and the compact planning-evidence
manifest. Resume restores this state without rerunning discovery. In-progress
todos still reset to pending using the existing resume rules.

Log planning calls with distinct labels:

- `phase=review-planning-discovery`;
- `phase=review-planning-initialize`;
- `phase=review-planning-reconcile revision=N`.

Record planner input/output token usage under a `planning` role so `/cost`
shows the price of stronger planning separately from review lenses. The plan
change log should identify added, removed, relinked, and status-changing steps
and todos.

### Remaining target implementation sequence

1. Add `PlanningState`, `PlanningDecision`, and validated initialization/update
   envelopes in `kres-core`; keep natural-language analysis out of routing.
2. Extend the workflow schema and `review.json` with discovery, initialize,
   reconcile, and model-policy prompt contracts.
3. Replace the current primary-slow `GoalClient`/`TodoClient` pair with one
   `PlanningClient` and a dedicated embedded planning system prompt.
4. Expose a bounded planning-gather method on `AgentRunner` that starts from
   slow-requested typed discovery and uses the existing fast/deterministic
   fetch path.
5. Replace the implemented pre-source staged plan with evidence-backed
   initialization. Review submission already seeds linked todos and does not
   launch the old monolithic initial task.
6. Add a serialized planner-reconciliation stage to the review reaper after
   findings are persisted and before new todos are dispatched.
7. Merge review's primary-slow todo and goal calls only after the planner path
   owns both atomically; retain main/todo clients for generic mode.
8. Persist planner revision/evidence and make resume restore it.
9. Add end-to-end tests for vague file targets, commit/range discovery,
   concurrent task completions, stale revisions, planner failure, stable IDs,
   negative-coverage evidence, turn-cap deferral, and followup exhaustion.
10. Continue comparing review quality, plan coverage, duplicate work, wall
    time, and token cost against saved runs. `kres-sol4` established that an
    unbounded nine-task initial wave produces high throughput but excessive
    token burn and reaper backlog; the staged graph is the current correction.

### Non-goals for the review phase

- Do not weaken the four correctness lenses or their exhaustive review
  contract. Memory safety and object lifetime intentionally share the
  `memory-lifetime` lens because ownership failures span both categories.
- Do not move source retrieval into the planning agent.
- Do not let every slow lens emit a competing global plan.
- Do not convert `/review` into the direct workflow executor or add a second
  review loop.
- Do not generalize the planner to fix/validation/triage until the review state
  machine and measurements are sound.

## Fix-series workflow

`configs/workflows/fix.json` is a statically authored 19-step graph containing
research, status recording, lore search, patch writing, commit-message writing,
commit, build, compile triage, review, orchestration, result recording, and
final assessed publication. Its `completion` expressions are the workflow's
explicit success and failure criteria. It does not use the session goal judge
or session plan.

Before running the full graph, `run_fix_series_driver` clones the workflow and
runs `research`, `invalidate`, and `unconfirm` as a
planning/status pass.
When research confirms the finding, `research.fix_plan` must contain one or
more typed todos with stable ID, title, scope, affected files and symbols, fix
contract, rationale, and dependencies.

The research step is declared `agent: slow`. It gets normal fast gathering and
deterministic tool execution, then primary-slow synthesis. Planning mode owns
the initial decomposition; per-todo mode verifies one item and can propose a
bounded, revision-checked mutation of current or pending work.

Rust validates the generated plan, rejects duplicate or empty IDs and forward
dependencies, then executes the complete static workflow once per todo in
dependency order. Same-ID and structural plan revisions are bounded. The outer
state persists the pre-series HEAD, plan revision, todo statuses, completed
commit SHAs, and review outcomes. Resume uses identity- and revision-qualified
inner snapshots.

Later decisions are distributed across roles: code/slow writes patches and
commit messages, slow lenses review, fast consolidation and the primary-slow
`orchestrator` choose retry routing, and deterministic reaper steps perform
mutations. Per-todo runs defer finding-result and patch publication. A final
primary-slow assessment receives the exact base, commit list, todo graph, and
outcomes; only its clean result enables JSON-defined final record/publish
steps.

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
| Define outcome goal | main | primary slow | static JSON completion/prompt | static JSON prompt/eval | static JSON prompt/eval |
| Build initial plan | main | primary slow, staged dependencies | static DAG; primary slow creates `fix_plan` | static DAG | static one-step DAG |
| Source-informed plan revision | first slow call, then todo | named-file scan before initial goal/plan; primary-slow reconciliation after tasks | primary-slow research with revision guard | none | none |
| Decide followup priority | todo | primary slow with deterministic identity/state guards | static DAG + primary-slow orchestrator | static dependency | n/a |
| Decide completion | main goal judge | primary-slow goal judge plus followup guard | final primary-slow assessment + Rust eval | Rust step/eval state | Rust step/eval state |
| Deep analysis | slow | parallel slow lenses | mixed by step | slow final pass | slow |

## Findings and remaining migration risks

1. There is no single planning abstraction to switch. Session plans, JSON DAGs,
   and `fix_plan` have different schemas, lifecycles, and owners.
2. Generic mode still combines main-model goal definition with mode
   classification. Review forces audit mode and uses the primary slow client,
   but a general planner role still needs an explicit routing contract.
3. Named-file review now scans before goal and plan creation. Ambiguous,
   directory, and non-file targets still lack the target architecture's general
   slow-directed discovery call and evidence manifest.
4. Review correctly suppresses competing lens rewrites. The primary-slow todo
   client is authoritative after tasks, but todo update and goal check are still
   separate calls rather than one atomic revisioned update.
5. Todo-plan linkage now starts complete and Rust protects stable IDs, links,
   completion, and concurrent in-progress state. Future planner changes must
   retain those deterministic guards.
6. `check_goal` still has legacy fail-open behavior. Slow ownership alone does
   not remove the risk of premature completion on parse/call failure.
7. Fix planning is the clearest model mismatch: the fast model authors both the
   series decomposition and the fix contracts even when a stronger slow model
   is configured.
8. Validation and triage have no adaptive planning hook. Using slow planning
   there requires either a preflight plan artifact consumed by the static DAG
   or a general executor-level goal/plan phase.
9. Workflow-runner CLI prompts reject main/todo model overrides because those
   roles are not wired into the executor. A unified planner should have an
   explicit model-selection rule that works in both REPL and executor paths.
10. Review planning calls still log through the existing main JSONL channel and
    usage roles. Dedicated planning labels and accounting remain necessary for
    clean cost and latency audits.

## Primary implementation references

- `kres-agents/src/goal.rs`: `GoalClient`, `define_goal`, `define_plan`, and
  `check_goal`.
- `kres-agents/src/prompts/goal.txt`: goal, mode, plan, and completion prompt
  contract.
- `kres-repl/src/session.rs`: session lifecycle, plan writers, todo updates,
  goal checks, and review dispatch.
- `kres-core/src/plan.rs`: session plan schema, normalization, rewrites, and
  todo/dependency linkage.
- `kres-agents/src/pipeline.rs`: first-slow rewrite behavior and review-lens
  rewrite suppression, gather validation, and schema-repair calls.
- `kres-agents/src/response.rs`: forgiving response extraction plus structural
  validation errors consumed by the gather boundary.
- `kres-agents/src/{mcp_fetcher,fetcher}.rs`: deterministic MCP dispatch and
  local tool/fallback execution.
- `kres-agents/src/todo_agent.rs`: model todo update plus deterministic stable
  identity, completion, in-progress preservation, and duplicate reconciliation.
- `kres-repl/src/workflow.rs`: review prompt adaptation and fix-series driver.
- `kres-agents/src/workflow_runner.rs`: JSON workflow role/model routing.
- `configs/workflows/{review,fix,validate,triage}.json`: shipped executable
  workflow contracts.
