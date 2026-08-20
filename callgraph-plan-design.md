# Design: call-graph grouping for the initial coverage plan

Status: implemented (`kres-repl/src/session.rs`). This document is the
record of the design and of the one place measurement contradicted it.

## The defect this addresses

`coverage_plan_steps` (kres-repl/src/session.rs:4888) partitions the
whole-file risk scan into audit steps by sorting on `risk_rating`
descending, name ascending, then `.chunks(per_step)` with
`per_step = 25` (call site session.rs:3151).

Membership is therefore decided by a per-function scalar. Two
functions land in the same step only if their ratings are adjacent.
A defect that lives in the *composition* of two functions — each
individually unremarkable — is only findable if both bodies reach one
lens prompt, and this partition has no mechanism that puts them there.

Measured on the live 2026-08-20 `arch/x86/kvm/mmu/mmu.c` review
(`~/local/linux.kvm/kres-kvm17`), 312 rated functions, 13 steps. The
slow fault-install chain partitions like this:

| function | risk | sorted pos | step |
|---|---|---|---|
| `kvm_tdp_page_fault` | 3 | 216 | 09 |
| `direct_page_fault` | 6 | 45 | 02 |
| `is_page_fault_stale` | 6 | 48 | 02 |
| `direct_map` | 5 | 69 | 03 |
| `make_mmu_pages_available` | 3 | 222 | 09 |
| `kvm_mmu_zap_oldest_mmu_pages` | 3 | 212 | 09 |
| `kvm_mmu_available_pages` | 2 | 285 | 12 |

One call chain, four steps, 240 sorted positions between the extremes.

