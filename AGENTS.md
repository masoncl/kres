# kres — Kernel Code Analysis Tool

## Architecture

kres is a multi-agent kernel code analysis REPL. Three agents collaborate:

- **Fast agent** (configurable model): Scopes work, identifies needed source code, builds a structured brief for the slow agent. Runs in task threads.
- **Slow agent** (configurable model): Deep analysis with all context pre-gathered. Thorough findings with file:line citations. Runs in task threads.
- **Main agent** (configurable model): Data retrieval only. Fetches code via semcode MCP, grep, read, git. Runs in service threads spawned per-task.

## Flow

```
User prompt → Task created → Task thread starts
  → Fast agent [round 1]: requests data via followups
  → Service thread: main agent gathers data (semcode/grep/read/git)
  → Fast agent [round 2+]: verifies, requests more or sets ready_for_slow
  → Slow agent: deep analysis with all gathered context
  → Task completes → followups sent through inference for dedup → new todos
```

## Key Design Decisions

### Compatibility Policy
- kres is new enough that there is no backwards-compatibility burden.
- Do not keep duplicate command paths, compatibility aliases, deprecated
  prompt formats, or renamed-file shims unless the current workflow
  actively needs them.
- Workflow-owned commands have exactly one implementation mechanism:
  the JSON workflow under `configs/workflows/` (or the operator's
  `~/.kres/workflows/<name>.json` override). `/fix`, `/review`,
  `/triage`, and `/validate` may be started from the REPL or from CLI
  `--prompt`; both entry points must derive behavior from the same JSON
  workflow.
- `/fix`, `/triage`, and `/validate` use the workflow executor directly.
  `/review` uses `review.json` to define its prompt contract and lenses, then
  runs through the REPL task/todo loop so followups become prioritized
  next-turn todos. Do not add a second review engine or a markdown
  prompt fallback.
- Do not reintroduce markdown prompt-template fallbacks, special REPL
  task paths, or command-specific alternate engines for workflow-owned
  commands. If workflow behavior is wrong, fix the JSON workflow and
  shared workflow runner/executor. Delete stale templates and tests
  instead of preserving alternate behavior.
- `/review` specifically must remain one JSON workflow with the old
  forward-progress semantics: each turn runs parallel slow-agent
  lenses, emits Findings plus typed followups, dedups those followups
  through the todo agent and ranks the runnable ones through the
  prioritization agent into the next review task, and continues until
  the turn cap or followup exhaustion. Do not replace
  this with workflow-local "fetch followups and repeat the same step"
  logic.
- Do not weaken the golden review prompt contract: every lens is
  exhaustive for its bug class, does not stop after the first issue, and
  emits typed followups when more source, callers, history, or API
  context is needed to be confident. A clean lens means confident, not
  merely "nothing proved from the first gathered context."
- For commit/range reviews, audit the semantic contract changed by the
  diff, not just the edited lines. If a patch changes representation,
  helper family, callback/dispatch, allocation, lifetime, locking,
  ordering, or accounting behavior, trace unchanged readers, writers,
  callers, callees, callbacks, setup/registration sites, and shared
  helpers that still rely on the old contract. Do this generically by
  following the changed contract; do not hardcode subsystem-specific
  rules. Missing unchanged paths are followups, not a clean review.
- A workflow step's synthesis call must run under a system prompt that
  matches the step's declared outputs. `agent: fast` defaults to
  `workflow-synthesis`; `agent: slow`/`code` default to the per-mode
  slow prompt; a step may name another with `synthesis_system`. Do not
  route a schema-bearing synthesis call through the fast-gather prompt:
  that prompt mandates the `ready_for_slow` envelope, and measured over
  384 validate runs the model obeyed it instead of the schema on 397 of
  784 calls, each rejection re-running the whole step.
- A workflow step's call-invariant instruction text (skills, includes)
  belongs in the prompt envelope's `stable_instructions` field, which
  gets its own cache block, not concatenated onto `question`. Nothing
  task-specific may enter that field: a head that varies per call is
  written once per call and read never, which costs more than not
  caching it.
- Reasoning invariants that quantify over a step's typed arrays, or
  that read across steps, belong in a `builtin` eval, not in prompt
  text and not in a `field_check` (the expression language has no
  quantifier). Validate's two — `validate_claims_wellformed` and
  `validate_verdict_consistency` — exist because the prompt already
  stated the rule and runs talked themselves out of it by relabelling a
  load-bearing unresolved claim as a severity question.
- Parallel runs never share a directory. Every kres run owns its
  artifacts — findings, report, session state, workflow snapshots —
  under `--results` when given and `~/.kres/sessions/<ts>-<pid>/`
  otherwise. Two runs are working on different inputs, so a file they
  both write is one of them destroying the other's record. Do not add a
  shared-path default for a run that arrives without a directory: that
  is a caller bug and it must fail rather than be papered over.
