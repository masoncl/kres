# Configuration — `~/.kres/` layout, models, and system prompts

## Config directory: `~/.kres/`

The `kres` REPL resolves agent config paths in this order:

1. explicit CLI flag (e.g. `--fast-agent /path/to/fast.json`)
2. provider file under `~/.kres/models/` containing the selected model

Non-agent paths such as `mcp.json` and `skills/` only use explicit
CLI flags and the same filename under `~/.kres/`.

Default non-agent paths:

| Flag              | Default                          |
|-------------------|----------------------------------|
| `--mcp-config`    | `~/.kres/mcp.json`               |
| `--skills`        | `~/.kres/skills/`                |
| `--findings`      | `<results>/findings.json`        |

A missing model file in `~/.kres/models/` is not an error by itself, but any
role whose model file cannot be resolved is treated as not configured unless
the matching explicit `--*-agent` flag was provided. Results default to a
unique directory under `~/.kres/sessions/`; the command history is written to
`~/.kres/history`.

## Model selection

`~/.kres/settings.json` carries a model selector for each agent role.
A selector is either a model id, when exactly one provider file offers it,
or `<provider>.json:<model-id>` when disambiguation is required:

```json
{
  "models": {
    "fast": "vertex-dummy.json:claude-sonnet-4-6",
    "slow": "anthropic-slow.json:claude-opus-4-7",
    "main": "vertex-dummy.json:claude-sonnet-4-6",
    "todo": "vertex-dummy.json:claude-sonnet-4-6",
    "classifier": "anthropic-fast.json:claude-haiku-4-5"
  },
  "model_aliases": {
    "sonnet": "vertex-dummy.json:claude-sonnet-5",
    "opus": "anthropic-slow.json:claude-opus-4-7"
  }
}
```

`model_aliases` defines operator-owned short names for model selectors. Alias
values may be either an unqualified model id or a provider-qualified selector.
They apply to role values under `models`, the `--*-model` flags, and `--slow`.
Configured aliases take precedence over the legacy built-in `sonnet` and
`opus` spellings. Aliases expand once, so an alias value must name a concrete
selector rather than another alias.

Project-local `.kres/settings.json` aliases override global aliases by name;
unmentioned global aliases remain available.

Each JSON file under `~/.kres/models/` describes one connection. Credentials,
provider, endpoint, proxy, headers, and TLS settings are top-level and shared.
The required `models` object contains per-model limits and thinking defaults:

```json
{
  "api_key": "...",
  "models": {
    "claude-sonnet-4-6": {
      "max_tokens": 64000,
      "max_input_tokens": 900000,
      "rate_limit": 800000
    },
    "claude-opus-4-7": {
      "max_tokens": 128000,
      "max_input_tokens": 900000,
      "rate_limit": 800000,
      "thinking": {"type": "adaptive", "effort": "xhigh"}
    }
  }
}
```

Role-specific model settings do not exist. Fast, main, todo, classifier, and
slow receive the same limits whenever they select the same model; their
behavior differs through role-specific embedded system prompts. A provider
file with multiple models must be qualified when passed directly, for example
`kres test ~/.kres/models/anthropic-slow.json:claude-opus-4-7`.

If multiple provider files contain the same model, an unqualified selector is
an error listing the candidates. Select it as
`foo.json:claude-opus-4-7`. Per-run `--fast-model`, `--slow-model`,
`--main-model`, `--todo-model`, and `--classifier-model` accept the same
selector syntax. `--slow sonnet` and `--slow opus` use `model_aliases` when
configured, then fall back to their shipped model ids. Ambiguity still
requires qualification.

Pointing fast and slow at the same model is fine: the fast/slow
distinction is driven by per-agent system prompts and the
context each agent receives, not by model choice. Two different
models is a cost/latency optimisation, not a correctness
requirement.

For OpenAI API access, set `provider: "openai"`. `base_url` is
optional and defaults to `https://api.openai.com/v1`; set it only for a
compatible proxy:

```json
{
  "provider": "openai",
  "api_key": "...",
  "models": {"gpt-5.5": {
    "max_tokens": 128000,
    "max_input_tokens": 900000,
    "rate_limit": 900000,
    "thinking": {"type": "adaptive", "effort": "medium"}
  }}
}
```

For Azure or Azure API Management deployments, use the same `api_key`
field plus `host`:

```json
{
  "host": "example.azure-api.net",
  "api_key": "...",
  "api_version": "2025-04-01-preview",
  "models": {"gpt-5.5": {
    "thinking": {"type": "adaptive", "effort": "medium"}
  }}
}
```

