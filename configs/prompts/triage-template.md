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
  the best fit from the FINDING.md evidence. Use `Unknown` when
  you can't tell, not a guess.
- Use `Unconfirmed` when the finding is not a bug report but a set
  of open questions or unverified hypotheses — i.e. the analysis
  could not demonstrate the defect, only flag callees or paths
  that *would* be bugs if some unverified condition holds. Tells:
  - FINDING.md's narrative is dominated by an `## Open questions`
    list, "Unverified callees", or "could not be verified from
    the supplied symbols" language.
  - The summary/impact uses "may", "if any internal allocation",
    "would sleep", "must thread", or similar conditional phrasing
    rather than a demonstrated failure.
  - `metadata.yaml`'s `open_questions:` list carries the load of
    the finding (the questions *are* the finding, not loose ends
    around a confirmed defect).
  - Example: `atomic_cgwb_create_gfp_sleep` — the call chain to
    `cgwb_create(GFP_ATOMIC)` is confirmed correct, and the
    finding only asks whether `percpu_ref_init()`,
    `percpu_counter_init()`, and `fprop_local_init_percpu()`
    honour the gfp flag. Nothing was shown to sleep; the finding
    is a question, not a bug. → `Unconfirmed`.
  Pick `Plausible` instead when FINDING.md actually demonstrates
  the defect path (concrete code citation showing the bad
  behaviour) but no crash/repro has been observed in the wild.
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

