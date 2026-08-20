# Todo rework effectiveness — session `kres-aug6-1`

Measured 2026-08-06 against the live run:

    kres --prompt "review: mm/page_alloc.c" --results kres-aug6-1 --turns 50

pid 3465429, started 05:44. Artifacts in `~/local/linux.kres/kres-aug6-1/`,
logs in `~/local/linux.kres/.kres/logs/b14c5338-b2f1-51df-b869-eb7a865103ab/`.

State at 06:08: 4 of 50 completed runs, 10 findings, todo list at 28 rows
(4 done, 11 inprogress, 13 pending), 0 deferred. 6 todo-agent calls in
`main.jsonl`.

## What works

**No silent drops.** For each of the 6 calls, diffing the `current_todo`
pending ids against the reply's `todo` + `newly_done` + `retired` yields
zero unaccounted rows. This is the failure the rework targeted — the doc
comment at `kres-agents/src/todo_agent.rs:83` records 57 rows handed in
and 34 returned during the 2026-08-05 mm/page_alloc.c review.

**Output shrank.** The agent picks up the edit contract by call 2 and
emits bare `{"id":"..."}` rows: 577 / 563 / 1004 output tokens for
10 / 18 / 25-row lists, against ~4.5k when it also writes prose for new
rows.

## Bug 1 — id-only replies fail schema validation (5 of 6 calls)

Five of six replies were rejected with

    todo-update response is invalid at todo[0]: missing field `name` at line 1 column 44

and went through a `repair_json_response` round trip.

The prompt at `kres-agents/src/todo_agent.rs:1032-1040` says:

> FIELDS YOU DO NOT EMIT for an item that already exists in
> `current_todo`: `status`, `coverage`, `depends_on`, `step_id`. […]
> Send `id` (the handle), plus `name`, `reason` and `type` **when you
> want to change the prose**.

But `TodoItem.name` (`kres-core/src/todo.rs:28`) carries no
`#[serde(default)]`, so serde requires it. The agent is doing exactly
what the prompt asks and is rejected for it.

Cost measured from the `usage` fields in `main.jsonl`:

| | input | output |
|---|---|---|
| normal todo/goal/plan calls | 106,623 | 18,933 |
| repair round trips | 23,864 | 9,621 |

Repair calls show no `cache_creation` and no `cache_read` — the repair
input is entirely uncached. Repairs are ~34% of the role's output
tokens. Wall time from the log timestamps: 5.5s, 33s, 10s, 35s, 11s
≈ 94s of a 25-minute session.

**Fix:** make `name` optional on the wire. Either `#[serde(default)]` on
`TodoItem.name`, or a dedicated wire struct for the todo-update contract
where `name` is `Option<String>`, with `None` meaning "unchanged" and
distinct from `Some("")`.

## Bug 2 — the repair fabricates `name`, and reconcile accepts it

The repair instruction is:

> Preserve every pending todo id, status, reason, every newly_done id
> and its coverage sentence, every retired id, and the plan decision.
> Correct representation and field types only.

The repairing model satisfies the missing required field by setting
`name` to the id. `reconcile_update` (`kres-agents/src/todo_agent.rs:607-609`)
only guards against an *empty* name:

    if !row.name.is_empty() {
        state[idx].name.clone_from(&row.name);
    }

so the fabricated value overwrites the stored prose. 18 rows lost real
names this way: 10 at the call logged at main.jsonl index 8, 8 more at
index 28. In `session.json`, 27 of 28 rows now have `name == id`. The
remaining ones were slug-named by the agent itself at creation.

`reason` survived intact on every row, so the semantic content is not
lost, but `name` feeds the dedup bag (`todo_agent.rs:402`) and the
display.

**Fix:** fixing bug 1 removes the trigger. Additionally, tell the repair
step never to invent a value for a missing field — omit it instead.

## Bug 3 — `newly_done` used once; completions fall back to placeholder coverage

Only the first call (main.jsonl index 4) declared a completion through
`newly_done`. The other three done rows carry

    "coverage": "completed by the reaped task"

which is Rust's placeholder from commit a04d116 ("kres: stamp
placeholder coverage on the fallback todo path"). Coverage is write-once,
so those three rows are permanently placeholders, and the dedup step
that consumes coverage gets no evidence from them.

## Bug 4 — `retired` never used; the list only grows

`retired` is empty in all 6 calls. Row count over the session: 5 → 11 →
20 → 28, against 4 completions. Nothing is being pruned.

## Note

Another Claude session (pid 3842468) was editing
`kres-agents/src/todo_agent.rs` and running `cargo test -p kres-agents todo`
at the time this was written.
