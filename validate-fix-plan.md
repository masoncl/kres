# Plan: put opus back on `/validate` and make its one call count

> **Status, 2026-08-08.** Parts C, A1–A4, 0 and B1–B3 are implemented on
> this branch. B4 (cross-finding dependency edges) is deferred as the plan
> recommends, and A5 (re-asking whether the multi-step split pays) waits on
> a clean before/after measurement. `scripts/validate-metrics.py` is the
> checked-in measurement script the "How to verify" section asks for; the
> as-built description of the new flow is `docs/workflow.md`'s Validation
> Flow section. Nothing below has been re-run against live models yet.

Written 2026-08-08 against `rebase` @ `e8e019d`. Inputs: `validate-workflow.md`
(how the flow works + §10 log measurements over 384 runs) and
`validation-bugs.md` (8 false positives that survived, with per-defect
diagnosis). Every file:line below was read; nothing here has been run.

## 0. The framing

`validation-bugs.md` closes with: *"Adding fetches, raising `effort`, or
swapping the model will not address this — the missing artefact is a step
whose only job is to ask whether all the confirmed preconditions can hold at
once."* Six of the eight false positives had the killing fact already in the
slow agent's context; three wrote it into `supported[]` and passed the finding
anyway.

So opus is not the fix for the correctness defects. It is a fix for a
different thing (depth on the reachability pass), and it makes every wasted
token and every avoidable second cost more. That splits the work in two:

- **Part A — efficiency.** Today a run spends 26.5% of its wall clock on a
  retry loop that produces nothing, and sends its two largest prompts with
  caching switched off. Fix that first; it is what makes opus affordable.
- **Part B — correctness.** Restructure the workflow so the conjunction of
  preconditions is checked, gating claims cannot be relabelled away, and
  "works as intended" is expressible.

Part C is the semcode comment bug, which is separate code and should land
first because it is cheap and it silently disables the highest-yield
false-positive check.

---

## Part 0 — Undo the sonnet forcing

Two places route the fast model into validate's slow role.

**0.1 `scripts/validate-all.py`.** `slow_model = configured_fast_model()`
(`:419`) reads `models.fast` from `~/.kres/settings.json` (currently
`"sonnet"`) and `validate_one` passes it as `--slow-model` (`:291`).

Replace with an explicit `--slow` selection rather than deleting the flag:

```python
cmd = [kres_bin, "--slow", slow_model, "--prompt", ...]
```

with `slow_model` defaulting to the `models.slow` selector (`"opus"`) and
overridable by a new `--slow-model` CLI arg on `validate-all.py` itself.

Why `--slow` and not "drop the flag and let settings decide": with
`args.slow` empty, `append_configured_secondary_slow` (`kres/src/main.rs:1950`)
appends `models.slow_secondary` (currently `"gpt"`) as a second slow spec.
Validate has no lenses, and I have **not** verified whether `extra_slow_cfgs`
(`kres/src/main.rs:2154`) reaches a lens-free workflow. Passing `--slow`
makes `args.slow` non-empty, which skips that branch outright — deterministic,
no second model, no question to answer.

Delete `configured_fast_model()` (`:45-55`) once nothing calls it.

**0.2 `kres --summary` / `--summary-markdown`.**
`summary_defaults_slow_to_fast` (`kres/src/main.rs:848-853`) routes the fast
agent into the slow role for standalone summary runs, and `/summary` validates
every finding through this workflow (`kres-repl/src/summary.rs:144-202`). That
is the same forcing by another route.

Decision needed from the operator, so state both and pick: (a) delete the
function and let `models.slow` apply, making `kres --summary` an opus run over
N findings at concurrency 20; or (b) keep it but require an explicit opt-in
flag. Recommendation: **(a)**, with the documented behavior in
`docs/workflow.md:1428-1430` and `docs/summary.md:19` updated in the same
commit. A summary built from sonnet validations of opus findings is the worse
failure mode.

**0.3 Update docs** — `docs/workflow.md` Summary Flow, `docs/summary.md`,
`README.md:141-163` (the validate-all example).

---

## Part A — Efficiency: make the opus call the only expensive thing in the run

Baseline per run, from `validate-workflow.md` §10 (384 runs):

| | median | mean |
|---|---|---|
| slow fresh input | 45,755 | 54,862 |
| slow output | 18,169 | 19,669 |
| fast-synth fresh input (all attempts) | 64,359 | 70,096 |
| slow calls / run | 1 (94% of runs) | |
| fast-synth calls / run | 2 (91% of runs) | |
| wall / run | 390s | 413s |

