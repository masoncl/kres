# Context assembly audit

How kres builds the input to every inference call, what changed while getting
there, whether the result matches the design criteria, and what is still
wrong.

The measurement in Part 0 is the evidence that started the work. Parts 1–5 are
the audit. Part 6 is per-subsystem reference detail. Part 7 is the regression
map.

Nothing here claims a measured saving: the benchmark rerun described in Part 5
has not happened yet.

**Contents**

- Part 0 — The measurement that started this
- Part 1 — How a request is assembled today
- Part 2 — What changed, and why the current design is better
- Part 3 — Verification against the design criteria
- Part 4 — Gaps
- Part 5 — Benchmark protocol
- Part 6 — Subsystem reference
- Part 7 — Regression-test map

## Part 0 — The measurement that started this

Snapshot taken from the live `mm/vmscan.c` review at `2026-08-04 17:04:32Z`.
The snapshot contained 120 completed inference calls and approximately
5.88 million logical input tokens.

“Logical input” includes cached context. Approximately 3.49 million tokens
were fresh or cache-created and 2.39 million were cache reads.

Provider APIs report token usage for complete requests rather than individual
JSON fields. The category estimates below allocate the reported total using
serialized character share. Dense source and JSON do not tokenize uniformly,
so the category values are approximate even though the overall provider total
is measured.

### Input by inference stage

| Stage | Calls | Input tokens | Share |
|---|---:|---:|---:|
| Slow review lenses | 50 | 3.40M | 57.8% |
| Fast gathering | 35 | 1.68M | 28.6% |
| Main/todo/goal | 17 | 388K | 6.6% |
| Consolidation/promotion | 15 | 299K | 5.1% |
| Change survey | 1 | 92.9K | 1.6% |
| Final file survey | 1 | 17.0K | 0.3% |
| JSON repair | 1 | 2.8K | 0.05% |

The change survey is no longer the major cost. The normal fast/slow review
loop accounts for about 86% of all input.

### Input by context category

| Category | Approximate tokens | Share |
|---|---:|---:|
| Source evidence | 1.72M | 29.2% |
| Workflow-built state | 1.46M | 24.8% |
| Loaded skills | 1.28M | 21.7% |
| Fixed system prompts | 681K | 11.6% |
| Prior model output sent back in | 635K | 10.8% |
| Per-call instructions and schemas | 111K | 1.9% |

### Source evidence

Approximate contributions:

- Raw semcode function bodies: 657K tokens
- Normalized `symbols` copies: 567K
- Git/history output: 116K
- Change diff, current target, and file survey: 103K
- Semcode type output: 44K
- Direct disk reads: 9K
- Source JSON labels, metadata, and manifests: 212K

The largest issue is duplication:

- Every normalized symbol body in `symbols` also appears in raw `context` in
  the same prompt.
- This duplicated about 1.36 million characters, approximately 567K logical
  tokens.
- Only 293K characters of distinct raw context were gathered, but they were
  transmitted as 2.00 million characters across calls, a 6.8x retransmission
  factor.

The possible savings from intra-call duplication and cross-call retransmission
overlap and must not be added together directly.

### Loaded skills

Only the automatic kernel skill was loaded, but its contents were repeatedly
attached to task prompts.

| Skill content | Occurrences | Approximate tokens |
|---|---:|---:|
| `mm-reclaim.md` | 68 | 372K |
| `technical-patterns.md` | 85 | 294K |
| `subsystem.md` index | 85 | 263K |
| `mm-pagetable.md` | 8 | 87K |
| `locking.md` | 7 | 67K |
| `mm-largepage.md` | 7 | 55K |
| `mm-folio.md` | 7 | 54K |
| Kernel skill scaffold | 85 | 37K |
| `rcu.md` | 9 | 16K |

The maximum serialized skill payload reached roughly 69K characters per call.

### Workflow-built context

The major contributors were:

- `plan` plus `question`: approximately 1.25M tokens.
- The whole-file risk scan appeared in both `question` and `plan.prompt` on
  every one of 85 task calls.
  - Both copies together consumed approximately 761K tokens.
  - Removing one copy would have saved roughly 380K logical tokens in this
    snapshot.
- The static review workflow contract repeated through `plan.prompt`:
  approximately 272K tokens.
- Plan goals, steps, and metadata: approximately 149K.
- Main/todo/goal workflow state: approximately 207K.

### Reused model output

- The same 26.8K-character `previous_findings` object was sent to 30 slow-lens
  calls, consuming approximately 336K tokens.
- Lens outputs, existing findings, prior analysis, todo summaries, and
  followups account for the remaining roughly 300K tokens.

This category is distinct from workflow-owned state because its contents were
originally produced by inference and subsequently fed back into later calls.

### Kres prompts

Typical fixed system-prompt sizes before tokenization:

- Fast agent: 12.5K characters
- Slow audit agent: 16.5K
- Review goal agent: approximately 17.1K
- Review todo agent: approximately 3.9K

These fixed prompts contributed approximately 681K logical tokens across all
calls. Per-call lens instructions, schemas, and contracts contributed another
111K.

Runtime workflow context is kept separate from fixed Kres prompts in the main
category table. It contains a mixture of Kres-authored contracts, target/task
state, generated plans, and the completed whole-file risk scan.

### Other context and accounting categories

- JSON field names, escaping, source labels, and manifests are real request
  overhead. Source-related framing alone accounted for an estimated 212K
  tokens.
- Response schemas and provider request envelopes are not always represented
  fully in the JSONL `content` field. Their cost is absorbed into the
  character-calibrated category estimates.
- Output tokens are not included in the 5.88M input total. Model output is
  counted only when kres sends it back as input on a later call.
- Cache-read fields have different API semantics: GPT reports cached tokens as
  a subset of input, while Anthropic reports cache creation and cache reads
  separately. The totals above normalize those representations.

### Highest-value reduction targets

1. Stop embedding the risk scan in both `question` and `plan.prompt`.
2. Send semcode source in either normalized `symbols` or raw `context`, not
   both.
3. Select `previous_findings` by structural relevance and place complete,
   byte-stable relevant findings in the shared cached lens prefix.
4. Put stable skill material in a reliably cacheable prefix and avoid
   repeatedly attaching the full subsystem index when it is not needed.
5. Replace the complete workflow prompt inside every `plan` with compact
   runtime plan state.

Targets 3 and 4 were revised during implementation. Both proposed selecting
what to send; both were reduced to placing stable bytes in a cached prefix and
sending everything. See "Phase 5 implementation" and "Phase 6 implementation"
for why. The list above is the snapshot-time plan, retained as written.

### Duplication-reduction plan, as written at snapshot time

Retained verbatim as the plan of record. Parts 2 and 3 say which phases landed
as written, which were reverted after implementation, and which was dropped.
Read this section as history, not as current behaviour — in particular Phase 5
and Phase 6 describe an approach the implementation has since abandoned.

#### Phase 1: Establish repeatable measurements

1. Add a read-only context-accounting utility that classifies every inference
   input into system prompt, workflow instructions, plan, task question,
   skills, normalized symbols, raw context, prior findings, and other model
   output.
2. Report both serialized bytes and provider-reported logical input tokens.
   Preserve the distinction between fresh/cache-created tokens and cache-read
   tokens.
3. Record stable duplicate identifiers for exact repeated values and source
   records. Do not infer duplication from model prose.
4. Use this `mm/vmscan.c` run as the initial benchmark. Re-run the same review
   after each phase and compare total input, cache behavior, findings, typed
   followups, and completed semantic coverage.

Acceptance criteria:

- Accounting totals reconcile with provider usage.
- Each request can be inspected independently without requiring inference.
- Measurements do not modify prompts or workflow behavior.

#### Phase 2: Remove the duplicate whole-file risk scan

1. Give the completed scan one canonical owner. Keep it in the task question
   or a dedicated structured field, but remove it from `plan.prompt`.
2. Store only a compact reference or summary in the persisted plan. The plan
   should contain the goal, current steps, dependencies, and target identity;
   it should not retain another copy of the complete bootstrap prompt.
3. Ensure goal, todo, fast, and slow calls that require the scan receive that
   canonical field exactly once.
4. Add request-construction tests asserting that the scan marker and scan JSON
   occur once per inference request.

Acceptance criteria:

- No task inference contains two copies of the scan.
- Resume reconstructs the same review state without regenerating the survey.
- Expected reduction on a run shaped like this snapshot: approximately 380K
  logical input tokens.

#### Phase 3: Unify source representation

1. Separate source evidence from retrieval diagnostics. A successful semcode
   result should have one canonical source body representation.
2. When semcode output is normalized into `symbols`, omit the same body from
   raw `context`. Preserve source identity, file, line range, and retrieval
   provenance as compact metadata.
3. Preserve raw local grep match lists as required: Rust cannot decide which
   match is semantically relevant. Do not expand every grep hit into a source
   body, and return every match without a per-file or shared output cap.
4. Preserve failed, empty, or unparseable semcode results so local fallback
   remains observable. Deduplicate only successful source bodies, not failure
   evidence or distinct match lists.
5. Deduplicate by explicit source identity and exact body hash. Do not use
   fuzzy text matching or prose classifiers.
6. Add tests for successful semcode normalization, semcode-to-local fallback,
   complete grep match preservation, and distinct line ranges.

Acceptance criteria:

- A source body appears at most once in a single request.
- Retrieval provenance and local fallback evidence remain available.
- No source candidate or typed followup is lost.
- Upper-bound reduction on a run shaped like this snapshot: approximately
  567K logical input tokens. This overlaps with cross-call savings below.

#### Phase 4: Reduce cross-call source retransmission

1. Build each task around the complete source subset requested by that task's
   semantic path rather than attaching unrelated session cache entries.
2. Within a multi-round gather, use one model conversation: put the complete
   task scope and seed evidence in the first user turn, then append only newly
   gathered source records in later user turns. Provider requests still carry
   the conversation history, so retain the previous user-turn cache boundary;
   never substitute an identity-only manifest for source the model has not seen.
