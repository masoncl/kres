# Converting a kres run into kres-review-inline.txt

Procedure for turning a kres review run into a mailing-list-ready
`kres-review-inline.txt`. Run this from inside the worktree the review was
performed in (`linux.<sha>`), so every source claim is checked against the
exact tree the reviewed commit sits in.

The output format is defined by
`@REVIEW_PROMPTS@/kernel/inline-template.md`, where `@REVIEW_PROMPTS@`
is the review-prompts tree `setup.sh --review-prompts` names (see
[docs/configuration.md](docs/configuration.md) § Kernel review prompts).
That file is the authority on wording, wrapping, quoting, and snipping.
This file only adds the kres-specific steps that come before it.

## 1. Take kres's refutations as final

The input is `kres-<sha>/summary.txt`. It contains only findings that already
passed the run's own validate workflow; artifacts live in
`kres-<sha>/summary-validation/`.

Do not re-derive anything kres marked `invalidated` in `findings.json`, and do
not re-litigate a finding that summary.txt already dropped. That work is done.
Spend the budget on the findings that survived.

If `summary.txt` does not exist, the run has not been summarized yet. Say so
and stop, or run `/summary` first. Do not silently substitute `findings.json`
— it still contains invalidated entries and unpromoted prose.

## 2. Verify the load-bearing claim of each surviving finding

A finding that passed validation still needs its *consequence* checked, not
just its mechanism. Validation confirms the code says what the finding says it
says. It does not always confirm the harm.

For each surviving finding, identify the single claim that makes it a bug
rather than a curiosity, and check that one in source:

- "leaks X" -> find the free path and confirm nothing else frees it
- "use-after-free" -> find the refcount or lock that would prevent it
- "overflow" -> find the bound that would stop it, and check its width
- "unreachable/dead code" -> `git log --all -S'<symbol>'` for any setter
- "callers can pass N" -> enumerate the callers, don't assume

If the consequence does not survive, keep the part that does and drop the
part that does not. A mechanism can be real while the escalation built on top
of it is wrong.

## 3. Classify every surviving finding: latent or introduced

This is the step that most often goes wrong. `git blame` the specific lines
the finding is about — not the file, not the function, the lines:

    git blame -L <n>,<m> --date=short <file>

Compare the blamed sha against the commit under review.

- **Introduced**: the finding's lines are in this commit's diff. Report it as
  a regression, question-framed, per the inline template.
- **Latent**: the lines predate the commit. The patch did not cause it.

A defect on lines the diff did not touch, reached by a path the diff did not
change, is not this commit's regression — even when the patch's new code sits
a few lines away. Blame is the cheap discriminator and it generalizes across
every bug class.

Also blame any bound, guard, or lock the finding says is missing. Sometimes
the guard is newer than the code it guards, which changes the story.

Confirm ancestry with `git merge-base --is-ancestor`, never by comparing
author dates. A commit can be an ancestor and still carry a later date.

**Blame is the discriminator, not the verdict. Judge causation.** Two cases
break the line-overlap rule in opposite directions, and both have come up:

- *Introduced, but the defective text is in an untouched file.* The patch
  adds `iter->bi_offset += bytes;` and that makes a kerneldoc comment in a
  different header false. Blame on the comment points at an old commit;
  blame on the lines that falsified it points at this one. Introduced.
- *Introduced, but the defective lines blame to the parent.* The parent added
  a narrowing assignment that was unreachable because every caller filled the
  struct from a native `struct iovec`. This patch adds a `copy_from_user()`
  path that lets a wider value reach it. The lines are older; the
  reachability is new. Introduced.

So the question is not "does blame name this sha" but "did this diff make the
defect exist or make it reachable". When blame and causation disagree, say so
explicitly in the report and give both shas — a maintainer can then judge it
themselves rather than dismissing the whole review over a wrong attribution.

## 4. Resolve what kres left open

If a finding records an open question, an unresolved locking assumption, or a
"could not determine", try to resolve it before writing anything up. These are
cheap and they cut both ways: one run resolved an "unresolved fdinfo locking"
note by finding the single caller inside `mutex_trylock(&ctx->uring_lock)`,
which refuted the finding outright and removed it from the report.

Also look for consequences the finding did not draw. Verifying a mechanism
often puts you one step from a second effect on the same lines — a wrapped
value reaching a second test, a flag consumed by a path the finding did not
name. Those belong in the report even though kres never filed them.

## 5. Decide what to include

