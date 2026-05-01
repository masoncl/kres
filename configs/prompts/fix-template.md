You are running the kres FIX flow. The block above this paragraph is
the BUG INPUT. It is one of:

1. An absolute path to a kres finding directory (the shape produced
   by `kres --export`, see ~/local/kernel-bugs/findings/<id>/). Read
   `FINDING.md`, `summary.md`, and `metadata.yaml` from that exact
   path before doing anything else. `metadata.yaml` records the
   `git.sha` the audit ran at, plus `filename`,
   `relevant_symbols`, and `relevant_file_sections`. The sha is
   context for understanding where the bug was found — the
   workspace may be at a different HEAD. If the code at the
   affected symbols has changed since that sha, verify the bug
   still exists before writing a fix.
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
`make` and `cargo` action types handle compile verification (enabled
by default — do NOT use `bash` for builds). `git add` and
`git commit -s` land the patch in the same turn as the fix (step 2)
so the commit message is composed while context is fresh. If the
build fails afterward, `git commit --amend -s` folds fixes in.
No push/fetch.

When you compose the commit message, follow the COMMIT MESSAGE STYLE
section that this prompt appends after the FIX-flow body. That
section is the kernel's own commit-format rules (75-column wrap,
subject at most 75 chars naming both what AND why, mandatory
`Assisted-by:` trailer for tool-generated patches, etc.). Do not
fall back to your training prior — read the appended rules and
apply them verbatim.

STEPS — drive the iteration in this order. Each step is one or more
pipeline turns. Advance only when the prior step is complete.

1. RESEARCH (fast agent gathers context)
   Pull every file the bug touches, every caller of the affected
   symbols, and the relevant commit history. If the research shows
   the bug is invalid, STOP HERE: write `[INVALID]` plus the
   evidence in `analysis` and let the goal terminate the run. Do
   NOT fabricate a fix to satisfy the flow.

   RESEARCH THE ORIGIN OF THE BUG. Identify the commit whose
   diff actually creates the defect. Use git history and code
   analysis together; this is research and the answer must be
   supported by concrete evidence, not a guess. Record the 12+
   char SHA, the commit's full subject, and a one-sentence
   justification in `analysis`. Step 2 emits a `Fixes:` trailer
   only when that justification is concrete. If the bug predates
   git history, was introduced gradually across multiple
   commits, or no single commit's diff actually creates the
   defect, skip the trailer — a wrong Fixes: tag is worse than
   a missing one.

   SELF-FIX TRAP: once you have emitted code_edits or a git
   commit, the files on disk contain YOUR work. If you re-read
   them and see the fix in place, that is your own edit — not
   upstream prior art. Do not declare `[INVALID]` based on
   reading back code you just wrote. `[INVALID]` after step 1
   is legitimate ONLY when new evidence shows the bug itself is
   wrong (unreachable path, misread semantics, already fixed by
   a DIFFERENT commit that predates this session). Cite the
   evidence explicitly — a file:line or commit ref that existed
   BEFORE this run started.

