# Non-prose technical description techniques

This report catalogs non-prose techniques for explaining bugs, technical
changes, data structures, and measured effects. All names and examples are
fictional. Use a technique whenever it makes the causal relationship clearer
than prose, and choose the smallest applicable form. Do not force a descriptor
onto a fact that is already clearer as one short sentence.

The strongest examples replace dense prose. They do not repeat the diagram,
table, trace, or calculation in a paragraph immediately afterward.

## Hard rule: never draw boxes

LLMs are not allowed to draw ASCII or Unicode boxes. Do not use matched
corners and edges, box-drawing characters, bordered grids, or any construction
that depends on aligned borders. Their alignment is fragile and their output
is hard to read when spacing changes. Use an ordered list, an indented
hierarchy, aligned key/value rows, or connector lines without an enclosing
border.

## Concurrency and time

### Two-column CPU or thread timeline

Use a timeline when correctness depends on the ordering of operations on two
execution contexts. Align related events vertically and mark the failing use.

For example, show workspace teardown racing device unregistration:

    CPU 0 (workspace teardown)   CPU 1 (device unregister)
    ==========================   =========================
    cleanup_workspace()
      detach_registry()          widget_unregister()
        event_channel = NULL       widget_notify()
                                      listener_active(NULL)
                                      /* crash */

The same form can show a reference acquisition racing a final release:

    CPU 0 (attach)                    CPU 1 (unregister)
    widget_read_lock()
    find node, refcount == 1
    widget_read_unlock()
                                       widget_ref_put() -> 0
                                       widget_remove(node)
                                       widget_free(node)
    widget_ref_get(node)               /* use-after-free */

### Ordered event sequence

Use a short sequence when one actor passes through several relevant steps and
parallel columns would add no information:

    publish object
      drop protecting lock
        remove object from lookup structure
          release final reference
            stale user dereferences object

This is common in lifetime and teardown fixes. The sequence should contain
only the operations needed to expose the broken ordering.

### State-machine transition path

Use an arrow-separated path when named states are more important than the
functions implementing them. A reset flow can be summarized as:

    IDLE -> QUIESCING -> DRAINING -> TEARDOWN -> IDLE

Annotate transitions when their timing matters:

    IDLE
      -> QUIESCING     gate new requests synchronously
      -> DRAINING      flush dirty state for a bounded interval
      -> TEARDOWN      discard state that cannot progress
      -> IDLE          wake blocked callers

### Lock-order or dependency chain

Use a dependency chain to show an inversion or unsafe context transition:

    existing path:  cache_lock -> object_lock
    proposed path:  object_lock -> cache_lock
    result:          circular dependency

A dependency-checker report is another form of the same evidence. Reduce it to
the essential dependency:

    object_lock -> index_lock
    interrupt-safe  interrupt-unsafe

Prefer the reduced dependency over a complete diagnostic dump when it proves
the same point.

## Control flow

### Linear call chain

Use a call chain when a single path reaches the defect:

    update_widget()
      replace_widget()
        flush_widget_cache()
          walk freed widget_entry

### Branching call graph

Use a call graph when two paths interact or one caller has multiple important
descendants:

    fault path
      widget_handle_fault()
        widget_lookup_cached()
          widget_install_batch()
            uses stale index

    teardown path
      widget_replace()
        clears slot
        frees old object

This form is especially useful for lifetime bugs where allocation, use, and
teardown are in different subsystems.

### Distilled stack trace or backtrace

Retain the diagnostic, identifying frame, and useful call chain. Remove
timestamps, register dumps, module lists, and unrelated frames. Reduce a
sanitizer report to the relevant path:

    BUG: sanitizer: global-out-of-bounds in widget_probe()
    Call Trace:
      report_failure()
      widget_probe()

The trace is evidence of reachability, not a substitute for explaining the
invalid access.

### Enumerated branches or failure modes

Use bullets or a numbered list only when a bug has distinct failure modes.
For example, enumerate separate timer, wakeup, generation, and error-handling
failures. Keep each item to one causal statement:

    1. A stale timer remains armed when a new transfer is claimed.
    2. Blocked senders remain asleep after shutdown begins.
    3. An old generation modifies timer state after ownership changes.

