# CLI and REPL commands

## CLI

```
kres test <key_file> [--prompt ...] [--model ...]
kres turn <key_file> -o <output.md> [-i <input.json>] [other flags]
kres [--fast-agent ...] [--slow TAG | --slow-agent ...] [--main-agent ...]
     [--todo-agent ...] [--mcp-config ...] [--skills DIR]
     [--results DIR] [--findings PATH] [--report PATH] [--todo PATH]
     [--prompt PROMPT] [--template PATH] [--turns N]
     [--follow] [--resume]
     [--gather-turns N] [--stop-grace-ms MS] [--stdio]
     [--allow ACTION]... [--summary] [--markdown]
```

Pass `kres --help` for the full list with argument-by-argument
descriptions.

Related docs:

- [turns-and-follow.md](turns-and-follow.md) — `--turns N`,
  `--turns 0`, `--follow`, stagnation cap.
- [action-allowlist.md](action-allowlist.md) — `--allow ACTION`
  and the dispatcher's non-MCP allowlist.
- [summary.md](summary.md) — `--summary`, `--template`,
  `--markdown`.
- [configuration.md](configuration.md) — model-id overrides
  (`--fast-model`, `--slow-model`, `--main-model`,
  `--todo-model`).

## REPL commands

| Command                        | Action |
|--------------------------------|--------|
| `/help`, `/?`                  | Command list |
| `/tasks`, `/task`              | Show active tasks and states |
| `/findings`                    | Summarise current findings list |
| `/stop`                        | Cancel running tasks (auto-continue pauses) |
| `/clear`                       | Cancel tasks, reset findings + todo + accumulated context |
| `/compact`                     | Replace accumulated context with short fast-agent summary |
| `/cost`                        | Print API token usage |
| `/todo` / `/todo --clear`      | Show or clear the todo list |
| `/plan`                        | Show the current plan + per-step status |
| `/resume [PATH]`               | Load a persisted `session.json` |
| `/followup`                    | List items deferred by goal-met or `--turns` cap |
| `/summary [FILE]`              | Render `report.md` + `findings.json` to a plain-text bug report |
| `/summary-markdown [FILE]`     | Same as `/summary`, markdown output |
| `/review <target>`             | Compose the review template + target, submit |
| `/extract …`                   | Copy artifacts out (`--dir`, `--report`, `--todo`, `--findings`) |
| `/done N`                      | Remove the N'th pending todo |
| `/report <path>`               | Write findings to markdown |
| `/load <path>`                 | Submit a file's contents as a prompt |
| `/edit`                        | Open `$EDITOR`, submit on save (also ctrl-g) |
| `/reply <text>`                | Prepend last analysis to new text, submit |
| `/next`                        | Dispatch the next pending todo |
| `/continue`                    | Dispatch every unblocked pending todo |
| `/quit`, `/exit`               | Leave the REPL |
