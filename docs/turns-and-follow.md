# `--turns` and `--follow` — stopping the run

`--turns` controls when kres decides a non-interactive run is "done".
A "completed task" throughout this page means a unit that ran all
the way through fast → main → slow and produced a non-empty analysis
(`kres-core/src/task.rs:309-311`).

- **`--turns N` (N ≥ 1)** — stop after N completed tasks. Useful for
  a single focused question (`--turns 1`) or a time-boxed review
  (`--turns 5` etc.). The REPL exits as soon as the Nth task
  finishes, regardless of what the goal agent or the followup queue
  look like. `--follow` has no effect in this mode; the run-count
  cap wins.

- **`--turns 0` (the default)** — no run-count cap. kres trusts the
  goal agent: after every task the goal agent checks the accumulated
  analysis against the per-task goal; when it declares the goal met,
  its handler drains the todo list and the reaper exits on the next
  tick (nothing is active, nothing is pending). Until then kres
  keeps dispatching the followup tasks the goal check spawns.

  - Add `--follow` to layer a cost cap on top: if 3 consecutive
    analysis-producing runs fail to grow the findings list, exit
    even if the goal agent is still saying "not met". Use this when
    you want a hard ceiling on how long kres will keep pulling on
    threads.

  (`kres-repl/src/session.rs` — see the `turns_limit == 0` branch in
  the reaper for the exact predicates. If you run without a
  `main-agent.json`, no goal agent is wired up and kres falls back
  to "stop when the active batch finishes"; `--follow` switches that
  fallback to "drain the todo list with the 3-run stagnation cap".)

On any `--turns` exit path — run-count cap, goal-met drain, or
stagnation cap — kres

1. cancels any in-flight work,
2. runs `/summary` automatically, producing `bug-report.txt`
   (`bug-report.md` with `--markdown`) in the results directory, or
   in the current working directory when `--results` was not given,
   and
3. exits.

Remaining pending or blocked todo items are moved to the "deferred"
list; `/followup` shows them if you re-enter the REPL later, and
`/continue` will dispatch them.
