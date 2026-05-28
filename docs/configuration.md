# Configuration — `~/.kres/` layout, models, and system prompts

## Config directory: `~/.kres/`

`kres repl` resolves agent config paths in this order:

1. explicit CLI flag (e.g. `--fast-agent /path/to/fast.json`)
2. model file under `~/.kres/models/<resolved-model-id>.json`

Non-agent paths such as `mcp.json` and `skills/` only use explicit
CLI flags and the same filename under `~/.kres/`.

Default non-agent filenames looked up in `~/.kres/`:

| Flag              | Default under `~/.kres/`         |
|-------------------|----------------------------------|
| `--mcp-config`    | `mcp.json`                       |
| `--skills`        | `skills/`                        |
| `--findings`      | `findings.json`                  |

A missing model file in `~/.kres/models/` is not an error by
itself, but any role whose model file cannot be resolved is treated
as not configured unless the matching explicit `--*-agent` flag was
provided. The `history` file is always written to `~/.kres/history`.

## Model selection

`~/.kres/settings.json` carries per-user default model ids per
agent role. `setup.sh --slow MODEL` / `--model MODEL` populate
the slow slot and the fast / main / todo slots respectively. The
classifier slot is shipped as `claude-haiku-4-5`. Defaults are
`claude-opus-4-7` (slow), `claude-haiku-4-5` (classifier), and
`claude-sonnet-4-6` (the rest).

Runtime precedence (`kres-repl/src/settings.rs::pick_model`):

1. The agent config's explicit `"model"` field.
2. `settings.models.<role>` in `~/.kres/settings.json`.
3. `Model::sonnet_4_6()` — built-in fallback.

Model files set `"model"`, so settings selects which model file each
role loads. Per-run REPL overrides (`--fast-model`, `--slow-model`,
`--main-model`, `--todo-model`, `--classifier-model`) beat
`settings.json`. The one-shot workflow executor accepts `--fast-model`,
`--slow-model`, and `--classifier-model`; it has no main/todo agent
roles. `--slow <name>` selects a
slow model config: `sonnet` and `opus` are aliases for the shipped
model ids, while any other value must match exactly one JSON file under
`~/.kres/models/` by filename. Exact stem matches win over substring
matches. `--slow` and `--slow-model` are mutually exclusive.

For example, with these files:

```text
~/.kres/models/claude-sonnet-4-6.json
~/.kres/models/gpt-5.5-high.json
~/.kres/models/gpt-5.5-xhigh.json
~/.kres/models/local-coder.json
```

`--slow sonnet` selects the Sonnet alias, `--slow gpt-5.5-xhigh`
selects the exact filename stem, `--slow local` selects
`local-coder.json` if it is the only filename containing `local`, and
`--slow gpt-5.5` fails because both GPT files match.

Pointing fast and slow at the same model is fine: the fast/slow
distinction is driven by per-agent system prompts and the
context each agent receives, not by model choice. Two different
models is a cost/latency optimisation, not a correctness
requirement.

GPT model ids such as `gpt-5.5` use the OpenAI adapter in `kres-llm`.
The normal layout is one file per model:

```text
~/.kres/models/gpt-5.5.json
```

That file carries model credentials plus default request parameters.
Role sections are optional; add them only when a role needs different
tuning. The top-level `api_key` is used for every role that loads that
model file:

```json
{
  "api_key": "...",
  "model": "claude-sonnet-4-6",
  "defaults": {
    "rate_limit": 800000
  },
  "fast": {
    "max_tokens": 64000,
    "max_input_tokens": 800000
  },
  "main": {
    "max_tokens": 16384
  },
  "todo": {
    "max_tokens": 32000
  },
  "slow": {
    "max_tokens": 64000,
    "max_input_tokens": 900000
  }
}
```

For a role-specific load, kres merges model-file fields in this order:

1. Top-level fields.
2. `defaults`.
3. The selected role section: `fast`, `slow`, `main`, or `todo`.

Later entries replace earlier entries for the same key. Config files
are strict: unknown fields are rejected instead of ignored.

For OpenAI API access, set `provider: "openai"`. `base_url` is
optional and defaults to `https://api.openai.com/v1`; set it only for a
compatible proxy:

```json
{
  "provider": "openai",
  "api_key": "...",
  "model": "gpt-5.5",
  "defaults": {
    "max_tokens": 128000,
    "max_input_tokens": 900000,
    "rate_limit": 900000,
    "thinking": {"type": "adaptive", "effort": "medium"}
  },
  "slow": {
    "thinking": {"type": "adaptive", "effort": "high"}
  }
}
```

For Azure or Azure API Management deployments, use the same `api_key`
field plus `host`:

```json
{
  "host": "example.azure-api.net",
  "api_key": "...",
  "api_version": "2025-04-01-preview",
  "model": "gpt-5.5",
  "defaults": {
    "thinking": {"type": "adaptive", "effort": "medium"}
  },
  "slow": {
    "thinking": {"type": "adaptive", "effort": "high"}
  }
}
```

