# Agents — flow of work between the fast, main, slow, and todo agents

A task goes through three agents, all configured from `~/.kres/`:

- **fast** (`fast-code-agent.json`): scopes the task, figures out
  what source kres needs to look at, and emits a structured brief.
  When it's ready it returns a list of "followups" — concrete fetch
  requests (grep, file read, semcode symbol/callchain, git log)
  that the main agent should run.

- **main** (`main-agent.json`): the data fetcher. It takes the
  fast agent's followups and dispatches them to local tools and to
  any MCP servers configured in `mcp.json` (semcode in particular).
  The output is funnelled back into the fast agent for another
  round. This fast↔main loop runs until the fast agent says
  `ready_for_slow`, or the `--gather-turns` cap is reached.

- **slow** (`slow-code-agent-<tag>.json`, default `sonnet`): the
  deep analyser. It receives the gathered symbols and file sections,
  the cumulative findings from earlier tasks, and the task brief,
  then produces a new analysis and any new findings. Slow-agent
  output is cheap prose plus structured findings records.

- **todo** (`todo-agent.json`): after the slow agent returns, the
  todo agent dedups its followup suggestions against the existing
  pending/done todo list and emits an updated list. This is what
  drives larger reviews — see below.

- **merger**: a non-agent fast-client call that merges the new
  task's findings into the cumulative findings list. Duplicates get
  folded; old findings that a new one supersedes get marked
  `invalidated`.

All inference happens over the Anthropic streaming API. Every
round-trip is logged to `.kres/logs/<session-uuid>/` so you can
inspect what each agent saw and replied.

## Building up a larger review

A single `--prompt 'review: fs/btrfs/ctree.c'` call seeds exactly
one task. That task's slow-agent response usually contains followup
suggestions like "investigate memory lifetime of the path argument"
or "check callers of btrfs_search_slot". The todo agent turns those
into todo items.

From there:

- **`/next`** runs the first pending todo item as its own task.
- **`/continue`** dispatches every pending todo item.
- **auto-continue**: when there are pending todos and no active
  tasks, kres launches `/continue` automatically after 5 seconds of
  idle. You can override the idle by typing anything, including
  `/stop`.

Each task feeds back into the same pipeline: fast → main → slow →
merger, plus the todo agent deduping any new followups against the
existing list. The goal agent (a special mode of the main-agent
model) periodically checks whether the original prompt has been
satisfied; if yes, work stops even if followups remain.

A full review of a substantial source file usually takes between 5
and 50 task runs, depending on how branchy the code is and how
aggressive the slow agent is about producing followup questions.
`--turns` caps that (see
[docs/turns-and-follow.md](turns-and-follow.md)); `/quit` lets you
bail out early and resume later with `--resume`.
