# Coding tasks — reproducers and in-place fixes

Prompts like `--prompt 'write a reproducer for the UAF in
net/sched/cls_bpf.c'` or `--prompt 'fix the missing frag-free
in bnxt_xdp_redirect'` get classified as **coding mode** by the
goal agent. Coding mode replaces the lens fan-out and findings
consolidator with a single slow-agent call producing source
code on two channels:

- **`code_output`** — `{path, content, purpose}` records. The
  reaper writes each entry to `<workspace>/code/<path>` via
  tmp + rename. Use for fresh artifacts (reproducers, test
  harnesses, trigger programs, whole-file fixes).

- **`code_edits`** — `{file_path, old_string, new_string,
  replace_all}` records (same shape as Claude Code's Edit
  primitive). `old_string` must match exactly once unless
  `replace_all: true`; the file is rewritten atomically via
  `kres_agents::tools::edit_file` (tmp + rename). Preferred for
  surgical fixes: the `old_string` anchor forces the agent to
  quote real bytes rather than reconstruct from memory. Per-edit
  results fold into the analysis trailer as
  `Edits applied (N/M[, K FAILED]):` so the next slow turn sees
  which edits landed.

The slow-code prompt
(`configs/prompts/slow-code-agent-coding.system.md`) enforces two
rules worth knowing: the verbatim file contents must be in
`symbols` or `context` before an edit is emitted (otherwise the
agent must issue a `read` followup and wait); and within one
batch each `old_string` matches the file state AFTER prior edits
in the same batch have landed.

**Bash verification** — the slow agent can emit a `bash` followup
(`cc -o repro repro.c && ./repro`, `make -C test`, …) to build
and run what it wrote. The main agent executes from the workspace
root and feeds back `[exit N]` + stdout + stderr. `bash` is OFF
by default; see [action-allowlist.md](action-allowlist.md). A
typical invocation:

```
kres --prompt 'write a reproducer for the stack OOB in x_tables' \
     --allow bash \
     --results repro-run
```

Artifacts land in `<results>/code/<path>` (code_output) and
in-place under `<workspace>` (code_edits). Coding tasks skip the
findings merger — their output is source, not bug records.
