# `--turns` and `--follow` — stopping the run

A "completed task" here means one that went all the way through
fast → main → slow and produced non-empty analysis or code output
(`kres-core/src/task.rs:328-337`).

- **`--turns N` (N ≥ 1)** — stop after N completed tasks. The REPL
  exits as soon as the Nth task finishes, regardless of the goal
  agent or followup queue. `--follow` has no effect here.

- **`--turns 0`** (the default) — no run-count cap. kres trusts the
  goal agent: after every task it checks whether the accumulated
  analysis satisfies the per-task goal; goal-met drains the todo
  list and the reaper exits once nothing is pending or active.

  - Add `--follow` to layer a cost cap: if 3 consecutive
    analysis-producing runs fail to grow the findings list, exit
    even with the goal agent still saying "not met".

  Without a `main-agent.json` configured there is no goal agent;
  kres falls back to "stop when the active batch finishes", and
  `--follow` switches that fallback to the 3-run stagnation cap.
  See the `turns_limit == 0` branch in `kres-repl/src/session.rs`
  for the full predicate.

On any `--turns` exit — run-count cap, goal-met drain, or
stagnation — kres cancels in-flight work, auto-runs `/summary`
(`bug-report.txt`, or `bug-report.md` with `--markdown`) in the
results dir (cwd when `--results` was absent), and exits.
Remaining pending / blocked todos move to the deferred list;
`/followup` lists them and `/continue` dispatches them if you
re-enter the REPL.
