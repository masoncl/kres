# Development

## Workspace layout

```
kres/
├── Cargo.toml                     Rust workspace manifest
├── kres/                          binary crate (`kres` command)
├── kres-core/                     Task, TaskManager, Plan, shutdown, findings
├── kres-llm/                      model transports, streaming clients, rate limiting
├── kres-mcp/                      stdio JSON-RPC client for MCP servers
├── kres-agents/                   fast / slow / main / todo / consolidator pipelines
├── kres-repl/                     TUI/REPL, commands, sessions, workflow integration
├── configs/                       shipped runtime defaults
│   ├── models/
│   │   ├── anthropic-fast.json
│   │   ├── anthropic-slow.json
│   │   ├── claude-codes.json
│   │   ├── codex-codes.json
│   │   └── vertex-dummy.json
│   ├── settings.json
│   ├── mcp.json
│   ├── prompts/                   system prompts + report/artifact templates
│   └── workflows/                 shipped JSON workflow contracts + schema
├── docs/                          JSON-schema docs + feature guides
├── AGENTS.md                      project architecture and implementation rules
├── CLAUDE.md                      pointer to AGENTS.md for Claude Code
├── setup.sh                       bootstrap ~/.kres/ from configs/
├── .githooks/pre-commit           runs cargo fmt + clippy on every commit
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

`.githooks/pre-commit` runs `cargo fmt --all --check` + `cargo clippy -D
warnings` on every commit. Enable it per-clone with:

```
git config core.hooksPath .githooks
```

## Wire-format references

- [findings-json-format.md](findings-json-format.md)
- [prompt-json-format.md](prompt-json-format.md)
- [response-json-format.md](response-json-format.md)