3. Give each source record a stable structured ID. Conversation history tells
   the fast agent which retrievals are already present; do not attach a second
   identity-only index or repeat old source in each new logical user message.
4. Keep a stable ordering and stable cacheable prefix for evidence shared by
   parallel lenses. Append lens-specific source after that prefix.
5. Test that two lenses requesting different paths receive their common
   evidence once and only their respective path-specific evidence afterward.

Acceptance criteria:

- Per-call source volume follows the lens's requested evidence frontier rather
  than total session cache size.
- Cache hits improve or remain stable.
- Review lenses retain enough concrete source to support citations and
  negative coverage claims.

#### Phase 5: Compact skills without weakening review policy

1. Split skill content into a stable common prefix and task-selected subsystem
   guides.
2. Replace the full subsystem index in normal inference calls with the selected
   guide names after routing has completed. Keep the index only in the routing
   call that chooses guides.
3. Load `technical-patterns.md` and `mm-reclaim.md` once in the stable cached
   prefix used by all relevant task calls.
4. Send optional guides such as page-table, locking, folio, large-page, and RCU
   material only to tasks whose source or explicit skill routing selected
   them.
5. Preserve the full skill text for agents that need it; do not replace review
   invariants with lossy AI-generated summaries.

Acceptance criteria:

- Every selected guide remains available verbatim to the relevant agent.
- The subsystem index is not repeated after routing.
- Stable common skill content receives consistent cache hits.

#### Phase 6: Send findings and model output by relevance without state loss

1. Keep canonical findings in structured storage and give each finding a
   stable ID and revision.
2. Send each lens only findings relevant to its assigned semantic path, plus a
   compact manifest of other finding IDs and statuses.
3. Independent inference calls must receive every complete relevant finding;
   do not substitute a changed-only delta for state the call cannot recover.
   Put byte-stable relevant findings in the shared cached lens prefix.
4. Consolidation still receives the complete set of lens outputs needed for
   deduplication. Do not remove evidence before the component responsible for
   merging it has consumed it.
5. Add tests proving that every relevant finding remains complete, irrelevant
   findings retain complete identity/anchor manifests, and final consolidation
   sees every finding exactly once.

Acceptance criteria:

- Identical relevant `previous_findings` payloads use the shared cached prefix;
  unrelated findings are represented once by complete identity/anchor manifests.
- Finding IDs, evidence, severity, validation state, and typed followups survive
  round trips unchanged.
- Expected reduction for the repeated object observed here: up to roughly
  336K logical input tokens, depending on per-lens relevance.

#### Phase 7: Compact plan and workflow state

1. Persist the operator prompt and immutable workflow contract once at session
   scope.
2. Send task calls a compact plan projection containing the current goal,
   applicable step, dependencies, completed-step summary, and stable references
   to immutable session data.
3. Keep the full plan only in calls that can legitimately rewrite or judge it,
   such as goal and todo updates.
4. Remove duplicated instruction text from user fields when the same invariant
   already exists in the role's system prompt. Retain explicit structured
   contracts where Rust validates the response.
5. Test resume, goal checks, todo rewrites, turn caps, and dependency scheduling
   with the compact projection.

Acceptance criteria:

- Fast and slow task calls no longer receive the complete original workflow
  prompt inside `plan`.
- Goal and todo agents preserve the staged review graph and forward-progress
  semantics.
- Workflow behavior remains defined by the single JSON review workflow.

#### Phase 8: Validate quality and enforce losslessness

1. Run representative current-file, commit, and range reviews before and after
   the changes.
2. Compare findings, typed followups, source citations, goal decisions, task
   count, wall time, fresh input, and cache-read input. Token reduction alone
   is not sufficient if semantic coverage regresses.
3. Add non-production diagnostics that flag duplicate scan blocks, duplicate
   source bodies, identical findings broadcasts, and unexpectedly repeated
   skill payloads. Size is diagnostic only and never a request ceiling.
4. Fail tests on structural duplication and on any dropped prompt, source,
   finding, tool-output, or prior-attempt field. Large naturally partitionable
   inputs must reconstruct byte-for-byte from their parts.
5. Land phases independently so measurements identify which transformation
   changed cost or review quality.

Final success criteria:

- At least one copy of every required review invariant and every selected
  piece of source evidence reaches the responsible inference call.
- No exact source body or whole-file scan is duplicated within a request.
- Unchanged session-wide data is not repeatedly serialized into unrelated
  calls.
- Local fallback, exhaustive lenses, typed followups, negative-claim evidence,
  plan dependencies, and resume behavior remain intact.
- The same benchmark review uses materially fewer fresh and logical input
  tokens without losing confirmed findings or coverage.

This report describes the fixed snapshot only. Calls made after
`2026-08-04 17:04:32Z` are not included.


## Part 1 — How a request is assembled today

This part is the audit proper: every path that reaches a provider, what it
puts on the wire, and where the bytes come from. Line references are to the
tree this document ships with.

### 1.1 Every provider call site

Every provider call in kres goes through `Client::messages_streaming` or
`Client::messages`. There are eleven modules that call one of them:

| Module | Calls | Prompt shape | Cache split | Request meta logged |
|---|---|---|---|---|
| `pipeline.rs` fast gather (`:2212`) | 1/round | two documents on round 0, one after | yes, round 0 | yes (`log_code_user_request_content`) |
| `pipeline.rs` slow synthesis (`:1140`) | 1/task | one document | no, by design | yes |
| `pipeline.rs` lens fan-out (`:688`) | 1/lens/model | shared stable + per-lens delta | yes | yes |
| `pipeline.rs` bootstrap slow (`:916`) | 1/call | cached prefix + tail | yes | yes |
| `main_agent.rs` (`:252`) | ≤5 turns/service | growing conversation | no | yes |
| `goal.rs` (`:74`) | define/check/plan | one document | no | yes |
| `todo_agent.rs` (`:215`) | 1/update | one document | no | yes |
| `consolidate.rs` (`:137`) | 1/task | one document | no | yes |
| `promote.rs` (`:199`) | 1/task | one document | no | yes |
| `json_repair.rs` (`:311`) | 1/failure | one document | no | yes |
| `workflow_runner.rs` (`:638`, `:1066`, `:1803`, `:1940`) | 1/step | one document | no | yes |
| `session.rs` `/compact` (`:4395`) | 1/compact | one document | no | yes |
| `summary.rs` (`:1556`) | 1 site, 5 callers | one document | no — see G2 | yes (`try_call_and_extract`) |

Both logging holes this table originally recorded have been closed (G2, G3).
Summary still does not cache; that half of G2 is open.

### 1.2 The prompt envelope

`CodePrompt` (`kres-agents/src/prompt.rs:23`) is the only builder for fast and
slow agent turns. Its fields, in the order serde emits them (serde_json without
`preserve_order` sorts keys, so the order is alphabetical and deterministic):

`common_skills`, `context`, `lens_instruction`, `parallel_lenses`, `plan`,
`plan_rewrite_allowed`, `previous_findings`, `question`, `skills`, `symbols`.

Absent fields are omitted entirely (`skip_serializing_if`), so a prompt's key
set is a function of what the caller actually attached.

Three fields are worth naming because they are *not* what they used to be:

- `plan` is a `PlanPromptView` (`kres-core/src/plan.rs:122`), not a `Plan`. It
  carries goal, mode, active step id, and per-step id/title/description/status/
  dependencies. It excludes `Plan::prompt` (already represented by `question`),
  `created_at`, `todo_ids`, and per-step `context`.
- `previous_findings` is every current finding, in full, redacted only of
  store-owned narrative and provenance. There is no manifest class.
- `skills` / `common_skills` are two halves of one payload whose union is the
  live skill set verbatim.

There is no `previously_fetched` field. Multi-round gather is a conversation,
so "what you already have" is the conversation history, not a manifest.

### 1.3 Wire framing: one or two JSON documents

`to_split_documents` (`prompt.rs:149`) partitions the serialized prompt's keys
into a **stable** document and a **delta** document by a caller-supplied key
list. `to_delta_document` (`prompt.rs:166`) returns only the delta, for callers
that already hold the stable bytes. Both halves are complete JSON objects.

On the wire the two become two text blocks in one `Message` (`cached_prefix` +
`content`), each with its own `cache_control` marker. The model sees them
concatenated. The stable document ends in `\n` (`prompt.rs:226`) so the pair
reads as two whitespace-separated JSON values:

```
{
  "question": "review mm/vmscan.c",
  "skills": { "kernel": { "content": "guide" } }
}
{
  "symbols": [ { "definition": "void shrink_node(void) {}", ... } ]
}
```

Properties that hold by construction, because the split partitions one map:

- every field lands in exactly one document;
- the union of the two documents is the unsplit prompt's field set exactly;
- an empty delta is `{}` and leaves the stable bytes untouched;
- when no stable field is present the stable document is empty and the caller
  sends the whole prompt as one block.

Agents are told this in their system prompts: "one or two consecutive JSON
objects… read the union of their fields… never conclude a field is absent
because it is missing from the first object."

### 1.4 Fast gather

`AgentRunner::gather` (`pipeline.rs:2092`) is the single fast↔main loop; both
single-slow synthesis and lens fan-out call it. It is one model conversation,
not N independent calls.

Round 0 sends the full task scope, split by `CACHED_PREFIX_FIELDS`
(`pipeline.rs:192`): `question`, `previous_findings`, `skills`, `plan` in the
stable document; seed `symbols`/`context` in the delta. Gather sends one
undivided `skills` payload — the common/task split happens only at synthesis,
so `common_skills` is deliberately absent here.

Round *n* > 0 sends one document containing only:

- a fixed continuation sentence as `question`;
- `symbols`/`context` records that `append_prompt_evidence` confirmed are new;
- skill files grafted by *this round's* `skill_reads`.

Everything earlier stays in conversation history. The assistant's structured
response is appended too, so the next turn continues from its own prior
analysis, followups, skill reads, and readiness decision.

Cache markers per round, verified by
`gather_cache_markers_leave_room_for_cached_system_prompt`:

