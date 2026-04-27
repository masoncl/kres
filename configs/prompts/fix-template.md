You are running the kres FIX flow. The block above this paragraph is
the BUG INPUT. It is one of:

1. An absolute path to a kres finding directory (the shape produced
   by `kres --export`, see ~/local/kernel-bugs/findings/<id>/). Read
   `FINDING.md`, `summary.md`, and `metadata.yaml` from that exact
   path before doing anything else. Treat `metadata.yaml` as ground
   truth for `git.sha`, `filename`, `relevant_symbols`, and
   `relevant_file_sections` — your fix MUST land at that sha (or its
   tip-of-branch successor) and must touch the named symbols.
2. Free-form prose describing the bug. Use it as the problem
   description and gather any code context yourself via `source` /
   `read` / `git log` followups.

GOAL — terminate the run when EITHER is true:

- A reviewed git commit + patch is on HEAD that fixes the bug, the
  patch compiles cleanly, and a self-review pass against the diff
  found no defects you have not addressed; OR
- You have produced concrete evidence (file:line citations, commit
  refs from `git log`/`git show`) that the bug is INVALID — already
  fixed, not reachable on the targeted code, or a misread of the
  source. In this case do NOT write or commit a patch. Mark
  `[INVALID]` at the top of `analysis` and explain the evidence.

This session runs in coding mode. The slow agent emits `code_edits`
for surgical fixes (preferred) or `code_output` for large rewrites.
`git add` and `git commit -s -m "<subject>"` followups land the
patch. `bash` followups run `make` / `cargo build` for compile
verification. No push/fetch/amend; commit messages follow the
kernel-style template the operator's CLAUDE.md describes (subsystem
prefix, problem paragraph, "Fix by" paragraph, 72-char wrap).

STEPS — drive the iteration in this order. Each step is one or more
pipeline turns. Advance only when the prior step is complete.

1. RESEARCH (fast agent gathers context)
   Pull every file the bug touches, every caller of the affected
   symbols, and the relevant commit history. If the research shows
   the bug is invalid, STOP HERE: write `[INVALID]` plus the
   evidence in `analysis` and let the goal terminate the run. Do
   NOT fabricate a fix to satisfy the flow.

2. FIX (slow agent writes the patch)
   Quote the verbatim current bytes of the lines you change — the
   coding-mode rule applies: every `old_string` in `code_edits`
   must come from the file as it actually is on disk. Prefer
   `code_edits` for small fixes; `code_output` only when most of
   the file is being rewritten. After the edits land, emit
   `git add <paths>` then
   `git commit -s -m "<kernel-style message>"`.

3. COMPILE (`bash`)
   Build the narrowest scope that exercises the changed file. For
   Linux trees that is typically
   `make -j$(nproc) <subdir>/` or
   `make <path/to/object>.o`. For Rust crates,
   `cargo build` (or `cargo build -p <crate>`). Capture the
   `[stderr]` block — that is where new warnings and errors
   surface.

4. FIX WARNINGS (slow agent revises)
   For every NEW warning or error introduced by the patch, emit a
   `code_edit` to address it and re-run step 3. Pre-existing
   warnings unrelated to the fix are not in scope — cite them in
   `analysis` and move on. Do not silence a warning by deleting an
   unrelated check; address the root cause.

5. REVIEW (max two turns)
   Once the build is clean, run a self-review pass against the
   diff (`git show HEAD`). Apply the lenses below; for each lens,
   list every distinct concern.

   - object lifetime: pointer ownership, refcounting, RCU, free
     ordering
   - memory: leaks, use-after-free, double-free, allocator API
     misuse
   - bounds: array / index correctness, untrusted indices
   - races: lock coverage, ordering, missed wakeups
   - general: anything else the patch introduces

   The review pass is bounded to TWO turns. Turn one issues
   findings against the patch. Turn two either:
   - confirms the patch is clean (goal met, run terminates), OR
   - applies ONE final round of `code_edits` + a fresh compile.
     If that round is clean, declare the goal met. Do not iterate
     review beyond two turns — escalate to the operator with the
     remaining concerns in `analysis` instead.

OUTPUT RULES

- Never paste a unified diff or `.patch` file as `code_output`
  content. Edits go through `code_edits` (or `code_output` whose
  `path` is the file being rewritten); diffs are produced by
  `git show HEAD` after the commit lands.
- The final state of the run must be exactly one commit ahead of
  the starting HEAD. Squash work-in-progress commits with further
  edits or finish with a single trailing commit.
- `analysis` is your running narrative: what step you are on,
  what the build said, what the review pass found, what is left.
  Keep it tight — the artifacts are the diff and the commit
  message.
- If at any point the bug turns out to be invalid, abandon any
  in-progress edits, do not commit, and emit the `[INVALID]`
  evidence in `analysis`. The run terminates without a patch.
