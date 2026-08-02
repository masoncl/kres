# CLI and REPL commands

## CLI

```text
kres [OPTIONS] [COMMAND]
```

The subcommands are `test`, `turn`, `validate-workflow`, and `run-workflow`.
Without a subcommand, kres starts the analysis REPL or performs the terminal
operation selected by flags such as `--summary`, `--summary-markdown`,
`--export`, or `--export-index`.

Pass `kres --help` for the full list with argument-by-argument
descriptions.

`--assisted-by TEXT` overrides the exact value used after
`Assisted-by:` by the fix workflow's generated commit message. When
omitted, kres derives `kres:<slow-model-id>` from the resolved slow
agent model.

`--slow NAME` selects a slow model. `sonnet` and `opus` first resolve through
`settings.json:model_aliases`, then fall back to the shipped model ids. Other
values are model ids; when multiple provider
files offer one model, qualify it as `provider.json:model-id`.

Model selection has three review modes:

- One slow model: the selected model runs every active lens.
- Two slow models without `--compare`: the first model runs every active lens;
  the second adds only the supplemental lens (`general` for `/review`,
  `maintainer` for `/fix`). Configure this persistently with
  `models.slow_secondary`, or per run with repeated/comma-separated `--slow`.
- Multiple slow models with `--compare`: every model runs every active lens.
  Outputs are tagged by model and the per-turn comparison is written to
  `<results>/comparison.json`.

For example, both `--slow opus,gpt` and `--slow opus --slow gpt` select Opus as
the primary and GPT as the supplemental model. Adding `--compare` performs the
full cross-model lens comparison instead.

Any explicit `--slow` value replaces both `models.slow` and
`models.slow_secondary` for that run. `--slow-model` remains a single-primary
override, preserves a configured secondary model, and is mutually exclusive
with `--slow`.

Examples:

| Provider files present | CLI | Result |
|------------------------|-----|--------|
| `anthropic.json` offers Sonnet | `--slow sonnet` | Selects the Sonnet alias. |
| `azure.json` offers `gpt-5.5` | `--slow gpt-5.5` | Selects the unique provider. |
| `foo.json` and `bar.json` offer Opus | `--slow claude-opus-4-8` | Fails as ambiguous. |
| `foo.json` and `bar.json` offer Opus | `--slow foo.json:claude-opus-4-8` | Selects `foo.json`. |
| Opus primary, GPT supplemental | `--slow opus,gpt` | Runs all lenses with Opus and one supplemental lens with GPT. |
| Compare Opus and GPT | `--slow opus,gpt --compare` | Runs every active lens with both models. |

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
| `/review <target>`             | Run the JSON-defined review workflow through the task/todo loop |
| `/triage <finding-dir>`        | Run the JSON-defined finding triage workflow |
| `/validate <finding-dir> [source-workspace]` | Validate an exported finding against source |
| `/fix <target>`                | Run the JSON-defined fix workflow |
| `/extract …`                   | Copy artifacts out (`--dir`, `--report`, `--todo`, `--findings`) |
| `/done N`                      | Remove the N'th pending todo |
| `/report <path>`               | Write findings to markdown |
| `/load <path>`                 | Submit a file's contents as a prompt |
| `/edit`                        | Open `$EDITOR`, submit on save (also ctrl-g) |
| `/reply <text>`                | Prepend last analysis to new text, submit |
| `/next`                        | Dispatch the next pending todo |
| `/continue`                    | Dispatch every unblocked pending todo |
| `/quit`, `/exit`               | Leave the REPL |