### A1. Give fast-step synthesis a system prompt that matches its output contract

**Defect.** For `agent: fast` steps the synthesis call is issued with
`self.fast_system` (`kres-agents/src/pipeline.rs:1285-1286`) — that is
`configs/prompts/fast-code-agent.system.md`, whose line 30 mandates
`{analysis, followups, skill_reads, ready_for_slow}`. The workflow appends an
`OUTPUT SCHEMA` tail asking for `claim_validation`
(`kres-agents/src/workflow_runner.rs:800-804`). **397 of 784 synth calls
(50.6%) obeyed the system prompt instead of the schema**, and each rejection
re-runs the whole step including its gather phase.

**Fix.** The mechanism is already sketched in a code comment at
`kres-agents/src/workflow_runner.rs:5205-5210`:

> *"Hardcoded by step id because today the orchestrator is the only
> pure-routing step. If more arrive, replace this with a typed step field
> (e.g. `synthesis_system: "routing-agent"`)."*

Do exactly that:

1. Add an optional typed `synthesis_system` field to `Step`
   (`kres-agents/src/workflow.rs`), values naming an embedded prompt.
2. Delete `use_routing_prompt_for_synth` (`workflow_runner.rs:5211-5213`) and
   set `synthesis_system: "routing-agent"` on fix.json's `orchestrator` step.
