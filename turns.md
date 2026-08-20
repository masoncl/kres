# Turn analysis: `review: mm/page_alloc.c`, 50 turns

Run `kres-aug5-5`, log `a08d4031-2b3b-5cdf-a4a9-2fe562b27459`. Ran to the cap:
50/50 completed runs, 53 todos done, **33 findings** (6 high, 24 medium, 3 low;
11 invalidated, 22 active). 184 minutes.

Claims below are checked against `~/local/linux.kres` at HEAD `6caa892333d1`.

## 1. What each turn generated

The todo agent ran 51 times and created **127 distinct todo ids**. The initial
plan had 6. So 121 were generated in flight.

New todos introduced per todo-agent call:

```
turn:  1  2  3  5  7  8  9 10 11 12 13 14 16 17 19 21 22 23 24 25 26 27 28 29 30
new:   6  1  8  5  6  2  2  3  5  5  3  4  3  5  1  2  4  1  3  3  2  1  1  4  2
turn: 31 33 34 35 37 38 40 41 42 43 44 45 47 48 50 51
new:   1  1  2  2  3  3  3  2  2  2  4  4  5  2  4  5
```

Generation never decayed. Turns 47–51 — the last five — created **20 new
todos**. The run hit its cap while still expanding, not converging. It ended
with 53 done out of 127 created; the remainder were drained as deferred.

### The opening decomposition was good

The plan agent partitioned `mm/page_alloc.c` into five semantic paths:

| step | what it covers |
|---|---|
| `review-audit-slowpath-and-freelist` | `__alloc_pages_slowpath`, `get_page_from_freelist` |
| `review-audit-contig-range-family` | `alloc_contig_range` / `free_contig_range` |
| `review-audit-pcp-and-fallback-paths` | pcp lists, fallback/claim/steal, highatomic |
| `review-audit-page-prepare-and-pageblock-accessors` | prep/post-alloc hooks, pageblock bits |
| `review-consolidate-page-alloc-findings` | cross-contract close-out, depends on all four |

That is a defensible carve-up of the file's real structure, with the
consolidation step correctly declared dependent on all four. Breadth was
established correctly at turn 1.

### Then it went almost entirely to depth

Grouping the 127 ids by leading token: `contig` 12, `alloc` 6, `pcp` 5, `fpi`
5, `zone` 5, `emit` 5, `review` 5, `bulk` 4, `nr` 3, `hwpoison` 3, `kswapd` 3,
`nofail` 3, `gfp` 3, `early` 3, `page`/`pageblock` 6, and a long tail of
singletons.

Only 5 of 127 are `review-*` — the original breadth steps. Essentially
everything else is evidence-closing: *prove*, *bound*, *enumerate*, *close*,
*cite*. Representative:

- `pcp-index-order-bounds` — "Bound order_to_pindex/pindex_to_order"
- `fallback-array-bounds-and-claim-inputs` — "fallbacks[][] dimensions"
- `nolock-nmi-caller-enumeration` — "Enumerate concrete NMI/BPF callers"
- `gfp-movable-reclaimable-tree-search` — "Tree-wide search for masks co-setting…"

This is the right *kind* of work — AGENTS.md demands that negative coverage
claims be earned with concrete evidence, and these todos are exactly that. But
the ratio is extreme: **the review spent ~95% of its turns deepening five
initial hypotheses rather than widening coverage of a 8,000-line file.**

Whole regions of `mm/page_alloc.c` never acquired a todo: the watermark
family (`__zone_watermark_ok`, `zone_watermark_fast`), `build_zonelists` and
NUMA zonelist construction beyond one seqlock todo, the `show_free_areas`/
meminfo reporting path, and the early-boot memmap init family beyond
`init_pageblock_migratetype`. Whether that is correct triage or tunnel vision
depends on the risk scan — but nothing in the todo stream shows the agent
*deciding* those were low-risk. They simply never came up.

## 2. Prioritisation quality

**Good:** dependency edges were used properly. `close-slowpath-evidence-gaps`
depended on `review-audit-slowpath-and-freelist`; the consolidation step
depended on all four audits. The middle wave stayed at 3–4 parallel groups as
the review policy prompt requires.

**Good:** the goal agent was appropriately hard to satisfy — 37 checks, 14
`met=false`, and its reasons are specific and correct:

