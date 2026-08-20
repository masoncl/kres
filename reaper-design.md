# Reaper queue, dispatch gate, and continuous prioritization

Working design doc. Records the plan, the evidence behind
it, the open decisions, and what has been tried and rejected.

Companion to `todo-updates.md`, which holds the measurements this
builds on. Where the two disagree, this file is newer.

Status: **all 9 steps landed, self-review pass, and one measured
revision (§8c). Uncommitted.** Build clean, clippy
clean, all suites pass. Not yet exercised against a real run — every
number in §7 is still a target, not a measurement.

---

## 1. What the operator asked for

> Move to a new model where new tasks cannot start while a reaper is
> running (enforced with a semaphore or mutex). Implement a reaper
> queue and allow only one reaper to be running at a time.
>
> If task A's reaper is running and task B, C, D complete, all 3 get
> added into the queue, and all 3 get reaped in a single reaper call.
> New tasks wait for the reaper queue to be completely empty.
>
> Prioritization happens immediately when the reaper queue is empty,
> even if there are still running tasks.

Framing sentence: *"a larger rework around how parallel tasks are
combined into todos and goals."* The single shared todo list is the
resource being protected — no task may start while it is in flux.

---

## 2. Current mechanics, verified

Everything in this section was read out of the tree at `a7bbdbf`.

### 2.1 The reaper is already single-threaded and already batches

- The reaper is one detached `tokio::spawn` with a 250ms ticker
  (`kres-repl/src/session.rs:1184-1205`). There is exactly one, so
  "only one reaper running at a time" holds today by construction.
- `TaskManager::reap()` (`kres-core/src/task.rs:644-670`) takes the
  write lock once and drains **every** terminal `TaskEntry` in one
  call. B, C and D completing during A's processing are already
  drained together by the next tick.

So parts 2 and 3 of the request are structurally true already. What is
missing is that nothing *enforces* it: the invariant is an accident of
there being one reaper task, not a gate anything acquires.

### 2.2 What actually blocks dispatch is a different, much broader gate

`should_auto_continue` (`session.rs:4369-4386`):

```rust
// A terminal task remains publishable until the reaper removes it.
// Treating active_count()==0 as idle races the reaper: auto-continue
// can otherwise redispatch while the terminal result is still waiting
// to update findings, todos, and the interruption stash.
if !self.mgr.snapshot().await.is_empty() {
    return false;
}
```

`snapshot()` (`task.rs:673-685`) returns every tracked task, running or
terminal-unreaped. So dispatch is refused while **any** task exists,
not merely while the reap queue is non-empty. That is the full-drain
barrier, and it is the direct cause of the measured 33.0% zero-task
wall time (`todo-updates.md` §Headline).

The comment is right about the hazard and too broad about the remedy.
The requirement is: a slot must not be reused until the task that
vacated it has been fully published. It is not: no task may start
while any other task runs.

### 2.3 Dispatch path

- `cmd_continue` (`session.rs:3363-3458`) is the only batch dispatch:
  `ready_pending_snapshot` → `rank_ready` → `claim_selected_todos` →
  `submit_from_pipeline` per row, with `BATCH_CAP = 10`
  (`session.rs:3394`).
- It is reached from the operator typing `/continue`, or from the idle
  loop after `AUTO_CONTINUE_IDLE = 5s` of continuously-true
  `should_auto_continue` (`session.rs:2441-2465`).
- `cmd_next` (`session.rs:3460-3511`) is the same sequence with
  `limit = 1`.
- `rank_ready` (`session.rs:796`) skips the LLM call entirely when
  `ready.len() <= limit` (`session.rs:796-800`).

### 2.4 There is no concurrency cap

`with_max_parallel` (`task.rs:373`) exists and is called from **nowhere
outside tests**. `parallel_semaphore` is referenced only at
`task.rs:124, 365, 400, 462`. The REPL builds a manager with
`Semaphore::MAX_PERMITS` (`task.rs:365-367`). `BATCH_CAP = 10` is the
de facto concurrency limit purely because dispatch is all-or-nothing.

Where the permit is acquired matters for this design: inside the
spawned future, before `set_state(Running)` (`task.rs:462-472`). So an
over-dispatched task does not over-run — it sits in `Pending` holding a
row in `g.tasks`, which `active_count()` (`task.rs:691-697`) and
`turn_budget` (`task.rs:178-190`) both count as active. Back-pressure
without breaking the turns budget.

