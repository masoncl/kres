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

A missing file in `~/.kres/` is not an error — the "not configured"
branch fires as if the flag were absent.

The `history` file is always written to `~/.kres/history` regardless
of other flags; it holds readline line-edit history.

## Model selection

`~/.kres/settings.json` carries per-user default model ids per
agent role. `setup.sh --slow MODEL` / `--model MODEL` populate
the slow slot and the fast / main / todo slots respectively;
default values are `claude-opus-4-7` (slow) and
`claude-sonnet-4-6` (the rest).

Model-id precedence at runtime (see
`kres-repl/src/settings.rs::pick_model`):

1. The agent config's explicit `"model"` field when present.
2. The matching `settings.models.<role>` string in
   `~/.kres/settings.json`.
3. `Model::sonnet_4_6()` — the built-in fallback when both of
   the above are absent.

The shipped agent configs no longer set `"model"`, so in a
fresh install step 2 drives the actual choice. Reintroducing a
`"model"` line in one of the agent configs still takes effect
and overrides settings.json for that agent only.

CLI overrides for a single run: `--fast-model`, `--slow-model`,
`--main-model`, `--todo-model` all beat `settings.json`. A
known `--slow <tag>` (sonnet/opus) implies a slow model id too,
unless `--slow-model` is also passed.

Running `--slow` and `--model` against the same model id is
fine and often what you want if you only have one model's
credentials. The difference between "fast" and "slow" work is
driven by the per-agent system prompts shipped under
`configs/prompts/` and the amount of context each agent
receives, not by the model choice — so pointing both at the
same id still produces the full fast/main/slow pipeline, each
agent thinking as hard or as lightly as its prompt asks. Using
two different models is an optimisation for cost or latency,
not a correctness requirement.

## System prompts

Agent `*.system.md` prompts (fast / slow / slow-coding /
slow-generic / main / todo) are compiled into the kres binary
via `include_str!` (see `kres-agents/src/embedded_prompts.rs`).
`setup.sh` does NOT install them on disk. Rebuilding kres
refreshes them.

The shipped agent configs under `configs/*.json` reference
`system_file: "system-prompts/<name>.system.md"`; the path is
resolved relative to the config file's directory, so at runtime
it becomes `~/.kres/system-prompts/<name>.system.md`.

Load order used by `AgentConfig::load`:

1. **Disk override**: `~/.kres/system-prompts/<basename>`. If
   this file exists and is non-empty it is used verbatim.
2. **Embedded**: the compiled-in copy keyed by basename.
3. **Error**: neither present → config load fails with a
   message that names both paths.

To customise an agent prompt for your own install, drop the
edited file at `~/.kres/system-prompts/<basename>`. The default
install has no files there; the embedded copies do all the work.

Slash-command templates (`/review`, `/summary`,
`/summary-markdown`) live in a separate module
(`kres-agents/src/user_commands.rs`) with their own override
directory at `~/.kres/commands/`. See
[commands.md](commands.md).

### Why distinct override directories?

Older installs populated `~/.kres/prompts/` directly from
setup.sh (both `*.system.md` and `bug-summary*.md`). Keeping
the override in the same directory would mean those leftover
files shadow the embedded defaults and produce stale behaviour
after an upgrade. Two fresh directory names
(`~/.kres/system-prompts/` and `~/.kres/commands/`) sidestep
that — a fresh kres reads only the embedded defaults until the
operator deliberately drops a file under the new paths. Stale
files under `~/.kres/prompts/` are safe to delete (the
slash-command loader still reads `<word>-template.md` from
there as a back-compat fallback, but will never find a
filename matching one of the shipped embedded commands there
since setup.sh never writes those names to `prompts/`).
