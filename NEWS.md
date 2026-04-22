# NEWS

## April 22

Agent system prompts and slash-command templates are now embedded
in the kres binary — rebuilding kres refreshes them. `setup.sh`
no longer copies `*.system.md`, `bug-summary*.md`, or
`review-template.md` anywhere.

Stale files left under `~/.kres/prompts/` from earlier installs
are ignored and safe to delete. See
[docs/configuration.md](docs/configuration.md) for the override
paths and [docs/commands.md](docs/commands.md) for the
slash-command templates.

## April 21

New support for writing patches: `--prompt 'fix …'` classifies
the task as **coding mode** and produces in-place edits plus
fresh files. See [docs/coding-tasks.md](docs/coding-tasks.md).