| Round | User turns | Markers | + cached system |
|---|---|---|---|
| 0 | u0 (2 blocks) | 2 | 3 |
| 1 | u0, u1 | 3 | 4 (at the protocol limit) |
| 2+ | last two cached, older folded back into content | 2 | 3 |

`mark_last_n_user_cached` (`kres-llm/src/request.rs:154`) folds a stripped
message's prefix back into its content, so dropping a marker never changes a
single byte the model sees.

Loop exits: `ready_for_slow`, no followups, only `question` followups, or every
followup already fetched (`fetched_keys`). The last one matters — it stops the
loop rather than issuing a byte-identical round.

### 1.5 Slow synthesis

One call, one document, no cache split (`pipeline.rs:1015-1020`). This is
deliberate: there is no fan-out to amortize a cache write against, and the slow
model cannot be assumed to share the fast model's cache. It receives the
*complete* canonical accumulated `symbols` and `context`, not the latest delta,
because it is stateless with respect to the gather conversation.

### 1.6 Lens fan-out

`prepare_lens_fanout` (`pipeline.rs:1791`) renders one stable document from
`LENS_SHARED_CACHE_FIELDS` (`pipeline.rs:197`): question, symbols, context,
previous_findings, common_skills, skills, plan. Every lens then contributes
only `parallel_lenses` + `lens_instruction` as its delta.

Per slow-model variant, the first lens runs alone to prime that model's cache;
the rest run in parallel and cache-read the same bytes. On seed failure the
variant's whole slate reruns in parallel without cache framing — with
`lens_logged = shared_prefix + lens_suffix` sent as one block, which is
byte-identical to the two-block form. Transport changes; content does not.

`every_lens_in_a_fanout_renders_byte_identical_stable_bytes` asserts three
independently-built lens prompts produce identical stable bytes. That is the
property the whole split exists for, and it was previously untested.

### 1.7 Source evidence

Three layers, with a strict ownership rule between them.

**Fetch.** `canonical_semcode_evidence` (`symbol.rs:27`) classifies a semcode
result structurally — one parseable result → normalized symbol; multiple
headers → keep raw, because choosing would hide candidates; unparseable →
keep raw *and* require local fallback. No conclusion is drawn from semcode
prose. The fetcher acts on that classification: `mcp_fetcher.rs` and
`main_agent.rs` emit either a symbol or a raw context entry, never both.

**Accumulate.** `append_symbol` (`symbol.rs:304`) drops exact duplicates, and
additionally drops a read range whose body is already present verbatim inside a
wider read of the same file (`range_body_contained`, `symbol.rs:278`). Both
line-range containment and exact substring are required. `append_context`
(`symbol.rs:329`) drops exact duplicates only, and deliberately keeps
empty-but-sourced results: "this search ran and found nothing" is evidence.

**Canonicalize.** `canonicalize_prompt_evidence` (`symbol.rs:134`) assigns a
UUIDv5 `evidence_id` over the record's own bytes, drops exact duplicates, and
preserves gather order. It does *not* revisit the fetcher's symbol-vs-raw
decision, because at that layer the tool name is gone and it would have to be
guessed from a source label.

What is never reduced: ambiguous or failed semcode output, local grep match
lists (no per-file cap, no automatic expansion into full reads), distinct
source ranges, and anything at all for size.

### 1.8 Skills

`apply_skill_reads` (`pipeline.rs:2404`) is the only mutator of the live skill
payload and returns the paths it grafted. `split_skills_for_synthesis`
(`pipeline.rs:2502`) partitions on exactly that set via `project_skills`
(`pipeline.rs:2518`): everything pre-existing → `common_skills`, everything this
task loaded → `skills`. A failed read's marker counts as task-specific.

`common_skills` reproduces the base payload byte for byte, *including an empty
`files` map*, so a task that loads nothing and a task that loads several emit
identical common bytes.

### 1.9 Tool output

Grep, find, read, git and MCP results reach the prompt complete. The single
exception is command output: above `TOOL_OUTPUT_INLINE_MAX` (200 000 bytes,
`tools.rs:313`), `spill_oversized_output` (`tools.rs:327`) writes the complete
output to `<workspace>/.kres/tool-output/<kind>-<hash>.log` and returns the
first and last 60 000 bytes, the exact total, and the path.

Verified recoverable end to end by
`a_spilled_log_is_readable_back_through_the_read_tool`: the path stated in the
tool result resolves through `resolve_workspace` and `read_file_range` returns
the complete log.

### 1.10 Provider limits

`input_limit_error` (`client.rs:1383`) types an over-limit rejection only on
HTTP 400/413/422, either from a provider marker or from the configured
capability. On 429, only an exact `count_tokens` result may attribute the
rejection to size; providers without exact counting wait and retry.

Nothing shrinks a request. `kres-core/src/shrink.rs` is gone. `OverInputLimit`
propagates as a typed error to whichever component owns the semantic structure,
and that component partitions losslessly or fails explicitly.

## Part 2 — What changed, and why the current design is better

The eight phases were implemented, then audited, then partly reversed. This
part records both directions honestly: three phases landed as designed, two
were reverted after implementation, one was dropped before implementation, and
four defects were found and fixed.

### 2.1 Landed as designed

**Phase 2 — one owner for the whole-file scan.** `Plan::prompt` owns the clean
operator prompt; `ReviewFileScanState` owns the completed scan. Planning sees
the scan once via a temporary `planning_text`; task prompts get it once by
prepending it to `question`. `PlanPromptView` cannot carry a second copy
because it excludes `Plan::prompt` entirely. Expected saving on a run shaped
like the snapshot: ~380K logical input tokens.

**Phase 4 — one gather conversation.** The measured 6.8× retransmission factor
came from re-sending accumulated evidence as fresh `symbols`/`context` every
round alongside an identity-only `previously_fetched` manifest. Now each record
is serialized into the conversation exactly once, and the manifest is gone —
history is the manifest.

**Phase 7 — compact plan projection.** ~20 lines that stop every fast, slow,
lens, and main-agent call from carrying the immutable operator prompt inside
`plan`. Goal and todo agents still receive the complete plan, because they
judge and rewrite it.

### 2.2 Reverted after implementation

Both reversals share one root cause: the phase proposed *selecting what to
send*, and selection is the thing that creates risk. In both cases the cache
already solves the cost, and the selection heuristic bought a fraction of the
remainder in exchange for a way to be wrong.

**Phase 6 — finding relevance routing. Removed.**

What it did: partitioned prior findings by whether one of their symbol/file
anchors appeared in the task prompt or gathered evidence. Matched findings went
out in full; the rest went as `FindingManifest` records with source bodies
stripped.

Why it was removed: the cost it targeted was one 26.8K-character object sent to
30 lens calls (~336K tokens). Putting `previous_findings` in the shared lens
prefix already means 29 of those 30 calls pay a cache read instead of a fresh
serialization. Relevance routing chased what was left — and the failure mode it
introduced is precisely the review kres exists to do. A finding whose filename
anchor does not appear in this task's evidence is *exactly* the cross-file
contract violation a reviewer needs to see. The heuristic was most likely to
strip source from the finding that mattered most.

Deleted: `FindingManifest`, `FindingSymbolAnchor`, `FindingFileAnchor`,
`manifest_for_agent`, `findings_context_for_agent`, `finding_task_evidence`,
`finding_context_for_prompt`, the `previous_finding_manifest` prompt field, and
its paragraphs in four system prompts.

**Phase 5 — routing-index removal. Reverted.**

What it did: dropped the subsystem routing index from synthesis calls once the
fast agent had loaded a concrete guide.

Why it was reverted: the index is read from disk once at startup and never
mutated, so it is byte-stable and cache-eligible. Dropping stable content to
save tokens trades a cache read for the possibility that a synthesis agent
cannot name a guide it needs. It also forced the generic pipeline to hardcode
one skill library's directory layout (`/subsystem/subsystem.md`), which
conflicts with the rule that pipeline behaviour stays subsystem-agnostic.

The cache-layout half of Phase 5 was kept, but reimplemented. The original
compared the base and live payloads with ~84 lines of generic JSON tree
diffing. `apply_skill_reads` already knows exactly which paths it inserted, so
it now returns them and the split partitions on that set. A structural diff can
disagree with what was actually loaded; a returned path list cannot.

### 2.3 Dropped before implementation

**Typed `SourceRecord` for gathered evidence.** Proposed to replace
`Vec<serde_json::Value>` with a typed record, on the grounds that stringly-typed
field probing was generating most of the fragility.

It was — but the other simplifications removed the probing rather than typing
it. Removing finding relevance deleted `finding_task_evidence`; dropping
`retrieval_preamble` deleted `symbol_source_identity`,
`symbol_reconstructs_raw_semcode`, and the `source.contains("type")` tool
classifier; moving duplicate detection off the write path deleted five more
sites. What remains outside tests is two sites in `main_agent.rs` building a
read header. The other eleven `main_agent.rs` hits probe LLM action JSON, which
is inherently untyped and which a `SourceRecord` would not help.

A refactor across `FetchResult`, `RunContext`, the `TaskManager` caches and
workflow seeds for two call sites is churn. Dropped, and recorded here so the
reasoning survives.

### 2.4 Defects found and fixed during the audit

**Terminal errors misreported as over-limit.** `input_limit_error` gated its
provider-marker test on HTTP 400/413/422 but not its configured-capability
test. Any terminal failure — 401, 403, 404, an exhausted 5xx retry — on a
request whose estimate exceeded the configured capability was returned as
`OverInputLimit`, hiding the real cause and sending a semantic owner off to
partition work that was never too large. Both tests are now gated on the same
status set.

**OpenAI 429s hard-failing off an estimate.** The OpenAI and OpenAI-Responses
429 paths returned `OverInputLimit` whenever the chars/4 estimate exceeded
`max_input_tokens`. No exact count exists on those providers
(`count_tokens_exact` returns `None` for non-Anthropic), so a shared-budget rate
limit on a large-but-legal request became a hard failure. Those paths now wait
and retry; a genuine capability rejection arrives as a 400 naming the limit.