3. Add `configs/prompts/workflow-synthesis.system.md` — for fast steps that
   **do** analyze code (unlike `routing-agent.system.md`, which opens "You are
   not analyzing code") but must emit the step's declared schema. It states:
   the gather phase is over; the user message's `OUTPUT SCHEMA` is the
   contract; `ready_for_slow` and the gather envelope are not valid here;
   unmet evidence needs go in the schema's own fields, not in `followups`.
4. Default `agent: fast` workflow synthesis to that prompt. `validate.json`'s
   `validate-claims` then needs no change.

**Expected effect** (measure, do not assume): the 51% reject rate on
`validate-claims` synthesis, worth 26.5% of wall clock and 14.2M fresh input
tokens across the sample.

### A2. Cache the static head of the synthesis and slow prompts

**Defect.** The synthesis message is built with caching explicitly off:

```rust
let messages = vec![Message {
    role: "user".into(),
    content: slow_logged.clone(),
    cache: false,
    cached_prefixes: Vec::new(),
}];
```
(`kres-agents/src/pipeline.rs:1218-1224` — both the `fast-synth` and `slow`
labels come from this one block, `:1234`, `:1311`.)

Measured consequence: `fast-synth` 784 calls, 26.9M fresh input, **0 cache
creation**; `slow` 420 calls, 21.1M fresh input, 372K cache creation. The
gather rounds, which do split and cache (`pipeline.rs:2530-2547` using
`CACHED_PREFIX_FIELDS`, `:192`), show 30.4M/25.8M cache creation against
~6K fresh.

What is being re-sent uncached is mostly static. Decomposing a 42 KB slow
`question`: bytes 0–17,018 are the `--- SKILLS ---` block (kernel.md + two
preloaded review-prompt files), 17,018–~30,000 are the 13 KB
`--- INCLUDES ---` triage template, and only the last ~12 KB is the step
prompt, the interpolated `claim_validation`, and the schema tail.

**Fix, two parts.**

1. **Stop folding skills and includes into the question string.**
   `build_step_prompt_texts` (`workflow_runner.rs:789-804`) concatenates
   `skills_prelude + includes_prelude + prompt + … + OUTPUT SCHEMA` into one
   blob that becomes `CodePrompt.question`. Emit them as their own envelope
   fields instead — `CodePrompt` already has `with_common_skills`
   (`kres-agents/src/prompt.rs:98-102`); add a sibling for `includes`.
2. **Split and cache at the synthesis call site.** Replace the plain `Message`
   at `pipeline.rs:1218-1224` with the same `to_split_documents` +
   `cached_prefixes` shape the gather path uses, stable keys
   `["common_skills", "includes"]`.

Two constraints from `AGENTS.md`, both load-bearing here:

- The head must be **byte-identical** across the callers meant to share it, or
  it costs an extra write of the largest payload for zero reads. Route both
  the fast-synth and slow construction through one constructor, in the spirit
  of `prompt::session_cache_head` (`prompt.rs:229-252`).
- `attempt: {attempt}` is inside the current blob (`workflow_runner.rs:802`).
  It must land in the volatile tail, never the head.

One consequence worth measuring rather than asserting: `validate-all.py` runs
20 processes concurrently against the same skills and the same triage
template, and the Anthropic prefix cache is server-side. If the heads are
byte-identical across those processes they can share one entry. I have not
verified that they would be; the split makes it testable.

### A3. Stop discarding the evidence requests from rejected attempts

**Defect.** The 397 rejected synth responses carried 771 typed followups;
**724 (94%) were never fetched anywhere in the run**, 401 of them `source:`
requests. The retry restarts gather from scratch and the gather agent
re-decides what to fetch. By contrast 1484 of 1565 (95%) followups that reach
the fetcher are served, so this is a plumbing loss, not a retrieval failure.

The worked example in `validate-workflow.md` §10.2 is the cost: three
`source:` requests dropped twice, and the accepted `claim_validation` then
marks a claim *supported* citing `kernel/sched/fair.c:1809 update_avg_scale()`
— a file:line for a function that was never in the prompt.

**Fix.** When a step attempt is rejected, carry its followups into the retry's
gather round as pre-seeded requests rather than dropping them. This is the
same information the gather agent would have to rediscover, and A1 largely
removes the rejections that produce it — but the drop is also live on the
legitimate path (a synth that emits followups alongside a valid
`claim_validation`), so fix both. Note in passing that `followups` is a
declared output of both validate steps (`validate.json:222-225`, `:376-379`)
that nothing consumes.

### A4. Make the slow envelope compliance a non-issue

**32 of 420** slow responses were unparseable as JSON — YAML-ish
(`analysis: |`), Markdown (`## Analysis`), or bare prose. Those drive the 78
`json-repair` calls (859K fresh input, 533K output, 51s mean). At opus rates
each one is a repeat of the most expensive call in the run.

Do not add a prose classifier. Options in order of preference: tighten the
`OUTPUT SCHEMA` tail for coding-mode slow steps; check whether
`slow-code-agent-coding.system.md` and the schema tail disagree the way A1's
pair does (I have not diffed them); and confirm the repair path is reached
before a full step retry so a formatting slip never costs two opus calls.

### A5. Only after A1–A4: reconsider the two-step shape

`validate-workflow.md` §9 listed "whether the two-step split beats one slow
pass" as unverified, and it still is. Do not touch it until A1–A4 land — with
half the synth calls being rejects, any comparison today measures the bug.
Part B adds a third step, which makes the question sharper, not moot: the
right end state may be *cheap fast pass → cheap conjunction check → one opus
pass*, which is what Part B builds.

---

## Part B — Correctness: check the conjunction, and make the checks mechanical

`validation-bugs.md` defects 1, 2 and 3 are one failure — the pipeline
verifies propositions and never verifies their conjunction. Defect 4 is the
enforcement gap that lets them reach the export. Defect 6 is a missing
vocabulary term. Defect 7 is unverified provenance.

Target shape:

```
validate-claims        (fast)   typed claims[] + design_intent + provenance
        │                       builtin eval: provenance + gating well-formed
        ▼
validate-conjunction   (fast)   can all supported preconditions hold at once?
        │                       conflicts[] + single_execution_witness
        ▼
validate-reachability  (slow/opus)  verdict incl. NotADefect, split reachability
                                builtin eval: verdict consistency
```

### B1. Type the claim record (defects 1, 4, 7)

Replace `validate.json`'s flat `supported[]/contradicted[]/unresolved[]`
(`:108-226`) with one `claims[]` array whose entries carry:

| field | purpose |
|---|---|
| `id` | stable slug (exists today) |
| `claim` | text (exists today) |
| `kind` | `mechanism` \| `precondition` \| `reachability` \| `impact` \| `design_intent` |
| `verdict` | `supported` \| `contradicted` \| `unresolved` |
| `gating` | bool — does the bug's existence depend on this? (defect 4) |
| `evidence[]` | `{ref, provenance: "fresh"｜"finding_quoted", location}` (defect 7) |

`gating` is the mechanical form of the rule the prompt already states and the
model already talks itself out of — *"Do not preserve a finding as Plausible
when any load-bearing component remains unresolved"* (`validate.json:287-288`,
ignored in `ddf48eb7`, `4815110e`, `e0539af5`).

`provenance` is the mechanical form of defect 7. The fast agent already
detects it unprompted — `264df6d5` filed
`enqueue_task_fair_direct_verification` as unresolved precisely because "this
body was only supplied via the finding's own embedded quote."