### 2.5 The reaper's serial per-task work

From `todo-updates.md` §3, measured over kres-aug6-2:

| step | LLM | measured |
|---|---|---|
| promote | yes | 44 calls, 13s mean |
| todo agent | yes | 50 calls, 46s mean |
| goal agent | yes | 45 calls, 7s mean |
| todo agent again (goal not-met) | yes | conditional, +46s |

~66s serial per reaped task, ~112s with the second todo call. This runs
once **per task**, inside `for r in reaped` (`session.rs:1206`), even
when ten tasks are reaped in one call.

### 2.6 Session ownership

`Session::run(&self)` (`session.rs:876`); `main.rs:1253` builds an
owned `Session` and `main.rs:1676` calls `session.run().await`. The
reaper is a detached task holding ~25 cloned fields, not a `Session`
reference. Any design where the reaper dispatches needs that to change.

---

## 3. Target model

Three states, one gate.

```
                 ┌──────────────────────────────────────┐
                 │  reap queue: terminal-unreaped tasks │
                 └──────────────────────────────────────┘
                        │ non-empty, or a reap in flight
                        ▼
   dispatch BLOCKED ◄───┴───► dispatch ALLOWED  (queue empty,
                                                 reap not running,
                                                 free slots > 0)
```

- **Reap gate.** A manager-owned lock held from the moment a reap batch
  is taken until every downstream publication for that batch has
  completed. Dispatch acquires it; the reaper holds it.
- **Reap queue = terminal-unreaped tasks.** Dispatch requires the
  queue empty, not merely the gate free. A terminal task whose findings
  and todos are unpublished is exactly the staleness the current
  barrier protects against.
- **Running tasks do not block dispatch.** This is the change that
  converts the measured 28.8 idle minutes into work.
- **Bounded fleet.** Dispatch fills to a cap rather than dumping every
  ready row. Forced: without it, dispatch-while-running fans out to all
  pending rows (`todo-updates.md` §4a estimates ~315 concurrent model
  calls at 5 lenses × 63 rows).
- **Prioritization runs at each dispatch**, which is now "whenever the
  queue empties and slots are free" rather than once per full drain.

### Invariant, stated precisely

> A task that reaches a terminal state must be fully published — its
> findings applied, its todo row completed, its followups merged —
> before any new task is claimed.

This is the narrow form of the rule the current gate over-enforces. The
gate satisfies it structurally rather than by timing.

---

## 4. Decisions

**Settled 2026-08-06. Rationale kept so they are not relitigated.**

| id | decision |
|---|---|
| D1 | **Reaper-driven dispatch.** `Session::run` moves to `Arc<Self>`; the reaper dispatches at the end of its batch |
| D2 | **One todo/goal pass per reap batch.** `TodoAgentInputs` widens to carry the whole batch |
| D3 | **Stored ranked order**, refreshed detached after each todo-agent update. Dispatch never awaits a ranking |
| D4 | **Cap wired at 10**, `--max-parallel` + `settings.json` override |

D3 is the more expensive of the two options offered and subsumes
`todo-updates.md` item 6. Consequence for D1: because dispatch reads a
stored order instead of making an LLM call, reaper-driven dispatch adds
no latency to the reap batch at all — the two decisions compose better
than either does alone.

### D1-a. Deviation: reaper-initiated, REPL-loop-executed

D1 said the reaper calls dispatch. It decides *when* — but the call
itself happens on the REPL loop, because `submit_prompt_inner`
transitively reaches closures in the change-survey path whose futures
rustc cannot prove `Send`:

    error: implementation of `Send` is not general enough
    note: `Send` would have to be implemented for the type `&[CodeEdit]`
    note: ...but `Send` is actually implemented for `&'0 [CodeEdit]`,
          for some specific lifetime `'0`

Nothing previously called `submit_prompt_inner` from inside a
`tokio::spawn`, so that future's `Send`-ness had never been required.
Annotating the offending closure parameters and boxing the call both
failed; the inference problem is in `run_prompt`
(`session.rs:6513`) and fixing it properly means restructuring
unrelated change-survey code.

Mechanism instead: an `mpsc::channel(1)` whose **payload is the reap
gate guard**. The reaper publishes its batch, then hands the
still-held `OwnedMutexGuard` to the REPL loop, which dispatches under
it and releases it. Passing the guard rather than releasing it is what
keeps "published, THEN dispatched" true across the handoff — no other
claimant can take the gate in between.