**Unbounded build output.** Removing the tool-output caps left `make`, `cargo`,
`meson` and `bash` output entirely unbounded, and build logs have no natural
semantic partitioner — a failed kernel build is megabytes of interleaved
diagnostics that cannot be split without inventing a classifier over compiler
prose. A recoverable compile error could therefore fail an entire workflow
step on input size. Now spilled to disk with head, tail, byte count and path.

**Overlapping reads duplicating source.** Removing range merging left a read of
lines 1-100 and a later read of 10-20 both in the same request, with the
smaller body duplicated inside the larger. Containment dedup is restored, but
strictly: line-range containment *and* exact substring equality. Range
containment alone is not enough, because two reads can straddle an edit and a
stale body is evidence that must stay visible.

### 2.5 The wire framing (this change)

The stable/delta split was previously produced by string surgery: chop the
closing brace off the first half, append a comma, strip the opening brace from
the second. Consequences:

- neither half parsed alone, so nothing downstream could validate either;
- an empty second half needed an `_empty_tail: true` sentinel key to keep the
  trailing comma syntactically legal — a fake field visible to the model;
- the stable bytes depended on which optional fields serde happened to emit and
  in what order. This had already cost one real cache regression, when
  `plan_rewrite_allowed` sorted ahead of `skills` and reduced the shared prefix
  to five bytes.

Splitting at the document boundary removes all three. Each half is a complete
JSON object; an empty delta is `{}`; and the stable key set is chosen
explicitly by the caller's key list rather than emerging from serialization
order.

The cost is a model-facing contract change, now stated in all four agent system
prompts. It is **unvalidated against a live run** — see Part 4.

### 2.6 Net effect on the measured categories

Against the snapshot's category table, with the caveat that none of this is
measured yet:

| Category | Snapshot | Mechanism now |
|---|---|---|
| Source evidence 1.72M | symbol bodies duplicated in raw context; 6.8× retransmission | fetcher emits one representation; conversation carries each record once; containment dedup for overlapping reads |
| Workflow state 1.46M | scan in `question` *and* `plan.prompt`; full workflow prompt in every plan | one scan owner; `PlanPromptView` |
| Skills 1.28M | full payload re-attached per call | stable/task split, common half byte-stable across tasks — **but see gap G1** |
| Prior model output 635K | 26.8K findings object × 30 lens calls | shared cached lens prefix |
| System prompts 681K | fixed | unchanged, cached |
| Instructions 111K | fixed | unchanged |

## Part 3 — Verification against the design criteria

Each criterion below was checked against the code, not against intent. "Held"
means there is a mechanism plus a test. "Held (untested)" means the mechanism
exists but nothing would catch a regression.

### 3.1 Preservation invariants

| # | Invariant | Verdict | Evidence |
|---|---|---|---|
| P1 | Every selected prompt instruction stays verbatim in a model-visible location | Held | `to_split_documents` partitions one map; `merged()` in the prompt tests asserts no key is dropped and none appears twice |
| P2 | Every selected source record stays byte-for-byte available to the responsible stage | Held | `prompt_evidence_is_never_removed_for_size`; slow synthesis receives the complete accumulated set (`pipeline.rs:1015`) |
| P3 | Ambiguous/failed/empty semcode output is never deduplicated away | Held | `canonical_semcode_multiple_results_preserve_raw_candidates`, `canonical_semcode_unparseable_result_requires_local_fallback`, `canonicalize_never_drops_a_distinct_context_record` |
| P4 | Every prior finding is represented exactly once, with no third "omitted" class | Held | there are now only two states — sent in full, or not existing. `every_prior_finding_reaches_the_prompt_with_its_source_intact` |
| P5 | `prior_attempts` serialized in full; provider rejection cannot mutate workflow state | Held | `prior_attempts_value_preserves_every_field`, `over_input_limit_never_mutates_prior_attempts` |
| P6 | A partition target controls call shape, not retained information | Held | `codex_transport_framing_preserves_every_utf8_byte`; summary UTF-8 partition tests |
| P7 | An indivisible unit that cannot fit fails explicitly | Held | `smaller_partition_budget` errors at 1 token; change-survey reduction errors rather than splitting a typed report |
| P8 | Optimization metadata never controls semantic workflow state | Held | `ContextStats` is written to the log and read by nothing; `duplicate_symbol_bodies_in_context` is test/tooling only |

### 3.2 AGENTS.md rules that bear on context assembly

| Rule | Verdict | Note |
|---|---|---|
| No prose classifiers over model output for control flow | **Held, newly** | The last one was `source.contains("type")` in `canonicalize_prompt_evidence`, deciding a semcode tool from a source label. Deleted with the raw-collapse path; the fetcher now owns that decision where the tool name is in scope |
| semcode is an accelerator, not an authority | Held | Structural header count + parse success drive routing; failure triggers local fallback |
| Local fallback preserves the complete grep match list | Held | No `--max-count`, no per-file cap, no automatic expansion of broad results into full reads |
| Build/shell output is the one bounded exception | Held | Nothing discarded; explicit byte count and path; `a_spilled_log_is_readable_back_through_the_read_tool` proves recovery |
| Prior findings sent in full, every time | Held | Newly added to AGENTS.md so it cannot be re-litigated silently |
| Request construction never trims to fit | Held | `shrink.rs` deleted; no `truncate` on prompt-bound data outside display/label paths |
| `max_input_tokens` is a capability, not a ceiling | Held | Used only to classify a rejection, never to shape a request |

A tree-wide sweep for `truncate` / `chars().take` / slicing outside `#[cfg(test)]`
returns only: TUI line fitting, status-line text, log labels, todo id
generation, workflow *trace report* rendering (`format_event`, consumed by
stderr and by `report.md`), and `preview.rs`. None is on a path to a prompt.

### 3.3 Things that are correct but non-obvious

- **Empty `files` map is load-bearing.** `project_skills` keeps `"files": {}`
  in the common half. Dropping it would make a task that loads no guides emit
  different common bytes from one that loads several, losing the cross-task
  cache hit. Caught by `synthesis_skill_halves_reunite_into_the_live_payload`
  during implementation, not by review.
- **A stripped cache marker changes no bytes.** `mark_last_n_user_cached` folds
  the prefix back into content, so cache-layout decisions are invisible to the
  model.
- **The lens no-cache fallback is byte-identical** to the cached form:
  `lens_logged = shared_prefix + lens_suffix` is exactly `SplitPrompt::rendered`.
- **`append_context` keeps empty-but-sourced results.** "This grep ran and
  matched nothing" is a negative-coverage fact an agent may not manufacture.

## Part 4 — Gaps

Ordered by how much they undercut the design.

G2, G3, G4 and G6 have been fixed since the audit was first written, and G9a
with them; each entry records what was wrong and what was done. G5 is accepted
with a documented workaround. G1, G7 and G8 remain open, and the reasons differ:
G1 needs measurement before it is worth the complexity, G7 needs a trigger that
has not occurred, and G8 cannot be settled without a live model run.

### G1 — The skill payload cannot cache across tasks

**This is the largest remaining avoidable cost and it is a design gap, not a
bug.**

Skills were 1.28M tokens / 21.7% of the snapshot, with a peak serialized
payload around 69K characters per call. The intent was for the byte-stable half
to cache across tasks. It cannot, as built.

A `Message` supports exactly one `cached_prefix`, so a turn has at most two
blocks. The gather round-0 stable document and the lens shared document each
bundle the byte-stable skill payload *together with* the task-specific
`question` and `plan` in a single block. A cache entry covers the request
prefix up to a breakpoint (`kres-llm/src/request.rs:11-26`, `:137-147`), so a
different `question` changes that block and the skills inside it are re-created
rather than read.

Ordering `common_skills` first inside the document does not help: the cache
breakpoint is at the end of the block, not between keys.

Fix: allow more than two blocks per message — `common_skills` in its own
leading block with its own breakpoint, then task scope, then evidence. That is
3 user blocks; with a cached system prompt that reaches the 4-marker protocol
limit, so the gather loop's two-newest-turn marking would need to yield one.

Cheaper interim: move `common_skills` out of `CACHED_PREFIX_FIELDS` and
`LENS_SHARED_CACHE_FIELDS` into its own `Message` earlier in the conversation.

I have not measured this. It should be the first thing the benchmark checks:
cache-read tokens attributable to skills across consecutive tasks.

### G2 — Summary pipeline logging — **FIXED**

`kres-repl/src/summary.rs` contained no reference to `TurnLogger`, so no
`/summary` inference appeared in `code.jsonl` and its spend was invisible to
the accounting the rest of kres is measured by. Phase 1 claimed coverage of
"compact/summary paths"; that was true of `/compact` (`session.rs:4393`) and
false of summary.

An earlier revision of this gap said summary made "eight `messages_streaming`
calls". That was wrong — those line numbers were `user_message(...)`
construction sites. There is exactly **one** provider call, in
`try_call_and_extract` (`summary.rs:1556`), reached from five callers. That
made the fix a single insertion point rather than eight.

Fixed by bundling the client, call config and logger into `SummaryCall` and
threading it through the seven helpers that previously took `client` + `cfg` as
an adjacent pair. `try_call_and_extract` now logs a user record with request
metadata and an assistant record with usage and response model, labelled
`phase=summary stage=<stage>`.

`all_summary_inference_goes_through_the_one_logged_call_site` is a structural
guard: it asserts the file contains exactly one provider call site. A new
summary path that talks to a client directly would silently reintroduce the
hole and no behavioural test would catch it, because the summary would still be
correct — just unaccounted.

**Still open:** summary remains uncached (`cache: false` everywhere), while
`stage_render` deliberately repeats the complete condensed task observations
across every render partition. That is a textbook stable prefix currently paid
for in full on each partition. Not fixed; it would be the first cached path in
that file.

### G3 — workflow_runner request metadata — **FIXED**

`map_review_ledger` (`:638`), `judge` (`:1803`) and `consolidate` (`:1940`)
logged via `log_code_labeled`, which passes `request: None`, so `system_chars`
was 0 and no model/thinking/system-fingerprint block was recorded.
`run_llm_step` (`:1066`) already did it correctly.

