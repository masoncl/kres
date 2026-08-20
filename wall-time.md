# Where the wall time went

`kres --prompt "review: mm/page_alloc.c" --results kres-aug5-5 --turns 50`,
log `a08d4031-2b3b-5cdf-a4a9-2fe562b27459`. Ran to the turn cap: 50/50
completed runs, 53 todos done, 33 findings.

**Wall clock 11,067s (184.4 min). 654 of 654 assistant records paired** — every
call is attributable, which is only true since the phase labels added in
`e3e9a07`.

## Headline

| | |
|---|---|
| total model-seconds | 79,238 |
| wall clock | 11,067s |
| average concurrency | **7.16** |
| wall with nothing in flight | **19s (0.2%)** |

Wall time is inference. Rust-side work, disk, and scheduling account for 19
seconds of a three-hour run.

But average concurrency of 7.16 hides the real structure: **54.5% of the run
has exactly one call in flight.** The parallelism is bursty — deep fan-outs
alternating with long single-call stretches.

## Model-seconds vs critical path

These two rankings disagree, and the disagreement is the whole story.

| stage | calls | model-s | % model-s | mean | max | **critical path** |
|---|---:|---:|---:|---:|---:|---:|
| slow-lens | 258 | 57,091 | **72.1%** | 221s | 647s | **0s (0.0%)** |
| **todo** | 51 | 9,238 | 11.7% | 181s | 316s | **5,250s (47.4%)** |
| consolidate | 50 | 7,337 | 9.3% | 147s | 268s | 0s (0.0%) |
| fast-gather | 142 | 4,051 | 5.1% | 29s | 117s | 20s (0.2%) |
| promote | 50 | 549 | 0.7% | 11s | 51s | 359s (3.2%) |
| goal check | 37 | 435 | 0.5% | 12s | 28s | 218s (2.0%) |
| slow (surveys) | 2 | 159 | 0.2% | 80s | 89s | 159s (1.4%) |
| cache-probe | 52 | 127 | 0.2% | 2.4s | 5s | ~0s |
| json-repair | 10 | 223 | 0.3% | 22s | 131s | — |

"Critical path" here means seconds during which that stage was the *only* call
in flight, so its latency was the run's latency.

**The lens fan-out is 72% of the compute and 0% of the critical path.** It is
completely overlapped — 258 calls averaging 221s each, absorbed by concurrency.
The probe work is why: 52 probes costing 127s total let every lens of a task
start at once.

**The todo agent is 11.7% of the compute and 47.4% of the critical path.**

## The todo agent is the bottleneck

Of its 9,238 model-seconds, **5,250s (57%) run with nothing else in flight at
all**. That is 47.4% of the entire run.

It is a barrier by construction: the reaper calls it after each task is reaped,
before the next work can be dispatched. When it does overlap, it overlaps with
slow-lens (2,890s) and consolidate (2,778s) — the tail of the previous wave,
not the next one.

It is slow because it is **output-bound and rewrites the entire todo list every
call**:

| call | prompt chars | reply chars | output tokens | secs |
|---|---:|---:|---:|---:|
| 1 | 43,321 | 6,498 | 3,976 | 42 |
| 3 | 50,900 | 17,129 | 8,948 | 88 |
| 7 | 59,401 | 25,161 | 13,260 | 128 |
| … | | | | |
| 49 | 95,301 | 60,660 | 27,698 | 245 |
| 51 | 100,651 | 45,440 | 22,656 | 208 |

Output grows 3,976 → 27,698 tokens as the list grows. Across 51 calls it emits
**1,009,298 output tokens — 27% of the run's entire 3.7M output** — at a
steady **9.2s per 1k output tokens**. Duration tracks output almost linearly;
this is generation time, not thinking time and not input size.

**If the todo agent were fully overlapped, this run could approach 97 minutes
instead of 184.**

Two directions, neither attempted here:

- *Overlap it.* The barrier exists so the next dispatch sees an updated list.
  Whether a reap can proceed against a slightly stale list is a correctness
  question about the todo/goal contract, not a performance tweak.
- *Make it emit less.* It rewrites all 53 rows to change a few. A delta
  protocol would cut output roughly in proportion — but the current design
  deliberately has the agent restate the full list so done-item history and
  `depends_on` edges survive, and AGENTS.md requires that history be preserved.
  Any delta scheme has to prove it cannot drop a row.

## Everything else

**consolidate** — 7,337 model-seconds, 9.3% of compute, **0s critical path**.
50 calls at 147s mean, entirely overlapped. Expensive but free in wall terms.

**promote** and **goal check** — small in compute (549s, 435s) but 359s and
218s on the critical path, because they too sit in the reap sequence. Together
5.2% of wall for 1.2% of compute.

**The change/file surveys** — 159s total, 1.4% of wall, both on the critical
path since they are the bootstrap. Two calls, no fallback, no retries: the
`from_context` `#ifdef` fix and the "Rust owns `uses`" change are holding.

**cache-probe** — 52 calls, 127s total, 2.4s mean. Bought the complete overlap
of 57,091 model-seconds of lens work. Time with a primer in flight and no lens
running is negligible; the 1,059s of seed serialization measured on the
vmscan run is gone.

**Evidence fetch** — 88 fetches, 1,016s total, 11.6s mean, 47s max. Visible
only because `e3e9a07` added the `phase=fetch` record; it was previously
inferable only as a gap and hidden by concurrency. It does not appear on the
critical path.

## Concurrency shape

```
 0 calls:     19s   0.2%
 1 call:   6,034s  54.5%   <- todo/promote/goal-check reap sequence
 2-9:      2,329s  21.0%
10-19:     1,295s  11.7%
20-48:     1,390s  12.6%   <- lens fan-out across parallel tasks
```

The run alternates between a wide fan-out phase (up to 48 concurrent calls) and
a narrow reap phase. The fan-out is efficient. The reap phase is the cost.

## What would actually move the number

Ranked by measured critical-path seconds:

1. **Overlap or shrink the todo agent — 5,250s (47.4%).** Nothing else is
   close. Its 1M output tokens are also 27% of the run's output spend, so this
   is simultaneously the largest latency and a large cost item.
2. **promote + goal check — 577s (5.2%).** Same reap-sequence barrier, much
   smaller.
3. **Bootstrap surveys — 159s (1.4%).** Already two calls; little left.

Optimizing the lens fan-out further is pointless: it is 72% of the compute and
already contributes nothing to wall time.

## Caveat

One run, one target. The todo agent's share grows with the todo list, and this
run ended with 53 rows — a shorter run would show a smaller barrier. The
`--turns 50` cap with 52 items still pending when it ended means the list was
still growing when the run stopped, so 47.4% is the figure for a long,
non-converging review, not a universal constant.