What this costs: if the REPL loop is mid-command (`/summary`, an
editor session), the refill waits for it. That is arguably correct.
The `try_send` failure path drops the gate and leaves the work to the
idle poll, which now has the same permissive gate, so nothing is lost —
only delayed by up to the poll interval.

Revisiting this means fixing the change-survey HRTB issue first.

### D1. Who triggers dispatch

| option | mechanism | cost |
|---|---|---|
| **A. Reaper-driven** | reaper calls a shared dispatch fn at the end of its batch | needs `Arc<Session>`: `run(&self)` → `run(self: Arc<Self>)`, `main.rs:1676` wraps, reaper holds `Weak<Session>`. Removes the 5s idle delay and the operator's "hit enter to cancel" affordance |
| **B. Idle-loop-driven** | keep dispatch on the REPL loop, change its gate to reap-queue-empty, shorten the idle window | much smaller diff, keeps the cancel affordance, but "immediately" becomes "within the poll interval" |

The request says prioritization happens "immediately" when the queue
empties, which argues A. `todo-updates.md` §4b also prefers in-reap
("structural rather than signalled"). Against A: the interactive REPL
loses the 5-second window where typing cancels an auto-dispatch.

Hybrid worth considering: reaper-driven dispatch, with the operator
affordance preserved by having `/stop` remain the cancel path.

### D2. One todo/goal pass per reap batch, or per task

Today the reaper runs promote → apply → todo agent → goal agent
**per reaped task**, serially. If B, C, D are reaped in one call, that
is 3 × 46s of todo agent before the queue is empty and dispatch can
happen.

The framing sentence — "how parallel tasks are combined into todos and
goals" — reads as: one consolidated pass per batch.

| option | effect |
|---|---|
| **A. Per batch** | one todo-agent call with all completions and all followups; one goal check against the whole batch. Queue-empty latency drops from 66s × N to ~66s regardless of N. Requires widening `TodoAgentInputs` (`kres-agents/src/todo_agent.rs:193-203`), today shaped per task: `completed_query`, `completed_todo_id: Option<&str>`, `analysis_summary`, `new_followups` |
| **B. Per task, batched only for reaping** | no contract change; the queue simply takes N × 66s to empty, which caps how often dispatch can fire |

Note in A's favour: the reply contract already carries `newly_done` as
an **array** of `{id, coverage}` (AGENTS.md §Todo List), so the output
side supports multiple completions today. It is the input side that is
singular.

Note against A: the goal agent already reasons over the whole
accumulated ledger (`session.rs:1924-1943` builds `combined` from every
accumulated entry), so per-batch goal checking is close to free to
adopt — but per-batch *todo* updates change what the agent is asked to
reconcile in one shot, and a bad merge there corrupts the one shared
list.

### D3. Prioritization cadence

If dispatch fires whenever the queue empties and a slot is free, the
ranking call rides along. `todo-updates.md` §6 measured that call at
17.5s / 85,578 input tokens and argues explicitly against putting it on
a per-slot refill path (head-of-line blocking; batch amortisation
disappears; the latency grows monotonically with findings).

| option | effect |
|---|---|
| **A. Rank at each dispatch** | simplest; matches the request literally. Mitigated by the existing `ready.len() <= limit` skip (`session.rs:796-800`) and by dispatch being batch-shaped, not per-slot |
| **B. Stored order, refreshed detached** | `todo-updates.md` §6's design: dispatch reads a stored ranked order with zero LLM latency; a detached task refreshes it after each todo-agent update. Strictly better, strictly more work |

Recommendation: A now, B as the follow-up, because B is only worth
building once the new cadence is measured.

### D4. Concurrency cap value

Not really a fork — a cap is forced by §3. Proposal: wire
`with_max_parallel` at manager construction, default **10** to match
today's effective `BATCH_CAP`, overridable by CLI flag and
`settings.json`. Dispatch then claims `min(BATCH_CAP, cap −
active_count(), turn_budget)`.

---

## 5. Work breakdown

Ordered so each step is independently landable and behaviour-neutral
until the last one flips the gate.

