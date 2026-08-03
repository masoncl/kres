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

Write a kernel changelog, not an audit report. Recent commits
favor a short causal explanation that a maintainer can read quickly:
what goes wrong, the relevant mechanism, and what the patch changes.
Use concrete identifiers, but do not turn the body into an exhaustive
proof of every path that was checked.

```
<Problem paragraph: observed bad behaviour, invariant violated, or
reason the change is worth making. Wrap at 75. Include user-visible
impact: crash signature, latency spike, lockup pattern, refcount
leak, dmesg excerpt — whatever helps a stable-tree maintainer
deciding whether to backport.>

<Optional mechanism paragraph. Cite prior commits as
`commit <sha-12+> ("<full subject>")` — at least 12 hex chars.
Cite code as filename:function. Never cite source line numbers.>
```

Preferred shape:

- 2-4 short prose paragraphs, normally 1-4 wrapped lines each.
- Start with the failing path and consequence in plain language.
- If the bug depends on ordering, callbacks, nested calls, state
  transitions, or two CPUs racing, use indented evidence blocks instead
  of dense prose. Prefer call chains and call graphs over prose for
  multi-function control flow. Choose the structure that makes each
  causal step clearest: a call chain, ASCII call graph, CPU timeline,
  before/after state block, short case analysis, numeric example, or a
  source snippet when the decisive fact is the local branch or
  expression itself. Keep each evidence block focused, normally 4-14
  lines. Multiple evidence blocks are fine when they replace paragraphs
  of compact prose and each block carries a different part of the bug.
- Simple ASCII art is allowed in indented evidence blocks. Use only
  ASCII characters such as `|`, `-`, `+`, `<`, `>`, and `->`; do not
  use Unicode arrows, box drawing, or other non-ASCII diagram glyphs.
- Keep scope/proof paragraphs narrow: say only what matters to explain
  why the patch is needed and why this fix is valid.

Evidence block examples to follow:

Race timeline:

    CPU 0                         CPU 1
    -----                         -----
    show_state()
    obj = container->obj;         mutex_lock(&container->lock);
                                  replace_object(container, NULL);
                                  free_object(obj);
                                  mutex_unlock(&container->lock);
    use_object(obj);              /* obj is freed */

Call chain with state transition:

    handle_event()
      mark_object_unavailable()
      split_object()
        remap_entries()
          inspect_unused_entry()
            read_object_data()    /* reads unavailable state again */

Call graph:

    fault path
      handle_fault()
        lookup_cached_object()
          batch_install_entries()
            uses stale index

    teardown path
      replace_object()
        clears slot
        frees old object

Before/after state:

    before: slot points at old owner
            callers take old_owner->lock and mutate the object
    clear:  slot becomes NULL before the object is moved
    after:  callers fall back to new_owner->lock while the object is
            still linked under old_owner->lock

Use evidence blocks to carry the mechanics, then keep prose short.
Do not restate every line of a block in prose.

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
that actually identifies the failure. Indent the distilled
backtrace by four spaces; do not use markdown fences. Example
shape:

    unchecked MSR access error: WRMSR to 0xd51 ...
    at rIP: 0xffffffffae059994 (native_write_msr+0x4/0x20)
    Call Trace:
    mba_wrmsr
    update_domains
    rdtgroup_mkdir

## What to AVOID

- Bullet lists used as a substitute for prose. The kernel body is
  prose paragraphs; lists are reserved for the enumerated-breakage
  shape.
- Dense proof-memo paragraphs. If a paragraph reads like a review
  transcript, split it, delete non-essential proof, or quote the
  decisive code snippet.
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