Add a required `design_intent: {checked: bool, evidence: string|null}`
(defect 6, fix direction 2), populated from the leading comment, any
enumerated design block, and bounded history on the hunk.

**Enforcement:** a `builtin` eval on `validate-claims`
(`eval_builtin` registry, `kres-agents/src/workflow_exec.rs:2510-2521`)
named `validate_claims_wellformed`, rejecting an attempt when any
`gating: true` claim is `supported` on `finding_quoted` evidence alone. The
retry then has a concrete reason to fetch. Builtin evals see the whole
`ExecContext`, so they can read other steps' outputs — which B3 needs.

### B2. New step `validate-conjunction` (defects 1, 2, 3)

Small input — the claims array and the finding's one-line thesis, no source —
so it is cheap and runs on the fast model. Outputs:

```json
{
  "consistent": true,
  "conflicts": [{"a_id": "...", "b_id": "...", "why": "..."}],
  "single_execution_witness": {
    "build_config": ["CONFIG_SCHED_CORE=y"],
    "runtime_state": ["sched_core_enabled(rq) == true"],
    "call_site": "file:line",
    "concurrent_writer": "function name or null"
  }
}
```

Three defects fall out of those fields:

- **Defect 1** is `conflicts`. `341e8ea6` put "cannot fire when core
  scheduling is active" and "reachable only when CONFIG_SCHED_CORE is enabled"
  adjacent in one `supported[]` array. A step whose only job is pairwise
  satisfiability finds that; nothing in today's flow ever puts the two side
  by side.
- **Defect 2** is `build_config` vs `runtime_state` as separate fields.
  `CONFIG_SCHED_CORE=y` and `sched_core_enabled(rq) == true` are different
  conditions, and collapsing them is what made the contradictory pair look
  compatible. `static_branch_*`, `sched_feat()`, sysctls, cgroup config and
  topology are runtime state, not kconfig.
- **Defect 3** is `concurrent_writer`. `e0539af5` published a finding on "the
  handful of instructions between the check and the `task_work_add()` call"
  without ever naming who writes `PF_EXITING` in that window (nobody can —
  `exit_signals()` runs on the task itself). `false-positive-guide.md` §8.1 has
  a mandatory checklist for *dismissing* a race and none for *asserting* one;
  a required `concurrent_writer` is that checklist.

A null witness does not by itself fail the finding — it constrains what the
slow step may conclude (B3).

### B3. Slow step: new verdict, split reachability, mechanical gate (defects 4, 6, 2)

1. **Add `NotADefect`** to the verdict enum (`validate.json:346-358`) and to
   the status decision tree in `configs/prompts/triage-template.md:120-243`,
   immediately after `Invalid`; machine form `not_a_defect`; add
   `intentional_design` to `reject_reasons`. Definition: *the described
   behaviour occurs, and source evidence — a comment, an enumerated design
   table, or the introducing commit message — shows it is intended.*
   `73173939` had the introducing commit body, the reviewers' names and the
   measured throughput win in context, concluded "documented, reviewed policy
   tradeoff, not an oversight", and still had to emit `Plausible` because no
   other cell in the enum was reachable.
   Also extend `apply_validation_outputs` (`kres-repl/src/summary.rs:320-345`)
   — today it errors on any verdict outside the five it knows — and
   `is_summary_candidate` (`:367-369`) so `NotADefect` filters out like
   `Invalidated`.
2. **Split `triage_coding.reachability`** into `build_config` and
   `runtime_state` groups (schema at `validate.json:452-509`), keeping the
   existing yes/no/n-a/unknown gates under each.
3. **Builtin eval `validate_verdict_consistency`** on `validate-reachability`,
   reading both prior steps' outputs. Fail the attempt when
   `verdict == "Plausible"` and any of: a `gating: true` claim is still
   `unresolved`; `conflicts` is non-empty and no `conflict_resolution[]` entry
   addresses each pair; `single_execution_witness` is null. Allow the slow step
   to clear a gating claim only by emitting
   `gating_override: [{claim_id, evidence}]` with a file:line — an explicit
   typed channel, not a paragraph.

That last point is the whole of defect 4: `4815110e`'s reachability pre-pass
said "lean toward Unconfirmed"; the slow pass overrode it with "a
probability/severity question, not an existence question" and shipped
`Plausible`. Prompt text the same model can talk itself out of is not a
control; `eval` is.

### B4. Cross-finding dependency (defect 4, fix direction 2)

`fork_inherits_throttled_flag`'s own fast pass wrote that the finding "has no
valid trigger and should be marked invalid alongside" its sibling. Each
finding is validated in its own process with no shared state, so the sibling's
verdict is only ever available as the sibling's *prose*.

