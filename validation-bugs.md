# Validation-pass defects: false positives that survived `/validate`

Found 2026-08-07/08 by re-validating the 72 active findings in the
`~/local/kres-sched2` export by hand against
`/home/clm/local/linux.kres` @ `6caa892333d1` (the same sha every finding
records in `git.sha`, so nothing here is source drift).

Eight of the 72 are false positives that the `validate` workflow passed as
`Plausible`. This file is about *why they passed*, not about the findings
themselves — the per-finding disproofs are in
`~/local/kres-sched2/invalidated-findings.md`.

Evidence: one kres session per finding under
`/home/clm/local/linux.kres/.kres/logs/<uuid>/code.jsonl`. All eight ran
`claude-sonnet-5`, `thinking: adaptive`, `effort: medium`, and all eight
ended at `verdict: Plausible`.

| finding | session |
|---|---|
| `asym_shared_alias_nr_idle_scan` | `007c905a-0ec4-5cf4-9aa3-9732a975cf41` |
| `fair_server_pick_task_retry_task_leak` | `7c0ec740-43fe-5728-adfa-a3c3214c12fa` |
| `retry_task_leaves_stale_dl_server` | `341e8ea6-3aad-5b2e-abb5-46689f12007a` |
| `throttled_flag_leaks_across_class_switch` | `264df6d5-72e8-5a62-bf01-6bae8ee45533` |
| `fork_inherits_throttled_flag` | `4815110e-c758-5e6c-a4e1-fd5d066a74a4` |
| `sched_change_begin_pi_lock_ttwu_race` | `ddf48eb7-8b9c-5f89-9e64-333c11ed650e` |
| `task_work_add_esrch_stale_next_leak` | `e0539af5-b5ec-558b-98fa-65404855306f` |
| `sic_idle_core_bias_outranks_capacity_fit` | `73173939-3b38-51bf-b770-88458e5b1f64` |

**The headline is not that retrieval failed.** Six of the eight sessions had
the disproving fact in the slow agent's context, and three of them wrote it
down as a `supported` claim before passing the finding anyway. These are
composition failures, not evidence failures. Buying more fetches or a bigger
model will not fix most of them.

---

## 1. `claim_validation` is a flat array, so contradictions between two
##    supported claims are never detected

**The defect.** `configs/workflows/validate.json`'s fast step emits
`supported[] / contradicted[] / unresolved[] / open_questions_to_close[] /
false_positive_risks[]`. Each entry is verified in isolation. The slow step's
prompt says to "use that report as a checklist, then close every material
open question that gates whether the bug exists." Nothing anywhere asks
whether the claims are *mutually satisfiable*.

**Worst case, `retry_task_leaves_stale_dl_server`** (`341e8ea6`, rec 13). Two
entries, adjacent, in the same `supported[]` array:

- `pick_task_fair_retry_source` — "returns NULL directly when
  `sched_core_enabled(rq)` is true (**so this RETRY_TASK path cannot fire
  when core scheduling is active**)"
- `core_sched_gating_of_dl_server_reads` — `__task_prio()` and `prio_less()`
  "both are **reachable only when CONFIG_SCHED_CORE is enabled**"

Every claim true; the conjunction empty. The finding needs the leak (core
sched off) and the reader (core sched on) simultaneously. The whole finding
dies on those two lines and the workflow has no step that would ever put them
next to each other.

**Same shape, `fair_server_pick_task_retry_task_leak`** (`7c0ec740`). Fast
pass listed `core_sched_variant_closed_for_fair` as supported *and* raised
`p_dl_server_reader_scoping` as unresolved. It then fetched the reader bodies
(`read:kernel/sched/core.c:190+70`, rec 17) and closed the open question in
rec 19 as:

> "reachable only under CONFIG_SCHED_CORE with core scheduling enabled — a
> real but config/feature-gated exposure, not a universal one."

It resolved the open question against source, correctly, and never compared
the answer to the supported claim four entries above it.

**Same shape, `sched_change_begin_pi_lock_ttwu_race`** (`ddf48eb7`). The
finding needs a conjunction: a `sched_change` call site that holds only the
rq lock **and** passes `DEQUEUE_CLASS` (without which
`sched_change_begin()`'s `switching_from` hook — the thing that drains the
delayed task and clears `p->on_rq` — never runs). The session verified the
two conjuncts against *different call sites*. Its slow output quotes the
flags verbatim:

