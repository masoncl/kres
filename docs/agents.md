# Agents — flow of work per task

Every task cycles through these roles, all configured under
`~/.kres/`:

- **fast** (`fast-code-agent.json`) — scopes the task and emits a
  list of `followups`: grep / read / semcode / git fetches the
  main agent should run.
- **main** (`main-agent.json`) — the data fetcher. Dispatches
  followups to local tools and MCP servers (semcode via
  `mcp.json`). Output is fed back into fast for another round.
  The fast↔main loop ends when fast emits `ready_for_slow` or
  `--gather-turns` is hit.
- **slow** (`slow-code-agent-<tag>.json`, default `sonnet`) — the
  deep analyser. Gets the gathered symbols, the cumulative
  findings, and the task brief; returns analysis prose plus
  structured findings.
- **todo** (`todo-agent.json`) — dedups the slow agent's
  followups against the current todo list, reprioritises, and
  may reshape the plan.
- **merger** — non-agent fast-client call that folds new
  findings into the cumulative list; supersedes become
  `invalidated`.

Every round-trip is logged to `.kres/logs/<session-uuid>/`.

## Building up a larger review

One `--prompt 'review: fs/btrfs/ctree.c'` seeds one task. Its
slow-agent response usually emits followup suggestions the todo
agent converts into todo items. To work through them:

- `/next` runs the first pending item.
- `/continue` dispatches every unblocked pending item.
- auto-continue fires `/continue` after 5s idle when there are
  pending todos and no active tasks. Typing (including `/stop`)
  cancels the idle.

The goal agent (a judge-mode call on the main-agent client)
checks after every task whether the original prompt is
satisfied; goal-met stops work even with pending followups.

A thorough review of a real source file runs 5–50 tasks
depending on branchiness and how aggressive the slow agent is
about follow-up questions. `--turns` bounds it
(see [turns-and-follow.md](turns-and-follow.md)); `/quit` bails
out and `--resume` picks up later.