GPT-5/o-series calls use the Responses API. `thinking` maps to
OpenAI `reasoning.effort`, and kres sends text verbosity `medium` by
default. Explicit thinking budgets are mapped onto OpenAI effort
tiers; adaptive `low` / `medium` / `high` are sent directly.

## Codex Codes

Use `provider: "codex-codes"` to run a model through the `codex-codes` Rust
crate. This backend maintains a Codex app-server connection; it does not use
kres's HTTP client. `codex_path` optionally selects a Codex
executable and defaults to `codex` on `PATH`. `base_url` and `api_key` are
optional and are forwarded to the SDK when present; otherwise the CLI's own
authentication and configuration apply.

`codex_home` sets an isolated `CODEX_HOME` for the child process; kres creates
the directory before starting Codex. Values in
`codex_config` are serialized as TOML and passed as repeated Codex CLI
`-c key=value` overrides before `app-server`. The shipped configuration uses
both to prevent kres's API-style calls from loading the operator's Codex
plugins, skills, hooks, project instructions, and managed `meta_core` MCP
server:

```json
{
  "provider": "codex-codes",
  "codex_home": "~/.kres/codex-home",
  "codex_config": {
    "mcp_servers.meta_core.enabled": false,
    "project_skill_configurable_directories": [],
    "features.skill_search": false,
    "features.plugins": false,
    "features.hooks": false,
    "project_doc_max_bytes": 0
  }
}
```

System-managed Codex requirements still apply. Other managed MCP servers must
be disabled by their own `mcp_servers.<name>.enabled=false` entry if present.

Kres creates a fresh ephemeral thread for every call, while reusing one
app-server process. A dispatcher routes interleaved notifications by thread ID,
so independent agent calls run concurrently without sharing conversation
context. Threads are read-only with approvals disabled. Select the shipped
example as `codex-codes.json:gpt-5.6-sol`.

## Claude Codes

Use `provider: "claude-codes"` to run models through the `claude-codes` Rust
crate and a locally installed Claude CLI. `claude_path` optionally selects the
executable and defaults to `claude` on `PATH`. `api_key` and `base_url` are
optional; without them, the CLI's normal authentication and configuration
apply.

Each kres call uses a fresh, non-persistent Claude process and therefore starts
with an empty conversation context. The process runs with tool access denied.
The shipped example contains both Sonnet and Opus; select one with, for example,
`claude-codes.json:claude-sonnet-4-6`.

## Anthropic Vertex and custom transports

Use `provider: "vertex-dummy"` for an Anthropic Messages API exposed
through Vertex `rawPredict` / `streamRawPredict`. This protocol requires
`base_url`, `project_id`, and `region`; the model is routed in the URL and
kres emits the Vertex Anthropic API version in the request body. Exact token
counting is unavailable on this protocol, so kres uses its local estimate.

Model configs can also define `headers`, a UUID-valued `session_header`, an
explicit `proxy`, additional `tls.ca_certificates` PEM bundles, and ordered
`tls.identity_candidates`. A candidate's `cert`
may hold a combined certificate/key PEM, or `key` may name a separate key.
`${NAME}` references in these transport fields expand from the environment.
Missing certificate candidates are skipped and certificate contents are never
included in logs or rate-limiter keys.

The shipped `vertex-dummy.json` shows the minimal multi-model Vertex shape
using intentionally non-functional connection values. Supply a real endpoint
and authentication transport in the operator-owned configuration.

Provider API credentials use the same JSON field name: `api_key`; transport-
authenticated providers may omit it.
Legacy `key`, `primary_key`, and `secondary_key` fields are rejected.

Provider files use each role's default embedded system prompt. An optional
top-level `system` or `system_file` overrides it for every model using that
connection.

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

The deterministic tool service's code navigation is enhanced by semcode
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

kres works without semcode: typed fast-agent requests are served with `read`,
`grep`, and `git` fallbacks. semcode adds a
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
Whole-file review treats `file_survey` specially: an unavailable server, tool
error, or empty text response falls back to a local definition scan. A valid
structured response containing an empty inventory is currently accepted as an
empty survey and does not trigger that fallback.
For broad source fallbacks, kres returns the grep match list without
adding a special per-file cap and without automatically reading full
source context around every hit. If the shared tool-output cap truncates
the list, the truncation marker is visible to the agent. The agent should
request targeted `read` followups for the specific file:line ranges it
needs to inspect.

Supported semcode operations include:

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
over the last 1000 commits". Without semcode the tool service
still gathers evidence, just via more grep round-trips and more false
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
