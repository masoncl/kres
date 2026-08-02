# Agents — flow of work per task

Every task cycles through these roles. Normal installs configure them
through provider files under `~/.kres/models/`; explicit `--*-agent` paths
remain available for one-off overrides.

- **fast** — scopes the task and emits a validated list of typed
  `followups`: grep / read / semcode / git fetches the deterministic
  tool service should run.
- **tool service** (not an LLM role) — dispatches validated followups to
  local tools and MCP servers (semcode via `mcp.json`). Output is fed back into
  fast for another round.
  The fast↔tool loop ends when fast emits `ready_for_slow` or
  `--gather-turns` is hit.
- **main** — authors the initial goal/mode/plan and checks completion for
  generic sessions. It is not interposed between fast followups and tools.
- **slow** (selected by `settings.models.slow` or CLI override) — the
  deep analyser. Gets the gathered symbols, the cumulative
  findings, and the task brief; returns analysis prose plus
  structured findings. Optional `settings.models.slow_secondary` adds a second
  model for the `general` review lens or fix workflow's `maintainer` lens.
  Explicit `--slow` selection replaces the configured pair; `--compare`
  expands every selected model across every active lens.
- **todo** — for generic sessions, dedups the slow agent's
  followups against the current todo list, reprioritises, and
  may reshape the plan.
- **consolidator/promoter** — fast-client calls that merge sibling lens
  outputs and recover concrete prose-only bugs. Rust then applies the findings
  delta deterministically; invalidated records remain as negative evidence.

Every round-trip is logged to `.kres/logs/<session-uuid>/`.

## Building up a larger review

A named-file `--prompt 'review: path/to/file.c'` first runs one file survey
and one non-lensed slow ranking call. The primary slow model then authors a
bounded semantic plan whose linked todos enter the normal task loop. Review
followups are reconciled by dedicated goal/todo clients backed by that primary
slow model; generic sessions continue to use the configured main/todo roles.
To work through pending items interactively:

- `/next` runs the first pending item.
- `/continue` dispatches every unblocked pending item.
- auto-continue fires `/continue` after 5s idle when there are
  pending todos and no active tasks. Typing (including `/stop`)
  cancels the idle.

The goal agent checks after every task whether the original prompt is
satisfied. Generic sessions use the main model; reviews use the primary slow
model. Concrete review followups prevent an early goal-met result from
discarding required work.

A thorough review of a real source file runs 5–50 tasks
depending on branchiness and how aggressive the slow agent is
about follow-up questions. `--turns` bounds it
(see [turns-and-follow.md](turns-and-follow.md)); `/quit` bails
out and `--resume` picks up later.
