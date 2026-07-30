# CLI and REPL commands

## CLI

```
kres test <model_config.json> [--prompt ...] [--model ...]
kres turn <model_config.json> -o <output.md> [-i <input.json>] [other flags]
kres [--fast-agent ...] [--slow NAME ... | --slow-agent ...] [--main-agent ...]
     [--todo-agent ...] [--mcp-config ...] [--skills DIR]
     [--results DIR] [--findings PATH] [--report PATH] [--todo PATH]
     [--prompt PROMPT] [--template PATH] [--turns N]
     [--follow] [--resume]
     [--gather-turns N] [--stop-grace-ms MS] [--stdio]
     [--allow ACTION]... [--assisted-by TEXT]
     [--summary | --summary-markdown]
```

Pass `kres --help` for the full list with argument-by-argument
descriptions.

`--assisted-by TEXT` overrides the exact value used after
`Assisted-by:` by the fix workflow's generated commit message. When
omitted, kres derives `kres:<slow-model-id>` from the resolved slow
agent model.

`--slow NAME` selects a slow model. `sonnet` and `opus` first resolve through
`settings.json:model_aliases`, then fall back to the shipped model ids. Other
values are model ids; when multiple provider
files offer one model, qualify it as `provider.json:model-id`. It is repeatable
for `/review` comparison mode, for example `--slow sonnet --slow opus`.
Review sends every slow-agent lens prompt to all configured slow models,
tags their outputs by model, and writes the consolidator's per-turn
comparison to `<results>/comparison.json`.

Examples:

| Provider files present | CLI | Result |
|------------------------|-----|--------|
| `anthropic.json` offers Sonnet | `--slow sonnet` | Selects the Sonnet alias. |
| `azure.json` offers `gpt-5.5` | `--slow gpt-5.5` | Selects the unique provider. |
| `foo.json` and `bar.json` offer Opus | `--slow claude-opus-4-6` | Fails as ambiguous. |
| `foo.json` and `bar.json` offer Opus | `--slow foo.json:claude-opus-4-6` | Selects `foo.json`. |

Related docs:

- [turns-and-follow.md](turns-and-follow.md) — `--turns N`,
  `--turns 0`, `--follow`, stagnation cap.
- [action-allowlist.md](action-allowlist.md) — `--allow ACTION`
  and the dispatcher's non-MCP allowlist.
- [summary.md](summary.md) — `--summary`,
  `--summary-markdown`, `--template`.
- [configuration.md](configuration.md) — model-id overrides
  (`--fast-model`, `--slow-model`, `--main-model`,
  `--todo-model`, `--classifier-model`), configurable model aliases, and
  workflow-executor limits.

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
| `/summary [FILE]`              | Render `findings.json` (including stored task prose) to a plain-text summary (default `summary.txt`) |
| `/summary-markdown [FILE]`     | Same as `/summary`, markdown output (default `summary.md`) |
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