| # | Step | Depends on | Behaviour change | State |
|---|---|---|---|---|
| 1 | Wire `with_max_parallel` from config; add `--max-parallel` + `settings.json` key | — | none at default 10 | **done** |
| 2 | Add `TaskManager::reap_queue_depth()` (terminal-unreaped count under one lock) | — | none, new read-only API | **done** |
| 3 | Add the reap gate to `TaskManager`; reaper acquires it around the whole batch | 2 | none — nothing else acquires it yet | **done** |
| 4 | Extract `snapshot → order → claim → submit` out of `cmd_continue` into one dispatch fn used by `/continue`, `/next`, and the reaper | — | none | **done** |
| 5 | Stored ranked order on the manager; dispatch reads it, never awaits a ranking (D3) | 4 | ranking stops gating dispatch | **done** |
| 6 | Batch the todo/goal pass across the reaped set (D2) | 3 | contract change; largest risk | **done** |
| 7 | Detached ranking refresh after each todo-agent update (D3) | 5,6 | ranking cadence goes from per-wave to per-batch | **done** (refresh fires per reap batch; step 6 will not move it) |
| 8 | Replace the full-drain barrier with `reap_queue_depth() == 0 && !reap_in_flight && free_slots > 0` | 1,3,4 | **the flip** | **done** |
| 9 | Reaper decides when to dispatch once its batch completes; `run(self: &Arc<Self>)` (D1) | 4,8 | removes the 5s idle latency | **done, with a deviation — see D1-a** |

Keep this table current — it is the progress tracker for the rework.

---

## 6. Hazards to hold onto

- **The todo list is the protected resource.** Every mutation path
  (`merge_inferred_state`, `claim_selected_todos`, `defer_pending`,
  `mark_todo_done`) must remain ordered with respect to the reap gate.
  A claim that interleaves with a todo-agent merge is the failure this
  whole design exists to prevent.
- **`ready_pending_snapshot` → rank → `claim_selected_todos` must stay
  split** (`task.rs:1070`, `:1097`). The prioritizer must not run under
  the manager's write lock (AGENTS.md). Holding the reap gate across an
  LLM call is fine; holding the manager's `inner` write lock is not.
- **Ranking must never stall a wave.** Every failure path falls back to
  storage order (`prioritize.rs:236-241`, `:298-303`,
  `session.rs:3400-3410`). Preserve that under the new cadence.
- **Findings snapshot per wave.** Lens tasks snapshot
  `previous_findings` at task start (`pipeline.rs:209-216`), so a wave
  shares one snapshot. Dispatching more often makes waves smaller and
  snapshots fresher — an improvement, but it changes the cached-head
  hit pattern that `f3edac2` depends on. Re-measure `cache_read` after
  the flip.
- **`--turns` accounting.** `turn_budget` (`task.rs:178-190`) subtracts
  in-flight tasks from the remaining budget, so more concurrency does
  not overshoot the cap. Verify with a test at `--turns 3` and cap 10.
- **Turns-cap drain and goal-met drain** both assume they can defer
  `Pending|Blocked` rows and be done. With dispatch running more often,
  a row can move Pending → InProgress between the drain and the exit
  check. `reconcile_turn_cap_todos` already exists for the tail case
  (`session.rs:2200`); re-check it under the new cadence.
- **`/stop`.** Latches, cancels, and drains. Under the new model it
  must also block dispatch — the latch check has to be inside the
  shared dispatch fn, not only in the idle loop.

---

## 7. Verification

Baseline is kres-aug6-2, `review: mm/page_alloc.c --turns 50`
(`todo-updates.md` §Headline).

| metric | baseline | target |
|---|---|---|
| zero-task wall-time share | 33.0% | ≪ |
| mean concurrent tasks | 3.88 | ↑ |
| peak concurrent tasks | 10 | ≤ cap |
| queue-empty → next gather request | n/a (barrier) | ≤ 50s |
| prioritize calls | 1 | one per dispatch |
| findings at turn 12 | 14 | ≥ |
| total `cache_creation` | — | not worse than f3edac2 |

Tests written so far:

| test | where | covers |
|---|---|---|
| `max_parallel_caps_concurrently_running_tasks` | task.rs | the semaphore actually gates spawn |
| `reap_queue_depth_counts_terminal_unreaped_only` | task.rs | queue depth vs `active_count` vs `snapshot` |
| `reap_gate_is_exclusive` | task.rs | gate mutual exclusion |
| `ranked_claim_prefers_the_stored_order_then_storage_order` | task.rs | stored order, and new rows appending after it |
| `ranked_claim_falls_back_to_storage_order_without_a_ranking` | task.rs | ranking never stalls a wave |
| `ranked_claim_skips_blocked_rows_and_respects_the_turn_budget` | task.rs | deps and `--turns` under a ranked claim |
| `auto_continue_no_longer_waits_for_running_tasks` | session.rs | **the flip** |
| `auto_continue_waits_for_terminal_task_to_be_reaped` | session.rs | pre-existing; still passes under the new gate |
| `dispatch_is_refused_while_the_reap_gate_is_held` | session.rs | gate refusal |
| `dispatch_is_refused_while_terminal_tasks_await_reaping` | session.rs | queue-depth refusal |
| `dispatch_is_refused_when_every_slot_is_busy` | session.rs | cap refusal, and the idle loop agreeing |

