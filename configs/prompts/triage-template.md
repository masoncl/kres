You are triaging a single kernel bug finding produced by `kres
--export`.

## Input

The **first line of this prompt is the absolute path of the
finding's directory** — call it `DIR`. Use that exact path
everywhere below; do not treat `$DIR` as a shell variable that
something else expands.

`DIR` contains:

- `DIR/metadata.yaml` — id, title, severity, status, filename,
  subsystem (may be empty), git head, optional `introduced_by`,
  and lists of `relevant_symbols` and `relevant_file_sections`.
- `DIR/FINDING.md` — full narrative: summary, mechanism,
  reproducer, impact, fix sketch, open questions, per-task
  analysis details, relevant symbols and file excerpts.

Read both before writing. Do not invent facts that aren't in
those two files or in the actual source tree at `metadata.yaml`'s
`git.sha`.

## Output

Write the triage to `DIR/summary.md`, replacing any existing copy.
Do NOT edit `FINDING.md`. `metadata.yaml` is editable in two
narrow ways — see "metadata.yaml updates" below.

Emit summary.md as a single `code_output` entry with `path` set
to the **absolute** `DIR/summary.md` path. The operator named
`DIR` in the prompt, so the consent gate already permits writes
there — no bash, no cp, no relative-path hack:

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

Wrap prose at 78 characters. Count as you write. Only verbatim
code excerpts may exceed.

The body of `summary.md` is exactly the structure below, in this
order. Every section is required.

```
[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)

# Subject: <one-line bug description>

# Status

<Fixed | Plausible | Unconfirmed | Unknown | Invalid>

# Subsystem

<single sentence naming the kernel subsystem AND the file and
function the bug lives in>

# Impact

<at most two paragraphs of plain description. A single sentence
is fine if that's enough. Plain language, no run-on sentences.>

# Requirements

- Host access?
- Remote access?
- Only under specific CONFIG_?
- As root?
- As regular user?

<then a short paragraph explaining the conditions needed to
trigger the bug>

# Details

<3–6 sentence synopsis. Full details are in FINDING.md; this
file is what gets read while triaging — keep it skimmable.>
```

Two structural rules that the model gets wrong if not stated
explicitly:

- The very first line of `summary.md` is the cross-link line,
  verbatim, followed by one blank line. Output that starts with
  `# Subject:` is a template violation.
- The Subject line is the `# Subject:` heading itself. Do NOT
  add any separate heading above it; the cross-link line is the
  only content that precedes `# Subject:`.

## Status decision tree

Decide the status BEFORE writing the rest of summary.md. Walk
these in order and pick the **first** match.

### 1. `Invalid`

`metadata.yaml` says `status: invalidated`, OR FINDING.md walks
through evidence that the originally suspected bug does not
exist.

### 2. `Fixed`

FINDING.md or metadata cite an upstream commit that resolves the
issue.

### 3. `Unconfirmed`

The finding's own narrative admits the bug is contingent on
something the analysis did not verify. This is the default for
question-style findings — anything where confirming the bug
would require reading code the analysis did not have.

ANY ONE of these tells forces `Unconfirmed`:

- Hedging tags in FINDING.md: `[UNVERIFIED]`,
  `[UNVERIFIED — depends on …]`, `(UNVERIFIED)`, "unverified
  callees", "could not be verified from the supplied symbols",
  "source was not provided", "source was not available".
- A non-empty `## Open questions` section in FINDING.md or
  `open_questions:` list in metadata.yaml whose answers would
  change whether the bug exists. (Loose ends around an otherwise
  demonstrated defect are NOT this — see `Plausible`.)
- Sentence shapes like "if X does Y, this finding is resolved",
  "the bug does not exist if …", "depends on whether …", "must
  thread … through every internal allocation", "must not take
  any mutex".
- Conditional Summary/Impact framing ("may sleep", "would
  sleep", "if any internal allocation ignores the gfp") with no
  demonstrated path that actually executes the bad behaviour.

Worked examples:

- `atomic_cgwb_create_gfp_sleep` — the call chain to
  `cgwb_create(GFP_ATOMIC)` is confirmed correct; the entire
  finding is whether three callees honour the gfp flag. Nothing
  was shown to sleep. → `Unconfirmed`.
- `dup_anon_vma_stale_dst_anon_vma` — FINDING.md's Summary opens
  with `[UNVERIFIED — depends on cleanup_partial_anon_vmas()
  behaviour]`, and Details say "the entire finding is
  conditional on whether `cleanup_partial_anon_vmas()` resets
  `dst->anon_vma`". One unverified callee gates the whole bug.
  → `Unconfirmed`, NOT `Unknown`.

### 4. `Plausible`

The defect path is **demonstrated** by FINDING.md evidence —
concrete code citations showing the bad path actually executes.
No crash / repro / upstream fix has been observed. Open
questions may exist around severity or triggerability, but they
do not gate whether the bug is real.

### 5. `Unknown`

Narrow fallback for when FINDING.md is too thin or contradictory
to classify at all (empty narrative, symbols don't match the
described path). "I can't tell whether the bug is real" because
the **finding itself** can't tell either is `Unconfirmed`, not
`Unknown`.

## metadata.yaml updates

Edit `metadata.yaml` in exactly these two ways; **NO** other
edits are permitted.

1. `subsystem:` — if the field is empty and you've identified
   the subsystem, fill it in.
2. `status:` — if and only if you picked `Unconfirmed` AND the
   metadata currently says `status: active`, flip it to
   `status: unconfirmed`. In every other case the status field
   is left exactly as it is (`active`, `invalidated`, etc.).

## Wording

- Spread information out. Dense paragraphs are hard to read.
  Break series of factual sentences into logical groups
  separated by blank lines, and put a blank line before any
  closing question.
- Impact stays in plain English. No "may" / "could" / "should"
  hedging unless FINDING.md uses it — and if it does, cite the
  spot. Don't speculate beyond what the finding documents.
- Requirements: answer each question with `yes`, `no`, `n/a`,
  or `unknown` before the explanatory paragraph. If FINDING.md
  doesn't say, write `unknown` — don't guess.
- Subsystem is one sentence. Name the kernel area (e.g. "btrfs
  extent allocator", "TCP input path", "mac80211 rx") plus the
  file and function. Pull the file from metadata's `filename:`
  when present.

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
