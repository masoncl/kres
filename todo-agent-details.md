# The todo / prioritize split

State as of `6517f27`. Numbers come from two runs:

- **aug5-5** — `review: mm/page_alloc.c --turns 50`, log
  `a08d4031-2b3b-5cdf-a4a9-2fe562b27459`. Ran to the cap: 51 todo calls,
  53 todos done, 33 findings, 11,067s wall. This is the *pre-rework*
  baseline.
- **aug6-1** — same target, log
  `b14c5338-b2f1-51df-b869-eb7a865103ab`. Sampled at 6 todo calls. This
  is the first run of the edit contract, and it is what `todo-bugs.md`
  measured.

Nothing here has been measured under the prioritization agent — no
session has run it.

---

## Why there are now two agents

One agent was doing two jobs with one prompt, one model, and one output
channel.

**Maintaining the list** — dedup a wave of followups against what is
already queued and already covered, mark completions, retire dead work,
keep plan links. Needs the list and the coverage prose.

**Choosing what runs next** — needs the session question, every finding
so far, the skills, and the plan. The todo agent was sent *none* of
that. It was ranking blind.

The two also disagree about output shape. Ranking wants a tiny answer:
a handful of ids. Maintenance wants edits to specific rows. Merging
them produced the worst case — the agent restated the entire list every
call so that *position* could carry the ranking. That cost **1,009,298
output tokens over 51 calls, 27% of the aug5-5 run's total output**, at
a steady 9.2s per 1k output tokens, and it put the todo agent on
**47.4% of the run's critical path** (5,250s of 11,067s) while being
only 11.7% of its compute.

Splitting them lets each channel be the right size.

---

## Where each one runs

```
task completes
      │
      ▼
  REAPER TICK (every 250ms, kres-repl/src/session.rs:1093)
      ├─ promote      fast agent      session.rs:1450
      ├─ apply delta  Rust            findings::apply_delta_to_list
      ├─ publish      Rust            report.md + session.json
      ├─ turns cap    Rust            → skip the rest if reached
      ├─ TODO AGENT   todo role       session.rs:1793
      └─ goal check   goal role       session.rs:1919
             └─ not met → TODO AGENT again   session.rs:2033
      │
      ▼
  idle 5s, nothing in flight  (should_auto_continue, session.rs:4215)
      │
      ▼
  DISPATCH  (cmd_continue session.rs:3377 / cmd_next session.rs:3438)
      ├─ ready_pending_snapshot        Rust, read lock
      ├─ PRIORITIZE AGENT              slow coding agent
      └─ claim_selected_todos          Rust, write lock
      │
      ▼
  next wave of tasks
```

The key placement decision: the prioritizer runs at **dispatch**, not in
the reap. The reap fires once per reaped task; dispatch fires once per
wave. With a 10-wide batch that is roughly a 10× difference in call
count, and the reap sequence is already the run's serial bottleneck.

---

## The todo agent

### Inputs

| Field | Half | Changed on N of 50 (aug5-5) | Mean size |
|---|---|---:|---:|
| `task` | stable | 0 | 13 |
| `instructions` | stable | 0 | 6,522 |
| `plan` | stable | 10 | 12,592 |
| `completed_query` | delta | 36 | 831 |
| `just_completed` | delta | per-reap | ~30 |
| `analysis_summary` | delta | 50 | 4,023 |
| `new_followups` | delta | 50 | 8,586 |
| `current_todo` | delta | 50 | 46,378 |

`lenses` used to be in the stable half. Its only consumer was the
ranking bullet; once that left there was nothing reading it, so it was
deleted — 1,242 chars of dead prompt per call.

Done rows are in `current_todo` every call on purpose: their `coverage`
is the evidence the dedup step reads.

### What it decides

