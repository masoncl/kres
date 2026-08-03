## Kernel problem description rules

These rules track Documentation/process/submitting-patches.rst in the
target tree. When the doc and this template disagree, the doc wins —
the kernel maintainers read patches against their own conventions.

## Rule 0 — 75-column wrap (the most important one)

Every prose line in the message wraps at 75 columns. Count before
emitting. The only lines allowed to exceed 75 are:

- Verbatim code fragments quoted from source (indent those by four
  spaces — never use markdown fences in commit bodies).
- The trailer tags listed below (`Fixes:`, `Reported-by:`, etc.).
  submitting-patches.rst:148 explicitly exempts trailer tags from
  the wrap rule "in order to simplify parsing scripts".

Subject line, body paragraphs, and list items: all wrap at 75.

## Body

Write a kernel changelog, not an audit report. Make a non-prose
descriptor from the appended "Non-prose technical description
techniques" catalog the default way to explain the bug. Whenever a
timeline, call chain, call graph, state block, layout, calculation,
excerpt, or other catalog form can carry the facts, use it instead of
paragraphs.

Prose is supporting material only. Use the shortest possible prose only
to set up a descriptor or describe why it matters. Do not translate a
descriptor back into sentences, narrate each of its entries, or add a
paragraph that repeats the same causal steps. One short sentence is
enough when the descriptor needs context. Omit prose entirely when a
subject and descriptor make the problem clear.

Use concrete identifiers, but do not turn the body into an exhaustive
proof of every path that was checked.

```
<Optional one-sentence setup: observed bad behaviour, invariant
violated, or reason the change is worth making. Include user-visible
impact when relevant.>

<The smallest applicable non-prose descriptors carrying the mechanism
and evidence. Omit this block when no catalog form improves on one
short sentence. Cite code as filename:function, never with line
numbers.>

<Optional one-sentence description of why the descriptor matters. Cite
prior commits as `commit <sha-12+> ("<full subject>")`.>
```

Preferred shape:

- Start with a descriptor unless one short setup sentence is necessary.
- Select the smallest catalog technique that exposes the causal
  relationship. Combine techniques only when each carries different
  information.
- Keep each descriptor focused, normally 4-14 lines.
- Put identifiers, state transitions, ordering, branches, values, and
  calculations in the descriptor rather than spelling them out in
  prose.
- When code explains the bug, use a verbatim contiguous source excerpt.
  Precede it with filename:function and include enough enclosing control
  flow to locate the branch. Never paraphrase source, substitute generic
  operations, or attach invented comments to retained source lines. You
  may replace two or more unrelated consecutive lines with a standalone
  `[ ... ] // omitted: <reason>` marker. Never omit a single source line,
  use source-language comment syntax for an omission, or omit control
  flow or state changes relevant to the bug. Output-format capitalization
  rules never apply to quoted source.
- Pseudocode is allowed only for explaining a solution. Label it
  `pseudocode`; never use it as evidence of the existing bug.
- Use prose only to set up a descriptor or describe why it matters.
  Keep each prose paragraph to one sentence whenever possible and never
  more than three wrapped lines.
- Keep scope/proof paragraphs narrow: say only what matters to explain
  why the patch is needed and why this fix is valid.

## Optimisation and trade-off claims

If the change claims a performance, memory, stack, or binary-size
improvement, INCLUDE NUMBERS that back the claim
(submitting-patches.rst:64-70). Also describe the non-obvious cost
(extra CPU, more memory, less readable, worse for a different
workload). A "this is faster" claim with no numbers and no cost
analysis is a reviewer red flag.

## Backtraces

If a backtrace helps document the call chain, distill it
(submitting-patches.rst:770-790). Strip timestamps, module lists,
register dumps, stack dumps. Keep the function chain and the line
that actually identifies the failure. Use the distilled-backtrace
form in the appended descriptor catalog. Indent it by four spaces;
do not use markdown fences.

## What to AVOID

- Free-form bullet lists. Use a specific descriptor technique instead;
  reserve numbered lists for genuinely distinct failure modes.
- Dense proof-memo paragraphs. If a paragraph reads like a review
  transcript, replace it with a descriptor or delete non-essential
  proof.
- Prose that restates, summarizes, or walks through a descriptor.
- Exhaustive inventories of callers, exports, fallback paths, or
  negative cases unless each item is needed to understand the bug or
  justify the fix.
- Per-file change breakdowns ("modified foo.c, modified bar.c").
  The diff already enumerates files.
- Test enumeration: don't list new test names, don't cite passing
  test counts, don't write "Full workspace test run is clean".
  The commit message describes the user-visible change, not the
  developer's process.
- Review-process narration ("after discussion with X we decided
  ..."). The mailing list / PR thread carries that — if it must
  be referenced, use Link: instead.
- Punting the explanation to a Link: target. The body must stand
  alone (submitting-patches.rst:130-133).
- Speculation hedges ("may", "could", "should") in the problem or
  fix paragraphs unless the source code itself is uncertain. State
  what is actually true.
- Optimisation claims without numbers. "Faster" is not a fact.
