# `--turns` and `--follow` — stopping the run

A "completed task" here means one that went all the way through
fast gathering → deterministic tool service → slow and produced non-empty
analysis, `code_output`, or `code_edits` (`TaskManager::finish_ok` in
`kres-core/src/task.rs`).

- **`--turns N` (N ≥ 1)** — stop launching new work after N
  completed tasks. The REPL records the completed task's outputs,
  drains pending / blocked followups to `/followup`, then waits for
  already-running tasks to finish and publish before exiting.
  In-flight parallel tasks may make the final count exceed N. `--follow` does
  not change the numeric cap, but before the cap it keeps generic goal-met
  deferred work eligible so the run can continue toward N; lensed review
  followups already have their own forward-progress guard.

- **`--turns 0`** (the default) — no run-count cap. With a configured goal
  agent, kres checks after each eligible non-coding task whether the accumulated
  analysis satisfies the inherited session completion goal parked for that
  task; goal-met drains the todo list and the reaper exits once nothing is
  pending or active.

  - Add `--follow` to layer a cost cap: if 3 consecutive
    analysis-producing runs fail to grow the findings list, exit
    even with the goal agent still saying "not met".

  In a generic session, without a configured main agent model file there is no
  goal agent; kres falls back to "stop when the active batch finishes", and
  `--follow` switches that fallback to the 3-run stagnation cap. Review uses
  its primary slow model for goal checks instead.
  See the `turns_limit == 0` branch in `kres-repl/src/session.rs`
  for the full predicate.

On a `--turns N > 0` run-count cap, kres lets in-flight tasks finish
so their findings are not lost. On goal-met and stagnation exits,
pending / blocked todos move to the deferred list; `/followup` lists
them and `/continue` dispatches them if you re-enter the REPL.
The exit path does not auto-run `/summary`; run `/summary` before
quitting, or run `kres --summary --results <dir>` afterwards.