Do not use a list merely to break ordinary prose into fragments.

## State and comparison

### Before/after state block

Use this form when one operation changes ownership or an invariant:

    before: slot points at old owner
            callers use old_owner->lock
    clear:  slot becomes NULL before the object moves
    after:  callers use new_owner->lock while the object is still linked
            under old_owner->lock

The block should expose the inconsistent interval directly.

### Before/after implementation layout

Use two concrete layouts when field placement, padding, alignment, or cacheline
count is the result. A structure-layout report can compare the result before
and after reordering:

    Before:
      size:       192 bytes
      cachelines: 3
      holes:      14 bytes

    After:
      size:       128 bytes
      cachelines: 2
      holes:      6 bytes

Full layouts are justified when the offsets themselves explain the saving.

### Scenario or coverage matrix

Use aligned dimension rows to show combinations exercised by a test or
affected by a rule:

    protocol:       ALPHA, BETA
    address family: short, extended
    cache state:    hit, miss, cold, diverse
    flags:          START, RESET
    early exit:     unknown target, invalid frame, fragment

When intersections matter, list only the relevant tuples and their outcomes.
Never draw cell borders:

    ALPHA, short, hit, START -> accepted
    ALPHA, extended, cold, RESET -> rejected
    BETA, extended, miss, START -> deferred

### Invariant snapshot

Use snapshots when a few values define valid and invalid states:

    valid:   in_hash == true,  refcount >= 1
    remove:  in_hash = false,  refcount = 0
    invalid: in_hash == true,  refcount = 0

This is effective for ownership, reference counting, and state-bit bugs.

## Data representation

### Memory or stack-layout list

Use a descending offset list for address order, offsets, and interface
relationships:

    high address
      [frame + 16] incoming stack arg 7
      [frame + 8]  return address
      [frame]      saved frame pointer
      [below frame] VM program stack
      [stack]      outgoing stack arg 7
    low address

### Pointer or data-flow mapping diagram

Use connector lines when source operands map to generated fields or storage:

    parse_descriptor(ctx, 0x7, 0)
                     |    |   +------ index
                     |    +---------- descriptor
                     +--------------- context
                       -> ctx.cache.descriptor_7[0]

### Bit or register map

Use one row per bit or field. For example, compare symbolic constants with a
fictional event-status register:

    bit 0  pushbutton active
    bit 1  hard reset occurred
    bit 2  readiness timeout
    bit 3  undervoltage warning
    bit 4  undervoltage fault
    bit 5  overtemperature warning
    bit 6  overtemperature fault
    bit 7  reserved

### Packet or frame layout

Use a borderless field sequence when an offset depends on protocol variant:

    regular frame: header -> timestamp(64) -> interval -> capability
    compact frame: header -> timestamp(32) -> sequence -> compatibility

Label widths and optional fields. The diagram should explain why a common
offset is valid or invalid.

### Tree, topology, or ownership hierarchy

Use an indented tree for parent-child relationships:

    root reference
      child object A
        derived slice A1
        derived slice A2
      child object B

This is useful for page tables, reference graphs, device hierarchies, and
recursive invalidation.

## Quantitative evidence

### Formula or worked calculation

Show the exact expression when precedence, units, truncation, or overflow is
the defect. For example, contrast these expressions:

    correct: delay = delay * TICKS_PER_SEC / 1000000
    broken:  delay *= TICKS_PER_SEC / 1000000

For an overflow, include the boundary substitution:

    inflight = 1,048,575
    (inflight + 1) * 4096
      = 1,048,576 * 4096
      = 2^32
      = 0 in u32 arithmetic

### Before/after benchmark table

Use measured values with units and enough workload context to reproduce the
comparison:

    Before: 1406 usec per iteration
    After:   402 usec per iteration
    Change:  3.5x faster

Include tradeoffs and avoid conclusions unsupported by the measurement.

### Profile, histogram, or call-cost tree

Use a profile tree to show where time moved rather than listing every sample.
A fictional profiler call tree might look like:

    70.20% release_object_range()
      46.41% release_object_pages()
        36.18% finalize_object_release()
          29.63% unlock_object_queue()