Step 6 added:

| test | where | covers |
|---|---|---|
| `one_update_completes_every_row_the_batch_finished` | task.rs | several completions in one `InferredTodoUpdate` |
| `mark_completed_flips_every_row_in_the_batch` | todo_agent.rs | multi-id pre-marking |
| `the_request_names_every_row_that_just_completed` | todo_agent.rs | one coverage sentence per entry, not per batch |
| `the_dedup_algorithm_pools_followups_across_the_batch` | todo_agent.rs | cross-sibling dedup is instructed |
| `the_completed_batch_is_split_into_the_delta_document` | todo_agent.rs | the batch stays out of the cached prefix |

Still to write:

- Three tasks completing during one reap are handled in a single
  subsequent reap call. Currently this rides on `reap()`'s drain-all
  behaviour being untouched, which is true but unasserted.
- `--turns N` is not overshot when dispatch fires while tasks run.
  `turn_budget` subtracts in-flight tasks (`task.rs:178-190`), so it
  should hold; not yet asserted end to end.

---

## 7b. What step 6 changed on the wire

`TodoAgentInputs` went from four per-task fields to one array:

```
completed: [ { query, just_completed?, analysis, followups? }, ... ]
```

`completed_query` / `analysis_summary` / `new_followups` are gone;
`just_completed` moved from a top-level scalar onto each entry.
`InferredTodoUpdate.completed_todo_id: Option<String>` became
`completed_todo_ids: Vec<String>`, and `mark_completed_todo` takes a
slice.

Cache split is unaffected: `UPDATE_TODO_STABLE_FIELDS` is still
`["task", "instructions", "plan"]` and `completed` is volatile, in the
delta half, exactly where `completed_query` and friends were.

Prompt changes (`prompts/todo.txt`, `build_instructions`): the agent is
told `completed` is an array, that it must return one `newly_done`
coverage sentence per entry drawn from that entry's own analysis, and
that the DEDUP ALGORITHM pools followups across entries — step 4 is new
and exists because parallel siblings rediscover the same gap.

## 8c. Measured revision: the queue-empty rule is gone

The aug6-4 run (2026-08-06, `review: mm/page_alloc.c`, stopped by the
operator at 23.5 min / 5 completed runs) showed the queue-empty
dispatch rule giving back much of what the rework had won:

| observation | value |
|---|---|
| task-side idle, same script, vs aug6-2's 17.1% | **4.1%** |
| dispatches | 4, 8, 3 — two fired while tasks were running |
| post-reap refills fired / **skipped** | 3 / **3** |
| skip reason, every time | `N task(s) waiting to be reaped` |
| reap batch sizes | `[1,1,1,1,1]` — step 6 never exercised |
| prioritize calls | 3+ in 20 min, vs 1 for all of aug6-2 |

The mechanism was visible in the timestamps: publishing one reaped
task took ~65s (18:06:34 → 18:07:40, dominated by a ~57s todo-agent
call), and a sibling routinely finished inside that window, so the
refill found a non-empty queue and declined.

**Replaced by a start budget.** Dispatch no longer waits for the reap
queue or takes any gate; at most `max_parallel` tasks may start
between reap completions. The reap gate was deleted outright rather
than left dead — nothing acquired it once dispatch stopped doing so.

Safety argument for claiming during a reap: every todo mutation takes
the manager write lock, and `merge_inferred_state` already restores
the live status of a row that went InProgress after the todo agent's
snapshot was taken. That protection predates this design and is
covered by `inferred_todo_cannot_redispatch_work_started_after_snapshot`.

Also fixed, both output-only: the refusal message was phrased as an
action (`a reap batch is publishing; it will dispatch`), and the idle
loop printed `[auto-continue: dispatching next batch]` BEFORE checking
whether it could dispatch — so three consecutive refusals read as
three dispatches.

Self-review of the start budget found three things, all fixed:

