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
`metadata.yaml` and `FINDING.md` are editable only in the narrow
ways described under "metadata.yaml updates" and "FINDING.md
status/severity header" below — no other edits to either file.

Emit summary.md as a single `code_output` entry with `path` set
to the **absolute** `DIR/summary.md` path. The operator named
`DIR` in the prompt, so the consent gate already permits writes
there — no bash, no cp, no relative-path hack. Also emit
`metadata.yaml` and `FINDING.md` as additional `code_output`
entries with their own absolute paths and full file contents so
the chosen severity is synchronized everywhere:

```
"code_output": [
  {
    "path": "<absolute DIR>/summary.md",
    "content": "<full body>",
    "purpose": "triage summary"
  },
  {
    "path": "<absolute DIR>/metadata.yaml",
    "content": "<full metadata.yaml body with severity updated>",
    "purpose": "triage metadata"
  },
  {
    "path": "<absolute DIR>/FINDING.md",
    "content": "<full FINDING.md body with severity/status header updated>",
    "purpose": "triage finding header"
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

<Fixed | Plausible | Unconfirmed | Unknown | Invalid | Confirmed Latent>

# Severity

<high | medium | low>

<one short paragraph explaining why this severity is correct. This
must match `metadata.yaml` and `triage_coding.severity`.>

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

FINDING.md walks through evidence that the originally suspected
bug does not exist, OR `metadata.yaml` already says
`status: invalidated` AND current source still supports that
nonexistence. A stale `invalidated` status is not authoritative on
its own: if current evidence shows the defect pattern is actually
real, do not stop here — let the tree reach `Confirmed Latent`
(real but no current trigger) or `Plausible` (real and
reachable).

### 2. `Fixed`

FINDING.md or metadata cite an upstream commit that resolves the
issue.

### 3. `Unconfirmed`

The finding's own narrative admits the bug is contingent on
something the analysis did not verify. This is the default for
question-style findings — anything where confirming the bug
would require reading code the analysis did not have.

ANY ONE of these tells forces `Unconfirmed`, **but only when the
hedge or question gates whether the bug exists**. A hedge or
open question attached to a peripheral concern — fix strategy,
severity, a sibling finding, a non-load-bearing detail — leaves
the verdict at `Plausible` if the core defect is otherwise
demonstrated. Read each tag in context; do not match by keyword.

- Hedging tags in FINDING.md: `[UNVERIFIED]`,
  `[UNVERIFIED — depends on …]`, `(UNVERIFIED)`, "unverified
  callees", "could not be verified from the supplied symbols",
  "source was not provided", "source was not available". Apply
  the gating test above — a `[UNVERIFIED]` next to "exact
  locking model determines fix strategy" does not force
  `Unconfirmed`; one next to "whether the bad path executes at
  all" does.
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

### 4. `Confirmed Latent`

The defect pattern **genuinely exists** in source — you verified
the code shape is real — but 100% of the finding has no current
in-tree trigger: every required precondition, hook, caller, or
state is absent or cannot occur at `metadata.yaml`'s `git.sha`, so
the defect cannot fire today. This is distinct from `Invalid` (the
defect does not exist as described) and from `Unconfirmed` (you
could not verify the gate either way). Use `Confirmed Latent` only
when the dormant structure is real AND every trigger gate is
proven closed — nothing is left open.

`Confirmed Latent` (and any downgrade that rests on something
being absent) is a negative-coverage claim, so it must be earned.
Before concluding a trigger, caller, hook, registration, or
precondition is absent, cite concrete evidence for that absence —
a grep/search that came back empty, a read of the registration
site, a callgraph, or history — not an assumption. semcode is an
accelerator, not an authority: a failed, empty, or "not found"
semcode result must be re-checked with local `grep`/`read` before
you treat the symbol or path as absent. If you cannot earn the
absence, the status is `Unconfirmed`, not `Confirmed Latent`.

When you pick `Confirmed Latent`, also reflect it in
`triage_coding`: add `latent` to `impact_classes`, add
`latent_only` to `reject_reasons`, and answer the trigger-related
`reachability` gates you closed with `no` rather than `unknown`.
State in `# Impact` that the pattern exists in source but is
unreachable in this tree, and what future change would make it
reachable.

### 5. `Plausible`

The defect path is **demonstrated** by FINDING.md evidence —
concrete code citations showing the bad path actually executes.
No crash / repro / upstream fix has been observed. Open
questions may exist around severity or triggerability, but they
do not gate whether the bug is real.

### 6. `Unknown`

Narrow fallback for when FINDING.md is too thin or contradictory
to classify at all (empty narrative, symbols don't match the
described path). "I can't tell whether the bug is real" because
the **finding itself** can't tell either is `Unconfirmed`, not
`Unknown`.

## metadata.yaml updates

Edit `metadata.yaml` in exactly these three ways; **NO** other
edits are permitted.

1. `subsystem:` — if the field is empty and you've identified
   the subsystem, fill it in.
2. `status:` — set it to match the verdict you picked in the
   Status section above, using this mapping:

   | summary verdict | metadata `status:` |
   | --------------- | ------------------ |
   | Fixed            | fixed             |
   | Plausible        | active            |
   | Unconfirmed      | unconfirmed       |
   | Invalid          | invalidated       |
   | Confirmed Latent | confirmed_latent  |
   | Unknown          | leave as-is       |

   Apply the mapping every time, in both directions. If the
   metadata already says `unconfirmed` and you have now picked
   `Plausible`, flip it to `active`. If it says `active` and
   you picked `Invalid`, flip it to `invalidated`. The only
   case where the field is left untouched is `Unknown`, which
   means the finding itself is too thin to classify.
3. `severity:` — always set it to the severity you chose:
   `high`, `medium`, or `low`. If the field is missing, add it
   near the top-level `status:` field. If it is present, replace
   the old value. This value is the canonical replacement for any
   earlier automated severity rating.

## Severity decision tree

Decide severity from the summary you are writing and the finding's
documented impact/reachability. Do not copy the old severity from
`metadata.yaml` or `FINDING.md`; those values may be wrong.

Pick exactly one:

### `high`

Use `high` when the demonstrated or fixed bug can plausibly cause
memory corruption, use-after-free, out-of-bounds write, cross-tenant
or remote data exposure, vm/container escape, privilege boundary
crossing, persistent data corruption, or a reliable remote/local
kernel crash in a commonly reachable subsystem.

### `medium`

Use `medium` when the bug is real and actionable but impact or
reachability is meaningfully constrained: authenticated-only remote
trigger without a stronger primitive, local denial of service,
information exposure with limited sensitivity, corruption limited
to one user/workload, niche but production-capable CONFIG, or a
latent defect with a credible path to becoming reachable.

### `low`

Use `low` for invalidated findings, fixed issues that only need
historical bookkeeping, test/debug-only paths, niche configurations
with weak impact, protocol-correctness issues without memory or data
integrity consequences, and latent-only defects with no current
in-tree trigger.

## FINDING.md status/severity header

`FINDING.md` carries `**Severity:**` and `**Status:**` lines near
the top. Always update `**Severity:**` to the chosen
`high`/`medium`/`low` value. Whenever you change `metadata.yaml`'s
`status:`, update `**Status:**` to the same value so the two stay
in sync. Do not touch any other part of `FINDING.md`.

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
- Subsystem is one sentence. Name the kernel area plus the file and
  function. Pull the file from metadata's `filename:` when present;
  do not invent a subsystem from the bug title alone.

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
