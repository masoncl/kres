# Reaper serialization, dispatch, and continuous ranking

Evidence base: the live run `~/local/linux.kres/kres-aug6-2`
(`kres --prompt "review: mm/page_alloc.c" --results kres-aug6-2 --turns 50`,
pid 404928, started 13:57Z), logs in
`~/local/linux.kres/.kres/logs/44bd1cfe-2117-587d-9892-f508a9be8a3d/`,
plus the replay measurements recorded in commit f2b653a.

## Status — 2026-08-06

| # | Item | State |
|---|------|-------|
| 1 | Slugify synthesized todo ids | **done** `4417123` |
| 2 | M1: findings volatility per reap | **measured**, see below |
| 3 | Shorten the reaper critical path | open — partly superseded, see §3 |
| 4a | Real concurrency cap on the manager | **next** |
| 4b | Incremental dispatch (reaper-driven refill) | open — the actual win |
| 5 | Prioritizer shares the lens session head | **done** `f3edac2`, `a7bbdbf`, with one deviation |
| 6 | Continuous ranking off a stored order | open |

## Headline

**33% of wall-clock time has zero tasks running, and every second of it
is the reaper draining completed tasks one at a time.**

Independently re-measured from task-label spans over 87.3 min:

| concurrent tasks | wall time | share |
|---|---|---|
| **0** | 28.8 min | **33.0%** |
| 1 | 8.3 min | 9.6% |
| 2-3 | 7.7 min | 8.8% |
| 4-6 | 14.5 min | 16.6% |
| 7-9 | 15.4 min | 17.7% |
| **10** | 12.5 min | **14.3%** |

Mean concurrency 3.88 including idle; peak 10, the `BATCH_CAP`. The
sawtooth is: dispatch 10 → all run in parallel → all finish → the reaper
serializes 10 reaps while nothing runs → dispatch 10 again.

Two mechanisms compound:

1. **The full-drain barrier.** `should_auto_continue`
   (`kres-repl/src/session.rs:4367-4369`) forbids dispatch while a single
   task remains in the manager, including terminal-but-unreaped ones.
2. **The serial reap loop.** `for r in reaped { … }` (`session.rs:1206`)
   processes reaped tasks one at a time with every LLM call awaited
   inline. This sets how long the drain takes.

## What already landed, measured

The todo-agent rework (`105ff28`) and the cache-prefix fix (`4692adc`)
are in this run, and both worked:

| todo agent | aug5-5 (full-list) | aug6-2 (edit contract) |
|---|---|---|
| output per call | 19,790 tok | **4,692** (−76%) |
| total output | 1,009,298 | **262,757** (−74%) |
| wall per call | 181.1s | **50.6s** (−72%) |
| total wall | 154.0 min | **47.2 min** |
| cache_read | 154,229 | **351,483** |
| cache_creation | 323,010 | **182,141** |

107 minutes recovered, and the cache flipped from 0.48× read/write to
1.93×. Seconds per 1k output held at ~10 (9.2 → 10.8), so the call is
still output-bound — the win came from emitting less, as designed.

**This reorders the problem.** The todo agent is still the largest single
reaper cost, but it has already been cut by 72%. The remaining 33% idle
is a *structural* cost of waiting for the drain, not a cost of any one
call being slow.

## Goal

Ranking should be a property the todo list always has, refreshed
whenever the list changes, rather than computed once per dispatch wave.
Getting there requires fixing the dispatch barrier first, and fixing the
barrier exposes the reaper as the next governor. The three are one piece
of work.

## Work order

| # | Item | Why | Blocks |
|---|------|-----|--------|
| 3 | Shorten the reaper critical path | ~20s of 71s per reap | — |
| 4a | Real concurrency cap on the manager | removing the barrier without it fans out ~315 calls | 4b |
| 4b | Incremental dispatch (reaper-driven refill) | **converts the 28.8 idle min into work** | 6 |
| 6 | Continuous ranking off a stored order | the stated goal | — |

**Recommended order: 4a → 4b, then 3, then 6.** This inverts the earlier
plan, which put 3 first on the grounds that reap latency becomes the
refill rate limiter after 4b. Two measurements changed that:

- Item 3 only *shortens* idle; 4b *removes* it. At 71s measured serial
  reap, item 3's ~20s saving is 133-200s per 10-task wave — roughly 11
  min of the run. 4b targets the whole 28.8 min.
