# Workflow Internals

This document is the source of truth for all four shipped kres
workflows — `/review`, `/fix`, `/triage` and `/validate` — plus the
runner they share: step contracts, output fields, evals, reaper
actions, retry and branch semantics, and the measured reasons behind
each rule. It is written for someone changing kres, not for someone
using it.

There is no second description of any of them. `/validate` in
particular used to have a standalone snapshot document; it does not
any more, and its behaviour is defined by [Validation
Flow](#validation-flow-validate) below together with the shared runner
sections at the top of this file. When validation behaviour changes,
this file changes with it.

If you want to know what the workflows do and how a run progresses,
read [workflow.md](workflow.md) first — this file assumes it.

Keep `AGENTS.md` short and point back here instead of duplicating the
flow details.

For a cross-workflow inventory of goal ownership, planning, plan mutation,
and completion decisions, see [Planning and Goal Ownership](planning-and-goals-audit.md).

## Shared Workflow Runner Behavior

The configured workspace is implicitly readable and writable by kres
tools. When the operator mentions an existing absolute file or
directory outside the workspace in a prompt, kres grants session-scoped
read/write access to that file's parent directory or to that directory
itself. The same grant is used by `read`, `edit`, `code_output`, and
workflow reaper paths; `/clear` or process restart drops the grants.

The workflow runner wires the normal orchestrator into every LLM step.
That means LLM steps use the fast-gather -> deterministic-fetch -> synthesis
loop:

1. Fast agent requests typed followups such as `read`, `source`,
   `type`, `grep`, `git`, `callers`, or `make`.
2. The deterministic tool service fetches data and adds it to symbols/context.
3. The step's declared agent receives the gathered context and emits the final
   response. `agent: fast` uses the fast model for synthesis; `agent: slow` and
   `agent: code` use the slow model.

The gather phase and the synthesis call run under DIFFERENT system
prompts, and the split is load-bearing. Gather uses
`fast-code-agent.system.md`, which mandates the
`{analysis, followups, ready_for_slow}` envelope. Synthesis must satisfy
the step's `OUTPUT SCHEMA` instead, so it uses the prompt named by the
step's optional `synthesis_system` field, defaulting to
`workflow-synthesis` for `agent: fast` and to the per-mode slow prompt
for `agent: slow` / `agent: code`. Set `routing-agent` on a step that
only routes over typed inputs and never analyzes code. Do not set
`fast-gather` on a step that has an OUTPUT SCHEMA: measured across 384
validate runs, when a fast step's synthesis ran under the gather prompt,
397 of 784 responses obeyed the system prompt instead of the schema and
each rejection re-ran the whole step including its gather phase.

A step's call-invariant instruction text — the `--- SKILLS ---` block and
any `--- INCLUDES ---` bodies — is carried in the prompt envelope's
`stable_instructions` field rather than concatenated onto `question`, and
it gets its own `cache_control` block on both the gather and the
synthesis call. It used to be per-call bytes: on a measured validate run
that was 30 KB of a 42 KB question re-sent as fresh input every time.
Anything task-specific must stay out of that field or the head is written
once per call and read never, which is strictly worse than not caching.

When a step attempt is rejected for a malformed or incomplete response,
the evidence requests that attempt emitted are carried into the retry's
gather and dispatched before round 0 (`RunContext::pending_followups`).
Previously they were dropped and the gather agent re-decided what to
fetch: across those same 384 runs, 724 of 771 such requests — 401 of them
`source:` — were never fetched anywhere in the run.

Deterministic workflow steps run in the reaper without an LLM. Reaper
steps are used for git commits, builds, finding invalidation, and patch
publication. Reaper actions cannot declare a later eval: successful completion
of the deterministic action is their acceptance boundary. Finding-artifact
mutations restore a directory snapshot on failure, and failed commit actions
restore the exact pre-action Git index. The AgentEnv one-shot path exists only for tests and
minimally wired callers.

Workflow output parsing preserves the raw slow-agent response before it
is projected into the standard kres response envelope. Typed workflow outputs such as
`valid`, `affected_files`, and `affected_symbols` are extracted from
that raw response, so schema-specific JSON is not lost when it is not an
`analysis`/`findings`/`followups` envelope.

Every LLM workflow step gets an `OUTPUT SCHEMA` tail. That schema
describes the workflow-specific keys that must be present, but the
response is still the normal kres JSON envelope. Standard kres response
keys such as `analysis`, `findings`, `followups`, `code_edits`, and
`code_output` are allowed. Workflow keys and standard kres keys must be
in the same top-level JSON object. Do not emit any other JSON object or prose.

When a workflow step runs through the orchestrator, the fast gather
phase receives a gather-only prompt instead of that final `OUTPUT
SCHEMA`. This keeps routing fields such as `clean`, `defects`,
`correction_step`, `valid`, and `result` out of the gather phase. Only
the final slow/coding/consolidate response is allowed to satisfy the
workflow step outputs.

`code.jsonl` records may include a `label` field such as
`phase=fast-gather step=review lens=memory`. The label is logging
metadata only; it is not sent to the model and does not affect prompt
caching. Use it to pair interleaved lensed user/assistant records
instead of relying on adjacent JSONL lines.

User records in `code.jsonl` and `main.jsonl` also carry a
`context_stats` object computed after request construction. It reports the
serialized size, a stable content fingerprint, per-field and per-category
character counts, whole-file scan occurrences, and exact duplicate evidence
counts. Request paths that log wire metadata also include the system-prompt
size and fingerprint, with its bytes classified separately. Warnings identify
duplicate scans, duplicate source bodies, repeated context entries, skill
payloads above 80K characters, and requests above one million characters. This
metadata is diagnostic only and is never sent to a model. Pair it with the
matching assistant record's provider usage to compare fresh, cache-created,
and cache-read tokens.

For multi-turn fast-gather calls, `content` remains the newest user turn for
compatibility with existing readers and `request_content` contains the complete
ordered model-visible conversation. `context_stats` is computed from that full
conversation, not only from the newest evidence delta.

All structured agent prompts require exactly one JSON value with no prose,
embedded JSON string, or transport wrapper. Prompts end with an explicit
raw-JSON-only instruction and prohibit Markdown headings, preambles, fences,
backticks, and trailing commentary.
As deterministic transport normalization, Rust first accepts whole-response
JSON or one whole Markdown JSON fence. It may then extract exactly one
outermost syntactically valid JSON object from surrounding prose. Zero or
multiple candidates are rejected; kres never chooses between objects. Illegal
literal control characters are escaped only inside that candidate's JSON
strings. The resulting object then runs through the unchanged strict contract
before inference repair. Serde DTOs are the
acceptance boundary; nested DTOs reject unknown fields, and
`serde_path_to_error` identifies the exact invalid field. `schemars` derives
the repair schema from the same Rust DTO, so prompts and deserialization do not
maintain separate representations of the contract. Workflow-defined extension
fields are allowed only when declared by that step and are subsequently checked
against the workflow's JSON Schema.

When surrounding text is discarded, `code.jsonl` retains the original
assistant response and adds a second row with role `normalization`, label
`json-normalization discarded-surrounding-prose`, the exact discarded prefix
and suffix, and the number of escaped control characters. This keeps prompt
drift and substantive out-of-envelope prose searchable.
Control-character-only normalization uses the separate label
`json-normalization escaped-control-characters`.

On failure, one repair inference receives the untouched response, a generated
schema projected to the fields valid for that inference stage, and the
serde/schema errors. Unreachable code-edit, file-output, plan, or workflow
fields are omitted from that repair schema. Its replacement is accepted only by
deserializing it through the identical contract. Beyond the narrow transport
normalization above, Rust does not infer fields, merge candidate objects, or
attempt to infer semantic equivalence between malformed and repaired prose. If strict repair fails, the caller retries or fails the
step. The original response remains in logs.

If representation-only repair still fails, the existing caller-specific retry
runs. Workflow steps rerun the complete model step with the specific
validator/parse error from the previous attempt (e.g. "findings is not
array<Finding>", "missing required output 'analysis'"). Each newly rejected
response is independently eligible for exactly one generic repair call; the
same rejected response is never repaired repeatedly. The workflow permits the
original step call plus three full response retries. If the driver still cannot map the response and the workflow
step has an eval retry budget, the executor repeats that same step
instead of terminating the workflow immediately. Side effects are staged only
after every model-owned required output and type has validated; machine-owned
outputs are derived and validated against that staged view. `code_output` and
`code_edits` are preflighted together into one final file map. The executor
commits that map only after eval accepts the attempt; rejected attempts discard
it. Commits bind opened directory descriptors and address targets through those
descriptors, so replacing a parent path with a symlink between staging and eval
cannot redirect a write. Existing contents and permissions are retained for
rollback, and rollback failures are surfaced. New targets retain the
temporary file's private creation mode; kres never forces a world-readable mode
that could bypass an operator's restrictive umask.

Workflow output definitions use ordinary JSON Schema in `schema`. There is no
compact shorthand or schema-detection heuristic. Array schemas describe the
array itself and place their object contract under `items`.

After side-effect and machine-populated outputs are added, the runner
validates that every required workflow output is present. A step that
emits only `analysis` when `valid` or `affected_files` is required fails
at that step with a missing-output error instead of crashing a later
`run_if` expression.

## Workflow JSON Format

The shipped workflow files are normal JSON and intentionally keep their
prompts inline as arrays of strings. The loader joins a prompt array with
newlines, so an empty string element means a blank line. A prompt can also
be a single string.

Local/operator workflows may use `prompt_file` instead of `prompt` for a
step prompt or consolidator prompt, and `judge_prompt_file` instead of
`judge_prompt` for a `judge_llm` eval. Relative prompt-file paths resolve
against the workflow JSON file's directory. `prompt` and `prompt_file`
are mutually exclusive in the same object; the same is true for
`judge_prompt` and `judge_prompt_file`. The shipped workflows do not use
prompt files because keeping the active prompt text in the JSON makes the
standard flows directly editable and reviewable.

The `$format` block at the top of each shipped workflow is non-executable
documentation. The schema permits it and the runner ignores it. Use it to
explain authoring conventions and control-flow intent without adding fake
runtime fields or invalid JSON comments.

`globals` is a reusable prompt namespace. A step opts into a global by
listing `{{globals.name}}` in `include`. A string global is inserted as
literal prompt context. An object global with `include` points at another
file, usually under `configs/prompts/`, and an optional `header` labels
that included content. Globals are for cross-step rules that must stay
consistent, such as commit-message style, output contracts, and the rule
that deterministic workflow steps own git/make/publish actions.

`defaults` fills missing per-step fields. Step-local `agent`, `mode`,
`actions`, and eval retry settings override the defaults for that step
only. This keeps the common case compact while still making exceptional
steps explicit.

Ordering is controlled by `depends_on`. A step is eligible only after all
dependencies complete, then `run_if` and `skip_if` decide whether it
actually runs. Parallelism is explicit: a step with `lenses` runs the same
prompt once per lens concurrently, binds lens fields as
`{{lens.<field>}}`, and merges results using `aggregate`. Without lenses,
steps run according to dependency order.

Individual lenses may carry `run_if` or `skip_if`; those expressions use
the same workflow context and expression language as step-level
conditions. Filtering happens before fan-out, so REPL and `--prompt`
entry points see the same active JSON-defined lens set.

Workflow-owned review parallelism must be expressed in the workflow JSON
only. Do not emulate lenses by listing reviewer names inside one prompt:
that creates one LLM call with one context window and is not a lensed
workflow step.

Evals control retries and branching. After a step produces outputs and
machine-populated outputs are added, `field_check` evaluates a small
local expression, `builtin` runs a named Rust-side validator, and
`judge_llm` asks an agent for `{pass, reason}`. Passing eval finishes review
ledger inference, serializes the exact staged effect, and persists the step as
`effects_pending` before touching files. The executor then applies that effect
and persists `done`. Resume from `effects_pending` replays the recorded effect
without calling the model or eval again. Deterministic commands are separate
reaper steps; the former `post_actions` mechanism has been removed.
Every run writes its snapshot into a directory it owns: an explicit state
directory wins, then the results directory. There is no fallback — a caller
supplying neither is an error, not a default. A snapshot is the run's private
record of what it has already done, so a directory shared with another run is
not a lesser answer but a way to lose that record: 50 concurrent `/validate`
processes writing one `<workspace>/.kres/workflow-state/workflow-validate.json`
killed two runs on a scratch-file rename and left every survivor's resume state
describing whichever finding finished last. `--results` is honoured when given;
otherwise a run gets `~/.kres/sessions/<ts>-<pid>/`, which the REPL and the
one-shot `--prompt` path both compute. Live observers do not disable snapshots.
Fix-series planning and each todo revision use separate subdirectories under
`workflow-state`.
Failing eval follows
`eval.on_fail.action`: `repeat` reruns the step, `branch_to` moves
control back to a named step and invalidates dependent work, `continue`
keeps going, and `exit_failure` terminates. `max_attempts` and
`on_exhausted` decide what happens after repeated eval failures. Driver
errors that occur before usable outputs are produced use the step's eval
retry budget when the step has an eval block; otherwise they fail unless
that driver has a specific recovery path.

Over-capability is one such path. When the gathered evidence exceeds the
model's input capability the request is never sent, and the step retries
within a bounded budget — three attempts — even when it declares no eval
block, with the reason handed to the next attempt. Nothing is trimmed to
recover: the request that did not fit is abandoned whole, the agent is
told the size it overshot and that nothing was cut, and
`reuse_gathered_context` is cleared so it re-gathers rather than
replaying the oversized evidence. Without this, `fix.json`'s
`lore-search`, `fixes-tag-search` and `compile-triage` had no budget at
all and one oversized followup ended the run: a fix died on
`OverInputLimit actual=924140 limit=900000`, of which a single
patch-bearing git log over a whole file was 3.4 MB.

### The review lenses are handed the patch, not asked to fetch it

`supplied_review_evidence` seeds the lensed review's gather with two
things from the worktree: `git diff HEAD~1`, and the current text
around every hunk that diff touches (POST-image line ranges, ±40
lines). Both are rendered through `render_readonly_payload`.

They are seeded into `symbols`/`context` rather than given a prompt
field of their own. That is a caching decision: those two fields are in
`LENS_TASK_CACHE_FIELDS`, so the evidence is written once per review
round and read by all six or seven lens calls in that round. A field of
its own, or placement in the per-lens delta, would re-send the whole
patch once per lens. A test pins that placement.

Deliberately worktree-based, not symbol-index based: the review prompt
warns that `source:` is stale for changed functions once a patch has
landed, and reading the tree is what makes the warning unnecessary
rather than merely stated. Callee lookups stay in the gather — they are
not derivable from the diff.

Measured on the 2026-08-11 06:24 linux.mm run, which motivated this:
across nine review rounds the lenses issued 24 gather calls costing ~8
minutes of wall time, against 3.4 minutes for all 47 parallel lens
calls. The most requested item was `git diff HEAD~1`, once per round,
and none of the 24 gather prompts carried a Rust-supplied payload. The
cost of a review round is its gather, not its lens count — which is why
skipping lenses is the wrong lever.

### A step that rewrites the finding's artifacts is handed them

A step declaring `summary_written` must emit `summary.md`, `metadata.yaml`
and `FINDING.md` in full, so it needs their current bodies.
`supplied_finding_artifacts` reads them from the finding directory and seeds
them into the step's context on every attempt. An absent file is stated
explicitly rather than omitted, so the step creates it instead of hunting for
it.

Leaving this to the gather made it a coin flip: nothing forces the fast agent
to re-read them, and on a re-entry there is no cached copy either. Measured on
the 2026-08-11 kres-inode batch, where a refutation sends
`validate-reachability` back for another pass — of six attempts on one finding
only two had the artifacts in context. Two of the other four refused to
rewrite files they could not see, which fails the `summary_written` gate and
ends the run; two rewrote FINDING.md from memory at a third of its size.

### A review verdict must be actionable in both directions

`clean` is the fix loop's routing signal: false sends the run to
`reconcile-review` and blocks publish. A consolidation that returns
`clean: false` with every typed defect array empty therefore tells
every downstream step the patch is unacceptable while naming nothing
to change, and `clean: true` alongside itemised defects would publish
over an unreviewed one.

`review_outcome_is_coherent` refuses both, and the driver re-runs the
CONSOLIDATOR to fix it — one extra merge call, never the lens fan-out,
bounded by `CONSOLIDATE_COHERENCE_RETRIES`. It is caught there because
the consolidator is what merged the lenses and is the only agent that
can say which lens's concern it dropped.

Observed on the 2026-08-10 20:07 linux.nfs run, review round 2:
`clean=False` with all four arrays empty. That run recovered only
because the reconciliation had a live objective of its own to work
from.

### The gathered cache is invalidated by source changes, not by re-entry

A step's gathered symbols and file sections are cached per step id and
seeded into steps that declare it in `depends_on`. Validity has exactly
one rule: a gathered record goes stale when the bytes it was fetched
from change. So the driver drops the whole cache when a step stages a
real source file or declares a `make`/`meson`/`bash` action, and only
then.

Two consequences follow, and both are load-bearing:

- A write **outside the source workspace** invalidates nothing. The cache
  holds evidence read out of the workspace, so a write into a consented
  finding directory or a results tree cannot stale any of it
  (`staged_write_can_stale_gathered_source`). `/validate` writes its three
  artifacts into the finding directory, which is routinely a different tree
  from the source under audit; those writes were wiping the whole cache.
- Staging `.kres-commit-msg.tmp` does NOT invalidate anything. It is
  workflow bookkeeping for the reaper's `git commit -F`, not source
  under review (`is_workflow_scratch_artifact`).
- A step re-entered inside the same run — `branch_to`, `rerun_chain`,
  or an orchestrator routing — keeps `reuse_gathered_context` set and
  seeds from its own prior gather. Only a step's first attempt in a
  fresh run starts from its dependencies alone, so a driver reused
  across runs cannot leak one run's gather into the next.

Note the scope: seeding applies to the non-lensed step path, which
builds a `RunContext` carrying `seed_symbols`/`seed_context`. The
lensed fan-out path does not set those fields, so a re-entered LENSED
step (`review`) still re-gathers. What the lensed path now does do is
`store_gathered` its shared gather, which is what lets a dependent
step — `reconcile-review` — seed from it. Making `review` itself reuse
across cycles is a separate change and has not been made.

Do not restore a blanket "any staged file invalidates" rule or force
`reuse_gathered_context` off on re-entry. Measured on the 2026-08-10
linux.nfs fix run, the two together made every commit-message rewrite
clear the cache for every step and then re-gather from nothing:
`write-commit-message` was re-entered 10 times and spent 38 fast-gather
rounds re-reading `dentry_create`, `do_open` and `may_open`, none of
which had changed. The one exception is deliberate and stays: the
over-capability path above clears `reuse_gathered_context` on purpose,
because there the gathered evidence is precisely what did not fit.

## Fix Flow (`/fix`)

`/fix <target>` in the REPL and `--prompt "fix: <target>"` on the CLI
dispatch the embedded `configs/workflows/fix.json` workflow. `fix` is
workflow-only: kres does not load `~/.kres/commands/fix.md`,
`~/.kres/prompts/fix-template.md`, or any embedded slash-command
template as a fallback.

`<target>` is one of:

- An absolute path to a kres finding directory, with `FINDING.md`,
  `summary.md`, and `metadata.yaml`.
- Freeform bug prose. The fast agent gathers context itself.

The workflow input derivation expands a leading `~/` and normalizes any
target that resolves to a finding directory into an absolute path before
setting `target_kind = "finding_dir"`. This matters because
`set-finding-status` and `publish-fix` require an absolute
`finding_dir`. Text that merely mentions `~/` later in the string is
left as prose.

When `fix:` is run with freeform prose and `--results DIR`, the workflow
uses `DIR` as `target_artifact_dir`. That gives prose runs the same
artifact/status path as finding-directory runs without changing the bug
input text. If `metadata.yaml`, `FINDING.md`, or `summary.md` are absent
in the results directory, the reaper creates minimal files before writing
status or patch artifacts.

For finding directories, `metadata.yaml.git.sha` is the audit's HEAD and
may differ from the workspace HEAD. The research step must verify that
the bug still exists at the current workspace HEAD.

### Shape of a Fix Run

A fix run is two nested loops.

**Outer — the series.** One bug report can need several independent
commits. `run_fix_series_driver` runs the whole workflow once per commit:

```text
planning ──> research only: is the bug real, and what commits does fixing it take?
   │
   ├─> todo ──> a full fix run, ONE commit          ┐
   ├─> todo ──> a full fix run, ONE commit          ├─ one per fix_plan item
   ├─> todo ──> ...                                 ┘
   │
   └─> final ─> series-assessment: did the series fix the reported bug?
```

State lives in `<results>/fix-series.json`; `--resume` re-enters at the
first incomplete item. See [Fix Series](#fix-series).

**Inner — one commit.** Everything below is one `todo` run:

```text
  target: a kres finding dir, or freeform prose
     │
     ▼
  research ──────────── is this bug real?
     │                  emits research_status, affected_files, fix_plan,
     │                  bug_inventory, is_latent
     │
     ├─ invalid ──────> invalidate + record-invalidation-results ──> END
     │                  (needs source/commit evidence predating the run)
     ├─ unconfirmed ──> unconfirm ──> END
     └─ confirmed
           │
           ▼
     record-bugs ────── write the bug inventory into the finding
           │
           ▼
     lore-search ────── has someone already posted this fix upstream?
           │            feeds the bug-coverage lens; a duplicate cites a
           │            lore Message-ID
           ▼
     write-patch ────── the slow coding agent emits code_edits
           │
           ▼
     fixes-tag-search ─ which commit introduced the bug?
           │            a Fixes: trailer is added ONLY if the sha is proven
           ▼
     write-commit-message ── writes .kres-commit-msg.tmp
           │
           ▼
     commit ─────────── git commit -F   (reaper: deterministic, no model)
           │
           ▼
     build ──────────── make (reaper)
           │
           ├─ failed ─> compile-triage: patch_error, or preexisting_error?
           ▼
     review ─────────── lenses in parallel, then the refinement loop below
```

From there the run either publishes or enters the refinement loop:

```text
                    ┌─────────────────────────────────────────┐
                    │                                         │
                    ▼                                         │
   ┌─────────  review (parallel lenses)                       │
   │             │                                            │
   │             │  every lens sees the same gathered source  │
   │             │  and NONE sees another's output            │
   │             ▼                                            │
   │           consolidate ── merges, never adjudicates:      │
   │             │            highest severity wins, so two   │
   │             │            lenses wanting opposite things  │
   │             │            BOTH survive into defects[]     │
   │             ▼                                            │
   │  clean? ──yes──> publish                                 │
   │    │                                                     │
   │    no                                                    │
   │    ▼                                                     │
   │  reconcile-review ── resolve the contradictions into     │
   │    │                 ONE instruction set                 │
   │    ▼                                                     │
   │  orchestrator ────── pick the next worker                │
   │    │                                                     │
   │    ├─> write-patch ──────────┐                           │
   │    ├─> write-commit-message ─┼─> commit ─> build ────────┘
   │    ├─> build ────────────────┘
   │    ├─> publish ──────> END  (ships with defects outstanding,
   │    │                        citing evidence)
   │    └─> exit-failure ─> END
   │
   └─ 10 review rounds max, then exit_failure
```

The patch is amended, never stacked, so `git diff HEAD~1` is always the
whole current patch and every round judges all of it.

Every LLM step in that pipeline is three calls, not one — fast-gather,
deterministic fetch, then synthesis — as described in
[Shared Workflow Runner Behavior](#shared-workflow-runner-behavior). A
step declaring `actions: []` cannot touch the workspace; `code_edits`
from such a step are refused, not applied.

### Fix Steps

Before the per-todo workflow starts, the outer driver runs `research` in
planning mode with primary-slow synthesis. Its `fix_plan` becomes revisioned,
atomically persisted `fix-series.json` state. Planning mode owns decomposition;
per-todo research verifies the selected item and can change remaining work only
through a bounded, revision-checked typed update.
`--resume` reloads this outer state and resumes any matching inner workflow
snapshot.

1. **research**

   The primary-slow audit agent receives fast-gathered affected files, callers, enough source
   context, and enough local history to decide whether the bug is real
   and how to fix it. It does not own exhaustive `Fixes:` provenance;
   that is a separate workflow step after the patch exists.

   Required outputs:

   - `research_status`: `confirmed`, `invalid`, `unconfirmed`, or
     `unexecutable`.
     `confirmed` means gathered source proves a concrete bug exists at
     workspace HEAD and `analysis` contains a specific fix contract.
     `invalid` means source/commit evidence disproves the bug.
     `unconfirmed` means the bug may be real but research could not
     prove or disprove it well enough to patch — another gather round
     could still settle it.
     `unexecutable` means no amount of further audit in this step can
     settle the todo, because its fix contract requires a capability
     the step does not have.
   - `valid`: compatibility mirror; true only when
     `research_status=confirmed`.
   - `invalid_evidence`: file:line or commit ref proving the bug is
     invalid; empty when `valid=true`.
   - `invalid_evidence_kind`: `source_or_commit_evidence` only when
     `invalid_evidence` is actionable source/commit proof that the bug is
     invalid; `none` otherwise.
   - `research_decision`: structured routing rationale with
     `bug_proven`, `fix_contract_proven`, `invalidity_proven`, and
     `needs_more_audit`. These booleans must agree with
     `research_status`; routing never depends on wording inside
     `analysis`.
   - `followups`: typed data requests. A blocking followup (default
     `required_for_progress: true`) is a request for another gather round. Any
     blocking followup makes the eval fail and triggers a retry that
     gathers the requested evidence. A nice-to-have followup
     (`required_for_progress: false`) is a deferred audit suggestion that does
     not block — it may ride alongside any terminal status.
   - `affected_files`: paths the patch will touch.
   - `affected_symbols`: relevant symbols.

   Optional outputs:

   - `analysis`: research narrative and fix sketch for later steps.
   - `unexecutable_requirement`: `none`, `disallowed_action`,
     `runtime_or_hardware`, or `human_decision`. What the fix contract
     needs that this step cannot supply. Required to be something other
     than `none` when `research_status=unexecutable`.
   - `unexecutable_detail`: one sentence naming the missing capability,
     for the human who has to re-plan the todo. Never parsed.

   The research step has a structural `builtin` eval named
   `fix_research_status`. It accepts only one of these typed states,
   and in all three cases the `followups` array must have no blocking
   entries:

   - confirmed: `research_status=confirmed`, `valid=true`,
     `invalid_evidence=""`, and `invalid_evidence_kind=none`.
     `research_decision` must say the bug and fix contract are both
     proven, invalidity is not proven, and no more audit is needed.
   - invalid: `research_status=invalid`, `valid=false`, non-empty
     `invalid_evidence`, and
     `invalid_evidence_kind=source_or_commit_evidence`.
     `research_decision` must say invalidity is proven while the bug and
     fix contract are not.
   - unconfirmed: `research_status=unconfirmed`, `valid=false`,
     `invalid_evidence=""`, and `invalid_evidence_kind=none`.
     `research_decision` must say invalidity is not proven and either the
     bug, the fix contract, or both remain unproven, or more audit is
     needed.
   - unexecutable: `research_status=unexecutable`, `valid=false`,
     `invalid_evidence=""`, `invalid_evidence_kind=none`,
     `unexecutable_requirement` set to something other than `none`, a
     non-empty `unexecutable_detail`, and `research_decision` saying the
     fix contract is not proven, invalidity is not proven, and
     `needs_more_audit=false`. That last boolean is the discriminator
     against `unconfirmed`: if another gather round could settle the
     todo, the status is `unconfirmed`.

   `unexecutable` is terminal for the todo and is deliberately excluded
   from the `fix_run_mode == 'todo' && research_status != 'confirmed'`
   failure clause, so the item run ends cleanly and the series driver
   sees the distinction. The driver records the todo as
   `FixTodoStatus::Unexecutable`, prints
   `[fix series] unexecutable i/n <id> (<requirement>): <detail>`, and
   stops the series with a message naming the requirement and the
   commits that already landed. It does not publish a partial series.
   The revision and plan-update paths cannot help here — both are gated
   on `unconfirmed`, and re-auditing is exactly what will not work.

   Every step's prompt now states its own allowlist, in an `--- ACTIONS
   AVAILABLE TO THIS STEP ---` block inside the cached
   `stable_instructions`, together with the rule that a plan or
   contract written for a later step must stay inside what the
   executing step can do. Before that the allowlist was enforced and
   never declared: it reached only the fetcher gate, which drops a
   disallowed followup and appends "followup kind 'x' rejected by step
   allowlist [...]" to the context. A step therefore learned its own
   capabilities by being refused, one kind at a time — and a step
   writing a PLAN never learned them at all, because planning
   dispatches nothing and so is never refused.

   `unexecutable` remains the backstop for when a contract is wrong
   anyway. On the 2026-08-20 futex2 series,
   finding `lsui_eagain_amplifies_unlock_pi_fph_leak` landed todo 1 of 3
   as commit `b5f08caba30e`, then todo 2's contract opened with "Step 1
   (evidence): build and run, with the tree's aarch64 gcc and with
   clang, at both -O2 and -O0, a minimal program …". The `research`
   step allows `read, source, type, git, grep, callers`. The model said
   so on its first round, asked for `bash` twice more anyway, and the
   series died as a generic "research_status was unconfirmed, expected
   confirmed" after 106 model calls.

   Malformed JSON, missing required outputs, inconsistent typed fields,
   or a blocking followup all consume the research eval retry budget
   and rerun research instead of routing prose into later workflow
   steps.

   Only `research_status=confirmed` reaches `write-patch`. If research
   cannot prove a concrete bug and fix contract, it must not set
   `confirmed` just to keep the fix pipeline moving.

   On eval-fail repeats, the runner snapshots the failing attempt's
   outputs into `StepState.prior_attempts` before clearing them. The
   next attempt's prompt sees that list through the
   `{{<step>.prior_attempts}}` interpolation token (a JSON-rendered
   array of prior output maps, oldest first). The fix-workflow
   research prompt uses this so the slow agent can read its earlier
   `fix_plan` / `research_decision` and either commit to one of those
   shapes by gathering specific evidence, or justify a different shape
   with concrete source/commit evidence — instead of cycling through
   plausible-but-different proposals every retry.

   Proving a concrete bug requires proving the alleged state is valid and
   reachable, not merely that one local function would mishandle that
   state if it appeared. For each required flag combination, object
   state, lock state, refcount state, callback ordering, or lifetime
   transition, research must inspect the creators/converters and
   validators of that state. Assertions, `WARN`/`VM_WARN` paths,
   page-table checks, type contracts, helper comments, and documented API
   rules can be invalidating evidence when they decisively reject the
   alleged state. If they raise doubt but do not disprove it, research
   should return `unconfirmed` and request the missing evidence.

   If the proposed fix restores behavior that current history
   deliberately removed, research must read that removal commit and
   require concrete evidence that the removal was wrong or incomplete
   before confirming. A patch must not be planned only to handle an
   invalid or deliberately forbidden state. Confirmed research needs a
   concrete trigger path: creator path -> transformed state -> affected
   function -> violated contract/bad outcome. If any link is speculative,
   research should return `unconfirmed` unless source/commit evidence
   proves the state is impossible or already fixed, in which case it
   should return `invalid`.

   If the bug is invalid and the input is a finding directory, the
   workflow goes to `invalidate` only when `invalid_evidence_kind` is
   `source_or_commit_evidence` and `invalid_evidence` is non-empty. If
   research cannot prove or disprove the bug, the workflow goes to
   `unconfirm` for finding-directory targets. If invalid research lacks
   structured proof, the workflow fails rather than guessing from prose
   or mutating finding status. If the input is freeform prose, there is
   no finding directory to update.

   A finding-directory target is automatically granted to the session
   outside-workspace access store before followups run. The operator
   supplied that path as the workflow target, so reading and writing
   files under that directory is part of the requested operation even
   when the kernel workspace is a separate checkout. Missing access,
   missing context, or a blocked read is not invalidity.

2. **invalidate / unconfirm**

   Reaper action `set-finding-status`, not an LLM edit step. It updates
   the status fields:

   - `metadata.yaml`: `status: invalidated` or `status: unconfirmed`
   - `FINDING.md`: `**Status:** invalidated` or
     `**Status:** unconfirmed`

   When the finding is invalidated, the same deterministic reaper action
   also writes `invalidation.md` in the finding directory. The file
   records `research.analysis` and `research.invalid_evidence` so the
   negative result is reviewable without scraping JSONL logs.

   These steps are terminal on success and run when `target_artifact_dir`
   is set. Finding-directory targets use the target directory; prose
   targets use `--results` when supplied. Per-todo invalid research in
   the automatic series driver is recorded as a partial invalidation
   instead of a whole-finding status change. The reaper refuses to
   invalidate unless `research_status=invalid`,
   `invalid_evidence_kind` is `source_or_commit_evidence`, and
   `invalid_evidence.trim()` is non-empty. It refuses to mark
   unconfirmed unless `research_status=unconfirmed`. It does not inspect
   `invalid_evidence` or `analysis` prose to guess which transition is
   actionable.

   `confirmed_latent` is handled later by `publish-fix`, after a valid
   patch has landed. That transition is gated only on structured fields:
   `research.is_latent == true` and a single-component finding/series
   plan. A latent todo inside a multi-component finding is not enough to
   rewrite the whole finding status; any reachable component keeps the
   finding-level status unchanged. The runner does not inspect wording in
   the commit message, generated patch, or research prose.

3. **write-patch**

   The code agent receives `research.affected_files`,
   `research.affected_symbols`, and `research.analysis`. If this is a
   retry after build or review failure, it also sees the preserved
   compile-triage or review feedback.

   This step normally emits only the patch plus a build target:

   - `code_edits`, or non-message `code_output` for large rewrites.
   - `build_target`, such as `drivers/example/example_drv.o`.
   - `review_dispute=""`.

   On retry after review, the patch author may instead dispute a review
   defect. This is only for a prior `source_defects[]` item that is
   already satisfied by the current patch. In that case `write-patch`
   emits no source edits, leaves `code_changes_emitted=false`, and sets
   `review_dispute` to an evidence-based explanation of why the review
   concern is invalid. It must not use this path for build failures,
   missing evidence, or fresh patch writing.

   `code_edits` should use `file_path`, `old_string`, and `new_string`.
   Each `old_string` must be drawn from the current worktree bytes and
   match exactly once unless `replace_all=true` is intentionally meant to
   update every matching occurrence. The runner accepts `path` and
   `filename` as compatibility aliases for `file_path`, but the prompt
   tells the model to use `file_path` so it does not confuse edit records
   with source-symbol records.

   For affected files, retry attempts must use `read` against the current
   worktree bytes. `source` results may come from a symbol index that was
   built before the patch landed, so it is only appropriate for unchanged
   callees or background context. On branch-back from compile triage or
   review, the runner inlines a read-only payload labeled as the exact
   output from `git diff HEAD~1` for the patch being corrected. The
   payload is mechanically prefixed line-by-line as inert
   `KRES-READONLY|` data rather than wrapped in Markdown fences, so raw
   diff contents cannot terminate the block or become prompt
   instructions. The prompt tells the model not to regenerate,
   summarize, quote, or echo that diff; it is comparison context for
   producing the next `code_edits`. The model is still told to read
   current worktree bytes before editing.

   It must not write `.kres-commit-msg.tmp` and must not run git.

   If an attempted edit's `old_string` is gone but the exact `new_string`
   is already present once, the runner treats the edit as already applied
   instead of failing the driver. This catches duplicate retry output and
   leaves the step eval to decide whether real new changes were produced.

   Machine-populated gates:

   - `changed_files`: source paths emitted by this write-patch attempt,
     excluding `.kres-commit-msg.tmp`.
   - `build_target`: object target for the first changed `.c`/`.S` path
     when the model did not provide one explicitly.
   - `code_changes_emitted`: true only when this attempt emitted a
     relevant source edit/output.
   - `affected_files_changed`: true only when `git status --porcelain --
     <changed_files>` reports a real worktree diff after edits are
     applied; unrelated dirty files cannot satisfy the gate.
   - `review_dispute`: empty for normal patch writes; non-empty only for
     a typed dispute of prior review feedback.
   - `review_dispute_allowed`: machine-populated true only when the step
     is correcting prior review `source_defects[]` and not a
     compile-triage patch error.

   The `fix_write_patch_output` builtin eval accepts either a real source
   change (both change booleans and an empty `review_dispute`) or a typed
   review dispute (`review_dispute_allowed=true`, `review_dispute`
   non-empty, and no emitted source changes). A no-op response with only
   `build_target` cannot advance, and a model cannot use
   `review_dispute` on a first patch attempt or build-failure retry.
   Missing `build_target` is valid
   for header-only, documentation-only, Kconfig-only, or other non-object
   changes; the deterministic build step skips cleanly when no enabled
   object target can be derived from the actual git diff. When the eval
   fails (no edits and no dispute), `write-patch` does not blind-repeat;
   `on_fail.action = branch_to orchestrator`, so the orchestrator sees
   the failed attempt in `prior_attempts` and decides whether to
   re-instruct `write-patch` (e.g. point at the specific source the prior
   attempt was missing, or correct a protocol mistake like a followup
   kind outside the step's actions allowlist) or to exit failure.
   `write-patch.max_attempts=12` is a backstop floor; the routing budget
   is owned by the orchestrator. `write-commit-message` uses the same
   shape: if `commit_message_written != true`, it branches to the
   orchestrator rather than retrying blindly.

   A review dispute does not publish or self-clear. Because the committed
   patch did not change, fixes-tag-search/commit/build/compile-triage are
   skipped for that pass. The prior fixes-tag-search outputs are
   preserved so provenance remains available if adjudication later routes
   to `write-commit-message`. The existing lensed `review` step then
   re-runs with the original
   `git diff HEAD~1`, the previous review output, and the patch author's
   dispute reasoning as read-only context. Review must either accept the
   dispute by setting `clean=true`, or reject it with a more precise typed
   defect.

4. **fixes-tag-search**

   A separate audit agent does only provenance research for the
   `Fixes:` tag. It runs after `write-patch`, before the commit message,
   so it receives the original bug context plus the actual patch under
   construction. On the first attempt, plain `git diff` is the full
   uncommitted patch. On retry after a build or review failure, a prior
   attempt may already be committed and the worktree diff may show only
   the incremental correction. In that case, the agent must inspect
   `git diff HEAD~1` for the full patch relative to the original base and
   also inspect `git diff` for the latest uncommitted correction. It must
   not base provenance on an incremental retry diff alone.

   The step runs at most once per fix todo. Later review/build cycles
   reuse the preserved `fixes-tag-search` output instead of repeating
   history research. Missing optional `Fixes:` metadata is not a reason to
   keep searching on every patch iteration.

   This step is intentionally stricter than a quick blame lookup.
   `git blame` can seed candidates, but the agent must inspect candidate
   diffs, follow moved/renamed code with `git log --follow`, and use
   pickaxe searches (`git log -S` / `git log -G`) for affected function
   names, helper calls, field names, missing cleanup calls, and
   distinctive surrounding code. It must prove the bug invariant changed
   across the chosen commit: the preimage lacked the bug or lacked the
   affected path, and the postimage contained the buggy state.

   Required output:

   - `analysis`: history search narrative and candidate evaluation.

   Optional outputs:

   - `fixes_sha`: 12+ character SHA only when the introducing commit is
     proven with kernel-review confidence.
   - `fixes_subject`: exact subject for the proven commit.
   - `fixes_evidence`: concise proof that the chosen commit introduced
     the buggy invariant.
   - `unproven_fixes_candidates`: plausible commits that were checked
     but not proven, formatted as `<sha> ("<subject>") - <why unproven>`.

   If certainty is not possible, the step must leave `fixes_sha` empty
   and preserve the best candidates in `unproven_fixes_candidates`. A
   wrong `Fixes:` trailer is worse than no trailer, but losing plausible
   provenance is also bad.

5. **write-commit-message**

   The code agent writes only `.kres-commit-msg.tmp` via `code_output`.
   It may inspect the patch with readonly git followups (`diff`, `show`,
   `log`, etc.), but it must not edit kernel source.

   On branch-back for commit-message review defects, the runner inlines
   read-only payloads containing the exact current commit message from
   `git log -1 --format=%B` and the exact current patch from
   `git diff HEAD~1`. The diff payload is current command output; in
   mixed source+message correction flows it may include uncommitted
   worktree changes on top of HEAD. Both payloads are mechanically
   prefixed line-by-line as inert `KRES-READONLY|` data. The prompt
   labels them as read-only comparison context and tells the model not
   to regenerate, summarize, quote, or echo them. The only requested
   artifact remains the rewritten `.kres-commit-msg.tmp` via
   `code_output`.

   The workflow passes an explicit `assisted_by` input into this step.
   By default this is derived from the resolved slow-agent model as
   `kres:<slow-model-id>`; `--assisted-by TEXT` overrides the exact
   value. The prompt requires the commit message to contain exactly
   `Assisted-by: <assisted_by>`. The review step treats that exact
   configured trailer as allowed; it may only report an `Assisted-by`
   defect when the trailer is missing, duplicated, or does not exactly
   match the configured value.

   Mutating git followups such as `git add` and `git commit` are rejected
   by the workflow fetcher. Commit/build/publish are deterministic reaper
   steps, not LLM actions.

   Every factual claim in the commit message must be provable from the
   patch or gathered source. In particular, leak/crash/refcount/race and
   cleanup claims should account for fallback cleanup paths before the
   message states them.

   In a fix series, the commit message is scoped to the current fix
   todo's patch only: before commit, the worktree/index diff against
   `HEAD`; after commit or during review rewrite, `git diff HEAD~1`.
   That parent already includes earlier todos in the series, so later
   commit messages must describe the bug and fix relative to that parent
   rather than the original pre-series tree. If a message needs to
   mention a sibling todo or earlier series commit, it must label that
   context explicitly as earlier/later series context and must not claim
   a sibling change is absent from, newly added by, or still needed in
   the current tree unless that is true for the current patch's
   parent-to-child diff. Stale pre-series snippets or ASCII traces for
   code already changed by an earlier commit should be omitted or
   explicitly labeled as pre-series history.

   The message should be a human-readable kernel changelog, not a proof
   memo. The expected shape is a small number of short paragraphs that
   explain the failing path, the consequence, the key mechanism, and the
   fix. When non-obvious control flow is the easiest way to understand
   the bug, the message should use a focused indented source snippet
   rather than a dense prose reconstruction of every branch. Scope and
   caller discussion should stay limited to what is needed to explain the
   bug or justify the fix.

   The commit-message step consumes `fixes-tag-search`, not opportunistic
   provenance from initial research. If `fixes_sha` is present, it emits a
   normal `Fixes: <sha> ("<subject>")` trailer. If no SHA is proven but
   `unproven_fixes_candidates` is non-empty, the final prose paragraph
   before trailers must start with:

   ```text
   Potential but unproven Fixes: tag candidates:
   ```

   followed by the candidate list. That paragraph is not a trailer and
   must appear before `Assisted-by:`.

   `commit_message_written` is machine-populated and true only when this
   attempt wrote a non-empty `.kres-commit-msg.tmp`. Stale files from
   previous runs or attempts are ignored.

6. **commit**

   Reaper action `commit-fix`, not an LLM action. It stages
   `write-patch.changed_files` first, falling back to
   `research.affected_files` only for older snapshots that do not have
   the machine-populated write-patch output. This keeps commit ownership
   deterministic even when research produced an incomplete file list.

   It runs:

   ```text
   git add -- <write-patch.changed_files>
   git commit -s -F .kres-commit-msg.tmp
   ```

   On retry attempts, it amends instead:

   ```text
   git commit --amend -s -F .kres-commit-msg.tmp
   ```

   This keeps final `HEAD` as one complete patch instead of a stack of
   incremental repair commits. The action returns `commit_sha`.

7. **build**

   Reaper action `make`. It runs:

   ```text
   make -j$(nproc) <write-patch.build_target> <other changed .c objects>
   ```

   The command is dispatched via argv, not through `bash -c`. Before
   running make, the reaper derives every changed `.c` file from the
   actual git diff (`HEAD~1..HEAD` after commit, or the worktree diff as a
   fallback), maps each to its `.o`, deduplicates with the model-supplied
   target, and then filters Kbuild-disabled objects for the current
   `.config`. For example, a file listed under
   `obj-$(CONFIG_FOO) += bar.o` is skipped when `CONFIG_FOO` is not set.
   This prevents compile verification from forcing objects that the
   configured kernel would not normally build. The model's `build_target`
   is therefore a hint, not the complete compile coverage boundary.

   If all targets are disabled by the current Kconfig, the build step
   returns `result=clean`, `exit_code=0`, and records the skipped targets
   in stdout / `skipped_targets`; there is no configured object to compile.

   Outputs:

   - `result`: `clean` or `failed`
   - `exit_code`
   - bounded `stdout`
   - bounded `stderr`
   - `skipped_targets`: objects intentionally not built because Kbuild
     disables them for the current `.config`

   A clean build skips compile triage entirely.

8. **compile-triage**

   LLM classification only, and only when `build.result == "failed"`.
   It does not edit, commit, or amend.

   Outputs:

   - `preexisting_error`: environment/pre-existing failure, such as a
     missing compiler, wrong config, or unrelated Kconfig fallout.
   - `patch_error`: failure caused by the patch.

   `patch_error` requires evidence from the actual patch hunks in
   `git diff HEAD~1`. An error merely appearing in a touched file is not
   enough, especially for objects that were skipped or forced outside the
   active Kconfig.

   `preexisting_error` lets the workflow continue to review.
   `patch_error` branches back to `write-patch`, preserving the triage
   analysis as feedback for the next patch attempt. Compile triage may
   branch back through patch writing ten times before exhausting.

9. **review**

   LLM review only. It reviews `git diff HEAD~1`, callee/source context,
   and the commit message. It runs parallel lenses so each reviewer gets
   its own context window:

   | lens | remit |
   |---|---|
   | `memory-lifetime` | allocation, ownership, refcounts, RCU, cleanup paths |
   | `bounds` | indexes, overflow, truncation, size arithmetic |
   | `races` | lock coverage, ordering, missed wakeups, shared state |
   | `assertions` | disprove every claim in the commit message and in comments the patch adds |
   | `maintainer` | antagonistic kernel-maintainer review; also gates changelog readability |
   | `bug-coverage` | does the patch fix the reported bug, per `metadata.bugs` |

   One shared gather, then N parallel calls. `maintainer` and
   `assertions` run every round: they audit the commit message and
   comments, so a changelog-only round is exactly when their subject
   changed. The other four skip a round that changed no source.

   The one supported shape for `/fix` review is the `review` step in
   `configs/workflows/fix.json` with a non-empty `lenses` array and
   `aggregate: "consolidate"`. Each lens emits the typed review contract:
   `clean`, `defects`, `source_defects[]`, `commit_message_defects[]`,
   `unresolved_risks[]`, `correction_step`, `outcomes[]`, and `analysis`.
   The runner validates each lens output as JSON shape (declared fields
   present, required fields non-empty); malformed or missing-field lens
   responses are retried with the JSON repair instruction (which carries
   the parse error for that lens by id). The runner does not validate
   cross-field invariants — those are the consolidator's job. Once every
   lens has produced a parseable structured output, the per-lens outputs
   are handed to the LLM consolidator (`consolidate.prompt`) which
   produces the single consolidated structured result for the step:
   `clean`, `correction_step`, deduped `defects[]`, `source_defects[]`,
   `commit_message_defects[]`, `unresolved_risks[]`, `outcomes[]`, and
   `analysis`. That consolidated output is the single source of truth
   for routing — no Rust-side override, no parallel deterministic
   consolidator. Prose in `analysis` is preserved for humans but is
   never interpreted by the runner as a routing or retry signal because
   the runner reads only typed fields. The JSON `consolidate.prompt` is
   the human-editable policy for how the consolidator should merge typed
   lens fields and pick the correction target. Do not add a second
   review step, a markdown prompt/template fallback, an in-prompt
   checklist of lenses, or a parallel deterministic consolidator. If
   fix review behavior is wrong, change this JSON lensed step.

   The maintainer lens is a hard quality gate. It aggressively looks for
   regressions, unnecessary churn, brittle assumptions, weak or overbroad
   fixes, hidden ABI/behavior changes, incomplete backportability, missing
   tests or validation, stale documentation for changed contracts, and
   places where the code agent has not justified why the exact change is
   the minimal correct fix. Maintainer objections become review defects
   when they identify correction requests or missing evidence needed
   before publication. If a patch changes accepted inputs, callback
   requirements, helper semantics, locking/lifetime rules, ordering,
   accounting, or any API contract while leaving matching docs/comments
   stale or misleading, the maintainer lens must set `clean=false`.

   The maintainer lens also enforces commit-message readability as a
   publication gate. A dense wall-of-text proof memo, a message that
   hides the problem and fix inside audit-style prose, unnecessary
   inventories of callers or negative cases, or prose that should be
   focused indented evidence blocks is a review defect. Evidence blocks
   may be call chains, ASCII call graphs, CPU timelines, before/after
   state blocks, short case analyses, numeric examples, or source
   snippets. Call chains and call graphs are preferred over prose for
   multi-function control flow. Diagrams must use ASCII only and must
   never use boxes. When that is the only defect, review routes back to
   `write-commit-message`;
   when it is mixed with source or behavior defects, review routes back
   to `write-patch`.

   The configured workflow `Assisted-by: <assisted_by>` trailer is not a
   maintainer defect. Review may still reject missing, duplicate, or
   mismatched `Assisted-by` trailers through the structured defect fields.

   The commit assertions lens runs only for committed patch review. It
   tries to disprove every factual sentence, causal claim, safety claim,
   scope claim, negative claim, and justification in the commit message,
   plus every comment added or modified by the commit. It must also audit
   existing declarations, kerneldoc, comments, and docs that describe any
   contract changed by the patch. Unsupported, contradicted, overbroad,
   unverifiable, or newly stale assertions are review defects.

   Review must read current worktree hunks/functions for changed files
   with `read`. `source` is allowed for callees and unchanged symbols, but
   it is not authoritative for changed functions because symbol indexes
   can return stale pre-patch bodies.

   Review must also prove every factual claim in the commit message. If a
   message claims a leak, crash, refcount pin, race, cleanup omission, or
   API contract violation, the review should trace the relevant code and
   fallback paths. Overstated or contradicted commit-message claims are
   defects even when the patch itself is mechanically correct.

   Review must not treat documentation as evidence only. When the patch
   changes a contract, the matching declarations, kerneldoc, comments, and
   docs are part of the reviewed surface. A runtime fix with stale contract
   documentation is not clean.

   Outputs:

   - `clean=true` when the patch is acceptable.
   - `clean=false` plus `defects[]` when it finds a problem.
   - `source_defects[]`: only defects requiring source, build, behavior,
     documentation, comment, test, or validation changes. This is the only
     review feedback passed to `write-patch`.
   - `commit_message_defects[]`: only commit-message/trailer defects. This
     is the only review feedback passed to `write-commit-message`.
   - `defects[]`: the schema-compatible mirror of the split defects. It must
     not contain extra defects absent from both split arrays when the lens
     uses split routing.
   - `correction_step`: typed enum, `write-patch` for
     source/build/behavior defects, or `write-commit-message` only when
     every defect is solely about the commit message/trailers.
   - `analysis`

   It does not edit, commit, or amend. Defects branch to
   `correction_step`, preserving the review output as feedback. This is
   deliberately not hardwired to `write-patch`: a review that proves the
   code is correct but the commit message is false must route to
   `write-commit-message`, then the deterministic commit step amends.
   Mixed source and commit-message defects route to `write-patch`.
   Commit-message defects are not included in the `write-patch` prompt, so
   source-patch retries cannot burn attempts trying to fix changelog text.
   Review may branch back through correction and re-review ten times before
   exhausting; commit-message rewriting gets six eval attempts per visit.

10. **publish**

   Reaper action `publish-fix`, only for standalone/non-series execution when
   `target_artifact_dir` is set
   and review is clean. It runs `git format-patch -1 --stdout HEAD`,
   writes the current fix's patch into the artifact directory, records
   the patch name under `auto_generated_fixes:` in `metadata.yaml`
   idempotently, and adds a cross-link in `summary.md`. Single-fix runs use
   `auto-generated-fix.diff`; series runs use `auto-generated-fix.diff`,
   `auto-generated-fix-2.diff`, and so on. A successful publish also
   deletes stale `invalidation.md` and `partial-invalidation.md` files
   from the artifact directory, because the current run has proven and
   published a valid fix. When `research.is_latent == true` and the
   structured plan says the finding has only one component, publish also
   sets `metadata.yaml`, `FINDING.md`, and the `summary.md` status section
   (when present) to `confirmed_latent`.

11. **series-assessment**

   During planning, the runner snapshots mandatory `metadata.yaml` and
   `FINDING.md` plus optional `summary.md` into the persisted fix-series state.
   Each opened artifact must resolve inside the finding directory. The
   primary-slow final assessment reviews that immutable snapshot with the revisioned series state, complete
   commit sequence, and current source. It does not judge only `HEAD~1`. The series is
   successful only when every planned todo is done and every original bug
   component is fixed, invalidated with evidence, or matched to a proven
   upstream duplicate. Deferred or unexamined components fail the gate.

   The typed decision is `complete`, `revise_pending_plan`, `unconfirmed`, or
   `failure`. A revision decision must carry a stale-checked plan update made
   only of `append_after_current` operations. Rust validates and persists the
   new todos, runs them through the normal fix pipeline, and repeats the final
   assessment. An `unconfirmed` result gets up to three total attempts,
   preserving typed prior outputs and gathered source only across its immediate
   repeat so the next gather pass can satisfy exact evidence requests. A typed
   `failure` decision terminates immediately. Eval and driver-error exhaustion
   use the same required terminal-snapshot path, so resume cannot grant another
   attempt. Resume refreshes the snapshot's workflow inputs from the current
   outer-series state. Accepted source edits invalidate gathered source before
   dependent steps run. A builtin validator requires outcomes
   to cover the authoritative todo IDs exactly once, with a valid disposition
   and non-empty evidence. Complete decisions also require empty remaining work
   and prohibit unresolved outcomes.

12. **final-record-results / final-publish**

   Series runs blank `target_artifact_dir` for every per-todo execution, so the
   ordinary record/publish steps cannot mutate finding success artifacts before
   the final gate. After `series-assessment.complete=true`, these JSON-defined
   reaper steps write the assessor's final per-bug outcomes and format the exact
   persisted commit list, oldest first. Publication is therefore downstream of
   whole-series completion rather than downstream of each local review.

### Fix Series

The `/fix` workflow starts with a planning/status pass when no
`current_fix_todo` is already present in workflow inputs. That pass runs
the JSON workflow's `research`, `invalidate`, and `unconfirm` steps. If
research is invalid or unconfirmed, the pass updates finding status when
an artifact directory is available and stops. If research is confirmed,
it must emit `research.fix_plan`, an ordered array of independently
committable todos.

Planning uses one todo only when the finding has one coherent failure
mode and one coherent patch/review surface. It must use multiple todos
when the input describes multiple independently triggerable failures,
multiple affected sites with different fix contracts, or changes that
should be reviewed, retried, built, and published independently. A todo
does not need to fix the entire finding by itself: series commits may be
complementary. If one fix depends on another to compile, be safe, or
make semantic sense, that ordering belongs in `depends_on`; it is not a
reason to merge independent failure modes into one commit.

Rust owns that array as the series todo list and runs the full JSON fix
workflow once per todo, in order. Rust parses the plan into typed todo
records, rejects duplicate IDs, empty core fields, and dependencies that
do not point to earlier todos, and tracks each todo as
`Pending -> InProgress -> Done|Failed`. Each per-todo run receives the
full `fix_series_plan`, the selected `current_fix_todo`, one-based
`fix_index`, and `fix_run_mode=todo` in workflow inputs. The planning
pass uses `fix_run_mode=planning`.

The outer snapshot also records the pre-series HEAD, completed commit SHA for
each todo, and each todo's typed review outcomes. Successful todo commit
identity is read from the repository's current HEAD, including when an accepted
review dispute skipped the commit step. Inner snapshot directories
include todo identity, outer plan revision, and todo revision. Before final
assessment, Rust verifies that the persisted commits are an exact parent chain
from the recorded base through current HEAD.

On resume, an absent outer snapshot falls back to the durable planning snapshot
instead of failing. Before each pending todo starts, Rust verifies workspace
HEAD against the completed commit prefix. It accepts a direct child only when
the matching inner snapshot records that commit or an accepted review-dispute
path, covering the crash window between inner commit persistence and outer-state
reconciliation.

Planning records an immutable `original_bugs` inventory separately from the
commit-oriented todo graph. Existing finding `metadata.bugs` is authoritative;
prose findings receive the planning model's typed `bug_inventory`. Every
outer-state reconciliation persists `fix-series.json` and rewrites
`metadata.bugs` from that immutable inventory. Structural revisions, splits,
removals, final appended work, and resume cannot change original bug identity.

Per-todo research must stay confirmed to reach patch writing. If the bug
is still proven but the current todo's fix contract is wrong or
incomplete, research returns `unconfirmed` with
`research_decision.bug_proven=true`,
`fix_contract_proven=false`, and `invalidity_proven=false`, plus a typed
`fix_plan` containing a revised version of the current todo with the
same `id`. Rust validates that replacement todo, updates the in-memory
series plan, and reruns the same todo. Each todo has a small revision
budget so a bad contract cannot loop forever.

When new evidence requires a structural change, per-todo research may instead
emit a revision-checked `plan_update`. Its `expected_revision` must equal the
persisted outer revision. The supported atomic operations replace or split the
current todo, append work immediately after it, revise later pending work, or
remove later pending work. Completed work is immutable, and Rust validates the
entire resulting ordered dependency graph before committing the update. A
stale, partial, empty, or invalid update fails without changing series state.

If research returns
invalid, unconfirmed without a usable revised todo, or exhausts the
revision budget, the workflow fails that todo instead of marking the
whole finding invalid/unconfirmed. For invalid per-todo research with
actionable source/commit evidence, Rust writes or appends
`partial-invalidation.md` in the finding directory with the todo id,
todo title, research analysis, and invalid evidence.

Each todo is treated as its own finding for retry and review purposes:
step attempts, eval failures, compile triage, review branch-backs,
commit-message rewrites, commit/amend state, and publish are scoped to
that per-todo workflow run. Later todos see the already-committed earlier
fixes in workspace history, but patch-writing is instructed to edit only
the current todo's scope.

Top-level fix series runs persist planning and every todo/revision in distinct
`workflow-state` subdirectories. The outer driver atomically stores the
revisioned todo graph and statuses in `fix-series.json`. On `--resume`, an
interrupted outer `InProgress` item becomes `Pending`, then its matching inner
workflow snapshot resumes so already-settled edits, commits, builds, and review
steps are not repeated. Completed outer todos are skipped. The final
`series-assessment` has its own resumable inner snapshot as well.

### One reconciled instruction set, not N lens verdicts

The review lenses run as a parallel fan-out over one shared gather.
Each is told the OTHER lenses exist — `parallel_lenses` carries
`lens_identity`, which is `{type, name, id, reason}` — and none is
shown a word of what another concluded. The consolidator that merges
them does not adjudicate either: its rules are dedup, promote
prose-only bugs, preserve unresolved work, and take the HIGHEST
severity when lenses disagree. Every one of those is additive, so two
lenses demanding incompatible things both survive into `defects[]`.

`reconcile-review` is the step that resolves them. It runs after
`review` and before `orchestrator`, on the slow coding agent, with
`actions: []` — it adjudicates the lenses' own evidence and must not
re-fetch. It receives:

- `review.lens_reports`, a driver-owned copy of each lens's structured
  output, unmerged. This is the only place a contradiction is visible;
  the consolidated view has already flattened it.
- the lenses' shared gather, seeded through `depends_on: ["review"]`
  now that the structured lens path calls `store_gathered`.
- a Rust-rendered NUMBERED REVIEW DEFECTS block plus the current
  patch. Rust owns the numbering for the same reason it owns the todo
  list: if the model restated the defect set it would also be choosing
  what the set contains, and a defect it declined to restate would be
  indistinguishable from one it resolved. The patch is best-effort —
  when `git diff HEAD~1` fails the block says so rather than failing
  the step, because the lens reports quote their own evidence.

It emits:

| output | meaning |
|---|---|
| `instructions[]` | the single worklist: `target` (source / commit_message), `kind` (behavior / documentation / changelog), `where`, `do`, `why` |
| `contradictions[]` | which lens demands conflicted, which one the patch follows, and why |
| `dropped[]` | defects deliberately not carried forward, with evidence or the scope statement |
| `objectives[]` | see below |
| `must_fix` | the one objective the loop is now dedicated to |
| `scope_amendment` | a change to the fix contract itself |

`write-patch`, `write-commit-message` and the orchestrator all read the
reconciled set as authoritative, with the raw per-lens defects kept as
context.

Nothing is silently dropped: every numbered defect must appear in some
`instructions[].covers` or `dropped[].covers`, in exactly one of them.
An instruction may cover no defect — extra work is allowed — but a
`dropped` entry naming no defect is refused.

Worked example, from the run that fixed
`nfsd_file_free_null_mapping_deref`: `assertions` said the new comment's
claims were unproven; `maintainer` said adding checks to prove them was
scope creep on a stable-tagged fix. Both are satisfiable one way —
delete the surplus and narrow the claim. The recorded resolution: *"No
may_open() call is added. The comment is reworded to the narrower true
claim."*

`reconcile-review` also owns the loop's **objectives**, which are its
only cross-round memory and the thing that lets it steer rather than
just report.

A defect is one round's wording. An objective is one sentence about what
must become true of the patch, carrying a stable id across rounds
however differently the review words the complaint:

```text
round 1   O1 open       "the comment must not claim parity with the open path"
round 2   O1 satisfied  evidence: fs/namei.c comment now reads "..."
```

The pass re-emits every objective each round and
marks it `open`, `satisfied` (with evidence) or `withdrawn` (with the
scope statement or the source that disproves it). Omission does not
retire one: an objective the pass goes quiet about is carried forward
unchanged, the same rule the todo list uses.

**Rust owns the ages.** `merge_emitted_objectives` stamps `first_round`
when an id is first seen and recomputes `rounds_open` every round, so a
pass cannot reset a stale objective's clock by renaming it or by
reporting its age itself. They live in the synthetic `objectives` step
(`OBJECTIVES_STEP_ID`), which no workflow lists in `depends_on` and so
nothing resets — every real step's outputs are taken into
`prior_attempts` and cleared when the orchestrator routes backwards.

**Everything a worker reads lives in the store, not on the step.**
`reset_dependents_preserving` preserves the outputs of the *branching*
step only — which is why `orchestrator.instruction` reaches a worker
and `reconcile-review.instructions` does not. The reconciliation pass
is a transitive dependent of `write-patch` (write-patch → commit →
build → review → reconcile-review), so branching back to the worker
takes its outputs into `prior_attempts` and clears them. `must_fix`,
`scope_amendment`, `instructions`, `contradictions` and `dropped` are
therefore copied into the synthetic step by `merge_emitted_objectives`
and referenced downstream as `{{objectives.*}}`. Referencing
`{{reconcile-review.*}}` from a worker renders empty: measured on the
2026-08-10 linux.nfs run, all eight `write-patch` prompts carried the
reconciled instruction block with an empty array, so the step was
writing into the void for the whole run.

**The forward-progress rule.** Once an objective has been open for
`OBJECTIVE_STALE_ROUNDS` (2) cycles, asking again in different words
has already failed twice. `stale_objectives_are_escalated` requires the pass to
either name one stale objective in `must_fix` — which tells the review
to check it first, tells both workers nothing else matters, and stops
the orchestrator publishing while it is open — or settle it as
satisfied or withdrawn on evidence. `scope_amendment` is the third
lever: when the review keeps demanding work the fix todo does not
cover, the pass narrows the contract, and when the fix cannot be
correct without work the todo omitted, it widens it.

This replaced an earlier guard that policed the WORDING of each
instruction — a `kind` enum plus a `behavior_accepted` certification,
with an eval rejecting any defect answered only by a comment rewrite.
That was rigid, and measuring whether the ask *worked* subsumes it: an
objective still open after two rounds trips the rule no matter how its
instruction was phrased. `kind` survives as a hint to the worker and is
no longer eval-enforced.

The evidence for all of it is the 2026-08-10 linux.nfs pair. Run one,
before the reconciliation step existed, failed at ten rounds but its
patch called `may_open()`. Run two, with the step, also failed at ten
rounds and its patch did not: the review raised the missing permission
check in all ten rounds, and of the fifteen source instructions the
step emitted across nine rounds every one was a comment or kerneldoc
rewrite. Coverage was satisfied every round, because documenting a
defect cites its index exactly as well as fixing it does.

The reason the step could not tell round one from round nine is that
its memory did not exist. `review_ledger` — a separate fast-agent pass
that was supposed to merge semantically identical complaints across
rounds — never ran once. Entries could only be born from review
defects, but `update_accepted_review_ledger` was reached only on eval
PASS and the review's eval is `clean == true`, which fails on exactly
the rounds that have defects; on a clean round there is nothing to
record. Its fallback writers were gated on `review.source_defects`,
which `reset_dependents_preserving` has already cleared by the time
they run. Measured across three fix runs including one that succeeded,
`phase=review-ledger` appears zero times, so four underlying problems
presented as 37 differently-worded defect strings and nothing noticed
they were four. That mechanism is deleted; the reconciliation pass owns
the store now, and writes it with no extra LLM call.

### How a Fix Run Ends

- **The review returns clean.** The intended gate.
- **The orchestrator publishes anyway**, citing evidence that the
  remaining defects are answerable. `publish` fires on
  `review.clean == true || orchestrator.next_step == 'publish'`.
- **`review` exhausts 10 rounds** → `exit_failure`, with a working patch
  left in the tree.
- **`orchestrator` exhausts 10 attempts** → `exit_failure`. This means it
  kept routing without either worker making progress. A worker whose
  `run_if` is false is silently skipped and control returns to the
  orchestrator, so a closed gate shows up as the orchestrator spinning
  rather than as a skipped step.

The review behaves more like an unbounded complaint generator than a
condition that closes on its own. What ends a healthy run is the
reconciliation settling every objective, or the orchestrator judging the
remainder answerable.

The 2026-08-10 20:07 run on `nfsd_file_free_null_mapping_deref`, a
finding that had failed three complete runs before it:

```text
review 1:  clean=False  defects=3
review 2:  clean=False  defects=0
review 3:  clean=True             ->  [fix series] done 1/1
```

Final patch 22 insertions in one file; the best previous attempt was 62
insertions over ten amends and still unfixed. Round 1 resolved the
scope-vs-proof contradiction and set a scope amendment. Round 2 settled
three objectives on cited evidence and added one. The orchestrator routed
on the reconciled set — *"exactly one live defect, and it is a
commit_message_defect"* — not on raw defect counts.

### Known Limits of the Fix Loop

1. **`must_fix` has never fired in a converging run.** The rule refuses a
   stale objective that is neither named nor settled, but in the run that
   converged the objectives were settled instead. Whether naming one
   unsticks a stuck loop is unobserved.
2. **`must_fix` forces commitment, not resolution.** A pass may name the
   same objective every round; only the turn cap bounds that.
3. **Id stability is the model's job.** Rust does not compare statements,
   so a fresh id for an old complaint resets its age.
4. **An exhausted reconciliation loses its round.** The merge runs only
   on eval pass, so those objectives never persist — and the previous
   round's state stays in the store, read by later steps as current.
5. **A behaviour defect routed to `target: commit_message`** is caught by
   no rule.
6. **One failed lens fails the whole review.** The fix review has no
   partial-failure tolerance; a transport error on one of six discards
   the other five. (`/review` is different: see `run_with_lenses`.)
7. **The review does not remember what it accepted.** It re-reviews the
   whole patch each round; the objectives store dedups the reconciler's
   view of the complaints, not the review's own output.
8. **Lens accuracy is not structural.** The nfs patch shipped with an
   inexact claim in a comment it adds: it says the new `vfs_open()` call
   matches "the vfs_create()/vfs_open() pair in the else arm", but
   `may_create_dentry()` returns `-EEXIST` for a positive dentry, so the
   else arm can never reach a pre-existing inode while this path can.
   `assertions` read that text twice and passed it.

### Fix Flow Invariants

- `[INVALID]` after research requires evidence that existed before the
  run started. Once code edits or commits land, reading the workspace
  back and seeing the fix is not upstream prior art.
- Missing optional Fixes metadata must not block the run. A wrong
  `Fixes:` trailer is worse than no trailer.
- The core loop does not require `bash`.
- `make` followups and workflow build actions are dispatched as argv,
  not through a shell.
- Mutating git followups emitted by LLM steps are rejected; reaper steps
  own `git add`, `git commit`, `make`, and `publish-fix`.
- The build step compiles all changed `.c` objects derived from the git
  diff, not only the model-supplied primary target, but skips objects that
  Kbuild disables for the current `.config`.
- Branch-back retries happen against the current workspace/commit. A
  duplicate edit whose replacement is already present is a no-op, not a
  fatal driver error; genuinely stale edits still fail.
- Compile and review never mutate the patch directly. They classify or
  report defects and branch back to `write-patch`.

### Workflow Reports

`write_workflow_artefacts` writes `report.md` with both human analyses
and a `Workflow trace` section. The trace records step starts, skips,
produced outputs, eval pass/fail events, branchbacks, build results, and
publish status. This is intentionally redundant with JSONL logs: the
report should be enough to audit whether the fix was patched, committed,
built, reviewed, and published without replaying the raw logs.

## Review Flow (`/review`)

`/review <target>` and `--prompt "review: <target>"` dispatch the
embedded `configs/workflows/review.json` workflow. There is no markdown
prompt path for review.

The shipped workflow is one lensed step named `investigate`. The lenses
and lens conditions are defined in `configs/workflows/review.json`;
Rust maps those JSON records into `LensSpec` and must not keep a
separate hardcoded review lens list. `/review <target>` and
`--prompt "review: <target>"` are only two entry points into that same
workflow-defined task loop. It uses the optimized lens path:

1. For a named source-file review, a Rust bootstrap uses `gix` to follow target
   renames and build one target-file diff from immediately before the oldest relevant
   change in the six-month window to the current working-tree file, including dirty
   edits. The primary slow agent assesses
   that net diff with low reasoning effort, judging the final code rather than retaining
   risks fixed later in the window. If the combined diff and current target source are
   large, kres partitions the target-file diff at hunk/line boundaries. Each diff
   chunk is assessed with the complete current target source when that fits the
   provider capability. For an independently large source, source scopes are crossed
   with the diff chunks so no ordinal source/diff pairing can hide a relationship.
   The calls run in parallel at low effort. Results are restored to deterministic
   source/diff order; an oversized set of complete typed reports is reduced through
   semantic batches before the final low-effort call reconciles later fixes,
   contradictions, and distinct evidence. No serialized report is split. Before the
   structural inventory exists this report may be sparse; after inventory, any missing
   authoritative function forces a corrective inference pass. Rust never fabricates a
   zero rating from an omitted function.
   It never takes a per-chunk maximum. The completed assessment is atomically written to `change-survey.json`
   beside `session.json`, keyed by target source content and mode, baseline, and working-tree endpoint. An explicit
   `--resume` reuses a matching assessment; fresh runs overwrite old state, and `/clear`
   deletes it. Major risks in functions outside the target are retained only as research
   candidates. The bootstrap then requests
   semcode's compact Tree-sitter `file_survey` exactly once. If semcode is unavailable,
   preserved local grep evidence plus lossless target-source partitions go through typed slow-agent
   fallback inventory calls when the source is large; Rust unions structural names and recomputes
   whole-file use counts. The bootstrap checks the net-diff response against the
   authoritative file-survey function set. Unknown target-function names trigger one
   corrective exact assessment; missing names are rejected until inference supplies a
   rating. It sends that inventory plus the compact net-change ratings to one
   non-lensed slow call. The file survey output contains one combined 0-100 risk rating for every defined
   function and one final file risk rating. Rust rejects a combined rating below its
   net-change rating or a file rating below its highest function rating. It converts
   every external risk into exactly one prioritized research question only when the structural
   call inventory or a code-level function-value reference in the target shows an interaction.
   Comments, string literals, and declarations do not establish an interaction. Rust rejects empty, missing,
   duplicate, or unrelated questions. The final file-survey synthesis is retried on malformed or
   semantically incomplete output; if it remains invalid, review bootstrap stops instead of
   silently continuing without the scan. That ranking is supplied to `define_goal` and
   `define_plan`, so the initial semantic groups are source-informed. It is also cached
   separately with its target, source hash, baseline, and head in
   `SessionState.review_file_scan` for later tasks and resume;
   `Plan::prompt` retains only the operator prompt. Model-facing task plans are
   compact projections containing the goal, mode, and current steps, not the
   immutable prompt or creation timestamp. The scan is injected once into each
   task request whose target matches it, and resume restores the dedicated scan
   cache without regenerating the survey. The typed scan state is part of
   session schema version 3; stale source or revision fingerprints are discarded on
   resume, and plan prose is never parsed to recover it. Scheduled tasks
   cannot request another survey; they gather targeted source, types, callers, grep
   results, and history before parallel slow review lenses run.
2. The active slow-agent review lenses run in parallel.
3. A fast consolidator merges and ranks the results.
4. The lensed step completes with `findings` and typed `followups` in
   its output.
5. The normal kres task reaper sends those followups to the todo agent,
   which deduplicates them against completed work and the current
   plan. Surviving followups become todos. At dispatch the
   prioritization agent ranks the runnable rows and picks the batch,
   and each picked todo runs as a fresh review task through the same
   JSON-defined lenses. If a lensed review task emits typed
   followups and the todo agent fails to keep any pending/blocked next
   work, the reaper restores those followups as pending todos instead
   of letting a narrow goal check terminate the run.

The parallel slow calls are the important part of `/review`; keep them
unless the operator explicitly chooses a cheaper custom workflow.

Source gathered for those calls has one canonical representation. A single
parseable semcode result becomes a normalized symbol and its duplicate raw body
is omitted. Ambiguous semcode results remain raw so the agent can choose the
candidate. Empty, failed, or unparseable results remain visible and trigger the
local grep/read fallback. Local grep match lists are never semantically filtered
or expanded wholesale by Rust. Prompt evidence receives a stable exact-content
ID and ordering; repeated exact entries are removed, but distinct match lists,
line ranges, and errors are retained. Parallel lenses share the resulting
canonical evidence as one cacheable prefix and still all receive the concrete
source needed for exhaustive review.

Skill routing keeps the subsystem index available to the fast gather agent.
After it has selected a concrete subsystem guide, slow synthesis receives that
guide verbatim with the stable skill scaffold and technical-pattern guides, but
does not receive the now-redundant routing index. Existing findings sent back to
task agents retain IDs, status, severity, summaries, relationships, and source
locations while omitting repeated embedded source bodies. Agents request any
source body they need through typed followups; canonical findings storage and
consolidation retain the full records.

Review prompts carry a resolved target kind. A source file or directory means the current
workspace contents and does not imply a revision, base, or diff. Only a target classified
as a git commit/range starts from `git show` or `git diff`; source reviews may request
targeted history later when a concrete semantic question requires it.

Multiple slow selectors have supplemental semantics by default. The first
model runs every active lens. Without `--compare`, each additional model runs
only the workflow's supplemental lens: `general` for `/review` and
`maintainer` for `/fix`. For five active `/review` lenses, two models therefore
make six slow calls after the shared gather. The selectors may come from
`models.slow` plus optional `models.slow_secondary`, or from repeated or
comma-separated `--slow`; explicit CLI selectors replace the configured pair.

`--compare` opts into the full Cartesian fan-out. Each active lens prompt is
sent to every selected slow model, so five active lenses and two models make
ten slow calls. Every per-lens output sent to the consolidator is tagged with
`slow_model`; the consolidator compares sibling outputs, keeps the best
evidence and Findings while deduplicating across models and lenses, and records
one entry per completed review turn in `<results>/comparison.json`.

Every selector uses the normal resolution rules: `sonnet` and `opus` consult
`settings.json:model_aliases` before their built-in fallbacks, and ambiguous
model ids must be qualified as `provider.json:model-id`.

### Review Lenses

- `memory-lifetime`: allocation and initialization, publication, pointer
  ownership, refcounts, RCU grace periods, asynchronous use, cleanup paths,
  callback ownership, object layout, free ordering, leaks, use-after-free,
  double-free, uninitialized memory, and allocator API misuse. It no
  longer defers index/range/arithmetic errors to `bounds`: that clause
  went when the guard procedure was folded in, on the rule that a lens
  is never told to stay quiet about a bug it found. The consolidator
  dedups overlap.

  This lens also carries the guard procedure. The TEST/WINDOW/USE
  definition ships as `configs/workflows/guards.md`, pulled into the
  review step's shared prompt via `@configs/workflows/guards.md` in
  `include`, so every lens reads it once from the cached prefix;
  `read_at_path` resolves includes against the RUN's cwd, so guards.md is
  also registered in `embedded_workflow_include` and travels in the
  binary. The lens names the state TEST read, the fact it establishes,
  the USE it protects, what in the WINDOW changes that state, and the
  ACTUAL requirement USE has of it.

  It rides with lifetime rather than standing alone because the defect is
  the same object seen twice. On the 2026-08-19 arch/x86/kvm/mmu reviews a
  standalone guards lens repeatedly reached the right window and cleared
  it — "does not drop mmu_lock, so the fact still holds at USE" — while
  memory-lifetime was independently reading `kvm_mmu_child_role()` and
  correctly reasoning that it propagates `role.direct`, never asking what
  else the same role word carries. An object whose memory is never freed
  can still have the property a guard checked about it go false, and can
  carry that false property into whatever it is used to build or publish;
  that is a lifetime question about a property rather than about storage.

- `bounds`: array/index correctness, trusted versus untrusted indexes,
  integer overflow, truncation, and size calculation mistakes.
- `races`: lock coverage, ordering, missed wakeups, shared-state races,
  and protection of accessed fields.
- `general`: NULL dereferences, missing error checks, semantic bugs, and
  concrete defects not covered by the other lenses.
- `assertions`: commit/range reviews only. Disprove every assertion in
  the commit message, every comment added or modified by the commit, and
  existing declarations, kerneldoc, comments, or docs that describe any
  contract changed by the commit; unsupported, contradicted, overbroad,
  unverifiable, or newly stale claims become Findings or typed followups.

Each lens emits `analysis`, `findings`, and typed `followups` for its
own review pass. The golden review contract is deliberately exhaustive:
find every concrete bug involving the target, do not stop after the
first issue, and use followups as the normal next frontier when more
source, type definitions, callers, history, or API semantics would materially improve
confidence. `analysis` is the audit narrative, while `findings` contains
the concrete bugs. Findings must be full kres `Finding` records, not
simplified review notes. The parser drops partial objects that do not
deserialize as `kres_core::findings::Finding` from the accepted findings list,
so `{file, what, severity}` is not a valid review finding shape. It retains the
raw rejected object separately for the repair path described below.

Malformed full-Finding attempts are retained with their raw JSON and exact
serde error rather than silently discarded. When the id matches an existing
finding, kres first overlays the supplied JSON fields on that stored record and
deserializes the result. This deterministically supplies fields omitted from an
update without changing fields the agent explicitly provided or spending a
formatting-agent call. New ids and existing-id updates that remain invalid then
receive one strict-schema repair attempt. The complete replacement response
must pass the same derived serde contract. A record that remains invalid is preserved in
`report.md` and `findings.json.task_prose`, and a blocking typed retry followup
is added; no missing line number or other evidence is fabricated.

Every followup must include non-empty `type`, `name`, and `reason` fields. The
generated JSON Schema and the post-parse semantic validator enforce the same
requirement; `reason` is not an optional legacy field.

For commit/range reviews, the target is the semantic change, not only
the edited lines. The gather pass and each lens must identify changed
contracts such as object layout, type/union interpretation, enum
selectors, ops tables, callback targets, helper-family migrations,
allocation/freeing rules, lifetime/refcount rules, lock rules,
accounting/visibility contracts, and ordering guarantees. They must
then trace unchanged readers, writers, callers, callees, callbacks,
setup/registration sites, shared helpers, and history that still rely on
the old contract. This is load-bearing: regressions often sit in
unchanged chains after a patch flips how an object is allocated,
advertised, dispatched, accounted, or freed. If those paths are not in
context, the lens must emit followups instead of calling the review
clean. This rule is generic: follow the changed contract rather than
hardcoding subsystem-specific examples.

Negative coverage claims must be backed by gathered evidence. A review
turn may not declare "no remaining users", "all callers updated", "old
path unreachable", "only reader", "only writer", or equivalent unless
the analysis cites concrete source, type, search, caller/callee, or history
results proving that claim. If that evidence is missing, the lens or
consolidator must emit a typed followup for the exact proof needed, the
todo agent must keep the relevant trace step pending, and the goal judge
must not declare the review complete.

The canonical wire and storage fields are owned by
[findings-json-format.md](findings-json-format.md). Review strengthens that
storage contract: actionable findings must include status, sufficient embedded
source anchors, a cited summary, a concrete reproducer path, and impact.
Embedded source bodies should be minimal but sufficient to prove the bug.

A clean lens should still return JSON with a brief `analysis`, an empty
`findings` array, and an empty `followups` array, but only when the lens
is confident no bug of its class remains in scope for the target. A lens
is not clean merely because the initial gathered context did not prove a
bug. Lenses should not report style issues, vague design concerns, or
missing context as findings. A concrete bug must not live only in
`analysis`: if the lens describes a real bug or strong suspect with a
concrete code path, trigger, and impact, it must emit a full `Finding`.
If proof is incomplete, the Finding should carry `open_questions` and
the lens should also emit typed followups for the exact missing evidence.
If the current evidence is too thin for a Finding, emit only the
followup. If a concern was disproved, the analysis should say that
explicitly and keep both findings and followups empty for that concern.

### Review Consolidation

The consolidator emits one deduped `findings` array of full kres
`Finding` records. It receives each lens' structured findings, its
analysis text, and the `slow_model` that produced it. That analysis is
included even when the final workflow step declares only `findings`,
because otherwise prose-only bug claims from a lens would be invisible
at fan-in.

The optimized shared-gather review path still honors the workflow JSON:
`consolidate.prompt` is interpolated and appended to the built-in
consolidator instructions as workflow-specific rules. This matters
because the optimized path uses `Orchestrator::run_with_lenses()` rather
than the generic per-lens workflow fallback; both paths must enforce the
same schema, deduplication, and prose-audit rules.

That is an implementation split, not a configuration split. Shipped
workflow behavior must still come from the single JSON lensed step:
`review.json` for `/review`, and the `review` step in `fix.json` for
`/fix`.

The consolidator deduplicates findings that share a root cause, cite
overlapping code for the same defect, or describe the same failure mode
in different words. For merged findings it keeps the most specific `id`
and `title`, preserves the highest severity, unions
`relevant_symbols`, `relevant_file_sections`, `open_questions`, and
`related_finding_ids`, and preserves the clearest `summary`,
`reproducer_sketch`, `impact`, `mechanism_detail`, and `fix_sketch`.

After merging structured findings, the consolidator also audits the
per-lens analysis for concrete bug claims that were not represented by a
Finding. It may promote such a prose-only bug only when the analysis
already contains enough file/function/location evidence to fill the full
Finding schema without inventing facts. Thin hypotheses, areas-to-check,
and disproved concerns are dropped rather than promoted.

The reaper runs the same prose audit after each completed review task as
a finding-delta pass. That pass may append a prose-only Finding, emit a
same-id `status: invalidated` delta when later prose directly disproves
an existing Finding, or emit a same-id `reactivate: true` delta when
later prose proves an invalidated Finding real again. It must not
re-emit existing Findings for wording tweaks. The findings store also
performs a conservative deterministic duplicate merge for active
findings that share code anchors and near-identical identity tokens, so
minor id variants for the same bug do not remain separate active rows.

The consolidator drops non-bugs and sorts high severity first, then
medium, then low. It must not add ad hoc `file`, `what`, or `lenses`
fields as a substitute for the full Finding schema. It preserves and
deduplicates followups from all lenses. When lens prose contains a
concrete unresolved suspicion, the consolidator must either preserve or
promote a strong-suspect Finding with `open_questions`, or convert the
suspicion into a typed followup for the exact missing evidence. It must
not let unresolved work disappear as prose.

After consolidation, `/review` uses a local `field_check` eval:
`analysis != ''`. This is only a malformed/empty-output retry guard; it
must not inspect `followups` and must not keep the workflow step in a
local repeat loop. Typed followups are preserved in the completed task
summary, then the reaper sends them through the todo agent. That outer
todo loop is the review forward-progress mechanism: completed review
tasks produce findings plus followups, the todo agent chooses the next
frontier, and the scheduler dispatches fresh review tasks until the turn
cap or followup exhaustion.

This shape is deliberate. The golden review behavior kept context
bounded by carrying completed todo history, cached source/context, prior
findings, and a compact accumulated-analysis ledger rather than
injecting full previous source reads into every next prompt. Do not
replace it with workflow-local “fetch previous followups and repeat the
same step” logic. The runner also validates `findings` declared as
`array<Finding>` by deserializing the output into
`Vec<kres_core::findings::Finding>`; simplified objects fail before the
workflow can report success.

When `--prompt "review: ..."` is recognized on the CLI, kres builds the
initial review prompt and session lenses from `configs/workflows/review.json`,
then enters the normal REPL task/todo loop. `--turns N` therefore means N
completed review tasks, matching interactive review continuation. `/fix`,
`/triage`, and `/validate` still use the workflow executor directly
because their JSON steps own the full ordered pipeline.

The `--turns N` cap is a launch cap, not permission to drop active work.
When the cap is reached, the reaper drains only Pending/Blocked todos to
`/followup`, blocks auto-continue from dispatching fresh tasks, and waits
for already-active review tasks to finish and publish findings. It exits
only after `active_count == 0`. This matters for lensed/parallel review:
one completed task can hit the counter while sibling tasks are still
finishing, and those sibling outputs must still be merged into
`findings.json`, `report.md`, and session state.

Once a completed task reaches the cap, the reaper must not make
continuation-only LLM calls. It still merges the completed task output,
records any followups emitted by that task as Pending local todos, and
lets the cap drain move them to `/followup`. It skips todo-agent and
goal-agent calls because those only decide what to run next, and there is
no next work after the cap.

## Triage Flow (`/triage`)

`/triage <finding-dir>` and `--prompt "triage: <finding-dir>"`
dispatch `configs/workflows/triage.json`. There is no markdown
prompt path. The workflow reads the exported finding directory, writes
`summary.md`, and updates status files through workflow `code_output`
handling.

The JSON workflow includes the same `configs/prompts/triage-template.md`
body used by the golden slash-command prompt. The slow triage step emits
both the human `summary.md` artifact and a `triage_coding` JSON object for
downstream batch tooling. Rust consumes that structured JSON directly; it
does not infer status, priority, or routing from free-form agent prose.

The wrapper must preserve the old practical behavior:

- The step may gather the finding files plus readonly source/type/history
  context through `read`, `source`, `grep`, `git`, and `callers`.
- It emits `summary.md` through `code_output`. The summary includes the
  chosen severity and the rationale.
- It emits synchronized `metadata.yaml` and `FINDING.md` updates through
  `code_output` whenever status changes, and always writes the chosen
  `severity: high|medium|low` into both files.
- `summary_written` is machine-populated after side effects. The eval
  requires it to be true, so a bare verdict without a non-empty
  `summary.md` retries and then fails instead of reporting success.
- `severity_written` is machine-populated after side effects. It is true
  only when the step emitted `summary.md`, `metadata.yaml`, and
  `FINDING.md`, and all three persisted files contain the selected
  severity.
- `verdict` is an enum and must be one of `Fixed`, `Plausible`,
  `Unconfirmed`, `Unknown`, `Invalid`, or `ConfirmedLatent` (a proven
  dormant defect; see the Confirmed Latent branch of
  `configs/prompts/triage-template.md`).
- `severity` is an enum and must be one of `high`, `medium`, or `low`.
- `triage_coding.schema_version` must be `1`, and
  `triage_coding.severity` must match `severity`; missing/malformed
  structured coding or incomplete severity file updates retry and then
  fail the workflow.
- `followups` are preserved when the agent needs more source/type/history
  evidence to classify the finding.

## Validation Flow (`/validate`)

`/validate <finding-dir> [source-workspace]` and
`--prompt "validate: <finding-dir> [source-workspace]"` dispatch
`configs/workflows/validate.json`. There is no markdown prompt path for
validation. The source workspace defaults to the active workspace (`.`);
when supplied, it becomes the workflow runner workspace so local source
tools, semcode MCP, git, and `skills: ["auto"]` all resolve against the
codebase being validated rather than the finding export directory.

The workflow is deliberately hostile to the finding it is handed:
false-positive elimination is the stated goal of every step, and the run
is not over when a verdict is reached — it is over when that verdict has
survived two attempts to break it.

### Shape of a Validation Run

One straight line with one loop in it.

```text
  target: an absolute path to a kres finding dir
     │    (FINDING.md, metadata.yaml — plus a source workspace)
     ▼
  validate-claims ──────── fast, coding, no `edit`
     │                     is each factual statement in the finding true?
     │                     emits claim_validation: thesis, claims[],
     │                     design_intent  +  Rust's citation_check
     ▼
  validate-conjunction ─── fast, coding, read/source/grep only
     │                     can the surviving preconditions hold on ONE
     │                     execution?  emits conflicts[] and a
     │                     single_execution_witness, or null
     ▼
  validate-reachability ── slow, coding, the only step with `edit`
     │                     closes the bug-existence gates, applies the
     │                     shared triage template, writes summary.md +
     │                     metadata.yaml + FINDING.md
     │                     emits verdict, severity, triage_coding
     │
     ├─ verdict != Plausible ──────────────────────> END
     │
     └─ verdict == Plausible
           │
           ├─> validate-refute ─────────── slow, primary model,  actions: []
           │        │
           │        └─ refuted ──> back to validate-reachability ──┐
           │                                                       │
           └─> validate-refute-secondary ─ slow, SECOND model, actions: []
                    │                      (guarded on slow_secondary_available)
                    └─ refuted ──> back to validate-reachability ──┘

                    3 branch-backs max, then `continue`
```

Nothing here runs in parallel and there are no lenses: the order above
is `depends_on` order (`configs/workflows/validate.json:55`, `:295`,
`:470`, `:1088`, `:1202`). Authority narrows as the run proceeds — the
first two passes may fetch source but not write, `validate-reachability`
is the only step carrying the `edit` action, and the two refuters carry
no actions at all, so they can neither write nor fetch.

Inputs (`:15-31`):

| input | required | meaning |
|---|---|---|
| `target` | yes | absolute path to the finding directory |
| `source_workspace` | yes | tree that source/grep/git/semcode and `skills: ["auto"]` resolve against |
| `slow_secondary_available` | no | machine-populated; gates the second refuter |

The finding directory and the source tree are usually different
checkouts, and the distinction matters: the finding is the thing under
suspicion, the workspace is the evidence. Both dispatch paths inject
`slow_secondary_available` (`kres-repl/src/session.rs:4246`,
`kres/src/main.rs:1970`), because a second opinion that silently fell
back to the primary model would be the first opinion repeated.

### The Four Passes

**`validate-claims` — every statement, checked on its own.** The first
pass turns the finding's prose into a typed report. It reads
`metadata.yaml` and `FINDING.md`, states the `thesis` in one sentence —
what the finding says is broken, as a specific defect rather than a
subsystem — and emits one `claims` entry per material statement. It runs
on the fast agent in coding mode with no `edit` action, so it can gather
freely and change nothing (`:55-68`). Four fields on each claim carry
the whole downstream contract:

| field | why it matters |
|---|---|
| `kind` | `precondition` entries are what the next pass compares; `mechanism`, `reachability`, `impact` and `design_intent` are not |
| `verdict` | `supported` and `contradicted` need evidence; `unresolved` needs `source_needed` naming exactly what would settle it |
| `gating` | true when the bug does not exist if the claim is false. Frequency, severity and fix strategy never are. Read by Rust, not by a later model |
| `evidence[].provenance` | `fresh` = fetched this session; `finding_quoted` = only present as text pasted inside the finding |

A required `design_intent` record forces the question no bug detector
asks itself: the leading comment, an enumerated design table, or the
introducing commit's message, looked at before the behaviour is called a
defect. The pass also carries a bounded-git rule, repeated verbatim in
the slow step: `git show -s --oneline <sha>` to ask whether a commit
exists and `git merge-base --is-ancestor <sha> HEAD` to ask whether it
is present, never `git log <sha>`, which dumps the entire ancestry
reachable from the commit.

**`validate-conjunction` — and then checked against each other.** Every
claim above was checked in isolation. The second pass asks the one
question that leaves: can they all be true of the SAME execution? It is
a fast coding step limited to `read`, `source` and `grep` (`:295-308`)
and is told not to re-validate anything.

The failure mode it exists for, in the prompt's own words: a report can
state "this path cannot run when core scheduling is active" and, four
entries later, "the reader of that state exists only when core
scheduling is compiled in and enabled" — each claim correct, the
conjunction empty, the finding impossible.

So it compares the preconditions pairwise, emits a `conflicts[]` entry
for each pair that cannot hold together, and then tries to build a
`single_execution_witness`: one configuration under which every gating
precondition holds at once. The witness separates `build_config`
(compile-time kconfig symbols) from `runtime_state` (static branches,
sysctls, scheduler features, cgroup settings, topology, locks held) —
see the subsection below on why. For a race it must name a
`concurrent_writer` that can execute inside the window; the same task,
or a writer excluded by a lock the window already holds, is not a race.
Where no such configuration exists the witness is null, and saying so is
the correct answer. That does not invalidate the finding by itself; it
caps what the next pass may conclude.

**`validate-reachability` — the verdict, and the only step that
writes.** The slow pass decides, and is the only one with `edit`
(`:470-483`). It receives both prior reports verbatim as a checklist and
closes every remaining question that gates whether the bug exists: if
the finding is only valid when a function can return NULL, it proves
whether that function can return NULL.

Status and severity semantics are not defined here. The step pulls in
`globals.triage_rules` — the same `configs/prompts/triage-template.md`
that `/triage` uses — so the status decision tree, the severity tree,
the `metadata.yaml` and `FINDING.md` rules and the `triage_coding` shape
exist in exactly one place. Its prompt begins with the bare finding path
because that template's contract is "the first line of this prompt is
`DIR`". What validation adds on top is strictness in four places: no
`Plausible` while a load-bearing component is unresolved; `Unconfirmed`
only for a gate genuinely still open, never one the run resolved
negatively; proven-impossible is `Invalid` and proven-intentional is
`NotADefect`; and latent-only requires every component to lack a
trigger, so one reachable component keeps the finding valid.

It then rewrites the finding: `summary.md`, `metadata.yaml` and
`FINDING.md` as absolute-path `code_output` entries, emitted together or
not at all — a `summary.md` saying NotADefect beside a `metadata.yaml`
still saying active is worse than either alone. `metadata.yaml` also
gets `validation_run: true`, the marker that tells a batch wrapper this
was validated rather than merely triaged. Its typed outputs are
`verdict` (Fixed | Plausible | Unconfirmed | Unknown | Invalid |
ConfirmedLatent | NotADefect), `severity`, and `triage_coding` — the
machine-readable projection of the summary, with `summary_status`,
`impact_classes[]`, `config_commonness`, `reject_reasons[]`,
`reachability`, `touches`, `actionability`, `priority` and `evidence`.

It also declares `followups` (`configs/workflows/validate.json:799`),
and nothing reads it: no `run_if` and no later step consumes it, so
evidence requests the verdict pass emits are recorded in the task
summary and go no further. Unlike `/review`, validation has no outer
todo loop to turn them into next work. Do not treat their presence as
a forward-progress mechanism.

**`validate-refute` and `validate-refute-secondary` — two attempts to
break it.** Covered in its own subsection below.

### Validation is gated by Rust, not by prompt exhortation

Three of the decisions above are checked in Rust after the model
answers, rather than asked for in prose. Both the fast and the slow step
use `builtin` evals rather than `field_check` expressions, because the
invariants quantify over arrays and read across steps.

A machine-populated `citation_check` output lints every claim citation
before the eval runs, computed by the driver because only it holds the
workspace and the fetched evidence. A `file:line` that names a
nonexistent file, or a line past end of file, fails the step. A `fresh`
citation naming a file absent from the delivered evidence is logged but
does not fail: replayed over 113 real reports that fired 14 times across
11 runs, and the cases examined were correct claims whose evidence came
from a grep — search results are delivered as bare `61:SCHED_FEAT(...)`
lines with no filename, so a genuinely searched file never appears in
the evidence blob. Giving search results their filename is what would
make that half enforceable.

`validate_claims_wellformed` rejects an attempt when: `schema_version`
is not 1; `thesis` is empty; `design_intent.checked` is false; a claim
id is missing or duplicated; a supported/contradicted claim has no
evidence; an unresolved claim does not say in `source_needed` what would
settle it; or **a gating claim is supported only by `finding_quoted`
evidence**. That last one is the point: a validation pass that confirms
a finding using the finding's own quotations has verified nothing.

`validate_verdict_consistency` keeps the old file-side guarantees —
`summary_written` and `severity_written` true, `triage_coding`
`schema_version == 1`, `triage_coding.severity == severity` — and adds
the verdict/`summary_status` casing map. The two file-side flags cannot
be asserted by the model: Rust derives them after side effects are
staged (`kres-agents/src/workflow_runner.rs:5455`, `:5488`), with
`summary_written` requiring an emitted `summary.md` with a non-blank
body and `severity_written` requiring all three artifacts emitted into
the same directory with the chosen severity present in each.

On top of that, a verdict of `Plausible` additionally requires all four
of:

1. no `gating: true` claim left `unresolved`, unless the slow step
   settles it in `gating_override` with file:line evidence;
2. every pair in the conjunction step's `conflicts` addressed in
   `conflict_resolution` with evidence;
3. every successful refutation answered in `refutation_rebuttal` with
   evidence (see below);
4. a non-null `single_execution_witness`.

Any other verdict is free of all four, which is the point: taking
`Invalid`, `NotADefect`, `ConfirmedLatent` or `Unconfirmed` is always
available and is not a failure of the run.

This exists because the prompt already said "do not preserve a finding
as Plausible when any load-bearing component remains unresolved" and
runs preserved findings anyway by relabelling the component as a
"probability/severity question, not an existence question". Prompt text
the same model can talk itself out of is not a control. Do not replace
these evals with prompt text or with a `field_check`; the expression
language has no array quantifier and cannot express any of them.

### Two independent attempts to break a surviving finding

`validate-refute` and `validate-refute-secondary` run after the verdict,
and only when it is `Plausible`. That is the only verdict claiming the
bug exists and is reachable today:

| verdict | metadata `status:` | is it a bug? | refuters |
|---|---|---|---|
| `Plausible` | `active` | yes — claimed real | **run** |
| `ConfirmedLatent` | `confirmed_latent` | no — every trigger proven closed | skip |
| `NotADefect` | `not_a_defect` | no — proven intentional | skip |
| `Invalid` | `invalidated` | no — disproven | skip |
| `Fixed` | `fixed` | no — already resolved upstream | skip |
| `Unconfirmed` | `unconfirmed` | unknown — gate still open | skip |
| `Unknown` | unchanged | unknown — finding too thin | skip |

The four settled non-bug verdicts are not worth two slow calls: nothing
downstream acts on them. The two unknown ones make no claim to break —
what they need is the open gate answered, which is
`validate-reachability`'s job and `gating_override`'s channel, not a
refutation.

Both refuters are told to break
the finding rather than assess it, and both are given everything the run
gathered with `actions: []` so they fetch nothing: the failure mode this
targets is not missing evidence. In a hand audit of eight surviving
false positives, six had the disproving fact already in the slow agent's
context and three had written it into a supported claim.

The second one carries `slow_variant: "secondary"`, which routes its
synthesis call to `settings.models.slow_secondary` (or the second
`--slow` selection). Agreement between two model families is worth more
than one model re-reading its own reasoning. It is guarded by the
`slow_secondary_available` workflow input, injected by both dispatch
paths: silently falling back to the primary would make the second
opinion the first one repeated. `scripts/validate-all.py` passes both
configured selectors and warns when only one is available.

The two do not run as independent samples. Both depend on
`validate-reachability`, and a refutation branches immediately, so the
secondary is reached only when the primary let the finding stand. That
ordering is deliberate and matches the goal — the question the second
model answers is "what did the first one miss" — but it means a wrong
refutation by the primary is never contradicted by the secondary, and
the two catch rates are not comparable. On the measured batch the
primary refuted 2 and the secondary 4, of 54, with no overlap.

A refutation is kept. Both `repeat` and a re-entry snapshot a rejected
attempt into `prior_attempts`, and the cascade covers steps downstream
of a branch target — but the branch *source* was exempted and then
skipped on the next pass, so the one record that a model broke the
finding was discarded. Six successful refutations in a 113-finding
batch survived nowhere but the raw JSONL. The `BranchTo` arm now
snapshots the branching step's outputs first, and the verdict step is
told to record in `summary.md` which pass broke the finding and what
changed.

Either refuter succeeding blocks `Plausible`. They are asked to break
the finding, not to vote — a refutation carries `decisive_evidence` and
a survival does not — so one is the stronger signal. A successful
refutation branches control back to `validate-reachability`, whose
prompt then carries a `PRIOR REFUTATIONS` block (built by
`prior_refutations_block`, because a step that runs before the refuters
cannot interpolate their output). `validate_verdict_consistency` then
requires a `refutation_rebuttal` entry naming the refuter and carrying
evidence, or a verdict below `Plausible`. Three branch-backs are
allowed, after which the eval's `on_exhausted: continue` lets the run
finish on the verdict it has rather than failing it.

### Compile-time and runtime gates are different conditions

`triage_coding.reachability` splits `build_config` (kconfig symbols that
must be set) from `runtime_state` (static branches, sysctls, scheduler
features, cgroup settings, topology). `CONFIG_X=y` and `X_enabled()` are
not the same condition, and merging them is how two contradictory claims
came to look compatible. `/triage` carries the identical schema; a test
asserts the two stay byte-identical.

### Status vocabulary

The validation prompt is stricter than triage about false positives. A
finding should not be kept `Plausible` when a load-bearing component is
still unresolved. Status follows what the evidence proved: a
bug-existence gate that is still genuinely open (neither proven nor
disproven) is `Unconfirmed`, while a gate the run resolved negatively is
not. Contradicted findings are `Invalid`.

A finding proven latent-only — the defect pattern genuinely exists in
source but 100% of it has no current in-tree trigger because every
required precondition, hook, caller, or state is absent or cannot occur —
is `Confirmed Latent` (verdict `ConfirmedLatent`,
`triage_coding.summary_status: confirmed_latent`, metadata
`status: confirmed_latent`), not `Unconfirmed` and not `Invalid`.

A finding whose behaviour *does* occur but which source evidence shows
is intended — a leading comment, an enumerated design table, or the
introducing commit's message — is `NotADefect`
(`summary_status: not_a_defect`, metadata `status: not_a_defect`,
`intentional_design` in `reject_reasons`). Without this value the
decision tree had no reachable cell for "the code is doing what its
author intended": `Invalid` means the behaviour does not happen and a
low-severity `Plausible` still says the code is wrong. A validator may
disagree with a documented design, but it must engage with the
documentation rather than record the decision as an oversight.
`kres-repl/src/summary.rs` maps `NotADefect` onto `Status::Invalidated`,
which filters it out of `/summary` like any other non-defect; the reason
survives in the finding directory.

These statuses, their decision-tree placement, and their `triage_coding`
tagging are defined once in `configs/prompts/triage-template.md`, so
`/triage` and `/validate` share the same definitions. The template also
carries the assert-side race checklist: before a check-then-use or
unlocked-read finding may be `Plausible`, it must name the instruction
that opens the window, **the specific writer that can execute inside
it**, and why that writer runs concurrently. There was a mandatory
checklist for dismissing a race and none for asserting one.

### Where the Verdict Goes

`/summary` is the biggest consumer, and the reason validation runs at
scale — see [Summary Flow](#summary-flow-summary) for the batch
mechanics. `apply_validation_outputs` (`kres-repl/src/summary.rs:320`)
turns each verdict into a finding status:

| `verdict` | `Finding.status` |
|---|---|
| `Invalid`, `NotADefect` | `Invalidated` |
| `Fixed` | `Fixed` |
| `Unconfirmed`, `Unknown` | `Unconfirmed` |
| `Plausible`, `ConfirmedLatent` | `Active` |

`project_validated_finding` (`:354`) replaces the stored summary with
the validated `summary.md` and clears the store-only detail fields, and
`is_summary_candidate` (`:374`) drops `Invalidated` and `Fixed` before
anything is rendered. So a finding the workflow killed does not appear
in the summary, while the reasoning stays in the finding directory.

`scripts/validate-all.py` is the batch wrapper for a whole export tree:
`kres --prompt 'validate: <dir> <workspace>'` with `cwd = workspace`, 20
parallel. It judges completion from the artifacts rather than the exit
code (`validate_state`, `:190`): all three files present, a valid
severity in each, the three agreeing, and `validation_run: true`. It
passes no model flags by default, because a non-empty `--slow` selection
replaces the configured pair outright and silently suppresses the
secondary refuter (`:277-292`).

## Summary Flow (`/summary`)

`/summary`, `/summary-markdown`, `kres --summary`, and
`kres --summary-markdown` all call `kres-repl/src/summary.rs`. Summary
is not itself a workflow and is not invokable through
`--prompt "summary: ..."`. Before rendering, it exports every canonical
finding with store-only `details` redacted and invokes the existing `validate`
JSON workflow for each finding. Up to 20 finding validations run concurrently;
their results are restored to canonical finding order before rendering. A failed
validation cancels the remaining batch and aborts the summary. The renderer
consumes only validation-produced summaries and structured verdicts, then owns
batching, template selection, and the final output write for both CLI and REPL
entry points. Both render variants prepend the same kernel problem-description
rules and format every surviving finding as a source-area subject plus a short
causal changelog without proposing a fix. The Markdown variant uses the subject
as a section heading; the text variant emits raw problem-description blocks.
The fix workflow composes those same problem rules with the separate kernel fix
rules in Rust before sending the commit-writing prompt. Standalone `--summary`
and `--summary-markdown` resolve the workflow's slow role from
`models.slow` like any other run; `--slow` (or another explicit
slow-model override) selects a different one. REPL slash commands retain the
session's configured fast and slow roles.