Fixed by moving each `call_cfg` construction above its logging block and
switching to `log_code_labeled_with_request(… Some(&call_cfg.request_meta()))`.
The four remaining `log_code_labeled` calls in that file are `assistant`
records, which correctly carry no request metadata.

### G4 — dead `common_skills` cache field — **FIXED**

The gather loop never calls `with_common_skills`; the common/task split happens
only at synthesis. The entry was inert and misleading. Removed, with a comment
recording why it does not belong there.

### G5 — Spilled tool output is never cleaned up — **accepted, documented**

`<workspace>/.kres/tool-output/` grows for the life of a session and is never
pruned. In a kernel tree `.kres/` is not in `.gitignore`, so spilled build logs
appear in `git status` and a careless `git add -A` could sweep them into a
commit.

Deliberately left as-is: the current path is proven to resolve back through
`resolve_workspace`, and relocating it under the session log directory would
need that re-verified for the case where the workspace differs from the
process cwd. Operators should add `.kres/` to their tree's exclude file, or
prune the directory between sessions.

### G6 — Spill marker units — **FIXED**

The marker reported byte counts but told the agent to "request a `read` … with
an explicit line range". `ReadArgs` is line-based, so the pointer named a unit
the agent could not act on.

The head and tail cuts now land on line boundaries and the marker names the
omitted region as a line range:

```
[kres] bash output was 440029 bytes over 40004 lines. The complete log is at
`.kres/tool-output/bash-27d3f62c8d3796a4.log`; no byte was discarded.
Lines 1-5455 and 34550-40004 are shown here.

[... lines 5456-34549 omitted from this message; `read` that path with that
line range to recover them ...]
```

Output with no newlines at all cannot be described as a line range, so that
case falls back to a byte count and a bare pointer at the log
(`spilling_one_enormous_line_falls_back_to_a_byte_pointer`).

### G7 — `previous_findings` grows without bound

By design, and cheap per call via the shared cached prefix — but it is a real
ceiling. A long session accumulates findings until the set approaches model
capability, and there is no partitioner for it. When that happens the fix is a
semantic partitioner over findings, **not** a relevance heuristic that hides
them; this is now written into AGENTS.md so the reversal in §2.2 is not
re-litigated.

### G8 — The two-document contract is unvalidated against a live model

All four agent system prompts now describe a one-or-two-JSON-object input. The
framing renders correctly and both halves provably reassemble to the unsplit
field set, but no live run has confirmed the models read the union correctly.
This is the single highest-risk item in the change and the benchmark must check
it before anything else — specifically, whether any agent reports a field
"missing" that is present in the other document.

### G9 — Two pre-existing test races

Unrelated to context assembly, but they corrupt the signal the benchmark needs.

- `export.rs` exec'd `findings-index.py` immediately after writing it and could
  fail `ETXTBSY` under concurrent test binaries. **Fixed** with a bounded
  retry; no recurrence in 14 runs.
- `session::tests::code_output_absolute_with_consent_writes_through` fails
  intermittently under concurrency. **Not diagnosed.**

## Part 5 — Benchmark protocol

### Current validation status

Passing at the time of writing:

- `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets -- -D warnings`;
- `cargo test --workspace -q` — 1,099 passed, 1 ignored, 0 failed;
- `git diff --check`.

That is static and unit-level only. It proves the assembly code does what this
document says; it proves nothing about token cost or review quality.

### What the rerun must compare

The `mm/vmscan.c` snapshot has still **not** been rerun. Nothing in this
document claims a measured saving. The rerun must compare:

1. **Correctness first.** Any agent complaining a field is absent → G8 has
   bitten; stop and fix the framing before reading any token number.
2. Findings, severities, typed followups, source citations, and negative
   coverage claims with their supporting evidence.
3. Fresh input, cache creation, and cache reads **by stage** — not just totals.
   Cache reads should rise as fresh input falls; if both fall, coverage
   regressed.
4. Skills cache-read tokens across consecutive tasks, to size G1.
5. `context_stats` duplicate diagnostics: `whole_file_scan_occurrences` ≤ 1 per
   request, `duplicate_context_items` == 0, and
   `duplicate_symbol_bodies_in_context` == 0 run offline over `code.jsonl`.
6. Task count, retries, wall time, and resume/scan-fingerprint behaviour.

Only after that should the phases be called complete from a product
perspective. They are implemented and internally verified; measurement and live
review-quality comparison remain pending.

## Part 6 — Subsystem reference

Detail behind Part 1, retained for reviewers who need the mechanism
rather than the summary. The three-concept split and the preservation
invariants below are the basis on which Part 3 judges the code.

### Three concepts this audit keeps separate

The work repeatedly went wrong by conflating these, so they are named up front:

1. **Deduplication** removes a byte-identical or structurally redundant second
   representation when the remaining representation contains the same evidence.
2. **Relevance routing** chooses whether a stateless call needs a full record or a
   source-body-free manifest. It never deletes the canonical record.
3. **Partitioning** sends one logical input through multiple calls when the
   provider cannot accept it in one call. Every owned semantic unit is retained.

None of these is a request-content ceiling. Kres no longer drops old context,
lower-severity findings, source entries, prior attempts, or arbitrary JSON/text
suffixes in order to fit an inference request. A provider capability can cause
semantic partitioning or an explicit failure, but never silent content removal.

### Non-negotiable preservation invariants

These invariants are the basis for reviewing the implementation:

- Every selected prompt instruction remains verbatim in at least one model-visible
  location appropriate to the call.
- Every selected source record remains byte-for-byte available to the responsible
  inference stage. Exact duplicates may collapse to one canonical record.
- Ambiguous, failed, empty, or unparseable semcode output is not deduplicated away;
  it remains visible and triggers the local fallback path.
- Every canonical prior finding is represented exactly once in a task prompt: as
  either a full finding or a complete semantic manifest. There is no third
  "omitted" class.
- Workflow `prior_attempts` are serialized in full. Provider rejection cannot
  mutate persisted workflow state to make a retry smaller.
- A partition target controls call shape, not retained information. UTF-8/source
  partitions reconstruct the original bytes, typed report partitions preserve
  whole reports, and finding partitions preserve the semantic finding core.
- If an indivisible semantic unit cannot fit, kres fails explicitly. It does not
  shave fields until the request happens to pass.
- Optimization metadata never controls semantic workflow state. Rust does not
  decide that a review is clean, complete, invalid, or irrelevant from free-form
  model prose.

### Code ownership map

| Concern | Primary implementation | Important types/functions |
|---|---|---|
| Request accounting | `kres-core/src/log.rs` | `ContextStats`, `RequestMeta`, `log_code_user_request_content` |
| Prompt envelope/cache split | `kres-agents/src/prompt.rs` | `CodePrompt`, `SplitPrompt`, `to_split_documents`, `to_delta_document` |
| Source canonicalization | `kres-agents/src/symbol.rs` | `canonical_semcode_evidence`, `canonicalize_prompt_evidence`, `append_symbol`, `append_prompt_evidence` |
| Multi-turn gather | `kres-agents/src/pipeline.rs` | `AgentRunner::gather`, `CACHED_PREFIX_FIELDS` |
| Shared slow-lens context | `kres-agents/src/pipeline.rs` | `prepare_lens_fanout`, `run_prepared_lens_fanout`, `LENS_SHARED_CACHE_FIELDS` |
| Skill cache split | `kres-agents/src/pipeline.rs` | `apply_skill_reads`, `split_skills_for_synthesis`, `project_skills` |
| Prior findings | `kres-core/src/findings.rs` | `redact_findings_for_agent` (all findings, shared cached prefix) |
| Compact task plan | `kres-core/src/plan.rs` | `PlanPromptView`, `PlanStepPromptView`, `Plan::prompt_view` |
| Scan persistence | `kres-core/src/session_state.rs`, `kres-repl/src/session.rs` | `ReviewFileScanState`, `review_file_scan_context` |
| Six-month net diff | `kres-repl/src/change_survey.rs` | `aggregate_target_diff`, `split_diff_for_inference` |
| Change/file survey orchestration | `kres-repl/src/session.rs` | `run_review_file_scan`, `assess_change_survey`, `ReviewFileSurvey::validate` |
| Summary partitioning | `kres-repl/src/summary.rs` | `condense_single_task`, `stage_render`, `partition_finding_evidence` |
| Summary call logging | `kres-repl/src/summary.rs` | `SummaryCall`, `try_call_and_extract` |
| Provider capability errors | `kres-llm/src/client.rs`, `kres-agents/src/error.rs` | `input_limit_error`, `LlmError::OverInputLimit`, `AgentError::OverInputLimit` |
| Oversized tool output | `kres-agents/src/tools.rs` | `spill_oversized_output`, `TOOL_OUTPUT_INLINE_MAX` |
| Codex transport framing | `kres-llm/src/client.rs` | `split_utf8_losslessly`, `start_codex_turn` |
| Workflow history preservation | `kres-agents/src/workflow_exec.rs` | `StepState::prior_attempts_value`, `DriverError::OverInputLimit` |

### Phase 1 implementation: request accounting

#### What is logged

Every inference call continues to log provider-reported usage on its assistant
record. User records now additionally carry a `request` object and computed
`context_stats` where the call path has a `CallConfig` available.

`RequestMeta` records:

- model ID;
- output-token setting;
- configured thinking shape, effort, or budget;
- system-prompt character count;
- a deterministic system-prompt fingerprint.

The model-visible user payload receives a deterministic FNV-1a fingerprint. This
is a diagnostic identifier for exact repeated bytes; it is not trusted as a
source identity and does not drive deduplication.

Stable `label` values distinguish overlapping calls, including fast gather round,
task, lens, and slow-model variant. This makes matching user/assistant records
possible when calls run concurrently.

#### Field and category accounting

`ContextStats::from_user_content_and_request` parses a JSON prompt when possible
and measures the serialized size of every top-level field. Fields then roll into
the same categories used by this report:

- `skills` and `common_skills` → skills;
- `symbols` and `context` → source evidence;
- findings, manifests, lens output, analysis, and rejected responses → reused
  model output;