- **Introduced** -> always include.
- **Latent and nearby** -> include, clearly marked as pre-existing. "Nearby"
  means the finding is inside a function the diff touches, or is about the
  same structure, flag word, or contract the diff is changing. A maintainer
  reading the patch has that code in front of them, so the comment is useful.
- **Latent and unrelated** -> leave it out. It belongs in its own thread.

Mark latent items in the body, never with a header or label. Use the same
undramatic register the inline template asks for:

    This is not a new bug from this patch, the code predates it, but since
    <function>() is being touched here: <question>?

Do not write "PRE-EXISTING" or any other all-caps tag. Do not claim or imply
the patch introduced something it did not — that is the fastest way to get a
review dismissed.

When every surviving finding is latent, say so plainly in the reply to the
user before writing the file, and let them decide whether it is worth sending
at all. A regression report against a patch with no regressions is still a
judgment call for a human.

## 6. Build the report

Follow `@REVIEW_PROMPTS@/kernel/inline-template.md` exactly from
here. The parts most often missed:

- Regenerate the diff with `git show <sha>`. Never reconstruct it from
  context or from kres output — the quoted text must match byte for byte.
- Check for `Link:` tags in the commit message; include them if present.
- Snip aggressively. Keep only hunks a question attaches to. Drop the diff
  header for any entirely snipped file; keep it for files with a surviving
  hunk. Mark every snip with `[ ... ]`.
- No line numbers anywhere. Use `file:function()` and code snippets instead.
- Wrap added prose at 78 columns; leave quoted diff lines at their original
  length.
- Plain text only, no markdown, no all caps outside quoted code.
- End the file with a blank line.

For a latent finding with no hunk to attach to, put it after the diff with
its own supporting snippets rather than inventing a location for it.

## 7. Write the latent-bug summary

Everything classified latent in step 3 at high or medium severity goes into
`<worktree>/kres-review-latent.txt`, whether or not it made the inline
review. The inline review is scoped to the patch; this file is where the rest
of the review's real findings survive instead of being discarded because the
commit did not cause them.

Include a finding here when it is latent and its severity is high or medium.
Skip low severity, skip anything kres invalidated, and skip anything refuted
in step 2. A finding that is both latent and in the inline review appears in
both files — that is expected, they have different audiences.

Same plain-text rules as the inline review: no markdown, 78-column wrap for
prose, `file:function()` rather than line numbers, undramatic wording.

Open with a header naming the reviewed commit and stating plainly that none
of the entries are introduced by it, with a one-line description of what the
commit actually does. That framing has to be at the top, not per-entry, so
the file cannot be skimmed into a misattribution.

Per entry:

- Severity and a one-line subject.
- `Introduced by:` the sha and subject from `git blame` on the cited lines,
  with the date. If registration and use sites came from different commits,
  name both — that split is often the whole story.
- Code snippets in the same quoted style the inline template uses, with
  `^^^^` under the specific token at fault.
- Where a fix is a one-liner, show it as a diff-style pair:

      -       unsigned long folio_size = 1 << imu->folio_shift;
      +       unsigned long folio_size = 1UL << imu->folio_shift;

  For anything larger, describe the direction in a sentence. Do not write
  patches here.
- Any guard, refcount, or bound that limits the impact, quoted. If checking
  one downgraded the finding from what kres claimed, say what the real effect
  is. A latent-bug file that overstates severity is worse than no file.
- Reachability, honestly. If you could not establish a trigger, say the
  finding is filed on the mechanism and name what is missing.

State once, near the top, that nothing in the file has been reported upstream
and that severities are the review's own assessment.

## 8. Self-check before writing

- Every question is a question, not an accusation, and does not mention the
  author.
- No bot or automated-review evidence anywhere: search case-insensitively for
  `sashiko`, `bot+bpf-ci`, `kernel-patches-review-bot`, `Claude`, `AI review`,
  `CI run summary`, `netdev-ai.bots.linux.dev`. Any hit means the whole issue
  comes out.
- Every claim in the report traces to a file you read in this worktree, not
  to summary.txt prose.
- Latent items are labelled as such in their body text.
- Quoted diff matches `git show` exactly.
- Every high/medium latent finding appears in `kres-review-latent.txt`,
  and that file's header disclaims attribution to the reviewed commit.
- Every `Introduced by:` sha came from `git blame` on the cited lines, not
  from the finding text.

Write to `<worktree>/kres-review-inline.txt` and
`<worktree>/kres-review-latent.txt`.
