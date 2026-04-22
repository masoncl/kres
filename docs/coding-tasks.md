# Coding tasks — reproducers and in-place fixes

Not every prompt is a review. Ask kres `--prompt 'write a
reproducer for the UAF in net/sched/cls_bpf.c'` or `--prompt 'fix
the missing frag-free in bnxt_xdp_redirect'` and the goal agent
classifies the task as **coding mode** instead of analysis. Coding
mode swaps out the review pipeline's lens fan-out and findings
consolidator for a single slow-agent call whose job is to produce
source code. Two output channels:

- **`code_output`** — a list of `{path, content, purpose}`
  records. Each entry is a full file body that the reaper writes
  under `<workspace>/code/<path>` via tmp + rename. Use this for
  fresh artifacts (reproducers, test harnesses, trigger programs,
  scratch fixes that rewrite a whole file).

- **`code_edits`** — a list of `{file_path, old_string,
  new_string, replace_all}` records, same shape as Claude Code's
  Edit primitive. The reaper applies each edit in order via
  `kres_agents::tools::edit_file`: `old_string` must appear
  exactly once in the current file contents (unless
  `replace_all: true`), and the file is rewritten atomically via
  tmp + rename (`kres-agents/src/tools.rs`). This is the
  preferred channel for surgical one-line fixes — the
  `old_string` anchor forces the slow agent to quote bytes from
  the real file rather than reconstruct them from summary-level
  descriptions. Each edit's result (replacement count for
  success, verbatim error message for failure) is folded into
  the task's analysis trailer under `Edits applied (N/M[, K
  FAILED]):` so the next slow-agent turn can see which edits
  landed and correct any that didn't.

The slow-code prompt (`configs/prompts/slow-code-agent-coding.system.md`)
enforces two rules that matter in practice: the verbatim current
contents of the file being fixed must be in the gathered symbols
or context before any edit is emitted (a `read` followup is
requested and waited on otherwise — the slow agent is explicitly
told not to fix from memory), and a multi-edit batch applies in
emission order with each `old_string` matching the file state
AFTER prior edits in the same batch have landed.

**Verification via `bash`** — the slow agent can emit a `bash`
followup (e.g. `cc -o repro repro.c && ./repro`, `make -C test`)
to build and run what it just wrote. The main agent executes it
from the workspace root, captures `[exit N]` + stdout + stderr,
and feeds the result back. This is the one flow where `bash` is
genuinely useful — but it is OFF by default (see
[action-allowlist.md](action-allowlist.md)) and must be explicitly
enabled for the session.

On a coding run you typically invoke kres with:

```
kres --prompt 'write a reproducer for the stack OOB in x_tables' \
     --allow bash \
     --results repro-run
```

Artifacts land in `<results>/code/<path>` (for `code_output`) and
in-place under `<workspace>` (for `code_edits`). The ordinary
`report.md` + `findings.json` ledger continues to accumulate
narrative; coding tasks skip the findings-merger path since their
output is source files, not bug records.