GPT-5/o-series calls use the Responses API. `thinking` maps to
OpenAI `reasoning.effort`, and kres sends text verbosity `medium` by
default. Explicit thinking budgets are mapped onto OpenAI effort
tiers; adaptive `low` / `medium` / `high` are sent directly.

All provider credentials use the same JSON field name: `api_key`.
Legacy `key`, `primary_key`, and `secondary_key` fields are rejected.

Model files use each role's default embedded system prompt unless a
role section overrides `system` or `system_file`. The default prompt is
injected by role when the loaded config has neither field.

Legacy role-specific filenames such as `fast-code-agent.json`,
`main-agent.json`, `todo-agent.json`, and
`slow-code-agent-<tag>.json` are no longer auto-discovered from
`~/.kres/`. Existing files with those names are ignored unless passed
explicitly with the corresponding `--*-agent` flag.

## System prompts

Agent `*.system.md` prompts (fast / slow / slow-coding /
slow-generic / main / todo) are compiled into the kres binary
(`kres-agents/src/embedded_prompts.rs`). `setup.sh` does NOT
install them on disk — rebuilding kres refreshes them.

When a model config under `~/.kres/models/` sets
`system_file: "system-prompts/<name>.system.md"`, kres resolves that to
`~/.kres/system-prompts/<name>`, then falls back to the embedded prompt
with the same basename. Shipped model configs normally omit
`system_file`; the loader supplies the correct role default.

`AgentConfig::load` order:

1. **Disk override**: `~/.kres/system-prompts/<basename>` if it is
   readable — used verbatim.
2. **Embedded**: compiled-in copy keyed by basename.
3. **Error**: neither present → load fails naming both paths.

To customise, drop the edited file at
`~/.kres/system-prompts/<basename>`. A default install has no
files there; the embedded copies do all the work.

Non-workflow prompt templates live in
`kres-agents/src/user_commands.rs` with their own override directory
at `~/.kres/commands/` — see [commands.md](commands.md). Workflow
commands such as `/review`, `/triage`, and `/fix` are configured via
`~/.kres/workflows/<name>.json` overrides instead. The prompt and
workflow override directories are distinct so command dispatch has one
path per shipped command.

## semcode MCP integration

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

semcode is not authoritative. It can be unavailable while indexing,
and it can miss macros, global symbols, or complex definitions. When a
semcode `source`, `type`, `callers`, or `callees` lookup fails, returns
no match, or returns output that cannot be parsed as a symbol, kres
falls back to local grep/read-style evidence from the workspace. A
missing semcode result must not be used by itself to conclude that
source is unavailable, a symbol is absent, or a review is clean.
For broad source fallbacks, kres returns the grep match list without
adding a special per-file cap and without automatically reading full
source context around every hit. If the shared tool-output cap truncates
the list, the truncation marker is visible to the agent. The agent should
request targeted `read` followups for the specific file:line ranges it
needs to inspect.

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

### When it helps

Whole-program questions that read/grep can only approximate —
"who calls `<function>`", "what does `<type>` look
like on this branch", "show me every change to this function
over the last 1000 commits". Without semcode the main agent
still answers, just via more grep round-trips and more false
positives.

### Install

Either drop `semcode-mcp` on your `PATH` before running
`setup.sh` (auto-install kicks in), or pass
`--semcode PATH/TO/semcode-mcp` explicitly. `--semcode ""`
force-skips the MCP install even when the binary is on `PATH`.

kres's `.gitignore` excludes `/.semcode.db/` at the repo root —
semcode's on-disk index cache; consult the semcode repo for how
it's populated and invalidated.

## Kernel review prompts

Subsystem knowledge for the kernel lives in a separate repo:
<https://github.com/masoncl/review-prompts>.

`skills/kernel.md` is a thin loader that references
`@REVIEW_PROMPTS@/kernel/technical-patterns.md` as a mandatory
read on every slow-agent turn, plus
`@REVIEW_PROMPTS@/kernel/subsystem/subsystem.md` as the index
into per-subsystem guides. `setup.sh` substitutes
`@REVIEW_PROMPTS@` with an on-disk path at install time.

Point `setup.sh` at your clone:

```
./setup.sh --fast-key $FAST_API_KEY --slow-key $SLOW_API_KEY \
           --review-prompts /path/to/review-prompts
```

`setup.sh --fast-key` and `--slow-key` replace the `@FAST_KEY@` and
`@SLOW_KEY@` placeholders in `~/.kres/models/*.json`. The replacement
lands in `api_key` fields.

Without a resolvable path, `setup.sh` leaves the kernel skill
uninstalled — agents still run, but the slow agent loses the
pattern catalogue and subsystem context.

When `--review-prompts` is omitted, `setup.sh` peeks at
`~/.claude/skills/kernel/SKILL.md` and offers the first
review-prompts path it finds there. Pass `--review-prompts PATH`
to bypass the interactive prompt.

## Workspace Skill Detection

kres scans `~/.kres/skills/*.md` at startup, then selects automatic
knowledge skills from the detected workspace type. Linux kernel trees
load `kernel.md` and use make-oriented build assumptions; systemd trees
load `systemd.md` and use meson-oriented build assumptions. Workflow
JSON can request the same behavior with `"skills": ["auto"]`.