- Item 3's own ramp-up concern is already answered by its idle-loop
  backstop dispatching `min(free_slots, budget)` at once, so a slow reap
  no longer gates recovery to full concurrency.

Item 3 is also less standalone than it looked. Its two parts:

- **Moving the goal agent after the refill** is meaningless until a
  refill exists, so it is gated on 4b, not independent of it. It also has
  an interaction the earlier plan did not note: on `check.met` the reaper
  drains pending/blocked to deferred. A refill that ran *before* the goal
  check would already have claimed rows into `InProgress`, which the
  drain does not catch (it drains `Pending|Blocked`), so up to
  `free_slots` tasks would run on past a met goal.
- **Moving `promote` (13.3s) off the serial path** is real but needs a
  loop restructure: `effective_analysis` is built inside the loop from
  the coding side effects, so promote cannot simply be hoisted. Running
  the promote LLM calls concurrently across the reaped batch and keeping
  `filter_promoted_delta` serial afterwards preserves id semantics —
  concurrent promotes against one stale universe would otherwise both
  mint `x__promoted_2`.

---

# 1. Bug: 27 of 40 ready ids are truncated prose

## Observed

The single `phase=prioritize` call (main.jsonl index 25, 14:20:00Z) was
handed 40 ready rows. 27 carried an `id` that is a 40-character prefix
of the row's `name`, cut mid-word:

    id= 'Prove pcp->batch can never be 0 on a liv'
        name= 'Prove pcp->batch can never be 0 on a live pageset (zone_batchsize, zo…'
    id= 'start_isolate_page_range()/undo_isolate_'
        name= 'start_isolate_page_range()/undo_isolate_page_range()/set_pageblock_is…'

The other 13 have real slugs (`clear-pages-unit-contract`) because the
todo agent supplied an `id`.

## Source

`assign_ids` (`kres-agents/src/todo_agent.rs:837-855`), called at `:262`
and `:513`:

```rust
let base: String = t.name.chars().take(40).collect();
let mut id = base.clone();
let mut counter = 2u32;
while seen.contains(&id) {
    let short: String = base.chars().take(37).collect();
    id = format!("{short}_{counter}");
    counter += 1;
}
```

The intent stated at `:507-514` is right — an id is required so
`depends_on` and the dispatch loop have a handle. The raw 40-char slice
is the wrong derivation. The affected rows are followup-derived: the
followup wire shape is `{type, name, reason, path?}` with no id.

## Impact

Nothing broke in this run — ids round-trip byte-exact, so
`known.get(pick.id)` (`kres-agents/src/prioritize.rs:322`) matched all
10 picks with zero unknown drops, duplicates, or over-budget cuts. Three
costs remain:

- **Output budget.** `prioritize.rs:21-24` names the tiny output as the
  design point. Echoing 40-char strings instead of ~25-char slugs
  inflates exactly what is being minimised.
- **Operator output.** `resolve_selection` prints
  `[prioritize] 1. Prove pcp->batch can never be 0 on a liv — …`
  (`prioritize.rs:330-335`), cut mid-word in the one place a human reads
  it.
- **Uniqueness by luck — the part that can actually break.** Two
  followups sharing a 40-char prefix collide; the loop re-cuts to 37
  chars and appends `_2`, so the id becomes order-dependent and any
  `depends_on` minted against the pre-collision id dangles. The todo
  prompt tells the agent "Each NEW item gets a short unique id (use the
  name, shortened)" (`todo_agent.rs:1131`), so the agent can reach this
  path too.

## Change

Slugify instead of slicing: lowercase, map non-alphanumerics to `-`,
collapse runs, trim, take the first N *tokens* up to a length cap.
`start_isolate_page_range()/undo_isolate_page_range()/…` becomes
`start-isolate-page-range-undo-isolate`.

- Suffix the full slug on collision (`foo-bar-2`) rather than re-cutting,
  so the first row's id never depends on the second row's presence.
- Existing rows are unaffected: the new derivation only runs where
  `id.is_empty()`, via the early-continue at `:840`. `--resume` across
  the change is safe.
- Preferably mint the slug where the followup is promoted, so an id
  exists from the row's first moment and `assign_ids` becomes a safety
  net that fires on nothing.