That is the chain holding the bug fixed upstream by commit
`2abd5287f083` ("KVM: x86: Check for invalid/obsolete root *after*
making MMU pages available"): `direct_page_fault` runs
`is_page_fault_stale()` before `make_mmu_pages_available()`, and the
reclaim inside the latter can zap the very root the former just
validated. Both halves were audited and both were signed off clean
in isolation — `report.md:425` records `is_page_fault_stale()`'s guard
window as clean from step 02, `report.md:51` records
`make_mmu_pages_available()` as clean from step 09, on the
`KVM_REFILL_PAGES - avail` question only. Neither prompt contained the
other function.

The existing comment on `coverage_plan_steps` already states the
diagnosis: "a per-function score cannot see a bug that lives in the
composition of individually-boring functions." It fixed *coverage*
(every function is now in some step). It did not fix *adjacency*.

## Goal

Partition the same function set so that call-adjacent functions land in
the same step, preserving every property the current experiment bought:

- exhaustive — every scan function in exactly one step;
- bounded — no step exceeds `per_step`;
- deterministic — same scan produces byte-identical steps, so
  checkpoints, `--resume`, and the session-state hash stay stable;
- ordered by risk — the highest-risk material still runs first.

Non-goals: no change to lens behaviour, requeue, the todo contract, or
the prioritization agent. No inter-step `depends_on` (that would
serialize the fan-out; AGENTS.md already records the opening plan step
as serial time the whole run pays). No model call to do the grouping —
this is arithmetic over typed data.

## Where the edges come from

There is no call graph in the scan today. `ScanFileSurvey` carries
`{name, uses, risk_rating}` per function and nothing else
(session.rs:6350ff).

semcode's `file_survey` does not supply one either. Verified against a
real payload logged during the kvm17 run
(`.kres/logs/80221bf1-.../code.jsonl`): it returns

```json
{"file": "arch/x86/kvm/mmu/mmu.c",
 "functions_defined": [["is_cr0_pg", 3], ["is_cr4_pae", 2], ...],
 "calls": [["write_unlock", 20], ["x86_emulate_instruction", 1], ...]}
```

Both arrays are flat and file-scoped. `calls` is the union of every
callee named anywhere in the file, with no attribution to a caller.
`FileSurveyInventory` (session.rs:6859) keeps exactly that shape:
`functions: BTreeMap<String, u64>`, `calls: Vec<String>`.

So edges have to be built. Two sources, unioned.

### Primary: local extraction from source

Deterministic, offline, no MCP round-trip, no indexing wait.

`ctags_function_inventory` (session.rs:6993) already shells out to
universal-ctags with `--output-format=json --languages=C --kinds-C=f
--sort=no` and discards every field but `name`. Adding `--fields=+ne`
yields `line` and `end` per function. Verified on the target:

```
{"name": "make_mmu_pages_available", ..., "line": 2794, "end": 2815}
{"name": "is_page_fault_stale",      ..., "line": 4682, "end": 4709}
{"name": "direct_page_fault",        ..., "line": 4711, "end": 4750}
```

That gives exact body ranges. For each function, slice its lines, run
the existing `code_without_comments_and_literals` (session.rs, backing
`identifier_occurrences`), and record which *target-local* function
names occur as whole identifiers. That is the intra-file edge set.

Two properties worth stating plainly:

- It over-approximates. `.rmap_zap = kvm_zap_rmap` is recorded as an
  edge even though it is an assignment, not a call. For co-membership
  that is a feature: an ops-table entry is exactly the kind of
  unchanged-dispatch relation AGENTS.md tells reviews to trace.
- It under-approximates through macros that construct names by
  pasting. Accepted; the semcode enrichment below covers some of it,
  and the spilled-neighbour text (below) covers the rest by asking.

This path also makes ctags unconditional rather than
fallback-only. When ctags is absent it currently returns an empty set
without failing (session.rs:7000); the grouping must degrade the same
way.

### Enrichment: semcode `find_calls`

`McpMethodMap` already wires `find_calls` and `find_callers`
(kres-agents/src/mcp_fetcher.rs:45-46), and the fetcher already has
`callers`/`callees` followup kinds. One `find_calls` per target
function gives authoritative out-edges, including macro-generated ones
the local scan misses.

Recommend this be **off by default**, behind a flag, until measured.
It is N index lookups at bootstrap (312 for this file) on the serial
path, and the serial cost of bootstrap has a measured history in this
repo: the standalone file survey was deleted partly for costing "~78s
of serial bootstrap per review."

Per the semcode rule in AGENTS.md, a failed, empty, or unparseable
result must not be read as "no edges" — it falls through to the local
set, which is already complete enough to group on.

### Secondary signal: shared external callees

Two target functions that both call the same *external* helper are
related even with no direct edge between them. Include these as weak
edges, with inverse-frequency damping: an external callee named by more
than `SHARED_CALLEE_MAX_FANOUT` (start at 8) target functions
contributes nothing. This keeps `write_lock` (20 uses in the payload
above) out of the graph while letting `set_memory_decrypted` tie
`__kvm_mmu_create` to `free_mmu_pages`.

### Last resort

No ctags, no semcode, no edges → fall back to today's risk-sorted
chunking and log the degradation. Grouping is an optimisation of the
partition, never a precondition for producing one.

## The grouping algorithm

Undirected graph over target functions, one edge per intra-file
call/reference. Then **seeded breadth-first accretion**:

1. Order all functions by `risk_rating` descending, name ascending —
   the same key the previous code used, so determinism and risk-first
   ordering are inherited rather than reinvented.
2. Take the first unassigned function with at least one in-scan
   neighbour as a seed; open a step.
3. BFS from the seed, expanding each frontier in that same risk order,
   until `per_step` members or the component is exhausted.
4. Repeat from 2 until no seed remains.
5. Everything unassigned — isolated functions, and anything whose
   cluster filled before BFS reached it — falls to trailing
   risk-ordered chunks, which is exactly the old partition and is
   correct for a static initialiser or a one-line accessor.
6. Adjacent under-full clusters merge while the total fits `per_step`,
   so a file of many small components does not become many small
   steps. Every step is a todo row and the todo agent stops
   maintaining the pending list above roughly 60 of them.

Determinism requires ordered containers throughout. No `HashMap`
iteration anywhere in this path.

### BFS, not max-score greedy — the design was wrong here

This document originally specified weighted greedy accretion: repeatedly
add the unassigned function with the greatest total edge weight to the
step's current members, with direct edges at weight 3 and
shared-external-callee edges at weight 1 under a fanout cap.

That was implemented and measured against the real 312-function
`arch/x86/kvm/mmu/mmu.c` scan from kvm17. It **failed the motivating
case**: the chain landed in steps 2, 6, 7 and 14 of 15 — no better
than the risk chunking it replaced.

The reason is structural, not a tuning problem. Max-score greedy
optimises the cluster's *internal density*, which is a different
objective from call adjacency. In a dense file a chain member with one
edge to the current members (score 1) loses to any of the many
candidates with two, so a linear chain is precisely what gets starved.

BFS keeps a function's own neighbours queued directly behind it, which
is what "call-adjacent" means. Same fixture, BFS:

| function | old partition | risk-chunk step | BFS step |
|---|---|---|---|
| `direct_page_fault` | 6 | 02 | **03** |
| `is_page_fault_stale` | 6 | 02 | **03** |
| `direct_map` | 5 | 03 | **03** |
| `kvm_tdp_page_fault` | 3 | 09 | **03** |
| `make_mmu_pages_available` | 3 | 09 | **03** |
| `kvm_mmu_zap_oldest_mmu_pages` | 3 | 09 | 09 (spilled, named) |
| `kvm_mmu_available_pages` | 2 | 12 | 15 (spilled, named) |

`is_page_fault_stale` and `make_mmu_pages_available` — the two halves
of the composition that commit `2abd5287f083` fixes — are in one step
for the first time. The two that did not fit are level-2 from
`make_mmu_pages_available` and hit the 25 cap; both are named in step
03's spilled-neighbour list, which is what that list is for.

### The two proposed edge sources that were dropped

**Shared external callees (weight 1, fanout cap 8).** Dropped. It adds
two tunables with no measurement behind either, and the motivating case
is carried entirely by direct edges — the table above uses direct edges
only. The design's own risk section warns that a dense, uninformative
graph degrades the partition, and weak edges are the main way to make
one.

**semcode `find_calls` enrichment.** Dropped, for the reason the design
gave for proposing it off-by-default: it is N index lookups on the
serial bootstrap path, and an unmeasured flag nobody turns on is dead
code. The local extractor found 303 of 312 functions to have at least
one in-scan edge, so the marginal coverage was never going to justify
312 serial round-trips. The option stands recorded here if a file ever
turns out to need it.

### The cap still bisects some chains

A hub has more neighbours than fit in `per_step`; any partition splits
it. The mitigation is in the step *description*: it appends, by name,
every direct neighbour of the step's members that is audited in a
different step. On the fixture above step 03 names 26 such functions,
including both chain members that did not fit. This turns an
unavoidable partition boundary into a gather instruction instead of
leaving the lens to infer which callees matter, and it is the part of
this design most likely to carry the benefit even when clustering picks
badly.

## Step titles and ids

Positional titles ("Audit functions 26-50 of the file: …") stop meaning
anything once membership is graph-derived. Name the cluster by its
seed — the highest-risk member:

> Audit the `direct_page_fault` call cluster (25 functions):
> direct_page_fault, is_page_fault_stale, make_mmu_pages_available …

Ids become `audit-cluster-NN`, still zero-padded and still ordered by
seed risk. Two consequences to check, not to work around:

- `review_todos_from_plan` (session.rs:4930) derives todo ids as
  `review-<step.id>`; nothing else parses the id text.
- `followup::is_opening_plan_step` is positional and must stay
  positional. Under risk-seeded ordering step 01 is still the
  highest-risk cluster, so the "opening step never requeues" property
  is preserved by construction — but it should be asserted in a test,
  not assumed.

No migration is needed for in-flight sessions: `--resume` reads the
persisted plan out of `session.json` and does not rebuild it.

## What this changes on kvm17, measured

Re-running the real kvm17 scan (312 functions, ratings from
`change-survey.json`) through the implementation: 15 steps, 312
members, none over the cap, and the five-function core of the chain in
step 03 — see the table above.

The claim this supports is narrow and worth keeping narrow: **both
halves of the composition are now in one lens prompt.** That is a
precondition the old partition failed, not a guarantee the lens draws
the right conclusion. The run's own `report.md:425` shows a lens
declaring `is_page_fault_stale()`'s guard window clean with only half
the evidence; nothing here proves it would decide differently with the
other half. It would, at least, be able to.

## Costs and risks, stated

- **Risk bands stop being uniform within a step.** A step now mixes
  risk 6 and risk 2 bodies. Intended — the premise is that this bug
  class lives among individually-boring functions — but it means a
  step's prompt can no longer be trimmed by rating if it ever needs to
  be.
- **Hub distortion.** Greedy accretion around a high-degree function
  pulls a large undifferentiated blob. The per-step cap bounds the
  damage; the inverse-frequency damping does not help for *intra*-file
  hubs. Accepted rather than mitigated, because every mitigation I can
  think of adds a tunable that would need its own measurement.
- **Bootstrap cost.** One extra ctags invocation with two more fields,
  plus a linear pass over the file. Negligible. The semcode enrichment
  is not, which is why it is proposed as off by default.
- **A worse partition is possible.** If the graph is dense and
  uninformative, clusters degenerate toward arbitrary. The fallback
  path and the spilled-neighbour text both hold in that case, so the
  floor is roughly today's behaviour.

## Verification

Unit tests in `kres-repl/src/session.rs`:

- `coverage_plan_covers_every_function_in_the_scan` — partition
  property with an empty edge set, which must reproduce the old
  risk-ordered chunking exactly; plus the malformed-scan assertions.
- `a_call_chain_shares_a_step_across_risk_bands` — the fixture that
  matters, shaped after the kvm17 chain: four functions spread across
  risk bands that a risk sort alone would split, asserted into one
  step.
- `coverage_plan_is_deterministic_and_respects_the_cap` — byte-identical
  steps on repeat, cap honoured, every function exactly once, and step
  01 seeded by the highest-risk function so
  `followup::is_opening_plan_step` (positional) keeps its meaning.
- `a_split_cluster_names_its_spilled_neighbours` — every neighbour is
  either a member or named in the spill note, and never both.
- `edges_cover_dispatch_tables_but_not_comments_or_strings` — an
  ops-table assignment makes an edge; a name appearing only in a
  comment or a string literal does not.

Degradation is by construction rather than by test: `ctags_function_ranges`
returns an empty map for a missing ctags, a non-zero exit, or
unparseable output, and an empty edge set makes every function isolated,
which is the trailing-chunk path.
