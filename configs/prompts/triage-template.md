You are triaging a single kernel bug finding produced by `kres --export`.

## Input directory

The **first line of this prompt is the absolute path of the finding's
directory** — call it `DIR`. Use that exact path everywhere below;
do not invent a different one and do not treat `$DIR` as a shell
variable that something else expands.

`DIR` contains:

- `DIR/metadata.yaml` — id, title, severity, status, filename,
  subsystem (may be empty), git head, optional `introduced_by`, and
  lists of `relevant_symbols` and `relevant_file_sections`.
- `DIR/FINDING.md` — full narrative: summary, mechanism, reproducer,
  impact, fix sketch, open questions, per-task analysis details,
  relevant symbols and file excerpts.

Read both before writing. Do not invent facts that aren't in those
two files or in the actual source tree at `metadata.yaml`'s
`git.sha`.

## Output

Write the triage to `DIR/summary.md`, replacing any existing copy.

Emit it as a single `code_output` entry with `path` set to the
**absolute** `DIR/summary.md` path. The operator named `DIR` in the
prompt, so the consent gate already permits writes there — no bash,
no cp, no relative-path hack:

```
"code_output": [
  {
    "path": "<absolute DIR>/summary.md",
    "content": "<full body>",
    "purpose": "triage summary"
  }
]
```

## Format

Unless you're quoting code, lines MUST be wrapped at 78 characters.  Long
lines are not allowed, count characters as you write.

The very first line of `summary.md` MUST be the verbatim cross-link
header below — no edits, no substitutions, no omitting:

    [FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)

Then one blank line. Then the section headings below, in this
order. Every section is required. Keep prose tight — short triage
doc, not a re-run of FINDING.md.

**Skipping the cross-link line is a template violation. Output
that starts with `# Subject:` is wrong** — the cross-link line
comes first, always.

```
[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)

# Subject: <one-line bug description>

# Status

<one of: Fixed, Plausible, Unconfirmed, Unknown, Invalid>

# Subsystem

<single sentence naming the kernel subsystem AND the file and
function the bug lives in>

# Impact

<max two paragraphs of plain description of the impact. Don't fill
space you don't need to — a single sentence is fine if that's enough.
Plain language, no run-on sentences.>

# Requirements

<Answer each, then explain the trigger conditions:>

- Host access?
- Remote access?
- Only under specific CONFIG_?
- As root?
- As regular user?

<Then a short paragraph explaining the conditions needed to trigger
the bug.>

# Details

<A short description of the bug. The full details are in FINDING.md;
this summary.md is what gets read while triaging, so keep it
skimmable.>
```

## Wording choices

- Dense paragraphs are hard to read.  Spread the information out so
it is easier to follow.
  - If you have a series of factual sentences, break them up into logical
groups with a blank line between each group.
  - If you have a series of statements followed by a question, put a blank
line before the question.

### AVOID
```
Looking at widget_claim() in drivers/example/widget.c, if CPU1 already called
widget_release() which sets w->owner = NULL, CPU2 checks owner, sees it is
NULL, and takes the 'already released' path with mutex_unlock/put_widget/goto
retry instead of calling widget_release() again.
```

### USE INSTEAD
```
Looking at widget_claim() in drivers/example/widget.c, if CPU1 already called
widget_release() and set w->owner = NULL:

CPU1
widget_release()
   w->owner = NULL;

CPU2 then sees this in widget_claim():
    if (!w->owner) {
        pr_debug("widget %p already released\n", w);
        mutex_unlock(&w->lock);
        put_widget(w);
        ...
        goto retry;
    }

and takes the goto retry path instead of calling widget_release() again.
```

## metadata.yml update

You may edit `metadata.yaml` in exactly two ways:

1. `subsystem:` — if the field is empty and you've determined which
   subsystem this bug belongs to, fill it in.
2. `status:` — if you picked `Unconfirmed` for the summary's
   `# Status` and the metadata currently says `status: active`,
   flip it to `status: unconfirmed`. Do not touch the status field
   in any other case (leave `active`, `invalidated`, etc. alone).
   Confirmed bugs stay `active`; only open-question findings
   become `unconfirmed`.

NO OTHER EDITS to `metadata.yaml` are permitted.

## Rules

