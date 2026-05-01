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
   and emit two `git` followups that reference it:

       code_output: [{
         "path": ".kres-commit-msg.tmp",
         "content": "<subject>\n\n<body wrapped at 75>\n\nFixes: ...\nAssisted-by: kres (<model-id>)"
       }]
       followups:
         {"type": "git", "name": "add <explicit paths>"},
         {"type": "git", "name": "commit -s -F .kres-commit-msg.tmp"}

   The reaper applies the `code_output` first, then validates
   the file (rejects any non-trailer prose line >100 chars),
   then runs git. Do NOT pass the message via `-m`; the reaper
   rejects `-m` outright. Compose the message body following the
   COMMIT MESSAGE STYLE section appended at the end of this
   prompt. The `Assisted-by:` trailer is REQUIRED per
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

3. COMPILE (`make` or `cargo`)
   Build the narrowest scope that exercises the changed file.
   Use the `make` action type for kernel trees:
   `{"type": "make", "command": "-j$(nproc) <path/to/object>.o"}`.
   Use the `cargo` action type for Rust crates:
   `{"type": "cargo", "command": "build -p <crate>"}`.
   Do NOT use `bash` for builds — `make` and `cargo` are
   first-class action types that are enabled by default. Capture
   the `[stderr]` block — that is where new warnings and errors
   surface.

4. FIX WARNINGS + AMEND
   For every NEW warning or error introduced by the patch, emit a
   `code_edit` to address it and re-run step 3. Pre-existing
   warnings unrelated to the fix are not in scope — cite them in
   `analysis` and move on. Do not silence a warning by deleting
   an unrelated check; address the root cause. After fixing, use
   `git add <paths>` + `git commit --amend -s` to fold the
   fix-up into the original commit.

   Context carries forward: this task sees the accumulated
   analysis from step 2 (the finding, the fix rationale, the
   commit message) plus the build output. Use it.

5. REVIEW (max two turns)
   Once the build is clean, run a self-review pass against the
   diff (`git diff HEAD~1`). Apply the lenses below; for each
   lens, list every distinct concern.

   - object lifetime: pointer ownership, refcounting, RCU, free
     ordering
   - memory: leaks, use-after-free, double-free, allocator API
     misuse
   - bounds: array / index correctness, untrusted indices
   - races: lock coverage, ordering, missed wakeups
   - general: anything else the patch introduces

   The review pass is bounded to TWO turns. Turn one issues
   findings against the patch. Turn two either:
   - confirms the patch is clean (proceed to PUBLISH below in
     the SAME turn), OR
   - applies ONE final round of `code_edits` + recompile +
     `git commit --amend -s` (steps 3-4 again), then on the
     turn after the amend lands runs PUBLISH below. Context
     from all prior steps (the finding, the fix, the build
     output, and the review findings) is in the accumulated
     preamble — use it to make the right edit. Do not iterate
     review beyond two turns — escalate remaining concerns to
     the operator in `analysis`.

   PUBLISH (must fire in the same turn that closes review):
   when review confirms clean AND the BUG INPUT was an absolute
   path to a kres finding directory (one that contains
   `metadata.yaml` and `FINDING.md`), emit exactly one followup
   in this turn:
       {"type": "publish-fix", "name": "<absolute finding dir>"}
   The reaper writes `auto-generated-fix.diff` (the output of
   `git format-patch -1 --stdout HEAD`) into that directory,
   appends `auto_generated_fix:` to its `metadata.yaml`, and
   adds a cross-link in `summary.md`. Emit publish-fix EXACTLY
   ONCE per HEAD — if you already emitted it on a prior turn
   for the current HEAD, do not re-emit. Skip publish-fix
   entirely when the BUG INPUT was free-form prose, and skip
   it on the amend turn (the next turn, after the amend
   commit lands, is when publish-fix fires).

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
