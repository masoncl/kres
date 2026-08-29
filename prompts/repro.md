# Reproduce a kres finding with virtme-ng

You're invoked from a linux worktree at `~/working/kres/linux.<tag>`
(or similar). The finding directory was included in the prompt as well.

Build a minimal reproducer, confirm the bug fires on the pre-fix
kernel and disappears on the fixed kernel, and report the exact
distinguishing output for both.

## Read first

- `findings/<tag>/metadata.yaml` — `git.sha` is the pre-analysis
  commit; `bugs[]` / `results[]` enumerate distinct mechanisms.
- `findings/<tag>/summary.md` — observable, trigger, mechanism
  citations.
- `findings/<tag>/auto-generated-fix.diff` if present — the patch
  under test (HEAD usually contains it).
- `findings/<tag>/FINDING.md` only if summary is insufficient.

## kverify structural analysis (if available)

If `metadata.yaml` contains a `validation_result.structural_evidence`
block, read it before writing the reproducer. It contains:

- `bug_class` — what kind of memory safety issue kverify's static
  analysis detected. Use this to pick your trigger strategy:
  - `unsafe_copy: true` → force a boundary-crossing copy (zero-length,
    negative, or oversized)
  - `untrusted_input: true` → craft input that reaches the function
    without sanitization
  - `unchecked_return: true` → force the error path (fault injection,
    resource exhaustion)
  - `dead_code: true` → function may be unreachable; check
    `reachability` before investing time

- `reachability.callers` — which functions call the buggy function.
  Start your trigger path from these callers. If `caller_count` is 0
  but the finding has smatch evidence, the function may be reachable
  via function pointer — check smatch caller_info.

- `suggested_sanitizers` — enable these CONFIG_ options. kverify
  derived them from the bug class; they're more specific than
  blanket KASAN.

- `staleness.status` — if `function_modified` or `deleted`, the code
  has changed since KRES analyzed it. Verify the mechanism still
  applies before building the reproducer. If `deleted`, skip — the
  function no longer exists.

- `upstream.fix_commits_found` — if > 0, a fix may already be in
  the tree. Check before reproducing.

- `conflicts` — if `present: true`, kverify's static stages disagreed.
  The `contradicting` list tells you what evidence pushed against the
  finding. Your reproducer should settle the dispute empirically.

This block is informational. Absence of `structural_evidence` means
kverify hasn't run on this finding — proceed with the existing
workflow.

## Build the reproducer

- Keep it under ~250 lines. Save to `<cwd>/<tag>_repro.c`,
  `gcc -O2 -Wall -Wextra -o <tag>_repro <tag>_repro.c`.

## Run in virtme-ng

Always invoke vng through the tmux MCP server, not Bash, and **always
pass `-v`**. `-v` routes the kernel console (oopses, BUG splats, KASAN
reports, panics) to host stderr where tmux captures it. Without `-v`,
the console is wired to `/dev/null` and the very crash you booted the
VM to see is silently dropped (see `virtme/commands/run.py:1815-1827`).

Also pass `oops=panic panic=10` so the guest panics + reboots on the
first oops and vng returns. Without it, an oops kills the calling
task in kernel mode, the guest stays alive, and your script never
reaches its trailing commands.

Drive the guest with an `--exec` script (not inline `bash -c '...'`).
Skeleton — write with the Write tool, not a heredoc:

```bash
#!/bin/bash
exec 2>&1
echo "===VNG_BEGIN==="
./<tag>_repro
rc=$?
echo "===VNG_END=== rc=$rc"
```

Then:

```
vng --build
sudo vng -v --user root --disable-microvm --cpus 8 --memory 16G \
    --overlay-rwdir=/root --qemu=/usr/libexec/qemu-kvm \
    --append "loglevel=8 oops=panic panic=10" \
    --exec /path/to/run_in_guest.sh
```

