# Kernel review prompts

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

Without a resolvable path, `setup.sh` leaves the kernel skill
uninstalled — agents still run, but the slow agent loses the
pattern catalogue and subsystem context.

When `--review-prompts` is omitted, `setup.sh` peeks at
`~/.claude/skills/kernel/SKILL.md` and offers the first
review-prompts path it finds there. Pass `--review-prompts PATH`
to bypass the interactive prompt.