**Dedup** — the substantive judgment. For each followup, list the files,
symbols, line ranges and section refs it would cover; compare against
every done row's coverage and every pending row's name+reason; drop at
≥50% overlap. 1,240 followups arrived across the 51 aug5-5 calls, and
most restate queued work. One call: 16 pending in + 39 followups → 21
pending out.

**Completion** — `newly_done: [{id, coverage}]`. Coverage names concrete
files, symbols and line ranges plus the bottom line, and is written
once.

**Retirement** — `retired: [{id, reason}]`, with criteria: evidence
already arrived, dead premise, question answered outright, or strictly
subsumed by another row. List length is explicitly not a reason.

**Plan rewrite** — optional `plan: {steps:[…]}`.

It does **not** decide order. That left the contract entirely.

### The wire type

`TodoEdit`, not `TodoItem`. Every mutable field is `Option`: absent
means unchanged, `Some("")` means cleared. An unchanged pending row is
exactly `{"id":"..."}`.

This mattered. `TodoItem.name` has no `#[serde(default)]`, so the
id-only row the prompt asks for failed schema validation — **five of six
aug6-1 calls** were rejected with `missing field 'name'` and paid a
repair round trip (23,864 uncached input, 9,621 output, ~34% of the
role's output, 94s of a 25-minute session). The repairing model then
satisfied the required field by copying the id into `name`, and
reconcile only guarded against an *empty* name, so **27 of 28 rows ended
the session with `name == id`**.

### What Rust enforces

Six layers, strictest first.

1. **Strict parse** (`json_repair.rs:91`) — no brace scanning, no
   transport unwrapping. Rejects duplicate keys (a custom visitor runs
   *before* `Value` materialises, since `Value` would collapse
   `{"met":false,"met":true}` last-wins), trailing content, unknown
   fields, and the whole array when one row is malformed.
2. **One repair attempt** (`json_repair.rs:273`) — schema + exact errors
   + rejected text, held to the *same* contract. Single-shot. It never
   fired on aug5-5 (51/51 parsed); it fired on 5 of 6 aug6-1 calls
   because of the `name` bug above.
3. **Reconciliation** (`reconcile_update`) — treats a valid reply as
   untrusted:

   | Agent does | Rust does |
   |---|---|
   | marks a nonexistent id done | ignores, logs |
   | retires a done row | ignores — history |
   | emits a row twice | keeps the first |
   | renames an id | resolved by name fallback |
   | sends `step_id`/`depends_on` | discarded, restored |
   | rewords settled coverage | discarded — write-once |
   | reorders the reply | discarded — storage order |
   | forgets a pending row | restored, logged |
   | names nothing and has no name | dropped, logged |

4. **Dedup backstop** — ≥70% token overlap, except that non-empty
   *disjoint* path-token sets are never duplicates. That is what keeps
   `compile-verify-v4` and `compile-verify-v6` apart.
5. **Post-pass** — `assign_ids`, `stamp_missing_coverage`.
6. **Total failure** — `fallback_dedup` merges raw followups by token
   overlap. The list never regresses on a flaky call.

---

## The prioritization agent

`kres-agents/src/prioritize.rs`. Runs on the **slow coding agent** —
same client, model, token budget and thinking config as the slow role,
with the slow coding system prompt, derived in
`Session::with_agent_runner` from the `AgentRunner` rather than from a
separate config entry. There is no way to point the two at different
models by accident.

### Inputs

| Field | Half | Why |
|---|---|---|
| `task` | stable | constant |
| `instructions` | stable | constant |
| `question` | stable | `Plan.prompt` — the operator's raw prompt |
| `skills` | stable | as sent to the slow agents |
| `plan` | stable | changes rarely |
| `ready` | delta | the candidate set, every wave |
| `limit` | delta | the dispatch budget |
| `previous_findings` | delta | grows on most reaps |

`previous_findings` is the largest input — it reached **352KB** on
aug5-5 — and it is deliberately *not* cached. Between two dispatch waves
a batch of tasks has run and produced findings, so the field changes
almost every wave; caching it would pay the 1.25× write with no read.
This is the same reasoning that took `completed_query` and
`original_prompt` out of their prefixes in `4692adc`.

It is sent in **full**, per the AGENTS.md rule that prior findings are
never routed, filtered, or reduced to a source-body-free manifest.

### Output

`{"selected":[{"id":"...","why":"one line"}]}`, best first. The `why` is
printed to the operator log and consumed by nothing.

### What Rust enforces

- ids not in the ready set → dropped with a log line
- duplicates → first wins
- more than `limit` → truncated
- unparseable after one repair → empty → storage-order fallback
- `ready.len() <= limit` → **no call at all**; there is nothing to rank
- `limit == 0` → no call
- every pick unclaimable → falls through to the unranked claim, so a
  stale pick set cannot dispatch zero and leave auto-continue re-ranking
  the same list every five seconds

### The snapshot/claim split

`claim_ready_todos_with_turn_limit` used to filter and claim under one
write lock. A ranking call cannot happen under that lock, so it split
into:

- `ready_pending_snapshot(turns_limit)` — read lock, returns the ready
  rows, the blocked count, and the turn budget. Claims nothing.
- `claim_selected_todos(&ids, turns_limit)` — write lock, claims the
  named rows in the order given, skipping any that stopped being
  Pending or whose dependencies stopped being satisfied.

The `--turns` budget is recomputed inside the write lock, so ranking
cannot buy extra runs.

---

## Who decides what, end to end

| Decision | Owner |
|---|---|
| Is this followup new work? | todo agent |
| Is this item finished, and what did it cover? | todo agent |
| Is this item dead? | todo agent |
| Which items are eligible? | Rust — `depends_on` all Done |
| Which eligible item is most valuable? | **prioritize agent** |
| How many run at once? | Rust — 10, or 1 for `/next` |
| Is there turn budget left? | Rust — `turns − completed − active` |
| Is the session finished? | goal agent |

The model judges; Rust gates. No agent can make a running task
dispatchable, satisfy its own dependency, exceed the batch cap, or
delete work without naming it.

---

## Cost

Replaying the 51 aug5-5 todo replies through the new edit contract:
**1,045,245 characters against 2,055,936 — 49.2% smaller.** Where the
old output went:

| field | share | fate |
|---|---:|---|
| `reason` | 42.9% | model's |
| `coverage` | 27.3% | model's, but write-once now |
| `name` | 14.1% | model's |
| `step_id` | 5.1% | overwritten by Rust |
| `id` | 4.4% | overwritten by Rust |
| `depends_on` | 2.4% | overwritten by Rust |
| `status`, `type` | 3.8% | mostly Rust |

Done rows alone were 44.6%, and Rust already reconstructed them whenever
the agent omitted one.

Against that, the prioritizer adds one call per wave carrying the full
findings set uncached. The pipeline already sends `previous_findings` on
341 calls per aug5-5-sized run; roughly 10–15 more is a small
proportional increase, but it is an increase, not a saving.

---

## Open

- **Unmeasured.** No run has executed the prioritization agent, the
  `TodoEdit` contract, the `just_completed` requirement, or the
  retirement criteria. Every figure above for the new behaviour is a
  replay projection.
- **Repair is all-or-nothing.** A failed repair discards the round's
  completions and retirements; only raw followups survive via
  `fallback_dedup`.
- **Convergence is unproven.** aug5-5 ended at 70 pending and still
  growing; aug6-1 went 5 → 28 rows against 4 completions with `retired`
  never used. The criteria added in `6517f27` are an attempt to make
  the channel usable, not evidence that it will be used. If the list
  still runs away, the fix is a drain to the deferred list — the
  mechanism `drain_pending_blocked()` already uses — not a cap.
- **`/next` pays a full ranking call for one slot.** Correct, but the
  most expensive form of the call per item dispatched.
