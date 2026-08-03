## Kernel fix description rules

You are composing a kernel-style git commit message. The block above
this paragraph is the CHANGE DESCRIPTION — either prose written by the
operator, or a structured brief naming what was changed and why. Use
that description plus the staged diff (`git diff --cached`) and the
relevant tree state (`git log`, `git blame`) as your only sources of
fact. Do not invent symptoms, mechanisms, or affected commits.

GOAL — produce ONE commit message that follows the kernel project's
conventions, write it to a workspace file via `code_output`, then
commit with `git commit -s -F <that-file>`. Sign-off comes from
`-s`; do not type `Signed-off-by:` by hand.

## Subject

```
<subsystem>: <imperative summary, lowercase, no period>
```

The subject must describe BOTH what the patch changes AND why it is
necessary (submitting-patches.rst:708-709). "fix foo" is not enough;
"fix foo to release X on Y" tells the reviewer the change is worth
reading past the first line.

- `<subsystem>` is the prefix used by nearby commits for the touched
  files. Prefer the shortest specific nested path that matches the file
  tree, and preserve historical capitalization when local history uses
  it. Do not invent a new prefix from the bug description alone.
- Imperative mood: "fix", "add", "drop", "reject", "release",
  "split". Not "fixes", "fixing", "fixed", "[This patch] fixes".
- One clause. No trailing period.
- The whole git commit subject (subsystem prefix included) must not
  exceed 55 chars. Default `git format-patch` prepends the literal
  `Subject: [PATCH] ` header prefix (17 chars), so 55 is the largest
  raw commit subject that keeps the full mail header line at or under
  72 chars. Count the generated header too: `Subject: [PATCH] ` plus
  your subject must be <= 72 chars.
- Do NOT include `[PATCH]` or `[PATCH vN]` — those prefixes are for
  the email Subject line that `git format-patch` produces, not for
  the git commit message itself.

## Fix description

```
<Fix paragraph: "Fix by <verb> <object>." For a refactor with no
behaviour change append "No functional change intended.">
```

End with one short "Fix by ..." paragraph.

After the blocks, add one short consequence paragraph and one
short "Fix by ..." paragraph.

Choose the right body shape for the change:

- **Bug fix**: symptom → root cause → "Reject/Fix/Drop/Release
  <object> and return <result>." Include user-visible impact in
  the symptom paragraph (crash, leak, lockup, regression).
- **Regression**: `commit <sha-12+> ("<subject>") did X; should
  have done Y.` then "Let's move ..." or "Restore ..." as the
  verb. Pair with a `Fixes:` tag.
- **Enumerated breakage**: problem paragraph, numbered list of
  distinct failure modes (each item one sentence), single
  closing "Fix by ..." paragraph. Reserved for changes that
  genuinely fix multiple distinct issues; the default is one
  failure per commit (submitting-patches.rst:81-83).
- **Cleanup / refactor**: one short paragraph + "No functional
  change intended."
- **Trivial**: one-sentence body is fine for a typo fix, a
  comment fix, a one-line const.

## Trailer tags (exempt from the wrap rule)

The following trailers go after the body, separated by one blank
line. Each on its own line. Tags can run past 75 columns
(submitting-patches.rst:148).

- **Fixes:** `<sha-12+> ("<full subject>")` — required when the
  change repairs a regression introduced by a specific commit.
  Helps the stable team route the fix and helps reviewers locate
  the introducing change.
- **Closes:** `<URL>` — references a public bug report this patch
  closes. Pair with Reported-by when applicable.
- **Link:** `<URL>` — typically a lore.kernel.org archive link to
  the discussion that produced the patch. Even with Link:, the
  body must remain self-contained — do not punt explanations to
  the link target (submitting-patches.rst:130-133).
- NEVER add `Cc: stable@vger.kernel.org`.
- **Reported-by:** `<Name> <email>` — credits the bug reporter.
  Pair with Closes:. Reporting must have been public.
- **Tested-by:** `<Name> <email>` — someone tested the patch.
  Requires explicit permission of the named person.
- **Reviewed-by:** `<Name> <email>` — someone reviewed and
  approved per the Reviewer's Statement. Requires explicit
  permission.