## Verify

No id produced by `assign_ids` contains a space, parenthesis, or `/`; and
a two-row list whose names share a 40-character prefix yields two ids
both derived from the full name, neither a re-cut of the other.

---

# 2. M1 — measure findings volatility per reap

**Gating measurement for items 5 and 6. Do it before writing that code.**

## The question

`previous_findings` dominates both the lens prefix and the prioritize
request. Two existing measurements disagree because they measured
different boundaries:

- `prioritize.rs:119-125`: findings are kept out of the cached prefix
  because "in an audit run it grows on most reaps, so caching it would
  invalidate the prefix rather than reuse it."
- `pipeline.rs:209-216`: over 265 lens prompts of the 2026-08-05
  mm/page_alloc.c review, `previous_findings` took **5 distinct values
  across 245 calls**, mean 174,193 chars, peak 352,564 — "snapshotted at
  task start, so a whole dispatch wave shares one snapshot."

Both can be true. The lens figure counts distinct values across *task*
boundaries within waves; the prioritizer claim is about *reap*
boundaries. A per-reap prioritizer re-snapshots at every reap, so the
lens figure does not transfer.

## What to measure

Replay the 2026-08-05 and kres-aug6-2 logs, counting distinct
`previous_findings` values at **reap boundaries** — one sample per
todo-agent invocation, the proposed ranking cadence. Use f2b653a's
replay methodology so the numbers are comparable.

## What each outcome means

- **Few distinct values per wave**: a cached findings block is reused
  across several rankings. Item 5 pays off, item 6 is nearly free after
  the first call per generation, and `prioritize.rs:119-125` needs its
  reasoning updated.
- **Changes on most reaps**: the block is written per reap and saves
  nothing for ranking. Item 5 still helps the lens fan-out; item 6 needs
  its fingerprint skip to be worth landing.

---

# 3. Shorten the reaper critical path

**The largest available win, and independent of everything else.**

## What the reaper does per task

`mgr.reap()` (`kres-core/src/task.rs:644-670`) takes the write lock once
and drains every terminal `TaskEntry`. Then `for r in reaped { … }`
(`session.rs:1206`) handles them one at a time:

| step | where | LLM | measured |
|---|---|---|---|
| publish, mark_todo_done, persist | `:1207-1228` | — | — |
| coding side effects (files, edits, git) | `:1233-1291` | — | n/a for review |
| build `effective_analysis` + trailers | `:1306-1343` | — | — |
| accumulated ledger + report.md append | `:1344-1375` | — | — |
| `/stop` latch → `continue` | `:1391-1396` | — | — |
| **promote** (prose-only bugs → findings) | `:1432-1523` | yes | 44 calls, **13s** mean |
| `append_task_prose`, `apply_delta` | `:1570-1634` | no | deterministic Rust |
| signature / quiescent / streak | `:1672-1712` | — | — |
| **todo agent** | `:1808` | yes | 50 calls, **46s** mean |
| `ensure_review_followups_remain_pending` | `:1872-1880` | — | — |
| **goal agent** | `:1930-2016` | yes | 45 calls, **7s** mean |
| **todo agent again** (goal not-met `missing`) | `:2048` | yes | conditional, +46s |

**Serial total: ~66s per reap**, up to ~112s when the second todo call
fires.

For contrast, `consolidate` (46 calls, **138s** mean, 2,113,289 input
tokens) is the largest single latency in the system but runs *task-side*
in the pipeline (`kres-agents/src/pipeline.rs:36`), so it scales with
parallelism. That is the distinction that matters: task-side work
parallelises, reaper work does not.

## Why 66s is the number that governs everything

- Today it sets the drain length: 10 tasks × 66s ≈ 11 minutes of dead
  time per wave, which is the 15:09:03 → 15:20:09 window exactly.
- After item 4b it becomes the *refill rate limiter*: one slot per 66s,
  so climbing back to 10 concurrent takes 11 minutes regardless of how
  fast tasks finish.

Task durations for scale: median 384s across 51 tasks, quartiles 222 /
384 / 471, range 42-747s. A 42s `[read]` followup task spends longer
being reaped than it spent running.

## Change

Get everything that does not gate the refill off the serial path.