- A second opinion must come from a different model. A workflow step
  that checks another step's conclusion names `slow_variant:
  "secondary"` and is guarded on the `slow_secondary_available` input;
  a step that silently fell back to the primary would be the first
  opinion repeated. Either of validate's two refuters succeeding blocks
  `Plausible`: they are asked to break the finding, not to vote, and a
  refutation carries evidence while a survival does not. They run on
  `Plausible` alone — the only verdict claiming a bug exists and is
  reachable today. Do not extend them to a verdict already established
  as not-a-bug (`ConfirmedLatent`, `NotADefect`, `Invalid`, `Fixed`);
  there is nothing downstream to protect and the refutation costs two
  slow calls.
- kres's own prompts must be bug-agnostic. No text kres ships to a
  model may name a specific regression, subsystem, file, function,
  helper, symbol, or one-off mechanism. Generalize to the review
  invariant instead: changed contracts, unchanged users, concrete
  trigger paths, strong suspects, and typed followups for missing
  evidence. Regression tests may check that generic invariants are
  present, but prompts themselves must stay reusable across all bug
  classes.

  This is a rule about the TEXT, not about your reason for writing it.
  It applies to every model-facing string equally: guidance, worked
  EXAMPLES, sample values, enum descriptions, schema field docs,
  defaults, and error messages. An earlier wording said "do not add
  guidance that names … because a recent run failed to find it", and
  both halves of that were an opening. On 2026-08-21 the
  `invalidation.premise` field's two illustrative examples shipped as
  `("mmu_lock is held for write across the whole update",
  "check_memory_region_flags rejects the two flags together")` — the
  second verbatim from the arch/x86/kvm/mmu findings that motivated the
  field. It survived self-review twice, because an example is not
  "guidance" and because the stated motive was "show the shape", not
  "a run failed". Neither distinction survives contact with the
  prompt, which ships to every review of every subsystem and seeds one
  subsystem's vocabulary into all of them.

  Model-facing means: `configs/prompts/**`, `configs/workflows/**`,
  every Rust string literal that reaches a prompt, and anything
  `steps[].include` pulls in. Rust `//` and `///` comments and
  `#[cfg(test)]` fixtures are NOT model-facing and may name whatever
  makes the reason legible — that is where a motivating bug belongs.
  Before committing a change to any model-facing file, grep the whole
  set for the identifiers of whatever you were just looking at.

  An operator's own domain guide is a different thing and is not
  covered: `~/.kres/skills/*.md` and the review-prompt trees they pull
  in exist precisely to encode how one subsystem's APIs must be used.
  Naming `is_page_fault_stale()` in a KVM subsystem guide is that
  file's job. Naming it in kres's audit system prompt is this
  violation.
- Negative coverage claims must be earned. Agents, consolidators, todo
  updates, and goal checks must not accept phrases like "no remaining
  users", "all callers updated", "old path unreachable", "only reader",
  or "only writer" unless the run has concrete source/search/callgraph
  or history evidence for that claim. Missing proof becomes a typed
  followup, not a clean result.
- semcode is an accelerator, not an authority. It can be unavailable
  while indexing and can miss macros, global symbols, generated-looking
  constructs, or complex definitions. Any failed, empty, unparseable, or
  "not found" semcode result must fall back to local `grep`/`read`
  evidence before research, review, todo, or goal logic treats source as
  unavailable or a symbol as absent. Never mark a finding unconfirmed,
  invalid, clean, or fully covered based only on semcode absence.
- Local fallback must preserve the complete grep match list for the requesting
  agent, because Rust cannot know which match is semantically relevant.
  Do not add per-file or shared output caps that hide candidates. Do not expand broad grep fallback results into full
  source reads for every hit; return the matches and require targeted
  `read` followups for the specific file:line ranges that need full
  context.
- Build and shell output is the one exception, and it is not a cap.
  Above `TOOL_OUTPUT_INLINE_MAX` the complete output is written to
  `<workspace>/.kres/tool-output/` and the tool result carries the
  head, the tail, the exact byte count, and that path. Nothing is
  discarded and the omission is explicit, so a targeted `read`
  recovers any region. Do not extend this to grep, source, or any
  evidence a review reasons over, and do not turn it into a silent
  truncation by dropping the pointer or the byte count.
- A finding delta carries only what changed. `Finding` requires `id`
  and nothing else on the wire: `title`, `severity` and `summary`
  default, because reusing an id to update or retire a record is a
  shape kres asks for in its own prompts. `merge_into` was always
  written for it — `prefer_longer` ignores an empty incoming string
  and severity merges by max, so an absent field cannot blank or
  downgrade a stored one.

  The wire type used to demand a whole record, which made both shapes
  unparseable. The retirement shape `review.json` instructs,
  `{id, status: invalidated, invalidation}`, failed on `missing field
  title`; so did `{id, open_questions}`. One such entry fails the
  WHOLE response, so the lens is re-run: measured across 26 concurrent
  reviews on 2026-08-22, 1,766 repair calls against 7,398 lens
  responses, 91 for a missing title and 28 for a missing summary. The
  prompt was instructing a shape the schema forbade.

  Completeness is enforced where the store can tell an update from a
  new finding, not at parse time. `apply_delta_to_list` refuses a
  record that matches no existing id and carries no title or no
  summary, counts `incomplete_refused`, and reports it as
  `refused-incomplete=N`. It refuses BEFORE `semantic_duplicate_index`
  runs: that match needs `id_title_token_overlap` over 0.70, a
  titleless record gives it almost nothing, and merging into the wrong
  record silently destroys a finding.

  Do not restore `title`, `severity` or `summary` as required fields,
  and do not move the completeness check back to parse time — parsing
  cannot see whether the id already exists, which is the only thing
  that distinguishes an update from a nameless new finding.

  **This has been shipped broken three times, so the rule is about
  the ENVELOPE, not the field.** Each delta-only channel was added
  correctly on its own terms and left the shape carrying it
  unparseable:

  * `reactivate`, a wire-only boolean that reverses an invalidation.
  * `resolved_questions`, added by commit `e5fd59b` ("kres: let a
    finding's answered questions be closed") and described there as
    "Wire-only: `merge_into` consumes it and it never reaches a stored
    record" — modelled, in its own words, on `reactivate`.
  * `invalidation`, added by commit `c114adb`. This entry's own
    neighbour below has said "A delta setting `status: invalidated` on
    an existing record carries `invalidation: {premise, evidence[]}`"
    since that commit, and `review.json` has instructed exactly that
    shape — while `{id, status, invalidation}` failed to deserialize
    on `missing field title` the entire time.

  Every author reasoned about the new field and about `merge_into`,
  and none checked whether a record containing ONLY that field could
  be parsed. It could not, so using any of these channels required
  re-sending a whole record. Models do not: measured 30 minutes into
  the 2026-08-22 batch, 479 of 1,504 finding records emitted were
  partial — 32% — with `{id, invalidation, resolved_questions,
  status}` among the most common shapes.

  So: **when you add a field that only makes sense on a delta, add a
  test that deserializes the minimal record the prompts instruct** —
  `{id, <your field>}` and nothing else — and a second that merges it
  and asserts the stored title, summary and severity survived.
  `the_delta_shapes_the_prompts_ask_for_actually_parse` and
  `an_update_keeps_the_stored_title_summary_and_severity` are those
  tests; extend them rather than writing new ones beside them. A
  field-level test on the new channel passes while the envelope is
  broken, which is exactly how this reached production three times.

- An invalidation must name the claim it rests on. A delta setting
  `status: invalidated` on an existing record carries
  `invalidation: {premise, evidence[]}`, where the premise is the one
  claim that, being true, means the finding is not a bug, and the
  evidence is its file:line citations. `apply_status_transition`
  REFUSES the flip without both; the rest of the delta still merges
  and `[findings]` reports `refused-invalidation=N`. Reversing needs
  only that the premise be contradicted, and `Status::Unconfirmed` is
  reachable from Active AND from Invalidated with no flag at all —
  only Invalidated → Active still needs `reactivate: true`.

  All of that exists because `invalidated` had become the bucket for
  three different things and only one of them was disproof. On the
  2026-08-20 arch/x86/kvm/mmu review, `mirror_root_dirty_log_kvm_bug_on`
  and `tdp_mirror_cas_retry_livelock` were retired on the claim that
  `check_memory_region_flags()` makes `KVM_MEM_GUEST_MEMFD` and
  `KVM_MEM_LOG_DIRTY_PAGES` mutually exclusive — true of the flags in
  one ioctl request, not of a slot over its lifetime. The same run
  later filed the `KVM_MR_FLAGS_ONLY` bypass as its own high finding
  (upstream `9935df5333aa`, `Cc: stable`), rewrote the retired
  finding's title to name that bypass, and left the status alone,
  because nothing linked a premise to the evidence refuting it. The
  last two deltas said so outright: "status remains invalidated
  pending confirmation of whether…" and "the decisive bodies were not
  obtained, so status remains invalidated/unresolved rather than
  disproved". Neither can produce a premise, so neither can invalidate
  now, and both describe `Unconfirmed`.

  Do not restore a bare `status: invalidated`, and do not let
  `Unconfirmed` acquire an entry fee. The asymmetry is deliberate:
  claiming a bug is dead should cost more than admitting you cannot
  tell yet.
- Every prior finding is sent to every agent, every time. Do not add
  relevance routing, anchor heuristics, or any other scheme that
  decides WHICH findings a review is allowed to see: a finding that
  looks unrelated by filename is exactly what a cross-file contract
  review must catch. If the accumulated set ever approaches model
  capability, add a semantic partitioner — do not hide findings.
- What is NOT sent is the source each finding carries.
  `findings_for_prompt_history` clears `relevant_symbols[].definition`
  and `relevant_file_sections[].content`, keeping name/filename/line so
  every symbol stays citable and re-fetchable through a `source` or
  `read` followup. This drops no finding and no claim.
  It was added after the 2026-08-06 mm/swapfile.c review: at 99
  findings `previous_findings` reached 1187 KB — definitions 427 KB,
  file sections 114 KB — and the cached head alone passed the
  1,048,576-character cap on the codex-codes JSON-RPC transport. The
  `general` lens failed with -32602 on twelve tasks and halted the
  session twice. Note this is a TRANSPORT cap, not model capability;
  if findings later approach the latter, the partitioner above is
  still the answer.
- Do NOT fold that stripping into `redacted_for_agent`.  That function
  also runs on findings a model just emitted, on their way INTO the
  store (`value_to_findings`), so stripping there stores every new
  finding without its evidence — and `/summary` renders exactly that
  evidence as `exact_text`.  Prompt shrinking belongs at the point the
  prompt is built, which is why `findings_for_prompt_history` is the
  single entry point for `previous_findings`: it also keeps the
  prioritizer's cached head byte-identical to the lens fan-out's.
- A lens that fails or returns nothing does not fail the task when
  others succeeded. `run_with_lenses` consolidates the lenses that
  ran and emits one `[MISSING]` followup per lens that did not, so
  the uncovered bug class is re-queued rather than silently counted
  as reviewed. Only an all-lenses-failed task is an error. Do not
  restore the all-or-nothing behaviour: discarding four good lenses
  because a fifth hit a transport limit both wasted the work and,
  through the consecutive-error watchdog, halted the session.
- A schema change is not done when the producer compiles. Every field
  in a shared payload — the whole-file scan, a `PlanStep`, a `Finding`,
  a `Followup`, a workflow step's declared outputs — is a contract with
  three kinds of consumer, and all three have to move together:
  1. **Rust**, which builds and reads the struct;
  2. **the prompts**, which tell an agent what it will receive and what
     it must return — the per-role system prompts under
     `configs/prompts/`, the workflow JSON's step prompts and shared
     globals, and any operator-facing prompt text;
  3. **the tests that pin those promises**, which otherwise either fail
     on the corrected prompt or keep passing while the prompt lies.

  Before changing a field, enumerate its consumers and list them; do not
  assume the compiler will find them, because a prompt that promises a
  field nobody sends still compiles. An agent told it will receive
  ratings that no longer exist is worse off than one told nothing: it
  will wait for them, or prioritise as though it had them. The same
  applies to a new field — adding it to the producer does nothing until
  the consuming agent is told it exists, what it means, and what to do
  with it.

  Removing a field means removing every promise of it, in the same
  change. Adding one means stating it in every prompt whose agent is
  expected to act on it.
- Rust must not infer semantic workflow state from free-form AI prose.
  Do not add substring/regex classifiers over model `analysis`,
  `invalid_evidence`, defect text, commit-message prose, or other natural
  language output to decide cleanliness, routing, invalidation, retry,
  completion, or followup creation. Use explicit JSON fields, typed
  arrays, enums, booleans, and workflow expressions. If a legacy/free-form
  output must be interpreted, add a targeted inference/judge step that
  returns structured JSON and make Rust consume only that structure.
  Prose may be preserved for humans and logs, but it must not be a hidden
  control channel.
- The converse also holds: Rust must not emit a control signal into a
  channel the model reads as criticism. `previous_rejection_block`
  renders a step's `last_eval_reason` as "Your last response for this
  step was not accepted: <reason>", so only an eval failure that will
  re-run THAT step to correct itself may set it — `repeat` and
  `rerun_chain`. A `branch_to` is how a CORRECT output moves control
  elsewhere: `orchestrator_dispatch` returns `(false, "route to
  <step>")` because returning false is the only way to trigger
  `branch_to_output`; `review` fails `clean == true` exactly when it
  found defects; `validate-refute` fails `refutation.refuted == false`
  exactly when it refutes. None of those is a bad response.

  On the 2026-08-20 futex2 series, finding
  `futex_private_hash_put_uaf_mm`: the orchestrator routed to
  write-commit-message, the branch was taken, and the step was then
  re-prompted with "Your last response for this step was not accepted:
  route to write-commit-message". It concluded its "next_step was
  rejected only because it emitted 'route to write-commit-message' as
  prose instead of the required JSON shape", re-issued the identical
  decision, and on the next pass reasoned that "the only worker that
  can land O4 is write-commit-message, and that routing was rejected
  for this step … That leaves no worker step that can discharge the
  open objective" — then picked `exit-failure`. The source patch was
  complete and every lens had passed it.
- No path from the author's machine may be committed as an
  instruction. A checked-in file that tells a reader or an agent to
  read, run, or write something names it with a placeholder the
  install path substitutes, or with a path relative to the repo root,
  never with `~/...` or `/home/<user>/...`. The review-prompts tree is
  `@REVIEW_PROMPTS@`, resolved by `setup.sh --review-prompts` and
  documented in [docs/configuration.md](docs/configuration.md) §
  Kernel review prompts; there is exactly one such tree, so do not add
  a second way to locate it.

  This covers every file the repo ships: `AGENTS.md` and the other
  root markdown procedures, `docs/**`, `skills/**`, `configs/**`, Rust
  string literals, and `setup.sh` itself. An absolute home path in any
  of them resolves on one machine and silently fails everywhere else,
  and the reader cannot tell a wrong path from a missing file.

  Two exceptions, both narrow. Analysis notes that record what was
  measured — `validation-bugs.md`, `todo-updates.md`, the `*-design.md`
  drafts — cite the tree the evidence came from, and that citation is
  the point. Usage examples in `--help` text and docs may show a
  concrete path when it is visibly an example (`e.g. /home/you/...`).
  Neither is an instruction to read a file.

  Commit `d0b28b2` ("kres: require the agent-parseable writing
  rules") is why this is written down. It added "Follow
  `~/local/src/review-prompts/WRITING-SKILL.md`" to the Writing
  section below, on a repo that already had `@REVIEW_PROMPTS@` and a
  `--review-prompts` install flag for that exact tree, and the same
  slip had already shipped twice in `kres-review-inline-template.md`.
  Before committing a doc or a prompt, grep the diff for `/home/` and
  for `~/`.

### Writing

Follow `@REVIEW_PROMPTS@/WRITING-SKILL.md`, where `@REVIEW_PROMPTS@`
is the review-prompts tree `setup.sh --review-prompts` names (see
[docs/configuration.md](docs/configuration.md) § Kernel review
prompts). It governs every
string someone parses without its author present: the prompts under
`configs/`, tool and error text, `[tag]` progress lines, commit
messages, and this file. Write so that exactly one reading is possible.

Five rules break most often in this repo.

- Lead with the command, or with the `If <condition>:` that gates it.
  A reason may ride along in a subordinate clause. Do not spend a
  whole sentence on rationale before the reader knows what to do.
- Hold one new proposition per sentence, two at most. A trailing
  "which", a bare appositive, a semicolon, and a colon before a second
  independent clause each smuggle in a third. Close the sentence
  instead.
- Name the relation between adjacent claims: "so", "since", "yet",
  "once", "unless". Four sentences opening with a bare subject means
  the connectives were deleted and have to go back.
- Use one term per concept. Pick check or verify or validate, then
  reuse it for the same action everywhere in the document.
- Never strengthen a hedge while shortening. "may fail" does not
  become "fails", and a shorter sentence must not claim more than the
  source did.

Read a draft back against the skill's second half before shipping it.
Both failure modes are real and they are symmetric, so a check that
hunts only long sentences drives you into a paragraph of bare
declaratives that connects nothing.

For a commit message the kernel template in the operator's global
instructions still sets structure and trailer order. The skill adds
what a template cannot: lead with the symptom rather than the
mechanism, say so when you reproduced the defect, and describe the
defect rather than only the edit. A message can agree perfectly with
its diff and still misdescribe the bug.

### Lints
- The pre-commit hook runs `cargo clippy --workspace --all-targets
  -- -D warnings`. Do not silence a clippy diagnostic with
  `#[allow(clippy::...)]` (or any other lint-suppression attribute)
  to make the hook pass. Fix the underlying issue instead:
  `clippy::too_many_arguments` → bundle related args into a struct
  or split the function; `clippy::while_let_loop` → rewrite as the
  suggested form; `clippy::needless_clone` → drop the clone; and so
  on. Suppressions are only acceptable when there is a concrete
  reason the lint is wrong for that call site, in which case a
  one-line comment above the attribute must say why. A `#[allow]`
  with no explanation is treated the same as an unfixed warning.
- This rule applies even when an existing `#[allow]` is already in
  the file. Pre-existing suppressions are technical debt, not a
  precedent.

### Async REPL
- Input runs in a separate thread (readline → queue)
- Main loop: 100ms poll cycle checking input queue + servicing tasks
- `async_print()` clears readline line before printing to avoid garbled output
- All background output (task status, results) uses `async_print`

### Ratatui TUI
- Read ratatui's widget implementation before changing rendering or
  scrolling behavior. Do not infer viewport behavior from terminal
  intuition.
- `Paragraph::wrap(Wrap { .. })` reflows logical input lines through
  ratatui's `WordWrapper`; `Paragraph::scroll((y, x))` is applied
  after wrapping, so `y` is a rendered-row offset, not a stored-line
  index.
- For scrollback panes that wrap text, do not pre-slice the last N
  logical lines and then hand that to `Paragraph`; older wrapped lines
  can consume the viewport and clip newer output. Build the paragraph
  from the full retained scrollback, compute `Paragraph::line_count(width)`,
  and use `Paragraph::scroll` to follow the rendered tail.
- `Paragraph::line_count` is behind ratatui's
  `unstable-rendered-line-info` feature. Keep that feature enabled
  rather than copying or approximating ratatui's private wrapping logic.
- Tests for scrollback visibility must render through ratatui
  (`TestBackend`/`Terminal` or equivalent) and assert on the rendered
  buffer. Tests that only validate local line-count approximations are
  not sufficient for TUI scrolling bugs.

### Task System
- Each todo item becomes a `Task` with its own thread
- Task states: `pending → inference → waiting_main → gathering → done`
- `TaskManager` handles scheduling (respects `depends_on`), servicing, reaping

### Requeue: fetching evidence without giving up the slot

A slow agent that names evidence it did not have has not failed — it
has said the gather stopped one hop short. That request used to become
a followup: a new todo row, ranked against every other row, whose
answer arrived (if ever) in a different task with a different brief.
On the 2026-08-19 `arch/x86/kvm/mmu` review a lens asked for
`make_mmu_pages_available`, the body landed four minutes later inside
an unrelated task, and the task that needed it never saw it.

A **requeue** fetches it instead and re-analyses, up to
`followup::MAX_TASK_REQUEUES` (3) times per task.

- It is not a retry. The response was valid; nothing is being
  corrected. What repeats is fetch-then-analyse.
- The slot is kept for free. `TaskManager::spawn` holds the semaphore
  permit across the whole of `work`, so looping inside it cannot admit
  another task, and nothing is reaped or published until the loop
  settles. Do not implement a requeue by finishing the task and
  re-dispatching: that returns the slot and re-enters ranking, which
  is the failure being fixed.
- What qualifies is decided from typed fields only, never prose
  (`followup::requeue_evidence_requests`): `required_for_progress`,
  because that is already the declared marker for a blocking evidence
  request; kind is not `question`, because no fetcher can satisfy one
  and a task that asked only questions must not spin; and the request
  has not already been served this task, so re-asking cannot buy
  another round.
- Each round seeds the next gather with the previous round's symbols
  and context. A requeue must never leave a lens seeing less than the
  round that raised the question.
- The plan's opening step never requeues. It builds the map the rest
  of the plan is written against and later steps wait on it, so a
  round there is serial time the whole run pays; the evidence it would
  add is evidence the parallel steps fetch for themselves. On the
  2026-08-19 arch/x86/kvm/mmu review the inventory step was the only
  task to run in nineteen minutes, spending two requeues while nothing
  else could start. The test is positional
  (`followup::is_opening_plan_step`) — a plan step id is
  model-generated prose and must not be matched on.
- A round fetches every qualifying request, with no cap. The fetches
  are main-agent/semcode work while the round costs a gather plus a
  full lens fan-out, so bounding them saves almost nothing. A cap of
  three was tried and removed: the caller pools all lenses' requests
  before selecting, so truncation gave the whole budget to whichever
  lens came first. On the 2026-08-19 arch/x86/kvm/mmu review the
  `general` lens twice named `__kvm_mmu_prepare_zap_page` as its FIRST
  request and lost both times to `memory-lifetime`'s list; that body
  was never fetched across nine blocking requests, while the ordinary
  gather in the same run pulled 148 distinct symbols.
- The budget is small because each round costs a gather plus a full
  lens fan-out. A chain still unresolved after three hops belongs in a
  followup for a future task, not in more rounds here.

Wired into the lensed path (`AgentRunner::run_with_lenses`), which is
what `/review` uses. `run_once_with_ctx` does not requeue yet.

### Dispatch Backpressure: the Start Budget

The shared todo list is the resource being protected, but protecting
it by making dispatch WAIT turned out to cost more than it bought.
Two rules were tried and removed; do not reintroduce either.

- **Removed: "no task starts while any task runs."** Requiring
  `snapshot().is_empty()` cost 33% of wall-clock time idle.
- **Removed: "no task starts while the reap queue is non-empty."**
  This serialised every new task behind a ~65s publication. Measured
  on the 2026-08-06 aug6-4 run: half the post-reap refills were
  refused with `N task(s) waiting to be reaped`, handing back much of
  the idle time the change had just reclaimed.

What bounds dispatch now:

- **The slot cap** — `--max-parallel` (default 10), owned by
  `TaskManager` and read through `max_parallel()` / `free_slots()`.
  Do not carry a second copy in `ReplConfig` or anywhere else, or the
  number that gates `spawn` and the number that sizes a dispatch will
  drift. A cap of 0 is refused at startup.
- **The start budget** — at most `max_parallel` tasks may start
  without a reap batch completing (`Inner.starts_since_reap`, re-armed
  by `note_reap_completed()` AFTER publication, not after `reap()`).
  The reaper shares one rate limiter with the tasks it is publishing;
  without this, a stream of fast tasks keeps claiming work while the
  reaper never gets a turn.
- **The turn budget** — the `--turns` remainder minus in-flight tasks.

All three are applied inside `claim_ranked_todos`, under the one write
lock that also flips the rows. Do not compute a bound in the caller
and pass a limit down: two dispatches could each read "5 free" and
each claim 5.

**Claiming during a reap is safe** because the todo list is
Rust-owned and every mutation takes that write lock. The one hazard —
a row claimed while the todo agent's round trip is in flight — is
handled by `merge_inferred_state`, which restores the live status of
any row that went InProgress or terminal after its inference snapshot
was taken (`inferred_todo_cannot_redispatch_work_started_after_snapshot`).

Other invariants:

- **One dispatch path.** `/continue`, `/next` and the post-reap refill
  all go through `Session::dispatch_ready`. Do not add a second
  claim-and-submit sequence.
- The reaper decides WHEN to refill, signalling over an
  `mpsc::channel(1)` that carries nothing. `submit_prompt_inner`
  reaches change-survey closures whose futures rustc cannot prove
  `Send`, so it cannot be awaited inside the reaper's spawn. Fix that
  first if you want to move it.
- **The bound covers pipeline dispatch, not operator submissions.**
  `submit_prompt` and `dispatch_workflow` start tasks directly, as
  they did before this design. Deliberate — an operator typing a
  prompt is not waiting on a reap.
- Auto-continue is NOT `/continue`. `Session::auto_continue` skips the
  two side effects that are statements of operator intent: clearing
  the `/stop` latch, and pulling the deferred ledger back into the
  todo list.

### One Reap Batch, One Reconciliation

`reap()` drains every terminal task in one call, and the todo and
goal agents then run **once over the whole batch**, not once per
task.

- Per-task work stays per-task: publication, coding side effects,
  `effective_analysis`, the report append, promote, `apply_delta`,
  the findings signature, and the consecutive-error watchdog.
- Batch work is the part that reasons over the shared list: one
  todo-agent call and one goal check per client group. A batch mixing
  lensed-review and non-review tasks costs two rounds, never N.
- The goal is per-task in storage but almost always identical across
  a batch (pipeline submissions inherit the cached session goal), so
  the check runs once per DISTINCT (goal, prompt) pair.
- This is a correctness change as well as a latency one: two parallel
  tasks routinely emit the same followup, and sequential rounds can
  only dedup the second against the first after it has already become
  a row.

### Shared Symbol Cache
- `TaskManager.symbol_cache` and `context_cache` are thread-safe (via `cache_lock`)
- Tasks seed from cache at startup — avoids re-fetching known symbols
- Source followups served from cache skip the main agent entirely
- Cache populated after service thread gathers data and when tasks are reaped

### Todo List with Completed History
- Completed items stay in the list as `status=done`
- All followup→todo additions go through `_update_todo_via_agent` (inference call)
- Main agent sees done items and won't re-add equivalent work
- `todo_lock` protects all list mutations from concurrent access
- Done items preserved even if main agent drops them from its response

The todo list is Rust-owned and the agent's reply is a set of edits
against it, not a rewrite of it (`kres-agents/src/todo_agent.rs`,
`reconcile_update`). The agent controls prose (`name`, `reason`,
`type`), completion (`newly_done`) and retirement (`retired`). It
controls nothing else:

- `todo` carries the PENDING list only. Done rows are reconstructed
  from Rust's own copy, so the agent never re-emits them.
- The request's `completed` is an ARRAY — one entry per task in the
  reaped batch, each with `{query, analysis, followups}` plus
  `just_completed` when that task owned a row. `newly_done` must
  carry one coverage sentence per completed entry, drawn from that
  entry's own analysis. `InferredTodoUpdate.completed_todo_ids` is
  likewise a set. Do not narrow either back to a single task.
- ORDER is not a channel. The pending list is stable storage:
  surviving rows keep their position and new rows are appended.
  Choosing what runs next belongs to the prioritization agent.
- `id`, `step_id` and `depends_on` on an existing row are restored
  from the original. Do not ask the agent to re-emit fields Rust
  overwrites; that is pure output cost.
- Coverage is write-once, at the completion that first marks a row
  Done. Later rounds cannot paraphrase settled evidence. Enforced in
  `merge_inferred_state` as well as the agent path, because the
  manager owns the list.
- The placeholder is NOT settled evidence. `mark_todo_done` stamps
  `PLACEHOLDER_COVERAGE` the moment a task completes, which is before
  the todo agent is ever asked to describe it, so write-once must test
  `coverage_is_unwritten` rather than `is_empty()`. Guarding on
  emptiness meant the placeholder always won: on the 2026-08-07
  kernel/sched/fair.c review all 74 done rows stored it while the
  agent had returned real coverage for 35 of them, and the DEDUP step
  that reads coverage to drop already-covered followups was blind for
  the whole run. Keep the fallback — a done row with EMPTY coverage
  drops out of that comparison entirely, which is worse — but never
  let it outrank the agent's sentence.
- Omission is not deletion. A pending row the agent neither kept,
  completed, nor retired is restored and the drop is logged.
  Deleting work requires naming it in `retired`.

Do not widen this contract back out. If the agent needs to change
something, add a typed channel for it; do not let it restate the
whole list and have Rust guess which differences were intentional.

### Prioritization Agent

Ranking is its own agent (`kres-agents/src/prioritize.rs`), split out
of the todo agent. It runs on the **slow agent** — same client, model
and token budget, derived in `Session::with_agent_runner` so the two
cannot drift apart via separate config — under the same system prompt
the session's lenses use, selected per call from `Plan.mode` via
`AgentRunner::slow_system_for_mode`. A review is `Audit`, which uses
`slow_system`, NOT `slow_coding_system`. That is not cosmetic: the
system block is part of the Anthropic cache prefix, so a mismatch
makes the shared cache block below impossible.

- Input is the ready rows only (`TaskManager::ready_pending_snapshot`):
  nothing done, running, retired, or blocked on an unfinished
  dependency. Plus the session question, the findings so far in full,
  the skills, and the plan — the same material the slow agents reason
  over.
- Output is at most N ids, best first, each with a one-line rationale
  that is logged and not otherwise consumed. N is the concurrency cap
  (`--max-parallel`, default 10) — ranking further ahead is output the
  next refresh overwrites before anything reads it.
- It cannot edit, complete, retire, merge, or invent work. Ids not in
  the ready set are dropped with a log line; duplicates and
  over-budget picks are truncated.
- **Nothing ever waits for it.** The ranking is a stored artifact
  (`Inner.ranked_order`, a preference list of ids — NOT a reordering
  of the todo list), refreshed detached after each reap batch by
  `Session::spawn_ranking_refresh`. Dispatch reads it via
  `claim_ranked_todos` and claims under one write lock with no
  inference call at all. Do not put the ranking call back on the
  dispatch path: at 17.5s and growing with findings, it idles a slot
  for its whole duration and destroys the amortisation that made one
  ranking authorise ten dispatches.
- Consuming a slightly stale order is safe by construction:
  `claim_ranked_todos` re-validates status and dependencies under the
  write lock, so staleness can only mean a suboptimal pick, never an
  invalid one. The refill right after a reap deliberately uses the
  order computed before that batch landed.
- It never runs under the manager's write lock —
  `ready_pending_snapshot` then rank then `set_ranked_order`.
- When it is unavailable, fails, or returns nothing usable, the
  previous order stands and unranked rows dispatch in storage order.
  Ranking is an optimisation and must never stall a wave.
- A refresh with every ready row fitting the depth skips the call
  entirely — there is nothing to rank.
- Its ONE cached block is the lens session head
  (`prompt::session_cache_head`, `{common_skills, previous_findings}`),
  byte-identical to what the wave's lens fan-out sends and read by it
  seconds later. Findings must be passed already redacted with
  `redact_findings_for_agent`, and skills as the common
  (task-independent) half, or the head diverges and buys an extra
  write of the largest payload in the request instead of a share.
  Route both callers through the one constructor; do not add a second.
- There is deliberately no prioritize-specific cached block. One was
  measured and removed: calls 943s and 783s apart against a 300s
  ephemeral TTL meant the entry expired every time — 21,886 tokens of
  cache creation per call against zero reads. Do not add a cached
  prefix to a call whose own cadence exceeds the TTL.

Do not put ranking language back into the todo agent prompt. Two
agents ordering one list is how the list stops being stable storage.

### Goal System
- Before processing, main agent defines a concrete completion goal
- After slow agent finishes, main agent checks if goal is met
- Goal met → suppress followups → no new todos → work stops
- Goal not met → only missing items become followups
- Auto-progress checks goal after each completed task for early exit
- Deferred items (identified but not started when goal met) saved via `/followup`

### Session Termination (`--turns`)

`--turns N` controls when the REPL exits. The counter is
`completed_run_count`, incremented in `TaskManager::finish_ok`
(kres-core task.rs) when a reaped task produced non-empty
`analysis`, `code_output`, or `code_edits`:

```rust
let produced = !entry.analysis.is_empty()
    || !entry.code_output.is_empty()
    || !entry.code_edits.is_empty();
if produced {
    g.completed_run_count = g.completed_run_count.saturating_add(1);
}
```

One completed task = one fast+slow agent cycle for a single todo
item.

**`--turns N > 0`**: stop launching new work after N completed
task runs. Once `done >= turns_limit` in the reaper loop, it
drains pending/blocked todos to `/followup` and latches the turns
cap so auto-continue cannot start more work. Already-running tasks
are allowed to finish and be reaped before root shutdown is
cancelled, because `completed_run_count` can reach the cap while
parallel tasks still have findings to merge into `findings.json`.
The cap is checked before continuation-only LLM calls: the reaper
still publishes the completed task's findings/report/state, records
any emitted followups locally, and drains them to `/followup`, but it
must not call the todo or goal agents after the cap has fired.

**`--turns 0` (default, unlimited)**: stop condition is computed
in the reaper's `else` branch (`// --turns 0 (unlimited)`):

```rust
let should_stop = if follow_followups {
    followups_drained || no_progress
} else if goal_configured {
    followups_drained
} else {
    no_goal_batch_stop
};
```

Where `followups_drained = active == 0 && pending_or_blocked == 0`,
`no_progress = no_new_findings_streak >= 3`, and
`no_goal_batch_stop = !goal_configured && !follow_followups && active == 0`.

So:
- With `--follow`: stop on drained OR 3-run stagnation streak.
- With goal agent (no `--follow`): stop **only** when drained.
  The stagnation streak is ignored.
- Without goal agent or `--follow`: stop when `active == 0`
  (batch finished), defers leftover followups.

`exit_on_idle` (set in main.rs `ReplConfig` init) controls whether
the REPL exits on stop or stays open for more input. True when
stdout is not a TTY (piped/batch) or `--one` is passed:
`args.one || !std::io::IsTerminal::is_terminal(&std::io::stdout())`.

**Goal-met drain**: when `check_goal` returns `check.met == true`,
the reaper calls `drain_pending_blocked()`, moving all
pending/blocked items to deferred. This makes
`pending_or_blocked == 0` so `followups_drained` fires on the
next reaper tick. Exception: when `--follow` and `--turns N > 0`
are both set, the goal-met handler immediately pulls deferred
items back into the todo list as Pending so auto-continue
dispatches them and the session keeps working until the turns cap
is reached.

**Implication**: under `--turns 0` with a goal agent, a todo item
stuck at `Pending` does not block termination if the goal agent
declares "met" (the drain clears it). But if the goal agent keeps
saying "not met" while a todo is stuck `Pending`, the session
cannot self-terminate — `pending_or_blocked > 0` forever and the
stagnation watchdog only fires with `--follow`.

**`--resume`**: loads a prior `session.json` (plan, todo list,
deferred list, completed_run_count). Deferred items are
automatically pulled into the todo list as Pending so
auto-continue can dispatch them immediately. When `--resume`
loads successfully, `--prompt` is ignored (with a stderr warning)
to prevent `define_goal` + `define_plan` from overwriting the
resumed plan.

**`--stdio` auto-detection**: `cfg.stdio` is set automatically
when stdout is not a TTY. This suppresses DECSTBM scroll-region
escape sequences that would otherwise pollute piped/redirected
output. The explicit `--stdio` flag still works as an override.

`--gather-turns` (default 5) is a **separate** cap: max fast↔main
gather rounds within a single task before forcing the slow agent.
Per-task, not per-session.

### Plan + Session Persistence
- `kres_core::Plan` holds the planner's decomposition: `prompt`, `goal`,
  `mode`, and `steps` (each with `id`, `title`, `status`, `todo_ids`
  linking to `TodoItem` rows). Lives on `TaskManager` as
  `Option<Plan>`; `sync_plan_from_todo` rolls up step status from
  linked todo statuses.
- `kres_core::SessionState` (`<results>/session.json`) is the
  resumable snapshot: plan + todo list + deferred list +
  `completed_run_count` + last prompt. Written atomically (tmp +
  fsync + rename) from the reaper tick and the various drain
  paths.
- Resume: `kres --results <dir> --resume` loads the snapshot,
  flips every `InProgress` todo/plan step back to `Pending` (its
  prior executor is gone), and seeds the manager + deferred list
  before the REPL starts. Without `--resume`, any existing
  session.json is left untouched on disk and the REPL starts
  clean; a note in the startup banner points at the file so the
  operator knows the state is recoverable.
- InProgress drains: ctrl-c, goal-met, and `--turns 0`
  follow-stagnation call
  `TaskManager::reset_in_progress_to_pending()` before moving items
  to the deferred list, so a task that was mid-run when the drain
  fired still ends up on `/followup` instead of being orphaned.
  The `--turns N` cap is different: it drains only Pending/Blocked
  items, blocks auto-continue, waits for InProgress tasks to finish
  and publish their outputs, then exits.
  `/stop` is separate: it moves `Pending|Blocked|InProgress` items
  to deferred directly via its own `matches!` pattern
  (kres-repl/src/session.rs), so a resumed REPL picks them up via
  `/continue`.

### Workflows

`docs/workflow.md` is the user-facing guide: what the four
workflows do and how a run progresses. `docs/workflow-internals.md`
is the source of truth for all four — `/fix`, `/review`, `/triage`
and `/validate` — plus workflow runner behavior, reaper actions,
retry semantics, and shipped workflow invariants. Do not add a
second per-workflow description anywhere else: a standalone
`/validate` snapshot existed once and went stale against the
workflow it described. Keep this section short and update
`docs/workflow-internals.md` when workflow behavior changes; update
`docs/workflow.md` when the change is visible to an operator.

### Workspace and Mentioned Paths

- The configured workspace is implicitly readable and writable by
  kres tools.
- If the operator mentions an existing absolute file or directory
  outside the workspace in a prompt, kres grants session-scoped
  read/write access to that file's parent directory or to that
  directory itself.
- The same grant is used by read, edit, code_output, and workflow
  reaper paths. Restarting kres or `/clear` drops the grants.

### Skills
- Scanned from `~/.kres/skills/*.md` at startup; automatic skills are
  selected by workspace detection (currently kernel and systemd)
- Skill files scanned for absolute paths in backticks — referenced files pre-loaded
- Full skill content + pre-loaded files sent to code agent as `skills` field in JSON
- Code agent can request additional files via `skill_reads` in response

## Configuration

All configs live in `~/.kres/`, installed there by `setup.sh` from
this repo's `configs/` tree:

| File | Purpose |
|------|---------|
| `models/<model-id>.json` | Model/role config selected by `settings.json` or CLI model flags. Uses `api_key`, provider fields, max_tokens, rate_limit, thinking, and optional role sections (`fast`, `slow`, `main`, `todo`). |
| `mcp.json` | MCP server definitions (installed only when semcode-mcp is available) |
| `settings.json` | Per-user defaults for per-role model ids. Optional `models.slow_secondary` adds a supplemental review model (`general` for `/review`, `maintainer` for `/fix`). CLI flags `--fast-model`, `--slow-model`, `--main-model`, and `--todo-model` override matching roles. Any explicit `--slow` selection replaces the configured slow pair; repeat or comma-separate it to select multiple models. Without `--compare`, the first model runs all lenses and later models run only the supplemental lens. `--compare` runs every lens on every selected slow model. `--slow` and `--slow-model` are mutually exclusive. Optional `max_parallel` sets how many tasks run at once (default 10, 0 = unbounded); `--max-parallel N` overrides it. |
| `system-prompts/*.system.md` | Optional operator overrides for agent system prompts. Default prompts are embedded in the kres binary (`kres-agents/src/embedded_prompts.rs`); a file at `~/.kres/system-prompts/<basename>` shadows the embedded copy. Empty by default |
| `commands/<name>.md` | Optional operator overrides (or additions) for non-workflow slash-command templates. Summary rendering reads `summary` / `summary-markdown` templates through `kres-agents/src/user_commands.rs`; workflow-owned names (`fix`, `review`, `triage`, `validate`) are reserved and cannot be resurrected as prompt templates. |
| `workflows/<name>.json` | Optional operator overrides for shipped workflows such as `fix`, `review`, `triage`, and `validate`. Disk overrides shadow embedded workflow JSON. |
| `skills/*.md` | Domain knowledge files |

Rate limiters are shared across agents that use the same API key string.

Agent config files may set a per-call thinking override:
`"thinking": {"type": "adaptive", "effort": "medium"}`,
`"thinking": {"type": "enabled", "budget_tokens": 32000}`, or
`"thinking": {"type": "disabled"}`. When omitted, kres uses the
model-aware default. Use this for models whose API contract requires a
specific thinking request shape instead of hardcoding private model
names in source.

The whole-file risk scan that precedes a source-target review is one
inference call, not two. The six-month change survey rates every
target function against the net diff; Rust then assembles the scan the
planner sees — ratings from the change survey, spelling counts and the
authoritative function list from the semcode `file_survey` fetch,
`file_risk_rating` as the highest function rating, and one research
question per external interaction Rust itself established.

There used to be a second slow-agent call, the "file survey", that
combined the two. It was removed after measurement, and should not be
reintroduced without new evidence:

- It was shown the change survey's ratings in its prompt and forbidden
  from rating any function below them. Of 236 functions on the
  2026-08-06 mm/page_alloc.c review it changed 12, all upward, one
  crossed into the high band, and none of the 12 produced a finding.
- Run standalone, without the change survey's report enumerating the
  function set, it returned 251 functions instead of 236 and failed
  Rust's `did not rate every target function exactly once` validation
  on two consecutive runs. Its apparent value included re-typing a
  list Rust already held and validated against.
- It cost 19k input tokens and ~78s of serial bootstrap per review.

The semcode `file_survey` FETCH is a different thing and stays, along
with `infer_fallback_file_survey_inventory`, the inference fallback
used when semcode is unavailable or unparseable. Deleting that would
violate the rule that a failed semcode result falls back to local
evidence rather than being treated as absent.

The whole-file change survey is a broad first guess, not an
inventory. It reads the six-month net diff and returns risk ratings.
Whatever comes back is what the scan uses:

- A rating naming something that is not a target function is dropped
  (`retain_known_functions`).
- A function the survey never mentions stays unrated, and the scan
  renders it as risk 0.
- It never fails the review, and there is no corrective re-run.

Large targets are still partitioned so a call's INPUT fits, and Rust
unions the partitions (`merge_change_survey_reports`, highest rating
per name). That is the only assembly step; no model reassembles a
roster Rust already holds.

There used to be a coverage contract, and it was wrong three separate
ways on kernel/sched/fair.c (421 functions), each of which killed the
whole review bootstrap over a heuristic:

- demanding an exact roster produced an invented
  `__account_cfs_rq_runtime_placeholder`;
- demanding exactness per 150-name batch discarded 147 correct
  ratings for being three short;
- demanding each batch stay inside its own slice rejected
  `__min_slice_update` and `detach_tasks`, both real functions the
  model rated unprompted.

Do not reintroduce one. If the survey's output looks thin, that is a
prompt question, not a validation question — the downstream review is
what establishes coverage, and it does so with evidence.

`--assisted-by TEXT` controls the exact commit-message trailer value
used by the fix workflow after `Assisted-by:`. When omitted, kres derives
`kres:<slow-model-id>` from the resolved slow agent model.

## REPL Commands

| Command | Action |
|---------|--------|
| `/tasks` `/task` | Show active tasks and states |
| `/todo` | Show pending items (ready/blocked) + completed count |
| `/plan` | Show the current plan + per-step status (produced by `define_plan`) |
| `/resume [PATH]` | Load a persisted `session.json` (defaults to `<results>/session.json.prev` → live file). Overwrites in-memory state |
| `/todo --clear` | Clear all todo items |
| `/cost` | Token usage by agent role and model |
| `/summary [FILE]` | Run the existing `validate` workflow for every finding, then have the fast agent render only validated summaries through the embedded `summary` template. Output defaults to `summary.txt`; validation artifacts live under `<results>/summary-validation/`. Auto-chunks oversized render inputs and combines the partials |
| `/summary-markdown [FILE]` | Same as `/summary` but uses the `summary-markdown` template and defaults the filename to `summary.md` |
| `/review <target>` | Run the embedded `review` workflow for `<target>` — CLI equivalent of `--prompt 'review: <target>'`. The shipped workflow defines the review prompt contract and lenses; execution uses the REPL task/todo loop so followups become next-turn review todos. This is workflow-only; no markdown prompt fallback exists |
| `/triage <finding-dir>` | Run the embedded `triage` workflow for a kres-exported finding directory. The workflow includes the golden triage template, preserves followups, and validates that `summary.md` was actually written. This is workflow-only; no alternate prompt path exists |
| `/validate <finding-dir> [source-workspace]` | Run the embedded `validate` workflow for a kres-exported finding directory against source workspace (default `.`). Three steps: the fast coding agent validates the finding's claims with typed provenance, a second fast step tests whether the surviving preconditions can hold simultaneously, and the slow coding agent proves reachability and writes `summary.md` plus severity updates like `/triage`. Both the claim step and the verdict step are gated by Rust-side `builtin` evals, not prompt text — see [docs/workflow-internals.md](docs/workflow-internals.md). This is workflow-only; no alternate prompt path exists |
| `/fix <target>` | Run the embedded `fix` workflow for `<target>` — CLI equivalent of `--prompt 'fix: <target>'`. `fix` is workflow-only; no slash-command template or alternate prompt path is used. Drives the research → write-patch → write-commit-message → commit → build → triage/review → publish pipeline (see [docs/workflow-internals.md](docs/workflow-internals.md)) |
| `/report <file>` | Write all findings to markdown file |
| `/followup` | Show deferred items (identified but skipped when goal met) |
| `/next` | Run next todo item |
| `/continue` | Resume interrupted work or continue todo processing |
| `/done N` | Remove todo item N |
| `/reply <text>` | Prepend last response to new prompt |
| `/load <file>` | Inline file contents into prompt |
| `/edit` | Open $EDITOR for prompt (also ctrl-g) |
| `/clear` | Reset all state |
| `/quit` `/exit` | Exit |

## Gotchas

### JSON Parsing
- Code agent sometimes outputs prose before JSON — `_extract_json()` uses brace-matching fallback
- `parse_code_response()` tries: whole text → fenced blocks → brace matching
- Never replace text with fenced content unless it parses as valid JSON with `analysis` key

### Tool Field Names
- Main agent sends `"path"` but tool handler expects `"file"` — accept both
- Main agent sends `"startLine"`/`"endLine"` — accept alongside `"line"`/`"count"`
- All values coerced to int with try/except for robustness

### Rate Limiting
- Shared `RateLimiter` when agents use same API key (same workspace limit)
- On 429: count tokens to distinguish provider/model input capability from
  shared rate limiting. Never shrink or delete request content; partition only
  naturally partitionable inputs and preserve every byte.
- 8 retries with exponential backoff, retry-after header support

### Token Management
- Request construction never trims, caps, or deletes inference input.
- Cheap estimates and exact `count_tokens` calls are diagnostics or choose
  lossless partition boundaries for naturally partitionable inputs.
- `max_input_tokens` describes a provider/model capability. It is not a Kres
  request ceiling and does not authorize shortening a request.

### Thread Safety
- `todo_lock` on TaskManager protects todo_list mutations
- `cache_lock` protects symbol/context cache
- `_print_lock` in `async_print` prevents output interleaving
- Task state changes via `set_state` use per-task `state_lock`
- MCP `call_tools_bulk` pipelines requests but collects responses by ID (out-of-order safe)

### Terminal and Ctrl-C
- TUI mode enables crossterm raw mode, which clears terminal-driver
  `ISIG`; while raw mode is active, Ctrl-C is a key event, not a
  kernel-generated SIGINT.
- The TUI Ctrl-C handler sends kres itself `SIGINT` so the shared
  Tokio Ctrl-C path can cancel/persist the session.
- Any hard-exit path that calls `std::process::exit` must first call
  `tui::emergency_restore_terminal()` and `status::restore()`.
  `process::exit` skips `Drop`, so relying on `TuiGuard` there leaves
  the controlling terminal in raw mode and makes Ctrl-C stop working in
  the parent shell.

### Git Commands
- Readonly whitelist: log, show, diff, blame, annotate, etc.
- Uses `shlex.split()` for proper quote handling
- Unknown subcommands rejected with error listing allowed ones

## File Layout

```
~/.kres/                      # Populated by setup.sh
  models/
    claude-sonnet-4-6.json    # Default fast/main/todo model config
    claude-opus-4-7.json      # Default slow model config
  mcp.json                    # MCP server registry (semcode, …)
  settings.json               # Per-user defaults (model ids per role)
  system-prompts/             # Optional agent system prompt overrides
  commands/                   # Optional non-workflow command overrides
  workflows/                  # Optional workflow JSON overrides
  skills/                     # Skill files (kernel.md, …)
  sessions/<ts>/              # Per-run artifacts when --results not set
    findings.json             # jsondb-backed canonical findings (delta-applied, no history)
    report.md                 # Append-only narrative
    session.json              # Plan + todo + deferred + counters (resume state)
    summary.txt               # Output of /summary or kres --summary (summary.md with --summary-markdown)

.kres/logs/<session-uuid>/    # Next to cwd, one dir per REPL run
  code.jsonl                  # All fast + slow agent turns
  main.jsonl                  # All main agent turns
```

`console.jsonl` is the transcript of everything `async_println`
emits: dispatch decisions, reap batches, promote/goal/todo verdicts,
rate-limit notices. It exists because that narrative used to live
only on the operator's terminal, so nothing reading a finished run
could explain WHY the scheduler did what the other two logs show it
doing. The tee lives inside `kres_core::io::async_println` — one hook
covers everything precisely because every progress line goes through
that function and nowhere else. A transcript sink must never call
`async_println` itself; it runs inside it and would recurse.

Note: `~/.kres/sessions/` holds per-run artifacts (findings.json,
report.md, session.json) but NOT the JSONL logs. The JSONL logs
live only in `<cwd>/.kres/logs/<uuid>/`, created by `TurnLogger::new`
(kres-core log.rs). The uuid is derived from pid + timestamp via
uuid5 so parallel kres processes don't collide.

## Reading JSONL Log Files

Both `code.jsonl` and `main.jsonl` are newline-delimited JSON. Each
line is a `LogEntry` with an RFC 3339 UTC `timestamp`, `role`, `content`,
and optionally `usage` (token counts) and `thinking` (slow agent reasoning).
Subtract matching user and assistant timestamps to measure a model call;
overlapping intervals show concurrent calls.

### console.jsonl

One record per progress line: `{timestamp, line}`. Grep it to
reconstruct scheduling behaviour — `[dispatch`, `[todo update]`,
`[goal check]`, `[promote]`, `[findings]`, `[prioritize]`.

### code.jsonl

Alternating user/assistant records for the fast+slow agent pipeline.

**User records** (`role: "user"`): the `content` field is a JSON
string containing the newest prompt turn assembled by the pipeline. Multi-turn
fast-gather calls also carry `request_content`, an ordered JSON representation of
the complete model-visible conversation; `context_stats` accounts for that complete
request. Key prompt fields:

| Field | Description |
|-------|-------------|
| `question` | The task prompt (e.g. `COMPILE TRIAGE ONLY ...` or the original user prompt) |
| `plan` | Current plan with `steps[]`, each having `title` and `status` (`pending`/`done`/`skipped`) |
| `skills` | Loaded skill file contents |
| `symbols` | Source code gathered by the main agent |
| `context` | Additional context (prior analysis, tool results) |

**Assistant records** (`role: "assistant"`): `content` is either
structured JSON (fast agent) or raw prose (slow agent).

Fast agent JSON — keys:

| Field | Description |
|-------|-------------|
| `analysis` | Free-text narrative of what the agent found/decided |
| `followups` | Array of `{type, name, reason}` — data requests or actions. Types: `read`, `source`, `git`, `make`, `search`, `publish-fix`, `bash`, `callers`, `question` |
| `ready_for_slow` | `true` = fast agent is done gathering, hand off to slow agent |
| `skill_reads` | Additional skill files to load |
| `code_edits` | Array of `{path, old_string, new_string}` surgical edits (coding mode) |
| `code_output` | Array of `{path, content}` file writes (coding mode) |

Slow agent raw text — not valid JSON. This is the deep analysis
or review output. Starts with `[INVALID]` when the bug is
determined to be not real. In review steps, may contain `DEFECT`
markers.

### main.jsonl

Alternating user/assistant records for the main agent, todo agent,
and goal agent.

**Main agent assistant responses**: either `<actions>[...]</actions>`
XML containing data-fetch requests (`read`, `source`, `git`, `grep`,
`mcp`, `make`, `bash`), or JSON with `goal`/`mode` (initial goal
definition) or `todo` (todo-list updates).

**Todo agent responses** — JSON edits against the Rust-owned list.
Logs from before this contract carry the full list under `todo`
instead; both shapes parse.

| Field | Description |
|-------|-------------|
| `todo` | Pending rows only, order-insensitive. An unchanged row is just `{"id":"..."}`; `name`/`reason`/`type` appear only when being edited, and an absent field means unchanged. A row absent from `current_todo` is new and carries `name` (required) plus `type`, `reason`, `depends_on`, `step_id` |
| `newly_done` | `[{id, coverage}]` — completions. `coverage` is written once, at this completion |
| `retired` | `[{id, reason}]` — pending rows deliberately abandoned. Logged, not stored |
| `plan` | Optional `{steps:[...]}` rewrite |

A pending row that appears in `current_todo` but in none of the three
arrays is restored by `reconcile_update` and logged as "dropped … live
item(s) without retiring them".

**Plain text responses** from the main agent (e.g. `"done"`,
`"compile clean — ..."`) appear between action rounds when the
agent reports results or concludes a service cycle.

### Tracing a session through logs

To understand what a session did:

1. Scan `code.jsonl` user records for the `plan.steps[].status`
   progression — this shows which steps completed vs stuck.
2. Scan `code.jsonl` assistant records: JSON = fast agent
   (look at `analysis` + `followups`), raw text = slow agent
   (look for `[INVALID]`, `DEFECT`, or review verdicts).
3. Check `main.jsonl` for `todo` entries to see todo item status
   transitions — this is where `compile-triage: pending → done`
   (or not) gets recorded.
4. Token usage is on every assistant record in the `usage` field:
   `{input, output, cache_creation, cache_read}`.