- **Acked-by:** `<Name> <email>` — maintainer or stakeholder
  signoff short of a full review. Requires explicit permission.
- **Suggested-by:** `<Name> <email>` — credits the idea source.
  Requires public suggestion.
- **Co-developed-by:** `<Name> <email>` — co-author. MUST be
  immediately followed by a Signed-off-by: from that co-author.
- **Assisted-by:** REQUIRED when an advanced coding tool helped
  produce the patch (submitting-patches.rst:637-644: "Failure to
  do so may impede the acceptance of your work"). kres-generated
  patches MUST include this trailer in the form:
  `Assisted-by: kres:<model-id>` — name BOTH the tool (`kres`)
  AND the underlying model that wrote the patch, e.g.
  `Assisted-by: kres:claude-sonnet-5` or
  `Assisted-by: kres:claude-opus-4-8`. A bare
  `Assisted-by: kres` without the model is INSUFFICIENT — the
  reviewer needs to know which model produced the change. Use
  the model id you are running under; do not invent one. See
  Documentation/process/coding-assistants.rst for the canonical
  wording in the target tree.

`Signed-off-by:` is added automatically by `git commit -s` and
must NOT be typed by hand. Co-developed-by entries are the only
case where additional Signed-off-by lines belong in the message
body (one per co-author, immediately after their Co-developed-by).

## What to AVOID

- `I did X` / `We did Y` / `we now ...` narration. Imperative
  mood, no first-person pronouns.
- `This commit ...` / `This patch ...` / `In this change ...`
  preambles.
- Trailing period on the subject.
- Emoji anywhere in the message.
- Markdown ` ``` ` fences in the body. Indent quoted code with
  four spaces instead.
- A manually typed `Signed-off-by:` trailer. Use `git commit -s`.
- An `[PATCH]` or `[PATCH vN]` prefix in the subject. That's the
  email Subject line that `git format-patch` synthesises; the
  in-repo commit subject is just `subsystem: summary`.

## Output

Write the commit message to a workspace file via a `code_output`
entry, then emit ONE `git` followup that references that file
with `-F <path>`. Subject on line 1, blank line, body with
paragraphs separated by blank lines, blank line, then trailer
tags one per line. Do NOT pass the message via `-m` — the
reaper rejects `-m` outright. The reaper also rejects
`--no-verify` and `--no-gpg-sign`. `--amend` is permitted when
folding a review fix-up into the original commit.

```
"code_output": [{
  "path": ".kres-commit-msg.tmp",
  "content": "<subject>\n\n<problem paragraph wrapped at 75>\n\n<fix paragraph wrapped at 75>\n\nFixes: <sha> (\"<original subject>\")\nAssisted-by: kres:<model-id>"
}]

{"type": "git",
 "command": "commit -s -F .kres-commit-msg.tmp",
 "reason": "land the change as one signed kernel-style commit"}
```

If files have not yet been staged, emit a preceding
`{"type": "git", "command": "add <explicit paths>"}` followup —
never `git add -A` or `git add .` (sweeps in stray files).

## Self-check before emitting

1. Did every prose line stay at or under 75 columns? (Tags exempt.)
2. Subject at most 55 chars including the subsystem prefix, so
   `Subject: [PATCH] <subject>` is at most 72 chars?
3. Subject describes BOTH what the change does AND why?
4. No period on the subject? No `[PATCH]` prefix?
5. Body in imperative mood with no `I`/`we`?
6. User-visible impact stated in the problem paragraph (for bug
   fixes) or numbers + cost (for optimisations)?
7. Code citations are `filename:function`, never source line numbers?
8. Commit citations include at least 12 hex chars plus the oneline
   summary in parens?
9. `Fixes:` trailer present when fixing a known prior commit?
10. `Assisted-by: kres:<model-id>` trailer present, with BOTH
    the tool name and the model id (REQUIRED per
    submitting-patches.rst:637-644)? A bare `Assisted-by: kres`
    is insufficient.
11. No test counts, no per-file bullets, no review-process narration?
12. `-s` present on `git commit` so Signed-off-by lands automatically?

If any answer is no, rewrite before emitting the followup.