Add a typed `depends_on_finding[]` to the finding metadata and cap a
dependent's verdict at the dependency's validated verdict. This is the largest
item in Part B — it needs a persisted, directed edge (`related_finding_ids`
exists but carries no direction or role) and a re-open path when a dependency
is later invalidated. **Recommend deferring it** to its own change after B1–B3
land; the batch driver (`validate-all.py`) is the natural place to order
dependents after dependencies.

---

## Part C — semcode drops the leading comment behind a forward declaration

Separate repo (`~/local/src/semcode`), ~20 lines, and it silently defeats the
cheapest high-yield false-positive check on mature kernel code
(`false-positive-guide.md` §4, intentional design).

Per `validation-bugs.md` §5: `extract_function_with_comments`
(`src/treesitter_analyzer.rs:906`) walks back from the definition, matches the
comment end, then scans the intervening lines and bails on any non-blank line
not starting with `//`, `/*` or `*`. A `static` forward declaration between
comment and definition — an extremely common kernel idiom — trips that and the
comment is dropped. `extract_type_with_comments` (`:1180`) carries a verbatim
copy.

Fix per that document: walk the tree-sitter tree instead of the line list
(previous named sibling; if `comment` take it, if `declaration` step back
once more), fix both copies or factor them into one helper, and add a
regression fixture with exactly the comment/prototype/definition shape at
`fair.c:6642-6651`.

Kres-side complement, independent of semcode: when a delivered `definition`
has no leading comment, the design-intent check of B1 should fall back to a
bounded `read:` of the ~15 lines above it. Today the agent has no way to know
a comment was silently dropped.

---

## Sequencing

| # | change | why here |
|---|---|---|
| 1 | Part C (semcode) | cheapest, unblocks B1's `design_intent`, no kres coupling |
| 2 | A1 (`synthesis_system`) | removes 51% of synth calls; every later measurement is noise until it lands |
| 3 | A2 (cache the static head) | makes opus affordable before opus is switched on |
| 4 | Part 0 (opus back on) | after A1+A2, with a measured before/after on the same finding set |
| 5 | A3, A4 | plumbing; A4 matters more once the slow call is opus |
| 6 | B1 + B2 + B3 | the correctness restructure, as one change — the eval in B3 depends on the fields in B1/B2 |
| 7 | B4 | separate, needs persisted metadata |
| 8 | A5 | re-ask the two-step question against a clean baseline |

## How to verify each step

Re-run the same finding set (`~/local/kres-sched2`, 113 findings, all currently
carrying `validation_run: true`) and diff the log-derived metrics from
`validate-workflow.md` §10. The measurement scripts are ad hoc; fold them into
one checked-in script so before/after are computed identically.

Gates:

- **A1** — reject rate on `fast-synth task=validate-claims` responses lacking
  the step's declared output. Today 50.6% (397/784). Target < 5%.
- **A2** — `cache_creation` and `cache_read` non-zero on `fast-synth` and
  `slow` records; fresh input per run down from a median 45,755 (slow) and
  64,359 (synth). Report the cross-process sharing effect separately; it is a
  hypothesis until the numbers show it.
- **A3** — count of followups emitted by a rejected attempt and never fetched.
  Today 724/771. Target 0.
- **A4** — unparseable slow responses. Today 32/420.
- **Part 0** — slow model id on the `assistant` records is the opus id; wall
  clock and output tokens per run recorded as the new baseline.
- **B1–B3** — the eight sessions named in `validation-bugs.md` are the
  regression suite. Re-validate those eight findings and require: `341e8ea6`
  and `7c0ec740` produce a non-empty `conflicts`; `e0539af5` produces a null
  `concurrent_writer`; `4815110e` cannot reach `Plausible` with its gating
  claim unresolved; `73173939` reaches `NotADefect`; `264df6d5` carries the
  `fair.c:6642` comment as `design_intent` evidence (which needs Part C).
  Also re-run the full 113 and confirm the artifact-consistency check in
  `validate-workflow.md` §10.5 still reports 0 issues.

## Open questions for the operator

1. **0.2** — does `kres --summary` go to opus too, or keep a fast-model
   default behind an explicit flag?
2. **B4** — worth building the cross-finding dependency edge now, or park it?
3. Is there a wall-clock or spend ceiling per validate batch that the opus
   switch has to fit inside? That decides whether A5 (collapsing the two-step
   split) is optional or required.