> "`scx_bypass()` demonstrates a concrete exposed call site:
> `raw_spin_rq_lock(rq); ... scoped_guard (sched_change, p, DEQUEUE_SAVE |
> DEQUEUE_MOVE) {...}` ... so the missing-precondition exposure is real"

`DEQUEUE_CLASS` appears 12 times in that same prompt, including the gating
line `if ((flags & DEQUEUE_CLASS) && p->sched_class->switching_from)`. It was
never cross-referenced against the flags it had just quoted. It then wrote
off "full lock context of the other five ext.c sites" as "non-gating detail"
— those five are exactly the sites that *do* set `DEQUEUE_CLASS`, and all of
them go through `task_rq_lock()`.

### Fix directions

1. Add a **consistency step** between `validate-claims` and
   `validate-reachability`. Feed it only the `supported[]` set plus the
   finding's one-line thesis and ask a single question: *can all of these
   hold at the same time, on one execution?* Emit
   `{consistent: bool, conflicts: [{a_id, b_id, why}]}`. A non-empty
   `conflicts` should force the slow step to address each pair explicitly
   before it may return `Plausible`.
2. Make the fast step emit `preconditions[]` separately from `supported[]` —
   claims of the form "this only happens when X". A finding whose
   preconditions are not simultaneously satisfiable is `Invalid`, and that is
   mechanically checkable once they are typed rather than prose.
3. Require the slow step's `evidence.reachability` string to name **one**
   concrete configuration (kconfig set + runtime state + call site) under
   which every precondition holds. Findings 1, 2 and 3 above cannot produce
   such a string; forcing the field would surface that.

---

## 2. Config and feature gates are scored as severity, never re-tested as
##    reachability

**The defect.** `triage_coding` has `config_commonness` and a
`specific_config` reachability slot, which makes "only under CONFIG_X" feel
like a *severity* input. In three of the eight sessions a config gate was
routed there instead of being tested against the finding's other
preconditions.

`341e8ea6`'s `false_positive_risks` is explicit about the mis-routing:

> "The entire misattribution impact is gated behind CONFIG_SCHED_CORE
> (core.c:205/249-254 are inside `#ifdef CONFIG_SCHED_CORE`) ... which
> substantially narrows real-world exposure (**severity should reflect this
> gating**, not just the theoretical mis-stamp)."

There is also a specific conflation worth naming: **compile-time gate vs
runtime gate.** `CONFIG_SCHED_CORE=y` and `sched_core_enabled(rq) == true`
are different conditions, and treating them as one is what made the two
contradictory claims in defect 1 look compatible. Build with core scheduling
compiled in but not enabled at runtime and both claims are true — but in that
state the reader is dormant too, which the session never rechecked.

### Fix directions

1. Split the notion in `triage_coding.reachability`: `build_config` (what
   must be `=y`) and `runtime_state` (static keys, sysctls, debugfs feats,
   cgroup config, hardware topology) as separate fields. `sched_core_enabled()`,
   `sched_cache_enabled()`, `sched_feat(...)` and `static_branch_*` are runtime
   state, not kconfig.
2. Add a rule to `configs/prompts/triage-template.md`: a gate discovered
   during validation must first be tested for *compatibility with the other
   gates*, and only then recorded as a severity/commonness input. "Narrows
   exposure" is only a valid conclusion after "does not eliminate exposure"
   has been shown.

---

## 3. "Narrow window" is accepted as a race without ever naming a writer
##    that can execute in it

**`task_work_add_esrch_stale_next_leak`** (`e0539af5`). The fast pass filed
the disproof as a *supported claim*:

> `live_task_reachability_nil`: "For a currently-live task, the race window is
> **essentially closed** because `task_throttle_setup_work()` checks
> PF_EXITING before calling `task_work_add()`, matching `exit_signals()`
> setting PF_EXITING well before `exit_task_work()` installs `&work_exited`."

Its `analysis` prose is blunter still: "Reachability for a live task is
foreclosed." All remaining effort then went to the *fork-inheritance* variant,
which it closed correctly and negatively. The published `summary.md` keeps
the finding on the residue:

> "the only remaining race is the handful of instructions between the check
> and the `task_work_add()` call itself."

Nobody asked *who writes PF_EXITING in those instructions*. `exit_signals()`
is executed by the task on itself (`kernel/exit.c:951`), and all three
`task_throttle_setup_work()` call sites target `rq->donor` or the task being
picked/set-next on this rq under this rq's lock — so no concurrent writer
exists and the window is not a window.

