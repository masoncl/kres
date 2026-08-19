# Guards

## Guard Definition

```
    if (!TEST(state))       // TEST   reads state, establishes a fact about it
            bail;

    stmt_a;                 // WINDOW everything between beginning of TEST and
    stmt_b;                 //        end of USE

    USE(state);             // USE    relies on that fact still holding
```

A `guard` is those three parts: `TEST`, `WINDOW`, `USE`.  Each one may be
arbitrarily complex, including function calls, conditionals and loops, and may
span multiple statements.

Name the `fact` `TEST` establishes — the specific property `USE` relies on. The
defect is the `fact` being false when `USE` runs. Nothing needs to be freed or
unmapped for that: an object can stay allocated, mapped and reachable while the
property `TEST` checked about it stops holding.

Every change to `state` from the beginning of `TEST` to the end of `USE` is
potentially a bug. Bugs may include:
  - `state` changes in the WINDOW being researched.
  - logic errors in `TEST`, `WINDOW`, `USE` sections
  - races with other CPUs

IMPORTANT: locking alone does not ensure correctness.  This review lens exists
specifically to research logic errors and `state` changes separate from
locking issues and without races against other CPUs.

NEVER declare a bug invalid simply because a lock is held without fully
researching the `state`, `TEST`, `WINDOW` and `USE`.  If there are missing function
definitions in the callchain for `TEST`, `WINDOW`, `USE` or `state`, read
them as followups with `required_for_progress: true`.  NEVER make conclusions
without evidence.

IMPORTANT: `USE` may not only read `state` — it can derive, copy, or publish new objects
from it. If the `fact` was false, those objects carry the falsehood outward:
follow what `USE` creates and where it is registered, and check whether that
structure assumes the `fact` held. A guard defect can be harmless at `USE`
itself and harmful in what `USE` leaves behind.

## Boundaries

`WINDOW` begins inside `TEST`, not at `USE`. All code between the
beginning of `TEST` and the end of `USE` are part of `WINDOW`.

`state` is whatever `TEST` or `USE` read AND all other conditions (`implicit_state`)
that `TEST` didn't read directly but that `USE` correctness requires.

When `TEST`, `WINDOW` or `USE` include function calls, you must research how
`state` and `implicit_state` are read AND modified throughout the entire call
chain.

## Other potential guard defects

- `TEST` fails to check all `USE` requirements.
- The decision is stored, returned, or cached, then used after it can
  have changed.
- A sibling path reaches `USE` without passing `TEST`.

<!--
Maintainer note. This whole file, including this comment, is included
verbatim into the review step's prompt via `steps[].include` in
configs/workflows/review.json, so every lens reads it. Keep it
definitional and keep it short.

The procedure that drives this analysis lives in that same file, on
lenses[id=memory-lifetime].investigate — the guard pass rides with the
lifetime pass because both are tracing the same object. It is not
duplicated here.
-->