**Do not put a trailing `dmesg` in the script and rely on it for the
crash.** If the test wedged a task, the script never reaches the
`dmesg` line. With `-v`, every kernel printk is already streaming to
the pane live; the BUG/RIP/Call Trace appears at the moment of the
crash regardless of whether userspace ever returns. A trailing
`dmesg` is only useful as a belt-and-braces dump of late warnings on
clean runs — never as the primary crash-capture mechanism.

- use more CPUs or memory if required

## Confirm both sides

1. If HEAD has the fix, run once and capture the FIXED outcome.
2. Back out the fix in place by checking out the fix-touched files
   from the buggy base SHA printed by `repro-one.sh`:
   `git checkout <base-sha> -- <files-the-fix-touched>`. Verify the
   diff matches the inverse of all `auto-generated-fix*.diff` patches.
3. `vng --build`, run again, capture the BUGGY outcome.
4. Restore: `git checkout HEAD -- <files>`.

Worktree is git; do not push, do not change branches, do not touch
anything outside this cwd and the guest.

## Kernel modifications

Beyond the fix revert in "Confirm both sides," the following are
permitted to make a bug easier to observe:

- **Debug-only source additions, anywhere in the tree:** `printk`,
  `pr_warn`, `pr_info`, `WARN_ON*`, `BUG_ON`, ftrace markers,
  counters, dumps of internal state. These are observation only --
  they must not change control flow that affects the observable.
- **Race-window widening inside the cited code path:** inserting
  `udelay`, `usleep_range`, `schedule()`, `msleep`, `mdelay`, or
  moving an existing barrier, to make an already-existing race window
  easier to hit. Keep these edits inside the file/function cited by
  `relevant_symbols[]` or `relevant_file_sections[]` of
  `metadata.yaml`.
- **Enabling debugging CONFIG_ options.** Use
  `scripts/config --enable <OPT>` (or `--disable`) followed by
  `make olddefconfig` before `vng --build`. Examples: `KASAN`,
  `KMEMLEAK`, `DEBUG_KMEMLEAK_DEFAULT_OFF` (disable), `PROVE_LOCKING`,
  `LOCKDEP`, `DEBUG_OBJECTS*`, `DEBUG_SLAB`, `DEBUG_LIST`,
  `PAGE_POISONING`, `PREEMPT`, `PREEMPT_RT` (if the bug claims
  CONFIG_PREEMPT_RT relevance), `KCSAN`, `UBSAN`.

**Symmetric rule:** every source edit and every CONFIG_ change above
must apply to BOTH the BUGGY and FIXED build. The only edit allowed
to differ between the two builds is the fix revert itself (which
must still pass the `diff matches inverse of auto-generated-fix.diff`
check).

**Forbidden:** introducing the bug itself. Do not delete checks,
locks, allocations, refcounts, RCU pairings, or initialisations
beyond `auto-generated-fix.diff`. Do not plant a UAF, OOB, leak, or
data race that wasn't already in the buggy source. The buggy-side
symptom must come from the original buggy source (plus the reverted
fix, plus permitted debug additions), not from a defect you added.

Artificially changing kernel logic to make the bug happen is never
allowed. Do not modify counters, flags, return values, branch
conditions, loop conditions, state machines, object lifetimes, list/tree
membership, refcounts, cgroup membership, allocation outcomes, or error
paths in order to steer execution into the suspected buggy path. If the
bug requires a function, loop, callback, teardown path, or retry path to
run twice, the userspace reproducer and normal in-kernel concurrency
must make that happen without artificial state changes. Timing-only
race widening is allowed; semantic forcing is not.

After every kernel source or CONFIG_ change used for reproduction,
create these two files in the Linux worktree root:

- `repro.diff` — a complete `git diff` containing 100% of the kernel
  source and configuration changes used to reproduce the bug, excluding
  generated build output and excluding the BUGGY-side temporary fix
  revert. If no kernel source or CONFIG_ changes beyond the fix/revert
  were used, write a short empty-diff note in the file.
- `repro-diff-summary.md` — a concise explanation of every hunk in
  `repro.diff`, classifying each as `debug`, `race-widen`, or
  `config`, and explicitly stating why it does not change the logic
  needed to trigger the bug.