Before/after profiles are most useful when the changed call path is visible.

### Structure-size and cacheline report

Use tool output when layout is the result:

    size:          128
    cachelines:    2
    member bytes:  122
    holes:         6

Keep only the fields needed to support the claimed saving.

## Concrete artifacts

### Verbatim source excerpt for a bug

When source code explains the bug, copy it verbatim. Do not rename identifiers,
simplify expressions, replace helpers with generic operations, change
indentation, or attach invented comments to retained source lines. Name its
location as filename:function immediately before the block.

Two or more consecutive lines of unrelated code may be replaced by a
standalone `[ ... ]` omission marker. Never replace a single source line with
an omission marker; retaining it is clearer. The marker may have an editorial
comment in exactly the form `// omitted: <reason>`:

    if (exact_source_condition) {
        [ ... ] // omitted: unrelated setup block
        exact_source_statement();
    }

The bracketed marker makes the comment visibly editorial rather than quoted
source. Do not use source-language comment syntax such as `/* ... */` for an
omission comment. Preserve the exact spelling, capitalization, punctuation,
indentation, and order of every retained source line. Never use an omission
marker to hide control flow, locking, state changes, cleanup, or an exit
relevant to the bug.

Include the smallest source region that both proves the claim and locates it
within the function. Preserve a nearby case label, condition, loop, or goto
target when it identifies where the failing branch occurs:

    queue.c:dispatch_item

        SPIN_LOCK(QUEUE_LOCK, &item->queue->lock);
        if (!item->node.leaf_p) {
                item_ref_put(owner);
                return ITEM_QUEUED;
        }
        [ ... ] // omitted: unrelated success-path accounting block
        tree_delete(&item->node);
        SPIN_UNLOCK(QUEUE_LOCK, &item->queue->lock);

Do not paste an entire function when a smaller excerpt establishes both the
location and the defect. Put interpretation outside the excerpt.

### Pseudocode excerpt for a solution

Pseudocode is allowed only when explaining a proposed or implemented solution.
Label it `pseudocode` and use it to show the intended algorithm without
pretending it is source:

    pseudocode:
      acquire queue lock
      revalidate that the item is linked
      on every exit, release the queue lock

Never use pseudocode as evidence of an existing bug. A problem description
that shows code must use a verbatim source excerpt instead.

### Diagnostic excerpt

Use the identifying diagnostic and path:

    BUG: sanitizer: use-after-free in flush_widget_cache()
    flush_widget_cache()
      replace_widget()
        update_widget()

Sanitizer, dependency-checker, warning, and hardware-error output all fit this
category.

### Trace or log comparison

Use traces when runtime ordering or level changes are the evidence:

    top-down:
      level=3 flood=0
      level=2 flood=0
      level=2 flood=1
      level=2 flood=2

    bottom-up:
      level=2 flood=0
      level=2 flood=1
      level=2 flood=2
      ... repeated for each orphaned child

Contrasting traces can explain hierarchical-object reclaim. Trim repetitive
entries once the pattern is established.

### Command and expected output

Use a command block for an operator-visible reproducer or interface:

    $ tool --show-state object0
    state: stalled
    owner: none

Include prerequisites outside the block and keep the command itself directly
runnable where possible.

### Configuration or reproducer fragment

Use a minimal configuration when the bug depends on a specific mode:

    service sample
        policy least-loaded
        endpoint node1 address=192.0.2.1 limit=1

Remove unrelated settings. State which operation triggers the failure.

### Protocol or API example

Show the smallest input/output pair that distinguishes correct behavior:

    input:    frame_type=COMPACT, timestamp_width=32
    observed: timestamp read at regular-frame offset
    expected: compact timestamp combined with compatibility field

This form is useful when prose would repeatedly describe the same field
mapping.

## Selection guidance

Use a timeline for interleaving, a call graph for control flow, a state block
for invariants, an ordered layout list for representation, and a calculation
for numeric failures. Use raw artifacts only after trimming them to the lines
that prove the claim.

Several forms can be combined when they carry different information. A race
timeline may establish ordering while a short source excerpt identifies the
missing check. Do not add a diagram and then repeat it in prose.
