# Workflow Documentation

This document is the source of truth for the shipped kres workflows.
Keep `AGENTS.md` short and point back here instead of duplicating the
flow details.

For a cross-workflow inventory of goal ownership, planning, plan mutation,
and completion decisions, see [Planning and Goal-Setting Audit](planning-and-goals-audit.md).

## Shared Workflow Runner Behavior

The configured workspace is implicitly readable and writable by kres
tools. When the operator mentions an existing absolute file or
directory outside the workspace in a prompt, kres grants session-scoped
read/write access to that file's parent directory or to that directory
itself. The same grant is used by `read`, `edit`, `code_output`, and
workflow reaper paths; `/clear` or process restart drops the grants.

The workflow runner wires the normal orchestrator into every LLM step.
That means LLM steps use the fast-gather -> main-fetch -> synthesis loop:

1. Fast agent requests typed followups such as `read`, `source`,
   `type`, `grep`, `git`, `callers`, or `make`.
2. The main/service path fetches data and adds it to symbols/context.
3. The step's declared agent receives the gathered context and emits the final
   response. `agent: fast` uses the fast model for synthesis; `agent: slow` and
   `agent: code` use the slow model.

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

All structured agent responses must be exactly one JSON value with no prose,
embedded JSON string, or transport wrapper. Prompts require raw, unfenced JSON.
As a deterministic transport normalization, Rust removes one Markdown JSON
fence only when it wraps the entire trimmed response, then runs the unchanged
strict contract before considering inference repair. Serde DTOs are the
acceptance boundary; nested DTOs reject unknown fields, and
`serde_path_to_error` identifies the exact invalid field. `schemars` derives
the repair schema from the same Rust DTO, so prompts and deserialization do not
maintain separate representations of the contract. Workflow-defined extension
fields are allowed only when declared by that step and are subsequently checked
against the workflow's JSON Schema.

On failure, one repair inference receives the untouched response, generated
schema, and serde/schema errors. Its replacement is accepted only by
deserializing it through the identical contract. Rust does not brace-scan,
unwrap, normalize, or attempt to infer semantic equivalence between malformed
and repaired prose. If strict repair fails, the caller retries or fails the
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
Production dispatch always supplies persistence: an explicit state directory
wins, then the results directory, with `<workspace>/.kres/workflow-state` as
the fallback. Live observers do not disable snapshots. Fix-series planning and
each todo revision use separate subdirectories under `workflow-state`.
Failing eval follows
`eval.on_fail.action`: `repeat` reruns the step, `branch_to` moves
control back to a named step and invalidates dependent work, `continue`
keeps going, and `exit_failure` terminates. `max_attempts` and
`on_exhausted` decide what happens after repeated eval failures. Driver
errors that occur before usable outputs are produced use the step's eval
retry budget when the step has an eval block; otherwise they fail unless
that driver has a specific recovery path.

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

   - `research_status`: `confirmed`, `invalid`, or `unconfirmed`.
     `confirmed` means gathered source proves a concrete bug exists at
     workspace HEAD and `analysis` contains a specific fix contract.
     `invalid` means source/commit evidence disproves the bug.
     `unconfirmed` means the bug may be real but research could not
     prove or disprove it well enough to patch.
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
     `nice_to_have: false`) is a request for another gather round. Any
     blocking followup makes the eval fail and triggers a retry that
     gathers the requested evidence. A nice-to-have followup
     (`nice_to_have: true`) is a deferred audit suggestion that does
     not block — it may ride alongside any terminal status.
   - `affected_files`: paths the patch will touch.
   - `affected_symbols`: relevant symbols.

   Optional outputs:

   - `analysis`: research narrative and fix sketch for later steps.

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
   its own context window: combined memory-safety/object-lifetime,
   bounds, races, commit assertions, an antagonistic kernel-maintainer
   lens, and bug coverage.

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
   multi-function control flow. Diagrams must use ASCII only. When that
   is the only defect, review routes back to `write-commit-message`;
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

   After every planned todo has completed, a primary-slow final assessment
   reviews the original finding, revisioned series state, complete commit
   sequence, and current source. It does not judge only `HEAD~1`. The series is
   successful only when every planned todo is done and every original bug
   component is fixed, invalidated with evidence, or matched to a proven
   upstream duplicate. Deferred or unexamined components fail the gate.

   The typed decision is `complete`, `revise_pending_plan`, `unconfirmed`, or
   `failure`. A revision decision must carry a stale-checked plan update made
   only of `append_after_current` operations. Rust validates and persists the
   new todos, runs them through the normal fix pipeline, and repeats the final
   assessment. Other non-`complete` decisions fail while preserving structured
   remaining work in the workflow trace. A builtin validator requires outcomes
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

1. For a named source-file review, bootstrap requests semcode's compact Tree-sitter
   `file_survey` exactly once, performs no other fetches, and sends that inventory to one
   non-lensed slow call. The slow call emits a no-reasoning 0-100 bug-likelihood guess
   for every defined function. That ranking is supplied to `define_goal` and
   `define_plan`, so the initial semantic groups are source-informed. It is also cached
   and embedded in the persisted plan prompt for later tasks and resume. Scheduled tasks
   cannot request another survey; they gather targeted source, types, callers, grep
   results, and history before parallel slow review lenses run.