Record every kernel edit and CONFIG_ change in the report so a human
can audit (see Report item 6 below), and include the absolute paths to
`repro.diff` and `repro-diff-summary.md`.

## Leak claims (only if applicable)

If the finding claims a memory leak:

```
scripts/config --enable DEBUG_KMEMLEAK --disable DEBUG_KMEMLEAK_DEFAULT_OFF
make olddefconfig
vng --build
sudo vng ... --append "kmemleak=on" --exec "./leak_scan.sh"
```

Inside the guest: loop the reproducer ≥5000 times, then
`echo scan > /sys/kernel/debug/kmemleak; sleep 6; echo scan > /sys/kernel/debug/kmemleak; cat /sys/kernel/debug/kmemleak`.
An empty file means no leak. Confirm kmemleak itself ran by
grepping dmesg for `kmemleak: Kernel memory leak detector initialized`.

## Claim mismatches (partial invalidation)

A successful reproduction is not a free pass for everything the
finding files say. Before declaring `bug_reproduced`, re-read each
prose claim in `summary.md`, `FINDING.md`, `metadata.yaml`, and the
commit-message text inside `auto-generated-fix.diff`, and compare
each one against what you actually observed:

- Affected architectures / word sizes (e.g. "both 32-bit and 64-bit"
  vs only one).
- CONFIG dependencies (e.g. "independent of CONFIG_PREEMPT_RT" vs
  only fires with PREEMPT_RT).
- Required capabilities (e.g. "unprivileged user" vs needs CAP_BPF).
- Required preconditions (cpu count, kernel cmdline, module).
- The mechanism description (the cited code path matches what the
  reproducer actually exercises).

Every claim the run contradicts goes into the `claim_mismatches` array
in the final JSON, one entry per contradiction, with:
- `source`: `summary.md`, `FINDING.md`, `metadata.yaml`, or
  `auto-generated-fix.diff` (the commit-message inside the diff
  counts as `auto-generated-fix.diff`). When the wrong claim is in
  the commit-message of `auto-generated-fix.diff`, use that source
  value -- the driver will add an "ACTION REQUIRED: auto-generated-
  fix.diff needs updating" banner at the top of `FINDING.md` so a
  human knows the fix patch must be regenerated/re-worded before it
  goes upstream.
- `wrong_claim`: the offending sentence quoted verbatim.
- `evidence`: what you observed + cite (file:line, dmesg line,
  command output) that contradicts it.