This is `false-positive-guide.md` §8.1 ("Race Dismissal: Full-Path
Verification") run in reverse: the guide has a mandatory checklist for
*dismissing* a race and none for *asserting* one.

### Fix directions

1. Add an assert-side counterpart to §8.1 in the triage template. Before a
   check-then-use or unlocked-read finding may be `Plausible`, require:
   (a) the exact instruction opening the window, (b) **the specific writer,
   named by function, that can execute inside it**, (c) proof that writer can
   run concurrently — a different CPU, an interrupt, a preemption point.
2. Reject "essentially closed", "nearly unreachable", "a few instructions"
   as terminal states. In a false-positive-elimination pass those phrases
   mean the analysis stopped one step early; they should route to `Invalid`
   unless (b) is supplied.

---

## 4. The slow step reclassifies load-bearing preconditions as
##    "severity detail" to avoid `Unconfirmed`

**`fork_inherits_throttled_flag`** (`4815110e`) is self-indicting across
three records.

Fast pass, `false_positive_risks`:

> "This finding's real-world reachability is **entirely inherited** from
> `throttled_flag_leaks_across_class_switch`; if that sibling finding is
> itself disproven ... this finding has **no valid trigger and should be
> marked invalid alongside it**."

Reachability pre-pass (rec 23), after proving nothing on the fork path clears
the flag:

> "the slow agent should **lean toward Unconfirmed** unless it can point to
> independently-supplied evidence elsewhere in context proving the sibling
> precondition."

Slow pass (rec 25), overriding it:

> "The remaining open item ('throttled_true_reachable_precondition') is a
> **probability/severity question, not an existence question** ... it is a
> triggerability/frequency detail, not a gate."

That reclassification is precisely what `validate.json`'s own prompt
forbids — *"Do not preserve a finding as Plausible when any load-bearing
component remains unresolved."* The prompt text exists and was ignored,
because nothing enforces it.

The sharp form of the question was never asked either: **can a task that is
able to call `fork()` have `p->throttled == 1`?** A task with that flag set
is parked in limbo and not running; the only running carrier is the
class-switched one from the sibling finding.

### Fix directions

1. Make it mechanical rather than exhortative. Have `validate-claims` mark
   each `unresolved[]` entry with `gating: true|false` — the fast agent is
   already reasoning about this — and add a `field_check` eval on the slow
   step: `verdict != "Plausible" || no unresolved entry with gating == true
   was carried forward unresolved`. Let the slow step flip `gating` to false
   only by supplying a `gating_override_evidence` string with a file:line.
2. Add a `depends_on_finding[]` typed field. A finding whose reachability is
   inherited from a sibling must carry the sibling's *validated* verdict, not
   its prose. If the sibling is unvalidated or `Invalid`, cap this finding at
   `Unconfirmed` / `Invalid` automatically. Today each finding is validated in
   its own session with no shared state, so cross-finding dependencies are
   always resolved by trusting the dependency's own narrative.
3. When a sibling is later invalidated, the dependents must be re-opened.
   That needs the dependency edge to be persisted in `metadata.yaml`
   (`related_finding_ids` already exists but carries no direction or role).

---

## 5. semcode drops the leading comment when a forward declaration sits
##    between it and the definition

**This one is a plain tooling bug and the cheapest fix in the file.**

`throttled_flag_leaks_across_class_switch` (`264df6d5`) turns entirely on
whether retaining `p->throttled` across a class switch is a defect or the
design. The answer is the comment at `kernel/sched/fair.c:6642-6649`, which
names the case explicitly:

    /*
     * Task is throttled and someone wants to dequeue it again:
     * it could be sched/core when core needs to do things like
     * task affinity change, task group change, task sched class
     * change etc. and in these cases, DEQUEUE_SLEEP is not set;
     ...
     */
    static void detach_task_cfs_rq(struct task_struct *p);      /* 6650 */
    static void dequeue_throttled_task(struct task_struct *p, int flags)  /* 6651 */

The session fetched `source:dequeue_throttled_task` (rec 19). What arrived
was the bare body — no comment. Grepping the slow agent's entire prompt for
`task sched class` returns nothing.

semcode *does* attach leading comments in general: the `nohz_balancer_kick`
delivery in session `007c905a` includes its `/* Current decision point for
kicking... */` block. The failure is adjacency, in
`~/local/src/semcode/src/treesitter_analyzer.rs:906` (`extract_function_with_comments`):

    let mut current_line = function_start_line.saturating_sub(1);   // 6650
    ...
    if *comment_end_line == current_line || (*comment_end_line + 1) == current_line {
        for line_idx in *comment_end_line as usize..function_start_line as usize - 1 {
            if line_idx < lines.len() && !lines[line_idx].trim().is_empty() {
                if !lines[line_idx].trim_start().starts_with("//")
                    && !lines[line_idx].trim_start().starts_with("/*")
                    && !lines[line_idx].trim_start().starts_with("*")
                {
                    has_non_whitespace = true;
                    break;
                }
            }
        }

Comment ends 6649, `current_line` is 6650, so `6649 + 1 == 6650` matches and
it enters the branch. The intervening-line scan then reads source line 6650,
the forward declaration, which is non-blank and does not start with `//`,
`/*` or `*` — `has_non_whitespace = true`, `break`, comment discarded.

Consequence: the one artifact that answers `false-positive-guide.md` §4
("check if intentional design choice — quote comment if the author thought of
the same issue") was structurally unreachable to the agent. The static
forward declaration immediately above a static definition is an extremely
common kernel idiom, so this is not a one-off.

`extract_type_with_comments` at `treesitter_analyzer.rs:1180` carries a
verbatim copy of the same loop and the same bug.

### Fix directions

1. In the intervening-line scan, also skip lines that are a **declaration of
   the symbol being extracted or of any other symbol** — minimally, skip
   lines ending in `;` that tree-sitter parses as a `declaration` node. A
   cheap version: allow the line through if it matches
   `^\s*(static\s+|extern\s+)?[\w\s\*]+\([^;]*\)\s*;\s*$`.
2. Better: stop line-scanning and use the tree-sitter tree. Walk to the
   definition node's previous named sibling; if it is a `comment`, take it;
   if it is a `declaration`, step back once more before giving up.
3. Fix both copies (`:906` and `:1180`), or factor them into one helper.
4. Add a regression fixture with exactly this shape (comment, prototype,
   definition) — it is one of the most load-bearing comment placements in the
   kernel tree.
5. Independently of semcode: the triage template's §4 check should be able to
   fall back to a bounded `read:` of the ~15 lines above the definition when
   the delivered `definition` string has no leading comment. Right now the
   agent has no way to know a comment was silently dropped.

---

## 6. The verdict vocabulary has no "works as intended" outcome

**`sic_idle_core_bias_outranks_capacity_fit`** (`73173939`) is the one case
where the pipeline did almost everything right and still could not express
the answer.

The fast pass flagged the risk itself:

> "The finding's 'oversight' framing for the -3>-2 ordering is likely
> overstated: the introducing commit's own subject line ... indicates the
> ordering was a deliberate design choice"

It then requested and received the full commit body (`git:show -s
--format=%B 25a32e400a14`, delivered in rec 17): *"Introduce SMT awareness in
the asym-capacity idle selection policy: when SMT is active, always prefer
fully-idle SMT cores over partially-idle ones"*, with a measured 15-18%
throughput win, `Signed-off-by: Peter Zijlstra`, reviewed by Vincent Guittot
and K Prateek Nayak. The reachability pre-pass (rec 18) concluded:

> "This is a documented, reviewed (Vincent Guittot, K Prateek Nayak) policy
> tradeoff, not an oversight ... upstream authors accepted this exact
> behavior for a net throughput win."

And then, in the same paragraph:

> "verdict should reflect that the defect ... is real and demonstrated
> (Plausible-shaped: the bad path is proven to execute), but severity should
> be reassessed as low"

Look at the decision tree it was working from
(`configs/prompts/triage-template.md:125`): `Invalid` is *"evidence that the
originally suspected bug does not exist"* — but the behaviour does exist.
`Confirmed Latent` requires no in-tree trigger — but there is one. Of
`Fixed | Plausible | Unconfirmed | Unknown | Invalid | ConfirmedLatent`,
nothing means *"this is the code doing what its author intended"*. So it
landed on `Plausible` + severity `low`, which is the only reachable cell.

To the pipeline's credit the evidence did survive into the artifacts
(`FINDING.md:14`, `summary.md:69-81`), where it argues the commit's stated
rationale (degraded effective capacity on a busy sibling) does not cover the
*complete*-misfit case. That is a real argument and my invalidation of this
finding is a judgement call against it, not a correction of a factual error.
But it is a critique of a policy's scope, and `false-positive-guide.md` §9
(performance tradeoffs) and §4 (intentional design) both point the other way
— and the code's own rank table at `fair.c:8613-8636` plus the comment "This
ensures that an idle core is **always** given priority over (partially) busy
core" enumerate the -3-beats--2 ordering as intended.

### Fix directions

1. Add `NotADefect` (machine: `not_a_defect`) to the verdict enum in
   `validate.json` and to the status decision tree, positioned immediately
   after `Invalid`. Definition: *the described behaviour occurs, and source
   evidence — a comment, an enumerated design table, or the introducing
   commit message — shows it is the intended behaviour.* Add
   `intentional_design` to `reject_reasons`.
2. Make the §4 intentional-design check a **required output** of
   `validate-claims`, not an optional consideration: a
   `design_intent: {checked: bool, evidence: string|null}` field, populated
   from the leading comment, any enumerated constants/design block, and
   `git log -L` on the hunk. Sessions `73173939` and `264df6d5` differ only
   in whether that evidence reached the agent.
3. When a validator disagrees with documented intent (as `73173939` did), the
   disagreement belongs in the rendered `summary.md` **Impact** section, not
   only in Details. The Impact text is what `INDEX.md` renders and what a
   human reads first; here it contains no hint that the behaviour is
   deliberate. This is the same rendering-drops-the-caveat defect as
   `validate-bugs.md` §3.

---

## 7. `source:` retrieval is satisfied by the finding quoting itself

Minor but it recurs. In `ddf48eb7` the body of `sched_change_begin()` — the
single most load-bearing function in that finding — reached the slow agent
only through `FINDING.md`'s own "Relevant symbols" excerpt.
`mcp:source:sched_change_begin` does not appear anywhere in the prompt; the
session fetched `source:sched_change_end` but never `begin`.

The fast pass caught the identical pattern in `264df6d5` and named it:

> `enqueue_task_fair_direct_verification` (unresolved): "This body was only
> supplied via the finding's own embedded quote (FINDING.md), not
> independently re-fetched as fresh source in this validation session."

So the agent can detect it. It just isn't required to.

### Fix directions

1. Tag evidence with provenance and make it visible to the model. Everything
   arriving via `mcp:source:` / `read:` is `fresh`; everything lifted out of
   `FINDING.md` is `finding_quoted`. The fast step's `evidence` strings
   already carry `sym-`/`ctx-` ids for fetched material and free prose for
   quoted material — formalise it.
2. Require every `supported[]` entry whose claim is load-bearing to cite at
   least one `fresh` evidence id. A validation pass that confirms a finding
   using only the finding's own quotes has verified nothing. This is cheap:
   the fast agent already knows which is which.

---

## Cross-cutting

Defects 1, 2 and 3 are one failure with three faces: **the pipeline verifies
propositions and never verifies their conjunction.** Every one of the eight
false positives is individually well-evidenced. Six of eight had the killing
fact in context; three wrote it into `supported[]` and passed the finding
anyway. Adding fetches, raising `effort`, or swapping the model will not
address this — the missing artefact is a step whose only job is to ask
whether all the confirmed preconditions can hold at once.

Defect 4 is the enforcement gap that lets 1-3 reach the export. The slow
prompt already says "Do not preserve a finding as Plausible when any
load-bearing component remains unresolved", and `ddf48eb7`, `4815110e` and
`e0539af5` all preserved one anyway by relabelling the component as severity
detail. Prompt text that the same model can talk itself out of is not a
control. The `eval.field_check` machinery in `validate.json` is already the
right shape for this; it just checks `summary_written`/`severity_written`
today, not the reasoning invariant.

Defect 5 stands alone and should be fixed first — it is a twenty-line change
in `semcode/src/treesitter_analyzer.rs`, it has a clean regression test, and
it silently defeats the one false-positive check (§4, intentional design)
that is cheapest to run and highest-yield on mature kernel code.

Defect 6 is why the pipeline cannot record the *correct* answer even when it
finds it. Ranked by expected yield: **5, then 1, then 4, then 6.**

## Note on this document's provenance

Every quotation above is from the eight `code.jsonl` files named in the table
and was re-read at the time of writing; every kernel line number was
re-checked against `/home/clm/local/linux.kres` at `6caa892333d1`. The
`treesitter_analyzer.rs` line numbers are from `~/local/src/semcode` as of
2026-08-08 and I have not run the fix, only read the code path and traced it
by hand against `fair.c:6642-6651`.

Finding 8 (`sic_idle_core_bias_outranks_capacity_fit`) is a judgement call,
not a demonstrated error by the validator, and defect 6 is written on that
basis. The other seven are factual.