> check 1: "The consolidation only records two Findings with placeholder
> locations (mm/page_alloc.c:0)…"
> check 2: "The analysis covers only two of the four path groups…"
> check 3: "Three of the four path groups are covered, but the
> pcp/fallback/claim-steal/highatomic group…"

That is real coverage accounting, not rubber-stamping. It caught the
placeholder-citation defect on the very first check.

**Bad:** it then returned `met=true` 23 times while 52 todos were still
pending and the consolidation step never completed. The `--turns 50` cap, not
the goal agent, ended the run.

**Bad:** the agent spent turns repairing its own output. Six todos are
self-repair:

| turn | todo | what it was fixing |
|---|---|---|
| 2 | `fix-finding-citations-slowpath` | placeholder `mm/page_alloc.c:0` locations |
| 31 | `emit-zone-pcp-reset-finding` | analysis existed, no Finding record emitted |
| 33 | `fix-mislabeled-finding-record` | a finding recorded under the wrong id |
| 51 | `emit-pcp-index-and-fpi-findings` | ditto |
| 51 | `emit-isolation-offline-findings` | ditto |
| 51 | `emit-hwpoison-findings` | ditto |
| 51 | `emit-alloc-tag-findings` | ditto |
| 51 | `fix-nofail-finding-citations` | placeholder citations, second attempt |

Turn 51 — the last — spent 5 of its 5 new todos on emitting findings that
earlier turns had analysed but never recorded. The agent correctly diagnosed
its own pipeline failures; it did not succeed in fixing them (see §4).

## 3. Are the findings any good? (checked against the source)

I verified two in detail.

### `high_order_numa_stats_undercount` — correct, well-judged

Claim: order-N allocations bump NUMA counters by 1 instead of 2^N.

Verified. `mm/page_alloc.c:3257` is literally `zone_statistics(preferred_zone,
zone, 1);`, and `zone_statistics()` at `:3186` forwards `nr_account` to
`__count_numa_events()`. The finding's own summary notes the adjacent line
passes `1 << order` to PGALLOC — an accurate and telling contrast.

Rated **low**, which is right: statistics only, no memory-safety impact. The
reproducer is concrete (snapshot `numastat`, force order-2 through both the
buddy and warm-pcp paths). This is a good finding.

### `fallbacks_row_index_highatomic_oob` — mechanism correct, severity not earned

Claim: `find_suitable_fallback()` indexes `fallbacks[][]` with an unvalidated
migratetype that can be `MIGRATE_HIGHATOMIC`, a one-row OOB read.

Every structural element verified:
- `fallbacks[MIGRATE_PCPTYPES][MIGRATE_PCPTYPES - 1]` — `page_alloc.c:1921`
- `fallbacks[migratetype][i]` with no row check — `:2266`
- `MIGRATE_HIGHATOMIC = MIGRATE_PCPTYPES` (both == 3) — `mmzone.h:122-123`
- `gfp_migratetype()` returns 3 when both `__GFP_MOVABLE` and
  `__GFP_RECLAIMABLE` are set — `gfp.h:37`, with `BUILD_BUG_ON` at `:30-31`
  asserting exactly that encoding

The call chain in the summary (`ac->migratetype` → `__rmqueue_claim`/
`__rmqueue_steal` → `__rmqueue` → `rmqueue_buddy`/`rmqueue_bulk`) is accurate.

**But there is no in-tree caller.** A tree-wide search for sources setting both
flags returns two hits, both inside `include/linux/gfp.h` itself — the
`GFP_MOVABLE_MASK` definition and the `BUILD_BUG_ON`. `gfp.h:26` guards the
combination with `VM_WARN_ON`, i.e. the kernel treats it as a caller bug.

The agent **ran this exact search** — todo `gfp-movable-reclaimable-tree-search`
at turn 35, "Tree-wide search for masks co-setting `__GFP_MOVABLE` and
`__GFP_RECLAIMABLE`". The negative result never reached the finding. It is
still **medium/active**, its summary still asserts the path, and its reproducer
still opens "Issue an allocation whose gfp mask has both … set" without noting
that nothing in the tree does.

This is the AGENTS.md negative-coverage rule failing in the *other* direction.
The rule exists to stop unearned claims of absence; here a genuine, earned
negative result was gathered and then dropped, leaving an unreachable defect
rated as a live one. A missing bounds check on a `VM_WARN_ON`-only contract is
worth reporting — as hardening, at low severity, explicitly marked
unreachable in-tree.

## 4. Three defects in the pipeline, not the review