- schemas, contracts, lens instructions, and validation errors → instructions;
- question, plan, goal, todo, task, and original prompt → workflow state;
- everything else → other.

System-prompt characters are recorded separately because the user JSON cannot
contain them. Provider token counts remain the authoritative total; character
categories are only an allocation aid.

#### Multi-turn accounting

Fast gather is now a real multi-turn conversation. Logging only the newest user
delta would undercount what the provider sees, so a fast-gather user log has two
representations:

- `content`: the newest logical user turn, retained for compatibility with
  existing JSONL readers;
- `request_content`: a JSON `messages` array containing the complete model-visible
  conversation for that API call.

When `request_content` contains a conversation, `ContextStats` walks every user
turn and accumulates across them rather than reporting the newest turn alone.
It detects:

- the same complete context object repeated on separate turns;
- a whole-file scan repeated anywhere in the conversation;
- combined skill payload size.

This distinction matters because the original bug was cross-call and cross-turn
retransmission, not just duplicate fields in one JSON object.

Every step on the write path is linear in the payload: one parse, one size pass
per top-level field, one hashed pass over context entries, one marker count.
Detecting a normalized symbol body repeated inside raw context is inherently
`O(symbols x context)` substring search, so it is **not** run when logging —
`duplicate_symbol_bodies_in_context` exposes it to tests and offline log
tooling. Running a quadratic scan on every write, over multi-megabyte lens
prompts, to re-detect something the fetchers now make impossible by
construction, is not a trade worth making.

#### Diagnostics are not limits

Warnings for skill payloads above 80,000 characters or requests above 1,000,000
characters are observations only. They do not alter, reject, truncate, or route a
request. They exist to make regressions visible in logs.

#### Call-path coverage

Request metadata/logging reaches the direct fast/slow pipeline, lens calls,
consolidation, promotion, JSON repair, the main agent, the todo agent, the goal
agent, `/compact`, and one of the four workflow driver calls. A call with a
model-visible multi-turn request uses `request_content`; a one-turn call logs
its exact prompt in `content`.

Coverage is complete as of the G2/G3 fixes. It was not when this section was
first written, and the claim that it was is what the audit caught: summary made
its provider call with no logging at all, and three workflow_runner calls
logged without request metadata. Both are fixed; see Part 4.

`summary.rs` is guarded structurally — a test asserts the file contains exactly
one provider call site, so a new unlogged path fails the build rather than
silently under-reporting.

#### Review caveat

The category allocation still uses serialized character share. It is deterministic
but remains an approximation because source, English, and JSON punctuation tokenize
differently. The implementation intentionally does not pretend to derive exact
per-field tokens from a provider total that is reported only per request.

### Phase 2 implementation: one owner for the whole-file scan

#### Ownership split

The operator prompt and completed scan now have different owners:

- `Plan::prompt` owns the clean operator/workflow prompt.
- `ReviewFileScanState::scan` owns the completed whole-file risk scan.

`ReviewFileScanState` also stores the target, source hash, baseline, and head. The
session schema was bumped to version 3 so resume cannot silently interpret a state
file with the old ownership model.

#### Planning path

The goal and plan calls need the scan to prioritize work. `submit_prompt` therefore
builds a temporary `planning_text` containing the clean operator prompt plus one
delimited scan block. After `define_plan` returns, kres replaces `plan.prompt` with
the clean `persisted_plan_prompt` before storing the plan.

This means planning sees the scan once, while later task prompts cannot receive a
second copy through `plan.prompt`.

#### Task path

Audit tasks retrieve the scan through the dedicated
`review:file-risk-scan` context-cache entry and prepend it once to the task
question. `CodePrompt::with_plan` serializes `Plan::prompt_view`, which excludes
`Plan::prompt`; therefore the same task cannot receive the scan through both
`question` and `plan`.

Derived todo tasks use the clean top-level plan prompt as `original_prompt`. The
currently submitted task is not copied into `original_prompt`, avoiding another
question/scan echo.

#### Freshness and resume

The scan is reusable only while all identity fields match:

- target path;
- exact current source hash, including executable mode on Unix;
- `WORKTREE@<HEAD>` identity;
- six-month baseline for resumed-state validation.

`review_file_scan_context` removes a cached entry when target, source hash, or head
no longer matches. If an audit task expects a scan but the fingerprint is stale,
the task is parked instead of running with missing or mismatched ratings.

The session snapshot persists the scan separately from the plan. Resume restores
the dedicated cache entry and validates the current source/window before reuse; it
does not regenerate or rediscover the scan from plan prose.

#### Why prose parsing was removed

Older cleanup attempts looked for scan text inside `Plan::prompt`. That made free
text a hidden state channel and could confuse an operator-authored phrase with a
generated scan. The current implementation consumes only the typed
`ReviewFileScanState` field.

### Phase 3 implementation: canonical source evidence

#### Semcode result classification

`canonical_semcode_evidence` classifies a result structurally:

- exactly one parseable function/type result → normalized symbol;
- multiple candidate headers → preserve raw output because choosing one would
  hide candidates;
- missing/unparseable result → preserve raw output and require local fallback.

No "not found" conclusion is inferred from semcode prose. The presence and count
of structural headers plus parse success control this routing.

#### One body representation for the safe case

A normalized symbol retains:

- symbol name and kind;
- file and line information;
- exact definition/body;
- compact retrieval provenance.

The header lines the body was parsed out of are not carried alongside it: the
name, kind, file, line, and call counts already state everything they said.

The choice between a normalized symbol and raw text belongs to the fetcher,
which is the only layer holding the tool name and the untouched tool output
together. When `canonical_semcode_evidence` reports a single parseable result,
the fetcher emits the symbol and no raw copy; otherwise it emits the raw output.
The prompt layer does not revisit that decision, because doing so would mean
re-deriving the tool from a source label, and a wrong guess could hide a
candidate.

#### Overlapping file reads

`append_symbol` also drops a read range whose body is already present verbatim
inside a wider read of the same file, and replaces a narrower record when the
wider one arrives second. Both the line-range containment and the exact
substring must hold. Range containment alone is not sufficient: two reads may
straddle an edit, and a stale body is evidence that must stay visible.

#### Exact identity and order

Each canonical record receives a UUIDv5 `evidence_id` derived from the serialized
record after removing any supplied ID. Kres recomputes IDs instead of trusting a
tool or stale accumulator. Exact duplicate records collapse; distinct records with
the same symbol name, different provenance, or different content remain
distinct. Distinct source ranges also remain distinct unless one contains the
other verbatim, per the rule above.

Gather order is preserved. This keeps prompt bytes stable and ensures later deltas
do not reorder evidence already visible in conversation history.

#### Local fallback preservation

Raw ambiguous semcode output, semcode failures, local grep match lists, local reads,
and empty/error envelopes remain in `context`. Broad grep output is not expanded
into full reads automatically and is not capped per file. The agent still chooses
targeted `read` followups from the complete match list.

### Phase 4 implementation: cross-call source retransmission

#### One conversation, append-only evidence

`AgentRunner::gather` is now the only fast↔main gathering loop used by single slow
synthesis and shared lens fan-out.

Round 1 contains:

- the full task question;
- seed symbols and context;
- relevant findings/manifests;
- the current skill payload;
- the compact plan projection.

After the fast assistant requests followups, fetched source is canonicalized and
appended to the task's accumulated `symbols` and `context`. The next user turn
contains only records that `append_prompt_evidence` confirms are new, plus only new
skill files loaded in that round. The previous question and evidence remain in the
conversation history, so the model still sees them without a second logical copy.

The assistant's structured response is also appended to history. This preserves
the prior analysis, followups, skill reads, readiness decision, and optional plan
rewrite that the next fast turn is expected to continue from.

#### Retrieval deduplication

Followups use stable cache keys. Re-requested followups are removed before the main
agent runs. If every requested followup was already fetched, gathering stops rather
than adding a byte-identical round. Exact evidence records are independently
deduplicated after fetch, so two different retrieval operations returning the same
record do not rebroadcast it.

#### Cache boundaries

The first user turn is sent as two independently valid JSON documents in two
cacheable text blocks: a byte-stable one holding the task scope, and a delta
holding this round's evidence. On later rounds, the two newest user turns are
marked cacheable. Together with the cached system prompt this stays within
Anthropic's four-cache-block protocol limit.

This is a cache-layout decision only. `stable + delta` is the exact text the
model sees, and its fields are the union of the unsplit prompt's fields — no
key appears in both halves. Agents are told to expect one or two consecutive
JSON objects and to read their union.

An earlier version instead spliced a single object across the block boundary,
chopping the closing brace off the first half and the opening brace off the
second. That made neither half parseable alone, needed an `_empty_tail`
sentinel key so an empty second half stayed syntactically legal, and coupled
the stable bytes to which optional fields serde happened to emit — a coupling
that had already cost one real cache regression when `plan_rewrite_allowed`
sorted ahead of `skills` and reduced the shared prefix to five bytes.

Log tooling reads a payload as a stream of JSON values rather than one value,
so both documents are accounted for; a single-value parse would have silently
filed every delta field under "other".

#### Final synthesis remains complete

The slow synthesis call is stateless relative to fast-gather history, so it receives
the complete canonical accumulated `symbols` and `context`, not just the latest
delta. This retransmission is required: provider cache history from the fast model
cannot be assumed to exist for the slow model.

#### Shared lens fan-out

All lenses for one task reuse the same gather result. `prepare_lens_fanout` builds a
shared prefix containing the task question, complete gathered evidence, relevant
finding state, skills, and compact plan. Per-lens suffixes contain only lens identity
and lens-specific instructions.

For each slow-model variant, the first lens runs sequentially with cache control to
prime that model's prefix. Remaining lenses then run in parallel and cache-read the
same bytes. If cache priming fails, the current implementation reruns that variant's
complete lens slate in parallel without cache control; prompt content remains
identical and only transport framing changes.

Review point: the cache-prime fallback currently treats all seed failures alike. A
typed over-input rejection can therefore cause one identical no-cache retry of the
seed lens before the repair layer propagates `OverInputLimit`. It does not drop
content, but it is avoidable work and should be considered in a future cleanup.

