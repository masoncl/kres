# Configuration — `~/.kres/` layout, models, and system prompts

## Config directory: `~/.kres/`

`kres repl` resolves every optional config path in this order:

1. explicit CLI flag (e.g. `--fast-agent /path/to/fast.json`)
2. same filename under `~/.kres/`

Default filenames looked up in `~/.kres/`:

| Flag              | Default under `~/.kres/`         |
|-------------------|----------------------------------|
| `--fast-agent`    | `fast-code-agent.json`           |
| `--slow` tag      | `slow-code-agent-<tag>.json`     |
| `--main-agent`    | `main-agent.json`                |
| `--todo-agent`    | `todo-agent.json`                |
| `--mcp-config`    | `mcp.json`                       |
| `--skills`        | `skills/`                        |
| `--findings`      | `findings.json`                  |

A missing file in `~/.kres/` is not an error — the "not
configured" branch fires as if the flag were absent. The
`history` file is always written to `~/.kres/history`.

## Model selection

`~/.kres/settings.json` carries per-user default model ids per
agent role. `setup.sh --slow MODEL` / `--model MODEL` populate
the slow slot and the fast / main / todo slots respectively;
defaults are `claude-opus-4-7` (slow) and `claude-sonnet-4-6`
(the rest).

Runtime precedence (`kres-repl/src/settings.rs::pick_model`):

1. The agent config's explicit `"model"` field.
2. `settings.models.<role>` in `~/.kres/settings.json`.
3. `Model::sonnet_4_6()` — built-in fallback.

Shipped agent configs no longer set `"model"`, so step 2 drives
a fresh install. Per-run CLI overrides (`--fast-model`,
`--slow-model`, `--main-model`, `--todo-model`) beat
`settings.json`. A known `--slow <tag>` (sonnet/opus) implies a
slow model id unless `--slow-model` is also passed.

Pointing fast and slow at the same model is fine: the fast/slow
distinction is driven by per-agent system prompts and the
context each agent receives, not by model choice. Two different
models is a cost/latency optimisation, not a correctness
requirement.

## System prompts

Agent `*.system.md` prompts (fast / slow / slow-coding /
slow-generic / main / todo) are compiled into the kres binary
(`kres-agents/src/embedded_prompts.rs`). `setup.sh` does NOT
install them on disk — rebuilding kres refreshes them.

Shipped configs reference `system_file:
"system-prompts/<name>.system.md"` resolved relative to the
config file's directory, i.e. `~/.kres/system-prompts/<name>`.

`AgentConfig::load` order:

1. **Disk override**: `~/.kres/system-prompts/<basename>` if it
   exists and is non-empty — used verbatim.
2. **Embedded**: compiled-in copy keyed by basename.
3. **Error**: neither present → load fails naming both paths.

To customise, drop the edited file at
`~/.kres/system-prompts/<basename>`. A default install has no
files there; the embedded copies do all the work.

Slash-command templates (`/review`, `/summary`,
`/summary-markdown`) live in a separate module
(`kres-agents/src/user_commands.rs`) with their own override
directory at `~/.kres/commands/` — see
[commands.md](commands.md). The two directories are distinct so
that leftover files from older installs under
`~/.kres/prompts/` never shadow the embedded defaults; stale
files there are safe to delete.
