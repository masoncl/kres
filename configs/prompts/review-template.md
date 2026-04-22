The block above this paragraph is the review target — a file path,
function name, commit ref (e.g. `HEAD`), diff, or code snippet
supplied by the operator. We're doing a deep security and bug
analysis of that target.

Focus on just the target itself and the supporting code it calls,
without expanding out into the rest of the kernel. Pay special
attention to chains of events that trigger obscure bugs.

- [ ] **[investigate]** object lifetime: #lifetime
  - where are pointers to objects stored
  - what flags control object behavior
  - what flags control object lifetime
  - how are pointers copied around
  - how do we ensure each copy of the pointer is properly handled
  - how are objects shared between processes
- [ ] **[investigate]** memory allocations: #memory
  - Are we leaking, use after free, double free, corrupting memory
  - are we using the APIs correctly
- [ ] **[investigate]** bounds checks: do any array accesses or memory access overflow.  Are they using trusted indexes? #bounds
- [ ] **[investigate]** races: #races
  - identify concurrent sections, synchronization primitives and objects being protected.
  - Find races
  - identify locking errors
- [ ] **[investigate]** general: what general bugs can you find? #general