### Phase 5 implementation: skill cache placement

#### The routing index is cached, not dropped

An earlier revision removed the subsystem routing index from synthesis calls
once a concrete guide had been loaded. That has been reverted. The index is
byte-stable — it is read from disk once at startup and never mutated — so it
lands in `common_skills` and is paid for as a cache read after the first call.
Dropping content to save tokens is what creates risk; the cache already handles
repeated stable content, and a synthesis agent that wants to name another guide
can still see the list.

Removing it also required the generic pipeline to hardcode one skill library's
directory layout (`/subsystem/subsystem.md`), which conflicts with the rule that
pipeline behavior stays subsystem-agnostic.

#### Stable and task-specific halves

`apply_skill_reads` is the only mutator of the live skill payload, and it
returns the paths it grafted. `split_skills_for_synthesis` partitions on exactly
that set:

- everything present before the task ran becomes `common_skills`;
- files this task's `skill_reads` loaded become `skills`.

No structural tree diffing is involved, so the split cannot disagree with what
was actually loaded. The union of the two halves is the live payload verbatim;
nothing is summarized or rewritten. A failed `skill_read` marker counts as
task-specific: it is new bytes this task produced and must not perturb the
stable prefix.

`common_skills` reproduces the base payload byte for byte — including an empty
`files` map — so a task that loads no guides and a task that loads several emit
identical common bytes and share the cache entry. It appears before
task-specific `skills` in the shared lens prefix.

### Phase 6 implementation: prior findings

#### Every finding, in full, once

`previous_findings` carries every current session finding, redacted only of
store-owned narrative and provenance. There is no relevance routing and no
source-body-free manifest class.

An earlier revision split the list by anchor overlap with the task prompt and
gathered evidence, sending unmatched findings as `FindingManifest` records
without source bodies. That was removed. The measured cost it targeted — one
26.8K-character object broadcast to 30 lens calls — is already addressed by
putting `previous_findings` in the shared lens prefix, where 29 of those 30
calls pay a cache read rather than a fresh serialization. Relevance routing
chased the remainder in exchange for a heuristic that decided what evidence a
review could see, and a finding that looks unrelated by filename anchor is
exactly the case a cross-file contract review must catch.

#### Lens behavior

The full finding list is part of the lens shared prefix, so every lens receives
identical prior state and only the first call for a given slow model pays to
create the cache entry.

### Phase 7 implementation: compact plan projection

`Plan` remains the persisted and mutable source of truth. Goal and todo agents still
receive the complete plan because they judge or rewrite it.

Fast, slow, main/source retrieval, and lens task prompts instead use
`PlanPromptView`, containing:

- goal;
- task mode;
- active step ID;
- each step's ID, title, description, status, and dependencies.

The projection excludes:

- `Plan::prompt`, already represented by the task/original prompt;
- `created_at`, which is persistence metadata;
- `todo_ids`, internal bidirectional linkage;
- step `context`, which is injected into the active task question rather than
  repeated for every step.

This projection is derived at serialization time and does not replace or mutate the
stored plan. Plan rewrites still use the typed `PlanRewrite` contract.

### Phase 8 implementation: lossless provider handling

#### Removed destructive shrinking

`kres-core/src/shrink.rs` was deleted. The removed behavior included:

- dropping source/context entries;
- dropping lower-severity findings;
- stripping fields from workflow attempts;
- deleting old workflow attempts;
- rewriting the final user message during an LLM retry.

The LLM client now retries transient transport and rate-limit failures with the same
messages. If exact counting or a provider response proves that the input exceeds the
model capability, it returns typed `LlmError::OverInputLimit { actual, limit }`.

`AgentError::from(LlmError)` preserves this variant. Gather, lens, consolidation,
promotion, main-agent, and JSON-repair call sites no longer flatten it into an
unstructured string before callers can act on it.

#### Workflow behavior

`StepState::prior_attempts_value` serializes every field from every attempt. The
executor removed its prune-and-retry loop. An over-input workflow step fails after
one driver call with its state intact because retrying byte-identical input cannot
make progress and deleting history would violate the workflow contract.

#### Provider error recognition

The client recognizes explicit context/input-limit responses on HTTP 400, 413, or
422 using provider error markers. On those same statuses it also recognizes an
over-limit request when a configured model capability exists and the count
exceeds it.

Both tests are gated on that status set. A terminal failure with any other
status — 401, 403, 404, an exhausted 5xx retry — is never reported as
`OverInputLimit`, however large the request was. Otherwise an auth failure on a
big prompt would surface as a size problem and send a semantic owner off to
partition work that was never too large.

On 429, only an exact `count_tokens` result may attribute the rejection to
request size. Providers with no exact count available fall back to waiting and
retrying, because the chars/4 estimate is far too coarse to turn a shared-budget
rate limit into a hard failure. A genuine capability rejection from those
providers arrives as a 400 naming the context limit and is typed by the path
above.

#### Oversized tool output

Build and shell output is the one tool result with no natural semantic
partitioner: a failed kernel build is megabytes of interleaved diagnostics that
cannot be split by any owner without inventing a classifier over compiler prose.
Above `TOOL_OUTPUT_INLINE_MAX`, `spill_oversized_output` writes the complete
output to `<workspace>/.kres/tool-output/<kind>-<hash>.log` and returns the head,
the tail, the exact byte count, and that path.

This is not the silent truncation the invariants forbid. No byte is discarded,
the omission is stated explicitly with its size, and the agent recovers any
region with a targeted `read` — the same contract the local grep fallback uses.
The alternative, inlining everything, converts a recoverable compile error into
a hard over-input failure for the whole workflow step.

If a provider reports only "too long" without a numeric limit, kres supplies a
conservative smaller partition target in the typed error. That number guides a
semantic owner; it is never used to truncate the original message.

#### Codex JSON-RPC framing

codex-codes rejects a single `UserInput::Text` value at 1,048,576 characters. Kres
now splits one logical prompt into ordered UTF-8-safe text items of at most 1,000,000
bytes. All items remain in the same user turn and concatenating them reconstructs
the original prompt exactly. This is transport framing, not model-input reduction.

#### No generic splitter

The earlier generic serialized-payload byte splitter was removed. Arbitrarily
splitting JSON can separate a key from its value or a finding from its evidence and
gives downstream inference no reliable reconstruction contract. Only components
that own the semantic structure may partition it.

### Whole-file change survey and file survey

This work both changed the review workflow and became an important test case for
lossless context handling.

#### One six-month net diff, not a commit matrix

The change survey no longer assesses every commit independently and no longer
attaches a commit × function matrix to the file survey. It builds one diff from the
parent of the oldest target-touching non-merge commit within six months to the
current working-tree target.

This produces one risk assessment per target function for the net code currently
under review. Changes introduced and then fixed during the window can be reconciled
as fixed rather than leaving stale per-commit risk rows.

#### Git history and rename handling

`recent_target_commits` uses `gix` to walk recent commits by commit time. Merge
commits are excluded from the risk window, but their parent edges can still inform
path tracking. For each commit edge, entry identity is checked first; rename-aware
tree diffing is invoked only for a touched edge so the target can be followed through
renames.

`aggregate_target_diff` then:

1. selects the parent of the oldest relevant commit as the baseline;
2. resolves the target's historical path at that baseline;
3. loads the baseline blob and mode;
4. compares it with the current working-tree bytes and mode;
5. renders one unified target-file diff.

The head identifier is `WORKTREE@<HEAD>` because uncommitted target changes are part
of the review. A mode-only change is retained. Ambiguous baseline rename candidates
fail explicitly.

#### Small-input path

When current target source plus net diff is below the semantic partition target, one
low-effort slow-agent call receives the complete source and diff. It emits typed
`target_function_risks` and `external_major_risks`.

The parser validates names, the 0–100 range, duplicate target functions, external
file identity, and duplicate `(file, function)` external keys. External entries
below 80 are discarded because the external channel is specifically for major
cross-file risk.

#### Large-input path

"Large" means the combined source/diff prompt needs semantic partitioning. It does
not mean repository-wide, and it does not cause content removal.

The diff is split on UTF-8/newline boundaries, preferring hunk boundaries. Continuation
chunks repeat file/hunk headers for orientation, while `source_start/source_end`
ranges prove that the actual diff bodies reconstruct the original bytes in order.

If source plus one diff partition is still large, current target source is also split
losslessly. The survey evaluates the full Cartesian product:

`every source scope × every diff chunk`

This is required because a changed line in one chunk can affect a function whose
current implementation appears in another source scope. Pair calls emit sparse
evidence only; absence from one pair is not interpreted as zero risk.

#### Parallel execution and cache priming

The pair matrix is generated lazily rather than materialized as a vector of complete
prompts. For every source scope, diff chunk 1 is run first to prime that
source-specific cached prefix. Remaining diff chunks then run with concurrency 8.

Each result carries the deterministic flat index
`source_index * diff_count + diff_index`. Results from unordered parallel execution
are sorted by this index before reduction, so inference scheduling cannot change the
reducer input order.

#### Hierarchical typed reduction

If all sparse reports do not fit one reduction call, `pack_change_survey_reports`
greedily packs complete `ChangeSurveyReport` objects into semantic batches. It never
splits a serialized report at a byte offset.

Intermediate reducers:

- may emit only functions present in their input reports;
- must not manufacture "no evidence" rows;
- reconcile evidence as parts of one logical net diff;
- preserve only still-major external risks.

Reduction repeats until the report set fits one final pass. Intermediate batches use
ordered `buffered` concurrency, preserving deterministic result order. The final pass
must emit every authoritative function when the structural inventory is already
known.

If every batch would contain one indivisible oversized report, kres fails with
`cannot be reduced without splitting a typed report`. This is intentional: splitting
unknown report JSON or deleting fields would violate the preservation contract.

#### Structural inventory and corrective pass

The workflow runs the change survey before the file survey as requested. Before the
file survey, the authoritative function set may not yet be available, so the first
change result is allowed to be sparse.

