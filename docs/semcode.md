# semcode MCP integration

The main agent's code navigation is enhanced by semcode
(<https://github.com/facebookexperimental/semcode>). When a
`semcode-mcp` binary is on `PATH`, `setup.sh` writes an
`mcp.json` that launches it as an MCP child:

```json
{
  "mcpServers": {
    "semcode": { "command": "semcode-mcp" }
  }
}
```

kres works without semcode — the main agent already answers
code questions with `read`, `grep`, and `git`. semcode adds a
function/type/callchain-aware index so the agent can ask
whole-program questions directly instead of deriving them from
raw regex.

Tools the main agent will call when wired up:

- Symbols: `find_function`, `find_type`, `find_callers`,
  `find_calls`, `find_callchain`, `grep_functions`.
- Commits / branches: `find_commit`, `compare_branches`,
  `diff_functions`, `list_branches`.
- Vector search: `vgrep_functions`, `vcommit_similar_commits`,
  `vlore_similar_emails`, `lore_search`.

Raw semcode symbol text is normalised into a uniform JSON shape
by `parse_semcode_symbol` (`kres-agents/src/symbol.rs`) before
reaching the fast/slow agents.

## When it helps

Whole-program questions that read/grep can only approximate —
"who calls `btrfs_search_slot`", "what does `struct inode` look
like on this branch", "show me every change to this function
over the last 1000 commits". Without semcode the main agent
still answers, just via more grep round-trips and more false
positives.

## Install

Either drop `semcode-mcp` on your `PATH` before running
`setup.sh` (auto-install kicks in), or pass
`--semcode PATH/TO/semcode-mcp` explicitly. `--semcode ""`
force-skips the MCP install even when the binary is on `PATH`.

kres's `.gitignore` excludes `/.semcode.db/` at the repo root —
semcode's on-disk index cache; consult the semcode repo for how
it's populated and invalidated.
