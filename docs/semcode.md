# semcode MCP integration

The main agent's code-navigation and searching can be enhanced
by semcode:

https://github.com/facebookexperimental/semcode

When a `semcode-mcp` binary is installed, `setup.sh` writes an
`mcp.json` that launches it as an MCP child:

```json
{
  "mcpServers": {
    "semcode": { "command": "semcode-mcp" }
  }
}
```

(`configs/mcp.json`).

kres works without semcode — the main agent can already answer
code questions with `read`, `grep`, and `git` against the
workspace (`CLAUDE.md:9,16`). When semcode is available, the
main agent gets a function/type/callchain-aware index to ask
instead of deriving the same information from raw regex.

Tools semcode exposes that the main agent will call when wired up:

- Function- and type-level lookups: `find_function`, `find_type`,
  `find_callers`, `find_calls`, `find_callchain`, `grep_functions`.
- Commit- and branch-level helpers: `find_commit`,
  `compare_branches`, `diff_functions`, `list_branches`.
- Vector-indexed search: `vgrep_functions`,
  `vcommit_similar_commits`, `vlore_similar_emails`, `lore_search`.

Raw semcode symbol text is normalised back into a uniform JSON
shape by `parse_semcode_symbol` (`kres-agents/src/symbol.rs:52-59`)
before reaching the fast/slow agents.

## When it helps

Whole-program questions that read/grep can only approximate —
"who calls `btrfs_search_slot`", "what does the definition of
`struct inode` look like on this branch", "show me every change
to this function in the last 1000 commits". Without semcode the
main agent still answers those, just via more grep round-trips
with more false positives.

## Install

Either drop `semcode-mcp` on your `PATH` before running
`setup.sh` (it auto-installs `mcp.json`, `setup.sh:265-269`) or
pass `--semcode PATH/TO/semcode-mcp` explicitly
(`setup.sh:41-45`). `--semcode ""` force-skips the MCP install
even when the binary is on `PATH`. kres's `.gitignore` excludes
a `/.semcode.db/` directory at the repo root (`.gitignore:4`) —
that's semcode's on-disk index cache; consult the semcode repo
for details on how it's populated and invalidated.
