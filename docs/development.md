# Development

## Workspace layout

```
kres/
├── Cargo.toml                     Rust workspace manifest
├── kres/                          binary crate (`kres` command)
├── kres-core/                     Task, TaskManager, Plan, shutdown, findings
├── kres-llm/                      Anthropic streaming client + rate limiter
├── kres-mcp/                      stdio JSON-RPC client for MCP servers
├── kres-agents/                   fast / slow / main / todo / consolidator / merger
├── kres-repl/                     readline UI, commands, signal handling
├── configs/                       per-agent JSON configs (shipped defaults)
│   ├── fast-code-agent.json
│   ├── slow-code-agent-opus.json
│   ├── slow-code-agent-sonnet.json
│   ├── main-agent.json
│   ├── todo-agent.json
│   ├── settings.json
│   ├── mcp.json
│   └── prompts/                   system prompts + review templates
├── skills/                        domain-knowledge markdown fed to agents
│   └── kernel.md
├── docs/                          JSON-schema docs + feature guides
├── CLAUDE.md                      project instructions for Claude Code
├── setup.sh                       bootstrap ~/.kres/ from configs/
├── .githooks/pre-commit           runs cargo fmt + clippy on every commit
├── NEWS.md
└── README.md
```

## Build, test, lint

```
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Pre-commit hook

`.githooks/pre-commit` runs `cargo fmt --check` + `cargo clippy -D
warnings` on every commit. Enable it per-clone with:

```
git config core.hooksPath .githooks
```

## Wire-format references

- [findings-json-format.md](findings-json-format.md)
- [prompt-json-format.md](prompt-json-format.md)
- [response-json-format.md](response-json-format.md)