Do not silently fix the prose, do not bury the mismatch in
`not_verified[]` (that field is for things you didn't test), and do
not lower the verdict to `not_reproduced` just because the prose was
wrong.

For general observations from the run that aren't a specific
claim-vs-evidence contradiction (timing surprises, additional
preconditions discovered, side effects, kernel modifications that
were necessary, what kind of stress is needed) put a short prose
paragraph in `repro_notes` (free text).

The downstream driver will prepend a `## Repro notes` section at the
top of `FINDING.md` (under the existing title) containing your
`repro_notes` prose and any `claim_mismatches` entries. Do not
manually prepend that same section to `FINDING.md`; the driver owns
that exact insertion.

You must still keep the other finding files in sync before your final
JSON:

- Update `summary.md` so its `# Status`, `# Impact`, `# Requirements`,
  and `# Details` sections reflect what the repro actually proved. If
  the original active bug was invalidated, state that plainly near the
  top and preserve the contradicting evidence. If the result is
  `not_triggerable`, mark the issue as latent / not presently
  triggerable and explain the blocking call-graph evidence. If the bug
  reproduced with caveats, keep the bug as confirmed but correct or
  qualify the claims contradicted by the run.
- Update `metadata.yaml` consistently with the verdict. At minimum,
  set `status:` to the same value the driver will set:
  `confirmed`, `confirmed_with_caveats`, `invalidated`, or
  `confirmed_latent`. Adjust `severity:` only when the repro evidence
  clearly changes impact. Add or update concise machine-readable
  notes if the file already has an appropriate reviewed-coding or
  result field; do not invent a large new schema.
- Record these edits in the final JSON `finding_edits` array with
  paths and a one-line reason for each edited file.

The driver will also set `metadata.yaml`'s `status:` from your verdict.
If you already set the same value, the driver update is idempotent.

## Verdict

Pick exactly one for the final JSON `verdict` field:

- `bug_reproduced` — built and ran a reproducer, observed the BUG
  on the buggy kernel, and the fix removed it. This is the only
  verdict that proves the finding.
- `not_reproduced` — built and ran a real reproducer against the
  buggy kernel and the BUG did not fire. The finding's mechanism
  is empirically wrong. Put the contradicting evidence (what fired
  vs. what summary.md claims) in `repro_notes`.
- `not_triggerable` — analyzed the code and concluded no in-tree
  caller can reach the buggy path with attacker-controlled inputs,
  or constructing a trigger would require planting the bug itself
  (forbidden by the "Kernel modifications" rules). Use this when
  the buggy code exists but is unreachable, not when tooling broke.
  Cite the call-graph evidence in `repro_notes` (each blocking
  caller with file:line and what blocks it — e.g. forces
  BTF_SHOW_UNSAFE, restricts to vmlinux BTF, requires CAP_SYS_ADMIN
  that no userland path grants).
- `setup_failure` — build/boot/tooling broke and you could not
  make a determination. Put the failure in `not_verified[0]`. Do
  NOT use this verdict to mean "no trigger path exists" — that's
  `not_triggerable`.

Driver behavior per verdict:

- `bug_reproduced` → `status: confirmed` (or `confirmed_with_caveats`
  if `claim_mismatches` is non-empty); reproducer source + binary
  copied next to the finding.
- `not_reproduced` → `status: invalidated`. The finding's mechanism
  is empirically wrong.
- `not_triggerable` → `status: confirmed_latent`. The bug code and
  mechanism are real but unreachable from any in-tree caller; the
  fix still has value as defensive hardening but the finding does
  not describe a presently reachable vulnerability.
- `setup_failure` → `metadata.yaml` untouched.

For every negative verdict the driver also prepends your
`repro_notes` to `FINDING.md`, so put the call-graph / empirical
evidence there — that is the human audit trail for the status flip.
Your `summary.md` and `metadata.yaml` edits are the parallel concise
record used by humans and database tooling.

## Rules

- Cite a file:line, command output, or log line for every factual
  claim. If you can't cite evidence, say "I haven't verified this"
  instead of guessing.
- A reproducer that produces the same output on both kernels proves
  nothing. Re-check the observable.
- Do not manually prepend repro notes to `FINDING.md`; the driver
  (`repro-one.sh`) owns that edit and copies the reproducer artifacts.
  Do update `summary.md` and `metadata.yaml` before the final JSON so
  the concise summary and machine-readable metadata match the verdict.
  If you need to change `FINDING.md` for something other than the
  driver-owned repro-notes block, keep it minimal and list it in
  `finding_edits`.
- For *partial* contradictions where the bug exists but specific
  claims are wrong, use `claim_mismatches` with verdict
  `bug_reproduced` — do not downgrade to `not_reproduced`.

## Report

End with:
1. BUGGY-side output (1-3 lines is fine).
2. FIXED-side output.
3. Paths to the reproducer source and binary.
4. Any finding-file edits made.
5. What you did *not* verify (write-after-update behavior,
   reach beyond the one trigger file type, race variants, etc.).
6. Kernel modifications: for each source edit, `file:line` and a
   one-line purpose (`debug: WARN_ON underflow`, `race-widen:
   udelay(50) before unlock`, etc.). For each CONFIG_ change, the
   option name and on/off. State explicitly that these were present
   in BOTH the BUGGY and FIXED builds.
7. Paths to `repro.diff` and `repro-diff-summary.md` in the Linux
   worktree root, and a one-line statement that `repro.diff` contains
   all kernel source and CONFIG_ changes used by the repro.
