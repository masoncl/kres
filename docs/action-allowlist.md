# Action allowlist

The main agent's non-MCP tools are gated by a session-wide
allowlist. Defaults: `grep`, `find`, `read`, `git`, `edit`.
`bash` is **OFF by default** because operators report it being
reached for as a general escape hatch for things the typed tools
already cover (`bash sed` for range reads, `bash find` for
filename locates). An action whose `type` isn't in the allowlist
is rejected at dispatch time with a message naming the allowed
set and pointing at the two ways to fix it.

## Three precedence levels

1. `--allow ACTION` CLI flags — additive on top of whatever the
   files resolved to. Repeatable (`--allow bash --allow git`) or
   comma-separated (`--allow bash,git`). The special value
   `--allow all` enables every action type the dispatcher knows.
2. Per-project `<cwd>/.kres/settings.json` — overrides global
   values field-by-field; an explicit allowlist replaces rather
   than unions with the global one.
3. Global `~/.kres/settings.json` — the default resting place
   for a per-user policy.

## Examples

**Enable bash for this session only:**

```
kres --allow bash --prompt 'reproduce the RDS UAF'
```

**Enable bash permanently in settings.json:**

```json
{
  "actions": {
    "allowed": ["grep", "find", "read", "git", "edit", "bash"]
  }
}
```

**Deny every non-MCP action (tight lockdown, leaves only MCP
tools available to the main agent):**

```json
{
  "actions": {
    "allowed": []
  }
}
```

The empty array is the explicit "lock it down" signal — kres
dispatcher enforces it and does not fall back to defaults.
A missing or absent `actions.allowed` (i.e. `null` or the key
unset) is different: it means "use the built-in default list".

## Typo detection

Tokens in `--allow` or `actions.allowed` that aren't recognised
action names produce a startup warning with a closest-match
suggestion (Levenshtein ≤ 2), e.g. `settings: unknown action
token 'bsah' (--allow) — did you mean 'bash'? known: grep,
find, read, git, edit, bash, mcp`. Unknown tokens are dropped
rather than silently inserted, so a typo never leaves a dead
entry in the allowlist.

## Startup banner

When a main-agent config is resolved, kres prints the effective
allowlist on startup and distinguishes "bash disabled by default"
from "bash disabled by explicit allowlist in settings.json".
Both point at `--allow bash` as the fix but the wording respects
the source of the decision.

MCP tools are gated separately (by mcp.json server registration,
not this allowlist) and don't enter the allowlist's dispatch
path. `--allow mcp` is a no-op and does not produce a typo
warning.