2. FIX + COMMIT (slow agent writes the patch and commits it)
   Quote the verbatim current bytes of the lines you change — the
   coding-mode rule applies: every `old_string` in `code_edits`
   must come from the file as it actually is on disk. Prefer
   `code_edits` for small fixes; `code_output` only when most of
   the file is being rewritten.

   In the SAME turn that you emit code_edits, also write the
   commit message to a workspace file via a `code_output` entry
   and emit `git` followups to stage and commit, PLUS a `make`
   followup to compile (step 3). The `git` followups are
   executed immediately by the reaper; the `make` followup
   creates the next turn. You MUST include the `make` followup
   — without it the run dies after the commit.

       code_output: [{
         "path": ".kres-commit-msg.tmp",
         "content": "<subject>\n\n<body wrapped at 75>\n\nFixes: ...\nAssisted-by: kres (<model-id>)"
       }]
       followups:
         {"type": "git", "name": "add <explicit paths>"},
         {"type": "git", "name": "commit -s -F .kres-commit-msg.tmp"},
         {"type": "make", "name": "-j$(nproc) <path/to/object>.o"}

   CRITICAL — the commit message MUST NOT mention attackers,
   exploits, security impact, or vulnerabilities. These commits
   fix bugs. Do not use the words "attacker", "exploit",
   "malicious", "vulnerability", "privilege escalation",
   "arbitrary code execution", "security", or "payload". Just
   describe the bug and how the patch fixes it.

   The reaper applies the `code_output` first, then validates
   the file (rejects any non-trailer prose line >100 chars),
   then runs git. The `make` followup becomes a new task —
   its build output will be in your context on the next turn.
   Do NOT pass the commit message via `-m`; the reaper rejects
   `-m` outright. Compose the message body following the COMMIT
   MESSAGE STYLE section appended at the end of this prompt.
   The `Assisted-by:` trailer is REQUIRED per
   Documentation/process/submitting-patches.rst:637-644.

   FIXES TAG — emit one when step 1 found the introducing
   commit. A valid Fixes: tag must:
   - Use SHA at least 12 hex chars
     (`git log -1 --pretty="%h" --abbrev=12 <sha>`).
   - Quote the introducing commit's exact subject line:
     `Fixes: <sha12+> ("Original subject line")`.
   - Stay on ONE line — tags are exempt from the 75-col rule
     (submitting-patches.rst:148) but must not wrap.
   - Reference a commit that exists and actually introduced the
     bug — verify with `git cat-file -t <sha>` and re-read the
     diff before trusting it.
   - Be omitted when no single introducing commit can be pinned
     down; a wrong Fixes: is worse than none.

   HARD WRAP RULE — every prose line in the commit-message file
   you emit via `code_output` MUST wrap at 75 columns.
   Paragraphs are separated by blank lines; lines inside each
   paragraph are broken at word boundaries. Count characters per
   line before you emit. The reaper REJECTS any commit whose
   message file contains a non-trailer line over 100 chars; you
   will see the rejection in the next turn's analysis trailer
   and have to re-emit a corrected `code_output`. Trailer tags
   (`Fixes:`/`Reported-by:`/etc.) and indented code are exempt;
   prose is not.

   CLEAR PARAGRAPHS — no paragraph may exceed 5 prose lines.
   State the bug plainly, one idea per paragraph. When you
   need to show a code path, use indented code (4 spaces)
   instead of describing it in prose:

   AVOID:
     In check_mem_access(), every SCALAR ctx load resets the
     destination register via mark_reg_unknown() before
     installing the load result except the BPF_LSM_MAC retval
     branch, which calls only __mark_reg_s32_range() and that
     helper intersects s32/s64 bounds in place and never
     clears reg->id or reg->var_off or u64 bounds...

   USE INSTEAD:
     Every SCALAR ctx load in check_mem_access() resets the
     destination register via mark_reg_unknown() — except
     the BPF_LSM_MAC retval branch:

         if (is_retval) {
             __mark_reg_s32_range(...);
             /* mark_reg_unknown() is missing here */
         }

     __mark_reg_s32_range() intersects s32/s64 bounds but
     never clears reg->id, var_off, or u64 bounds.

   Dense paragraphs are hard to review. Spread information
   out so the reader follows the sequence step by step.

3. COMPILE → REVIEW → PUBLISH (separate tasks)
   After step 2 emits the `make` followup, each subsequent step
   runs as its own task. The compile-verify and review-patch
   plan steps carry their own context with full instructions.
   If the review finds defects, it fixes and emits a make
   followup; the todo agent re-creates the review step so the
   loop repeats until the review is clean.

RECORD-INVALIDATION — when you reach `[INVALID]` AND the BUG INPUT
was an absolute path to a kres finding directory (one that contains
`metadata.yaml` and `FINDING.md`), update those two files in the
SAME turn that writes `[INVALID]` to `analysis`. The operator named
the finding directory in the prompt, so the consent gate already
permits writes there. Emit `code_output` entries with absolute
paths and the FULL post-update file body:

    code_output: [
      {
        "path": "<absolute finding dir>/metadata.yaml",
        "content": "<verbatim current body, only change is status: invalidated>",
        "purpose": "record [INVALID] determination"
      },
      {
        "path": "<absolute finding dir>/FINDING.md",
        "content": "<verbatim current body, only change is the **Status:** line set to invalidated>",
        "purpose": "mirror metadata status flip"
      }
    ]

Both files were loaded in step 1; quote them VERBATIM and change
ONLY:
- `metadata.yaml`: the `status:` field → `invalidated`. Leave
  every other field (id, severity, filename, relevant_symbols,
  verification, etc.) untouched.
- `FINDING.md`: the `**Status:**` line near the top of the file
  → `**Status:** invalidated`. Do not edit any other part of
  FINDING.md — the `[INVALID]` evidence stays in `analysis`,
  which the operator reads alongside the finding dir.