### 4.1 Finding ID reuse merges distinct defects

Several ids describe one defect while their title and summary describe a
completely different one:

| id | title actually describes |
|---|---|
| `contig_comp_ignores_bad_page` | `unpoison_memory()` double `folio_put()` |
| `pcp_drain_loop_undercounts_freed` | `gfp_migratetype()` returning `MIGRATE_HIGHATOMIC` |
| `clear_highpages_unbounded_batch` | `__GFP_COMP` arm alignment check |
| `decay_pcp_high_unlocked_high_rmw` | `zone_pcp_reset()` retiring `per_cpu_pageset` |
| `early_pfn_pool_leak_on_compressed_bail` | runtime `mem_alloc_profiling_key` toggling |
| `free_pages_prepare_drops_fpi_trylock` | `post_alloc_hook()` page_owner tail |

`contig_comp_ignores_bad_page` carries **9 detail entries from unrelated
tasks** (`page-prepare-and-pageblock-accessors`, `contig-badpage-trigger`,
`contig-frozen-refcount-handoff`). One record accumulated several distinct
defects and the last writer's title won.

So **33 findings understates the number of distinct defects**, and the
overwritten ones are unrecoverable from the record. The agent noticed —
`fix-mislabeled-finding-record` at turn 33 — and the corruption survived to the
end. This is the most damaging problem here: it silently loses review output.

### 4.2 Placeholder citations survive two repair attempts

**6 of 33 findings** still carry `mm/page_alloc.c:0` symbol locations:
`nofail_loop_skips_reclaim_compact_first`, `bulk_wrong_lowmem_reserve_index`,
`free_pages_prepare_drops_fpi_trylock`, `contig_comp_ignores_bad_page`,
`pcp_batch_zero_infinite_loop`, `early_pfn_pool_leak_on_compressed_bail`.

The goal agent flagged this on **check 1**. Todos at turn 2 and turn 51 both
targeted it. Both failed. A finding with a `:0` citation cannot be exported,
triaged, or verified — it fails the review's own contract that findings cite
`filename:line`.

### 4.3 Scope drift off the target file

Two findings cite no `mm/page_alloc.c` symbol at all
(`page_owner.c`+`stackdepot.c`; `pgalloc_tag.h`+`alloc_tag.h`), and
`vrealloc_shrink_frees_pages_outside_busy_loc` is entirely in `mm/vmalloc.c`
— unreachable from `page_alloc.c` review scope by any contract argument.

Following a changed contract into `memory-failure.c` or `memory_hotplug.c` is
legitimate cross-file work and AGENTS.md explicitly asks for it. `vrealloc` is
not that; it is drift. Roughly 12 of the 127 todos target subsystems
(`hwpoison`, `mte`, `codetag`, `page_ext`, `vmstat`) reached by following
helpers out of the file.

## 5. Effectiveness verdict

**What worked.** The initial five-way decomposition matches the file's real
structure. The goal agent's coverage accounting is genuine and specific. The
evidence-closing discipline is real — when the agent asserts a call chain, the
chain checks out line by line. A 33% self-invalidation rate (11/33) shows the
validation loop actively killing its own bad hypotheses, which is healthy.

**What did not.** The run never converged: 127 todos created, 53 done, still
generating 20 in its last five turns, consolidation never completed, ended by
the turn cap. Depth swamped breadth — 5 of 127 todos widened coverage. And the
three pipeline defects above mean the *output* is worse than the *analysis*:
findings merged under reused ids, six uncitable, some off-target.

**Where I would push next.** In order of damage:

1. **Finding identity** (§4.1). Distinct defects are being merged. Everything
   downstream — dedup, triage, export, the summary pipeline — trusts the id.
2. **Placeholder citations** (§4.2). Two self-repair attempts failed; this
   needs a Rust-side rejection at record time, not another agent todo.
3. **Reachability must feed severity** (§3). The agent gathered the negative
   result and dropped it. A finding whose trigger has no in-tree caller should
   say so and be rated accordingly.
4. **Breadth budget.** Something should force periodic widening, or a large
   file will always be reviewed as five deep shafts.

## Caveat

One run, one file. The 6/24/3 severity split and the 33% invalidation rate are
single-sample. Only two findings were checked against source in depth; the
remaining 31 are unverified here, and my ID-reuse conclusion rests on title
mismatch plus detail-entry provenance rather than on replaying the delta-apply.