- **`promote` (13s)** — it feeds `working_delta` before `apply_delta`,
  so it does gate the findings write, but it does not depend on the todo
  or goal agents. Run it concurrently with the non-LLM bookkeeping, or
  move it task-side into the pipeline where it parallelises.
- **The todo agent (46s)** — must stay serial and must stay before the
  refill: it mutates the shared list the refill reads. This is the
  irreducible core.
- **The goal agent (7s)** — nothing about the refill depends on its
  verdict. Its met/not-met outcome affects the *next* decision, not this
  one. Move it after the refill.
- **The second todo call (+46s)** — only fires on goal-not-met, and by
  construction it runs after the goal agent, so it lands after the
  refill too once the goal agent moves.

Reordering to `… → todo agent → refill → goal agent → (conditional todo
agent)` cuts refill latency from ~66s (or ~84s with a synchronous
prioritizer) to ~46s.

## Verify

- Zero-task wall-time share drops from 33.5%.
- Mean concurrent tasks rises from 4.01.
- Time from a task's last lens response to the next task's first
  gather request (the refill latency) — target ≤50s.

---

# 4. Incremental dispatch

## 4a. Real concurrency cap on the manager

`with_max_parallel` (`kres-core/src/task.rs:373`) exists and is called
from **nowhere** outside tests — `parallel_semaphore` is referenced only
at `task.rs:124, 365, 400, 462`. The REPL builds an effectively
unbounded manager (`Semaphore::MAX_PERMITS`, `task.rs:365-367`);
`BATCH_CAP = 10` (`session.rs:3368`) is the de facto concurrency limit
purely because dispatch is batched.

Remove the barrier without a cap and dispatch fans out to all 63 pending
rows — at 5 lenses each, ~315 concurrent model calls.