Do not invent new fields and do not append narrative. Skip this
block entirely when the BUG INPUT was free-form prose (no
directory to write back to). Skip it when `metadata.yaml`
already says `status: invalidated` on disk — re-asserting an
existing status is wasted I/O. Status is sticky in the kres
findings model; do not flip an `invalidated` finding back to
`active` from this template.

OUTPUT RULES

- Never paste a unified diff or `.patch` file as `code_output`
  content. Edits go through `code_edits` (or `code_output` whose
  `path` is the file being rewritten); diffs are produced by
  `git diff HEAD~1` after the commit lands.
- `analysis` is your running narrative: what step you are on,
  what the build said, what the review pass found, what is left.
  Keep it tight — the artifacts are the diff and the commit
  message.
- `[INVALID]` is permitted at any step when new evidence shows
  the bug itself is wrong (unreachable path, misread semantics,
  already fixed by a commit that predates this session). See the
  SELF-FIX TRAP note under step 1.

PLAN:
{"steps": [
  {"id": "research", "title": "Research the bug and gather context", "description": "Read finding files, pull affected source, callers, commit history. Identify the introducing commit. Determine if the bug is valid or [INVALID]."},
  {"id": "write-fix", "title": "Write the fix, commit it, and compile", "description": "Emit code_edits for the fix, write commit message to .kres-commit-msg.tmp, emit git add + git commit + make followups in one turn."},
  {"id": "compile-verify", "title": "Verify the fix compiles cleanly", "description": "Triage the make output only. Do not review patch logic.", "context": "COMPILE TRIAGE ONLY\n\nYour ONLY job is to triage the compiler output.\nDo NOT review the patch logic — a separate review task handles that.\nDo NOT emit publish-fix from this step.\n\nA) ERROR IN PATCHED CODE — emit code_edits to fix, then git add + git commit --amend -s + make followups to recompile.\nB) PRE-EXISTING / ENVIRONMENT ERROR — note 'compile failed (pre-existing): <reason>' in analysis. Do not debug Kconfig or .config issues.\nC) BUILD SUCCEEDED — note 'compile clean' in analysis. Stop here; the review task runs next."},
  {"id": "review-patch", "title": "Review the patch", "description": "Apply review lenses against git diff HEAD~1 with callee source. If defects found, fix and emit make followup to recompile.", "context": "REVIEW PROTOCOL\n\nYou are reviewing a kernel patch. The fast agent gathers git diff HEAD~1 and callee source during its gather rounds.\n\nApply these lenses exhaustively. For each lens, enumerate every distinct concern — do not stop at the first issue. Cite file:line from the gathered callee source for every claim.\n\n- [ ] object lifetime: pointer ownership, refcounting, RCU grace periods, free ordering. Trace every pointer in the changed lines to its allocation and release. Verify refcount balance across error paths.\n- [ ] memory: leaks, use-after-free, double-free, allocator API misuse. Check every error/goto path for missing frees or double frees.\n- [ ] bounds: array/index correctness, untrusted indices, integer overflow/truncation in size calculations. Verify every array access uses a bounds-checked index.\n- [ ] races: lock coverage, ordering, missed wakeups, data races on shared state. Identify what lock protects each accessed field and whether the patch maintains coverage.\n- [ ] general: NULL derefs, logic errors, missing error checks, semantic correctness. Does the patch actually fix the stated bug without introducing new issues?\n\nFor each lens, write your analysis with inline code snippets from the gathered source — not bare file:line references. If a lens is clean, state why in one sentence with a citation.\n\nAlso review the commit message (in the accumulated analysis preamble). It MUST NOT contain the words attacker, exploit, malicious, vulnerability, privilege escalation, arbitrary code execution, security, or payload. No paragraph may exceed 5 prose lines. If the commit message violates either rule, rewrite it via code_output to .kres-commit-msg.tmp and emit git commit --amend -s -F .kres-commit-msg.tmp.\n\nIf you find defects: emit code_edits to fix them, then emit git add + git commit --amend -s + make followups to recompile. The todo agent will re-create this review step so the amended patch gets a fresh review.\n\nIf the review is clean: emit a publish-fix followup to record the patch."},
  {"id": "publish", "title": "Publish the fix", "description": "Emit publish-fix followup to record auto-generated-fix.diff in the finding directory."}
]}