- The FIRST line of `summary.md` MUST be
  `[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)`
  followed by one blank line. Skipping it is a template violation —
  every summary.md needs the cross-link so a triager landing on it
  can drop into FINDING.md or metadata.yaml without leaving the
  page.
- The Subject line is the `# Subject:` heading itself — don't add a
  separate first heading above it. The cross-link line above is the
  only thing that comes before `# Subject:`.
- Status values are exactly one of `Fixed`, `Plausible`,
  `Unconfirmed`, `Unknown`, `Invalid`. Match the metadata's
  `status:` when it's `invalidated` (→ `Invalid`); otherwise pick
  the best fit from the FINDING.md evidence.

- **Decide status BEFORE writing prose.** Walk the checklist below
  in order and pick the first match. Do not reach for `Unknown`
  until you've actively ruled out `Unconfirmed`.

- `Unconfirmed` — pick this whenever **the finding's own
  narrative admits the bug is contingent on something the
  analysis did not verify.** This is the default for any
  question-style finding. Concrete tells, ANY ONE of which forces
  `Unconfirmed`:
  - Literal hedging tags in FINDING.md: `[UNVERIFIED]`,
    `[UNVERIFIED — depends on …]`, `(UNVERIFIED)`, "unverified
    callees", "could not be verified from the supplied symbols",
    "source was not provided", "source was not available".
  - A non-empty `## Open questions` section in FINDING.md, OR a
    non-empty `open_questions:` list in metadata.yaml, where the
    answer to any listed question would change whether the bug
    exists. (Loose ends around an otherwise demonstrated defect
    are NOT this — see `Plausible` below.)
  - Sentences in FINDING.md or its task narrative of the shape
    "If X does Y, this finding is resolved" / "the bug does not
    exist if …" / "depends on whether …" / "must thread … through
    every internal allocation" / "must not take any mutex".
  - Conditional framing in the Summary or Impact: "may sleep",
    "would sleep", "if any internal allocation ignores the gfp",
    "if … then … silent corruption", without a demonstrated path
    that actually executes the bad behaviour.
  - Worked examples:
    - `atomic_cgwb_create_gfp_sleep`: call chain to
      `cgwb_create(GFP_ATOMIC)` is confirmed correct; the entire
      finding is whether three callees honour the gfp flag.
      Nothing was shown to sleep. → `Unconfirmed`.
    - `dup_anon_vma_stale_dst_anon_vma`: FINDING.md's Summary
      opens with `[UNVERIFIED — depends on
      cleanup_partial_anon_vmas() behaviour]`, and the Details
      state "The entire finding is conditional on whether
      `cleanup_partial_anon_vmas()` resets `dst->anon_vma`". One
      unverified callee gates the whole bug. → `Unconfirmed`,
      NOT `Unknown`.

- `Plausible` — the defect is **demonstrated** by the FINDING.md
  evidence (concrete code citations showing the bad path actually
  executes), but no crash / repro / fix has been observed
  upstream. Open questions may exist around severity or
  triggerability, but they don't gate whether the bug is real.

- `Fixed` — FINDING.md or metadata cite an upstream commit that
  resolves the issue.

- `Unknown` — reserved for the narrow case where FINDING.md is
  too thin or contradictory for you to classify it at all (e.g.
  the narrative is empty, or the symbols don't match the
  described path). **If the finding clearly documents that it is
  contingent on unverified facts, that is `Unconfirmed`, not
  `Unknown`.** "I, the triager, can't tell whether the bug is
  real" because the finding itself can't tell either → that's
  `Unconfirmed`.

- `Invalid` — metadata says `status: invalidated`, OR FINDING.md
  walks through evidence that the originally suspected bug does
  not exist.
- Subsystem is one sentence. Name the kernel area (e.g. "btrfs
  extent allocator", "TCP input path", "mac80211 rx") plus the file
  and function. Pull the file from `metadata.yaml`'s `filename:`
  when present.
- Impact prose stays in plain English. No "may", "could", "should"
  hedging unless FINDING.md actually says so — and if it does, cite
  it. Don't speculate beyond what the finding documents.
- Requirements: answer each question with one of `yes`, `no`, or
  `n/a` before the explanatory paragraph. If FINDING.md doesn't say,
  write `unknown` — don't guess.
- Details is a synopsis, not a re-paste of FINDING.md. Three to six
  sentences is plenty.
- Do not edit FINDING.md. Only write summary.md.