- Add a config knob (default 10, matching today's effective behaviour),
  wire it through `with_max_parallel` at manager construction.
- Verify the semaphore actually gates `spawn` (`task.rs:462-470`) on the
  REPL path, not just the test path.

With `BATCH_CAP == max_parallel` behaviour is unchanged, so this lands as
a safe standalone step.

## 4b. Reaper-driven refill

Replace the full-drain gate with a free-slot test:

```rust
// today
if !self.mgr.snapshot().await.is_empty() { return false; }
// proposed
if self.mgr.active_count().await >= self.cfg.max_parallel { return false; }
```

`active_count` (`task.rs:691-698`) already excludes `Done`/`Errored`.

**The hazard the current gate exists for is real.** `session.rs:4363-4366`
says `active_count()==0` "races the reaper: auto-continue can otherwise
redispatch while the terminal result is still waiting to update
findings, todos, and the interruption stash." Under an all-or-nothing
wave that race is fatal. Under incremental dispatch the requirement is
narrower: a *slot* must not be refilled until the task that vacated it
has been fully reaped.

**Preferred: put the refill inside the reap block**, at the point
established in item 3 (immediately after the todo-agent update). "The
reap is complete enough to refill" then becomes structural rather than
signalled, and dispatch happens exactly once per reaped task.

The alternative — a reap-completion watch channel consumed by the idle
loop — needs a new signal, because `active_count` counts unreaped
terminal tasks as inactive and cannot express the condition.

Dispatch stops being a REPL-loop concern, so extract the
snapshot → claim → submit sequence out of `cmd_continue`
(`session.rs:3390-3410`) into one function that `/continue`, `/next` and
the reaper all call. Keep the idle loop as a backstop for when no task is
running and none will be reaped (post-drain, post-error).

**Ramp-up caveat.** Refilling one slot per reap means recovering from
zero to `max_parallel` takes `max_parallel × refill_latency` — ~7.7
minutes at 46s. The idle-loop backstop should dispatch `min(free_slots,
budget)` at once so a cold start or post-stall recovery still fans out in
one go; only the in-reap refill is one-at-a-time.

---

# 5. Prioritizer writes the lens session-head cache block

## What f2b653a changed

The lens prefix is now two cached layers (`pipeline.rs:232`, `:238`):

```rust
LENS_SESSION_CACHE_FIELDS = ["common_skills", "previous_findings"]
LENS_TASK_CACHE_FIELDS    = ["question", "symbols", "context", "skills", "plan"]
```

Replaying the 2026-08-05 prompts: 6 writes of the head plus 52 of the
tail is 4,516,154 chars against 12,860,628 for 52 whole prefixes — a
64.9% cut.

## The opportunity

Payload breakdown of the 235,563-char prioritize request:

| field | chars | share |
|---|---|---|
| `previous_findings` (14 findings) | 166,155 | **70.5%** |
| `ready` (40 rows) | 22,032 | 9.4% |
| `skills` | 17,265 | 7.3% |
| `plan` | 12,849 | 5.5% |
| `question` | 7,771 | 3.3% |
| `instructions` | 2,121 | 0.9% |

The todo-agent request at the same point (index 32, 81,667 chars)
contains **no findings at all**: `current_todo` 42.8%, `new_followups`
18.3%, `plan` 15.7%, `instructions` 10.9%, `analysis_summary` 5.5%. That
is the 90fd65b split — the todo agent maintains the list, the prioritizer
reasons over the evidence.

So the prioritizer pays full price for ~166KB that the lens fan-out pays
full price for again seconds later, on the same model under the same
system prompt. Timing lines up: in kres-aug6-2 the prioritize response
and all 10 task starts are stamped 14:20:17.

## Scope: head only, never the tail

The task tail is `{question, symbols, context, skills, plan}`. `symbols`
and `context` are gathered by the fast agent *during* the task, so at
dispatch they do not exist. The prioritizer can never warm the tail. The
probe (`pipeline.rs:717-800`) stays for that, gated on `lens_count >= 2`
(`pipeline.rs:3121-3127`).

The head is the part worth having anyway — `previous_findings` is
annotated "<- the prize" at `pipeline.rs:212`.

## Preconditions

1. **Same system prompt — already done.** `035adc2` moved the
   prioritizer to `slow_system_for_mode(Plan.mode)` (`pipeline.rs:2353`,
   `session.rs:829`) and says why: "the system block is part of the
   Anthropic cache prefix, so two call types with different system
   prompts can never read one cached block no matter how their message
   content is arranged."
2. **Same model.** The prioritizer runs on one slow variant; lenses fan
   out over `effective_slow_variants()` — in this run
   `claude-opus-5` (4 lenses) plus `gpt-5.6-sol` (1 supplemental). It
   warms one model's cache. Partial win, not a blocker.
3. **Byte-identical head document.** Two known mismatches: the
   prioritizer sends raw findings (`prioritize.rs:170`) where the lens
   sends `redact_findings_for_agent(...)` (`pipeline.rs:2021`); and it
   sends `runner.skills` where the lens sends `synthesis_skills.common`
   (`pipeline.rs:2026-2028`). At dispatch no `skill_reads` have run, so
   those should coincide — check, do not assume.
4. **Shared findings snapshot.** The task snapshots findings
   independently at task start; dispatch must pass its snapshot down.
5. **Slot budget.** System + session head + prioritizer-stable + uncached
   delta is 3 of Anthropic's 4 `cache_control` slots, the same shape the
   lens path uses.

## Change

Emit the prioritize request as layered documents through the same
`to_layered_documents` / `LENS_SESSION_CACHE_FIELDS` path, with the head
as `cached_prefixes[0]`. `PRIORITIZE_STABLE_FIELDS` (`prioritize.rs:126`)
becomes the second cached block; `ready`, `limit` and the instructions
stay in the uncached delta.

## Hazard

`pipeline.rs:229-231` warns a head varying per task is "strictly worse
than no split at all." Same here: a head differing by one byte buys an
extra ~191KB write with zero reads. This must be a **shared constructor**
plus a test asserting byte equality against what `prepare_lens_fanout`
emits — not two call sites that happen to agree today.

## Verify

- Test: prioritizer head bytes == `prepare_lens_fanout` session head
  bytes for the same inputs.
- Run: the first lens or probe of a wave reports non-zero `cache_read`
  for the head; total `cache_creation` drops by roughly one head write
  per wave versus the f2b653a baseline.

---

# 6. Continuous ranking off a stored order

## Where it stands

| | calls | input tok | output tok | wall |
|---|---|---|---|---|
| todo agent | 15 | 322,704 | 53,273 | 583s |
| prioritizer | 1 | 85,578 | 1,054 | 17s |

(measured at 14:33Z; 12 turns, 2 waves, 1 ranking)

Three gates produce the 1: the full-drain barrier, `BATCH_CAP = 10`, and
the `ready.len() <= limit` skip in `rank_ready`
(`session.rs:796-800`). That is the architectural ceiling, not a
malfunction.

## Do not chain ranking to refill

The obvious implementation — rank synchronously at each refill — puts a
17.5s LLM call on the slot-refill path, taking it from ~46s (after item
3) to ~64s. Three costs:

- Every slot idles for the ranking's duration.
- Head-of-line blocking: five simultaneous completions means the fifth
  slot waits five full reap sequences.
- Batch amortisation disappears. Today one 17.5s ranking authorises 10
  dispatches (1.75s per task started); at `limit = 1` it is 17.5s per
  task started.

And the latency grows over the session: 17.5s was measured at 40 ready
rows and 14 findings, and findings grow monotonically.

## Instead: maintain the order as an artifact

- **Store the ranked order on the manager.** Dispatch and refill read it
  and claim immediately — zero LLM latency on the refill path.
- **Refresh it after the todo-agent update, detached.** The ranking task
  is spawned, not awaited; it updates the stored order for whoever
  refills next.
- **New rows append** until the next ranking places them — exactly the
  stable-storage semantics the list already has.

This delivers the stated goal (the list is re-ranked after every
todo-agent run) without a slot ever waiting on a ranking.

Consuming a slightly stale order is safe by construction:
`claim_selected_todos` re-validates status and dependencies under the
write lock (`task.rs:1113-1122`). The cost is that the refill right after
a reap uses the order computed before that reap's findings landed —
one turn of staleness, against a 46s idle slot avoided.

## Cost control

Depends on M1:

- **If findings hold steady across reaps**: item 5 is the whole answer —
  rankings within one findings generation read the cached block instead
  of writing it.
- **If findings change on most reaps**: add a fingerprint skip. Track
  (findings set, ready-id set) at each ranking; if both are identical to
  the last, keep the stored order and make no call. Exact, not
  heuristic.

Explicitly rejected: ranked-window caching that avoids re-sending the
`ready` rows. Per the payload table, rows are 9.4% — the per-call cost is
fixed, not proportional to how many rows are ranked.

Baseline to beat: 85,578 input / 1,054 output / 17.5s per call.

---

# Verification suite

Re-run `review: mm/page_alloc.c --turns 50` and compare against the
kres-aug6-2 baseline:

| metric | baseline | target |
|---|---|---|
| zero-task wall-time share | 33.5% | ≪ |
| mean concurrent tasks | 4.01 | ↑ |
| refill latency (last lens response → next gather request) | n/a (barrier) | ≤50s |
| prioritize calls | 1 | one per todo-agent run |
| total `cache_creation` | — | ↓ by ~1 head write per wave |
| findings at turn 12 | 14 | ≥ |
| unknown-id / duplicate / over-budget from `resolve_selection` | 0 | 0 |

---

# Invariants

From AGENTS.md and the 90fd65b / f2b653a commit messages:

- **Ranking must never stall a wave.** Every failure path returns empty
  and falls back to storage order (`prioritize.rs:236-241`, `:298-303`,
  `session.rs:3400-3410`). Detaching the ranking (item 6) strengthens
  this; do not weaken it by making a refill await a ranking.
- **The prioritizer must not run under the manager's write lock.** The
  `ready_pending_snapshot` → rank → `claim_selected_todos` split
  (`task.rs:1070`, `:1097`) exists for this and must survive.
- **Ranking language stays out of the todo-agent prompt.** A regression
  test asserts this. Two agents ordering one list is how stable storage
  stops being stable.
- **The todo list stays stable storage** — surviving rows keep position,
  new rows append. The rank is a separate artifact; do not implement it
  by reordering the list.
- **A cache head that varies where it should be stable is worse than no
  split.** `pipeline.rs:229-231`, for the lens head; applies identically
  to item 5.
- **Do not add fields to `LENS_SESSION_CACHE_FIELDS` without re-running
  the distinct-value count.** `pipeline.rs:226-231`: adding `skills` took
  the head from 6 distinct values to 23 and the saving from 64.9% to
  41.3%.
- **A task that reaches terminal state must be fully published before
  its slot is reused.** The narrow form of the rule at
  `session.rs:4363-4366`. Item 4b relies on the refill sitting inside the
  reap block to satisfy it structurally.