The file survey is fetched exactly once. If its structured result is absent or
invalid, fallback inventory uses optional ctags plus low-effort source-chunk
inference. Source chunks overlap for syntax context, Rust unions definitions/calls,
and Rust recomputes identifier-use counts across the complete source. The fallback
must include every ctags function and exactly match those recomputed use counts.

Once the authoritative function set exists, `complete_function_coverage` accepts a
change report only if it contains every target function exactly once and no unknown
functions. Missing functions trigger one corrective change-survey inference pass
against the authoritative names. Rust does not create zero-risk rows for missing
functions.

#### External-risk interaction filter

The change survey may flag major risks outside the target file. Those risks become
research questions only when Rust establishes a target-file interaction.

`FileSurveyInventory::interaction_kind` accepts:

- a call in the structured file-survey inventory; or
- a non-call function-value reference in comment/string-stripped target source,
  covering callbacks, address-taking, initializers, returns, and similar uses.

A target-local definition shadows a same-named external function. Occurrences in
comments, string/character literals, declarations, and direct call syntax not backed
by the intended path do not manufacture callback evidence.

The final file-survey validator requires exactly one research question for every
interaction-filtered external `(file, function)` risk and rejects questions for
unrelated or invented external functions.

#### Single final rating per function

The final file survey receives:

- compact net-change function risks;
- the authoritative structural inventory;
- only external risks with established interactions.

It must emit exactly one `risk_rating` per target function. There are no per-commit
ratings. The validator enforces:

- target functions exactly equal the inventory set;
- use counts equal Rust's authoritative counts;
- each combined function rating is at least its change-survey rating;
- each rating is within 0–100;
- the single `file_risk_rating` is at least the highest function rating;
- external questions exactly match the interaction-filtered set and have priority
  80–100.

#### Checkpointing

`change-survey.json` stores version, target, source hash, baseline, head, and an
optional completed report. Writes use temporary-file, fsync, rename, and parent
directory fsync. Reuse requires an exact fingerprint match. A corrective
authoritative-function pass replaces the checkpoint report rather than adding a
second assessment matrix.

### Summary pipeline: lossless semantic partitioning

Summary generation has two different large-input problems: raw task material before
condensation and complete findings during final rendering. They now have separate
semantic partitioners.

#### Task-observation condensation

`bucket_task_material` groups every per-finding analysis and every file-level
`task_prose` entry by task. Normal batches contain whole tasks.

If one task cannot fit, `condense_single_task` recursively partitions in this order:

1. split the per-finding list, keeping task prose with the first half;
2. split task prose at a UTF-8 boundary;
3. split one oversized finding-analysis body while repeating its finding ID/title.

Every input body is sent through exactly one child branch. No task prose or finding
analysis is dropped. If only indivisible task identity/schema framing remains and the
provider still rejects it, the summary fails explicitly.

A provider can reject a call even after local sizing predicts it fits.
`try_call_and_extract` preserves the typed limit. `smaller_partition_budget` always
returns a strictly smaller semantic target—at most the reported limit, at most
current minus one, and normally no more than 75% of the current target—then retries
through the semantic splitter. It never slices the original serialized request.

#### Finding render units

The renderer first attempts one complete call. When staging is needed,
`partition_findings_to_fit` treats each full finding as the normal unit and greedily
packs fitting batches.

If one finding is too large, `partition_finding_evidence` removes its two source-body
arrays from a cloned semantic core. Every generated unit repeats that complete core,
including title, severity, summary, mechanism, impact, reproducer, fix sketch, open
questions, status, and relationships. Only source evidence is distributed across
sibling units.

An individual oversized source body is split into UTF-8-safe `exact_text` pieces.
Each piece is explicitly labelled with `exact_text_is_partitioned` and a one-based
`{index,count}` object. Fit testing reserves worst-case `usize::MAX` index/count
metadata before accepting a piece, so adding the real smaller label cannot turn a
fitting unit into an oversized one.

The semantic core itself is indivisible. If it cannot fit without any source body,
kres reports an error rather than dropping core fields.

#### Observation relationship preservation

Every render partition receives the complete condensed task observations. This is
intentional duplication: observations and findings are semantically related, and
putting them in separate calls would force a model to infer relationships from state
it does not have.

The partition note explicitly says that the complete finding is repeated and source
evidence is partitioned without omission.

#### Provider rejection after partitioning

If the provider rejects a render partition that local sizing accepted, `stage_render`
does not retry just that partial with missing context. It chooses a smaller partition
target and repartitions the entire complete finding set.

#### Combining generated partials

Generated partial summaries are combined through a size-aware tree. If a combine
call fits, inference merges the complete child partials. If fan-in is too large, the
set is divided and combined recursively.

When exactly two complete generated partials still cannot fit one combine call, they
are preserved verbatim and concatenated. Generated prose is not arbitrarily split or
silently discarded merely to force another combine call.

### Error propagation and retry semantics

#### Typed versus retryable failures

An input-capability error means the same request cannot succeed unchanged. It is
therefore distinct from transport interruption, 429 rate limiting, malformed model
JSON, and validation failure.

- Transport/ordinary rate-limit failures retain normal backoff/retry behavior.
- JSON/schema failures may receive a targeted repair inference or a rerun containing
  explicit validator errors.
- `OverInputLimit` must reach a semantic owner or fail the operation.
- Workflow state is never pruned in response to `OverInputLimit`.

#### Lens repair

Lens failures now carry `over_input_limit: Option<(actual, limit)>` separately from
their human-readable error. Before JSON repair/rerun and again after the final repair
attempt, `run_lenses_shared_gather_repairing` checks that typed field and returns
`AgentError::OverInputLimit`. This prevents the repair loop from treating input size
as if the lens had emitted malformed JSON.

#### Consolidation and promotion

Direct LLM calls in consolidation, promotion, JSON repair, and main-agent paths use
the centralized `From<LlmError> for AgentError` conversion. This retains capability
information across layers instead of flattening every provider failure into
`AgentError::Other(String)`.

## Part 7 — Regression-test map

The implementation is covered by structural tests intended to prove preservation,
not merely that calls return something.

### Scan and plan ownership

- `compact_plan_does_not_duplicate_scan_from_question`
- `review_file_scan_roundtrips_outside_plan_prompt`
- `review_scan_context_does_not_parse_plan_prose`
- `review_scan_context_requires_matching_target`
- source hash tests covering executable-mode changes

### Source canonicalization

- `canonical_semcode_single_result_uses_only_normalized_symbol`
- `canonical_semcode_multiple_results_preserve_raw_candidates`
- `canonical_semcode_unparseable_result_requires_local_fallback`
- `single_result_semcode_is_represented_only_as_a_symbol`
- `canonicalize_never_drops_a_distinct_context_record`
- `canonical_prompt_evidence_preserves_gather_order`
- `canonical_prompt_evidence_recomputes_untrusted_ids`
- `prompt_evidence_is_never_removed_for_size`
- `append_symbol_drops_a_range_already_contained_verbatim`
- `append_symbol_replaces_a_range_it_contains_verbatim`
- `append_symbol_keeps_a_contained_range_whose_body_differs`
- `append_symbol_keeps_adjacent_ranges_that_share_no_bytes`
- `append_symbol_never_collapses_across_files`
- `append_symbol_never_collapses_function_symbols`

### Prompt/cache construction

- `split_documents_each_parse_alone_and_merge_to_the_whole_prompt`
- `delta_document_reuses_an_externally_rendered_stable_document`
- `every_lens_in_a_fanout_renders_byte_identical_stable_bytes`
- `stable_document_is_byte_identical_whether_or_not_the_delta_is_empty`
- `split_returns_an_empty_stable_document_when_no_stable_field_is_present`
- `lens_instruction_stays_out_of_shared_cache_prefix`
- `gather_cache_markers_leave_room_for_cached_system_prompt`
- `slow_agent_prompt_contains_full_skills_payload`

### Skills and findings

- `synthesis_skills_split_stable_common_from_task_selected_guides`
- `synthesis_skill_halves_reunite_into_the_live_payload`
- `a_failed_skill_read_stays_in_the_task_half`
- `every_prior_finding_reaches_the_prompt_with_its_source_intact`

### Change/file survey

- net diff represents final target state rather than intermediate commits;
- worktree changes and mode-only changes are included;
- reused historical paths do not contaminate the target window;
- diff chunks reconstruct all source bytes and repeat hunk context;
- independent large source/diff inputs cross every scope with every chunk;
- report batching retains complete typed reports;
- final survey rejects per-commit rating matrices;
- final survey cannot lower net-change risk or authoritative use counts;
- external research requires an actual interaction and every qualifying external
  risk is preserved exactly once;
- comments, strings, declarations, and local shadowing do not create false external
  interactions.

### Provider and summary handling

- codex text-item framing reconstructs every UTF-8 byte;
- provider context rejection becomes typed even without a configured numeric limit;
- ordinary bad requests are not misclassified as input limits;
- `terminal_non_size_errors_are_not_misclassified_as_context_limit`;
- `oversized_request_on_a_size_status_is_still_typed`;
- `oversized_build_output_is_spilled_whole_and_pointed_at`;
- `output_within_budget_is_returned_inline_untouched`;
- `a_spilled_log_is_readable_back_through_the_read_tool`;
- `spilling_one_enormous_line_falls_back_to_a_byte_pointer`;
- workflow over-input failure performs one call and preserves all prior attempts;
- summary UTF-8 partitions reconstruct every byte;
- provider rejection always decreases the semantic partition target;
- source-evidence partition metadata is included in fit accounting.

### Logging

- `accounting_covers_both_documents_of_a_split_prompt`;
- `duplicate_detection_sees_across_the_document_boundary`;
- `all_summary_inference_goes_through_the_one_logged_call_site`;
- a delimited scan counts as one block;
- duplicate scan and duplicate context warnings are emitted structurally;
- multi-turn logs preserve the newest turn and complete request;
- cross-turn duplicate context is detected on the write path, and cross-turn
  symbol-body retransmission by `duplicate_symbol_bodies_in_context` off it.

