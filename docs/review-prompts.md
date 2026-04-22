# Kernel review prompts

kres can leverage the kernel review prompts for additional
subsystem knowledge. These live in a separate repo:

https://github.com/masoncl/review-prompts

The shipped kernel skill (`skills/kernel.md`) is a thin loader:
it references `@REVIEW_PROMPTS@/kernel/technical-patterns.md` as
a mandatory read on every slow-agent turn, plus
`@REVIEW_PROMPTS@/kernel/subsystem/subsystem.md` as an index into
per-subsystem guides. `setup.sh` substitutes `@REVIEW_PROMPTS@`
with an on-disk path at install time (see `skills/kernel.md:8`,
`skills/kernel.md:17`, `skills/kernel.md:29`).

Point `setup.sh` at your clone so the skill can resolve those files:

```
./setup.sh --fast-key $FAST_API_KEY --slow-key $SLOW_API_KEY \
           --review-prompts /path/to/review-prompts
```

Without a resolvable path, `setup.sh` leaves the kernel skill
uninstalled (`setup.sh:386-389`) — the agents will still run,
but the slow agent won't have the pattern catalogue or subsystem
context, so findings tend to be shallower and miss conventions
that are obvious to someone who has read the pattern files.

If a path wasn't given explicitly, `setup.sh` peeks at
`~/.claude/skills/kernel/SKILL.md` and offers the first
`review-prompts` path it finds there (`setup.sh:338-372`); pass
`--review-prompts PATH` explicitly to bypass the prompt.
