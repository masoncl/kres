# Action allowlist

The main agent's non-MCP tools are gated by a session-wide
allowlist. Defaults: `grep`, `find`, `read`, `git`, `edit`.
`bash` is OFF by default — operators reach for it as a generic
escape hatch for things the typed tools already cover
(`bash sed` for range reads, `bash find` for filename locates).
A disallowed action is rejected at dispatch time with a message
naming the allowed set.

## Precedence

1. `--allow ACTION` CLI flags — additive on top of file config.
   Repeatable (`--allow bash --allow git`) or comma-separated
   (`--allow bash,git`). `--allow all` enables every known
   action.
2. `<cwd>/.kres/settings.json` — per-project overrides; an
   explicit allowlist replaces rather than unions with the
   global one.
3. `~/.kres/settings.json` — per-user default.

## Configuring via settings.json

Enable bash permanently:

```json
{ "actions": { "allowed": ["grep", "find", "read", "git", "edit", "bash"] } }
```

Lock down to MCP-only:

```json
{ "actions": { "allowed": [] } }
```

The empty array is the explicit lock-it-down signal; a missing
or `null` `actions.allowed` falls back to the built-in default.

## Behaviour

- Typo detection: unknown tokens in `--allow` or
  `actions.allowed` print a Levenshtein-≤2 suggestion
  (`… 'bsah' — did you mean 'bash'?`) and are dropped, not
  silently inserted.
- Startup banner: kres prints the effective allowlist and
  distinguishes "bash disabled by default" from "disabled by
  explicit allowlist".
- MCP tools are gated separately (by `mcp.json` registration).
  `--allow mcp` is a no-op.