2. The active slow-agent review lenses run in parallel.
3. A fast consolidator merges and ranks the results.
4. The lensed step completes with `findings` and typed `followups` in
   its output.
5. The normal kres task reaper sends those followups to the todo agent,
   which deduplicates and prioritizes them against completed work,
   session lenses, and the current plan. Selected followups become the
   next todos, and each todo runs as a fresh review task through the
   same JSON-defined lenses. If a lensed review task emits typed
   followups and the todo agent fails to keep any pending/blocked next
   work, the reaper restores those followups as pending todos instead
   of letting a narrow goal check terminate the run.

The parallel slow calls are the important part of `/review`; keep them
unless the operator explicitly chooses a cheaper custom workflow.

Review prompts carry a resolved target kind. A source file or directory means the current
workspace contents and does not imply a revision, base, or diff. Only a target classified
as a git commit/range starts from `git show` or `git diff`; source reviews may request
targeted history later when a concrete semantic question requires it.

When multiple `--slow` selectors are passed, review runs in comparison
mode. Each selector resolves to one model (`sonnet` and `opus` consult
`settings.json:model_aliases` before their built-in fallbacks;
use `provider.json:model-id` when more than one provider offers the model).
Each active lens prompt is sent to every configured slow model, so a
commit review with five active lenses and `--slow sonnet --slow opus`
performs ten slow calls after the shared gather. Every per-lens
output sent to the consolidator is
tagged with `slow_model`; the consolidator compares sibling model
outputs for quality and depth, keeps the best evidence and Findings
while deduplicating across models and lenses, and records one entry per
completed review turn in `<results>/comparison.json`.

### Review Lenses

- `memory-lifetime`: allocation and initialization, publication, pointer
  ownership, refcounts, RCU grace periods, asynchronous use, cleanup paths,
  callback ownership, object layout, free ordering, leaks, use-after-free,
  double-free, uninitialized memory, and allocator API misuse. Pure index,
  range, and arithmetic errors remain owned by `bounds`.
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
serde error rather than silently discarded. After lens consolidation, and
again after the prose-promotion pass, kres gives the fast formatting agent one
strict-schema repair attempt. The complete replacement response must pass the
same derived serde contract. A record that remains invalid is preserved in
`report.md` and `findings.json.task_prose`, and a blocking typed retry followup
is added; no missing line number or other evidence is fabricated.

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

Required per finding:

- `id`: short stable snake_case slug.
- `title`: one-line human title.
- `severity`: `low`, `medium`, or `high`.
- `status`: normally `active`.
- `relevant_symbols`: embedded symbol records
  `{name, filename, line, definition}`.
- `relevant_file_sections`: embedded source slices when a cited region
  is not covered by `relevant_symbols`.
- `summary`: what is wrong and why, with file:line citations.
- `reproducer_sketch`: the code path, inputs, and state needed to
  trigger the bug.
- `impact`: what goes wrong when triggered.

Optional fields include `mechanism_detail`, `fix_sketch`,
`open_questions`, and `related_finding_ids`. Embedded source bodies
should be minimal but sufficient to prove the bug.

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

Validation is intentionally sequential and does not use review lenses:

- `validate-claims` runs as a fast coding step. It reads
  `metadata.yaml` and `FINDING.md`, checks factual claims against source,
  and emits structured `claim_validation` with object entries for
  supported, contradicted, and unresolved claims. Each claim entry carries
  a stable `id`, the claim text, and evidence or the exact source still
  needed. Open-question and false-positive-risk lists remain string
  summaries.
- `validate-reachability` runs as a slow coding step. It uses the claim
  validation report as a checklist, closes bug-existence questions such
  as return-value and reachability assumptions, determines whether the
  bug is reachable or latent, and then applies the same triage template
  used by `/triage`.

The final validation step writes `summary.md`, updates `metadata.yaml`
and `FINDING.md` with the selected severity, adds
`validation_run: true` to `metadata.yaml`, and emits `triage_coding`.
The same machine-populated checks as `/triage` apply:
`summary_written` and `severity_written` must be true, and
`triage_coding.schema_version == 1` with
`triage_coding.severity == severity`. Incomplete or malformed output is
retried and then fails the workflow.

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
`status: confirmed_latent`), not `Unconfirmed` and not `Invalid`:
nothing is left open, but the dormant structure is not a false positive
either. This status, its decision-tree placement, and its
`triage_coding` tagging (`latent` impact class, `latent_only` reject
reason, trigger reachability gates resolved to `no`) are defined once in
`configs/prompts/triage-template.md`, so `/triage` and `/validate` share
the same definition. If any component is currently reachable and valid,
the finding is not latent-only: validation keeps it valid and documents
that reachable component.

## Summary Flow (`/summary`)

`/summary`, `/summary-markdown`, `kres --summary`, and
`kres --summary-markdown` all call `kres-repl/src/summary.rs`. Summary
is not a workflow and is not invokable through `--prompt "summary: ..."`.
The summary renderer owns batching, condensation, template selection,
and output writes for both CLI and REPL entry points.