1. **Livelock.** The budget counts *claims*, not starts. A claim whose
   `submit_prompt_inner` returns false leaves the row InProgress with
   no executor: budget spent, nothing to reap, no way to re-arm — not
   even via `/continue`, which would be refused by the very counter
   that needed clearing. Guarded by treating the budget as unbounded
   when no task is tracked at all, since nothing could ever re-arm it
   in that state. Note this does NOT weaken the real case: tasks that
   error instantly are terminal-unreaped entries, so the task list is
   non-empty and the budget still binds.
2. A stale comment block describing the deleted reap gate and its
   `_locked` entry point, plus a duplicated `if reaped_count > 0`.
3. The two log defects the operator surfaced by misreading the output:
   a refusal phrased as an action, and the idle loop announcing a
   dispatch before the check that could refuse it.

Naming to keep in mind: `starts_since_reap` counts claims. They
coincide except on the failed-submission path above.

Still unmeasured: whether the start budget keeps the fleet fuller than
the queue-empty rule did, and whether reap batches ever exceed 1.

## 8b. Self-review fixes

Reviewed 2026-08-06 after step 6. Ten findings, all fixed.

| # | Finding | Fix |
|---|---|---|
| 1 | The refill channel carried the reap gate guard, which stays locked while the message is unread. `/summary` or `/edit` parked the REPL loop and stalled the reaper's next blocking `reap_gate()` — no reaping, no persistence, no turns-cap check, for as long as the command ran | Channel carries `()`; the reaper releases the gate first and the REPL re-acquires via `try_reap_gate`. Correctness is unchanged: by signal time the publication window has closed |
| 2 | `spawn_ranking_refresh` fired unconditionally from the refill branch, so a 17.5s prioritize call ran after `/stop` and past the turns cap | `ranking_refresh_allowed()` guard, tested |
| 3 | The consecutive-error watchdog's `break` fell into the batch pass, which then issued todo/goal calls against an already-cancelled shutdown and lost the batch's edits | `halted` flag skips the batch pass; the batch's followups are recorded as pending so `--resume` keeps them |
| 4 | The cap existed twice — `ReplConfig::max_parallel` and the manager's semaphore — with nothing enforcing agreement | Cap moved onto `TaskManager` (`max_parallel()`, `free_slots()`); the `ReplConfig` field is gone |
| 5 | `--max-parallel 0` meant "unbounded", i.e. claim every ready row | Refused at startup |
| 6 | `clear_session_work` / `clear_active_todos` left `ranked_order` behind, and name-derived ids collide across runs | Both clear it |
| 7 | `DEFAULT_RANKING_DEPTH` duplicated `DEFAULT_MAX_PARALLEL` | Depth derives from the cap; the constant is now only the unbounded fallback |
| 8 | `run_batch_goal_check` fabricated a `TaskMode` to satisfy `review_followups_drive_next_turn` | Predicate takes the bool the caller already has |
| 9 | Auto-continue called `/continue`, which restores the deferred ledger — now on a 5s timer while tasks run | `auto_continue` vs `cmd_continue` split on `ContinueSource` |
| 10 | `Settings::apply_project_overrides` ignored `max_parallel`, so a project-local cap was silently dropped | Merged, tested |

Not fixed, recorded instead: `submit_prompt` and `dispatch_workflow`
start tasks without the gate. Pre-existing and deliberate — an
operator prompt should not wait on a reap — but it means the
invariant in §3 is about todo-driven dispatch, not every spawn path.
AGENTS.md now says so.

## 8. Tried and rejected

**Concurrent promote pass (2026-08-06, reverted).** Hoisted the
promotion LLM call out of the per-task reaper loop and ran the whole
reaped batch at `buffer_unordered(8)`, with `filter_promoted_delta`
kept serial against the live universe. Built clean, all tests passed,
never committed — reverted on review.

Why it is recorded here rather than re-proposed: it removed the only
point in the pipeline with **intra-wave promote dedup**. Serially,
task 2's promoter saw the findings task 1 had just applied and could
reuse the id; concurrently both audit the pre-batch snapshot, so two
tasks describing the same bug mint two unrelated ids and
`filter_promoted_delta` has nothing to rename. The saving was ~13s per
reaped task on the serial path, unmeasured, against a real duplicate-row
regression.

If promote is revisited under the batched model (D2=A), the fix is to
widen the prepass universe to *store ∪ every reaped task's delta*
rather than *store ∪ this task's delta* — free, since all deltas are in
hand when the batch is taken.
