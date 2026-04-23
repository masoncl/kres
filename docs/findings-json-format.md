# findings.json format

`kres --findings FILE` establishes the base path for bug snapshots.
`FILE` itself is the canonical current state, rewritten after every
merge pass; `FILE-N` snapshots capture the pre-overwrite state for
each turn so an operator can walk the history. See
kres-core/src/findings.rs for the atomic write path.

Rationale:
- A machine-readable `findings` list lets every slow agent skip
  rediscovering known bugs and instead build chains from earlier
  findings into larger, composite bugs.
- External tooling (reproducer harnesses, report generators) can read
  this file directly without parsing analysis prose.
- Each finding embeds the relevant symbol bodies and file sections it
  needed. That makes the finding self-contained — a reader or future
  reproducer doesn't need the full session's gathered context to
  understand what the bug is.

## Top-level shape

```json
{
  "findings": [ <Finding>, ... ],
  "updated_at": "2026-04-18T03:42:10Z",
  "tasks_since_change": 0
}
```

- `findings`: array of Finding records (may be empty).
- `updated_at`: ISO-8601 UTC timestamp of the most recent write.
- `tasks_since_change`: how many consecutive tasks have completed
  without adding or modifying a finding. When this reaches 5, kres
  considers the analysis complete and prints a message.

## Finding record

```json
{
  "id": "netkit_scrub_noop_shared_netns",
  "title": "NETKIT scrub is a no-op when endpoints share a netns",
  "severity": "high",
  "status": "active",
  "relevant_symbols": [
    {
      "name": "netkit_run",
      "filename": "drivers/net/netkit.c",
      "line": 80,
      "definition": "static netdev_tx_t netkit_run(...) {\n\tif (!xnet)\n\t\treturn NETDEV_TX_OK;\n\t...\n}"
    },
    {
      "name": "skb_scrub_packet",
      "filename": "net/core/skbuff.c",
      "line": 5812,
      "definition": "void skb_scrub_packet(struct sk_buff *skb, bool xnet) {\n\t...\n}"
    }
  ],
  "relevant_file_sections": [
    {
      "filename": "include/uapi/linux/if_link.h",
      "line_start": 1285,
      "line_end": 1295,
      "content": "/* IFLA_NETKIT_SCRUB ...\n */"
    }
  ],
  "summary": "When both ends of a netkit pair live in the same netns, netkit_run() returns before the scrub path runs (drivers/net/netkit.c:80-81 `if (!xnet) return;`), so skb->sk / mark / tstamp bleed across the boundary. A guest task can emit a packet with a forged skb_owner and have it treated as host-local on RX.",
  "reproducer_sketch": "From inside the container netns, craft a packet whose struct sk_buff->sk points at a host-owned socket cookie (requires prior bpf_get_socket_cookie leak). Send on eth0; on the peer side, observe that xt_socket matches as if the packet were locally originated.",
  "impact": "Host-side firewall rules that match on skb->sk (xt_socket, nf_socket) are bypassed for guest-originated packets when the netkit pair shares a netns — CVE-2020-8558 class.",
  "first_seen_task": "netkit_run verdict semantics",
  "last_updated_task": "Host nft/ip rule configuration for netkit host-side device",
  "related_finding_ids": ["sslwall_bypass_ipv6_dstopts"]
}
```

### Required fields

| field | type | purpose |
|---|---|---|
| `id` | string | Short snake_case slug, ≤40 chars. Stable across updates. |
| `title` | string | One-line human title. |
| `severity` | enum | `low` / `medium` / `high` / `critical`. Scored by exploit potential, not textbook CVSS. |
| `status` | enum | `active` or `invalidated`. Default `active`. |
| `relevant_symbols` | array[object] | **Embedded** symbol records that the reader needs to understand the bug. Each: `{name, filename, line, definition}`. Pull only what's actually referenced in summary/reproducer_sketch — NOT the entire session's symbol list. At least one required. |
| `relevant_file_sections` | array[object] | **Embedded** source slices that aren't whole symbols (headers, tables, assembly, macros). Each: `{filename, line_start, line_end, content}`. Optional if every cited region is captured via `relevant_symbols`. |
| `summary` | string | 2-5 sentences. Must be sufficient for a reader to understand *what* is wrong and *why*. Reference code by `filename:line` — the reader can look it up in `relevant_symbols` / `relevant_file_sections`. |
| `reproducer_sketch` | string | The code path, inputs, and state required to trigger the bug. Even partial reproducers are recorded — do not leave blank. |
| `impact` | string | What goes wrong when triggered (crash, corruption, escape, firewall bypass, info leak, etc.). |

### Optional fields

| field | type | purpose |
|---|---|---|
| `first_seen_task` | string | `todo_item.name` of the task that produced this finding. |
| `last_updated_task` | string | `todo_item.name` of the most recent task that extended the finding. |
| `related_finding_ids` | array[string] | IDs of findings whose impact combines with, depends on, or amplifies this one. Used by the slow agent to build chains. |
| `mechanism_detail` | string | Specifics that pin down HOW the bug becomes exploitable: which struct-field type/offset gets clobbered, which invariant-establishing ordering contract in adjacent code is violated, what the actual kernel object behind an OOB target is (e.g. `tx_ring[8]` lands on a `tx_int` function pointer). These are the facts a reproducer or patch author would otherwise re-derive. |
| `fix_sketch` | string | 1-3 sentences describing a concrete patch the analysis identified (e.g. "cache the static-key result in a local bool at bnxt_xdp.c:353 and use it for both lock and unlock"). Omit entirely if no fix was analyzed — never fabricate. |
| `open_questions` | array[string] | Unresolved items that would settle or refine the finding: `[UNVERIFIED]` claims, call sites not yet confirmed, type-query followups, locking-order assumptions, etc. One sentence each. These accumulate across turns; the merger unions them. |
| `details` | array[object] | Per-task narrative captured at apply_delta time. Each entry `{task, analysis}` pairs a provenance stamp with the task's effective_analysis prose verbatim. **Store-local only** — every site that hands findings to an agent must run them through `kres_core::redact_findings_for_agent` first. Consumed by `/summary` so the plain-text summary can reach the richer exposition that would otherwise only live in report.md. Never emitted by agents; the store populates this field. |

## Sizing guidance for embedded bodies

`relevant_symbols[*].definition` and `relevant_file_sections[*].content`
should be JUST enough to prove the bug. Rules of thumb:

- A function that's the crux of the bug → include its full body.
- A function that only provides context (called from the buggy one
  but itself fine) → either skip it or include only the relevant
  snippet as a `relevant_file_sections` entry.
- A header / typedef / macro that defines a struct layout or constant
  the bug depends on → include just that declaration, not the entire
  header.
- Never include the whole `symbols` / `context` the slow agent was
  handed. That bloats the findings file and obscures the bug.

## Invariants

- `findings[].id` is unique within a file. Duplicate ids are a bug; the
  dedup pass must merge in-place by extending fields and linking
  `related_finding_ids`.
- `status: invalidated` findings stay in the file — they carry
  negative evidence so the slow agent doesn't re-propose them.
- A finding whose `relevant_symbols` / `relevant_file_sections` are a
  strict subset of another finding's — and whose `impact` overlaps —
  must be merged into the broader finding rather than kept separate.

## Relationship to other files

- `todo.md` (via `--todo`): the plan, what's next.
- `findings.json` (via `--findings`): the results, what's been
  proven. Maintained in-place by a jsondb-backed store; each task
  reap applies its delta deterministically (no per-turn snapshot
  history).
- `code.jsonl` / `main.jsonl`: raw inference transcripts.
- Report markdown (via `--report`): human narrative, appended per task.

The four are complementary and independent — writing
`findings.json` doesn't touch the others.

## Agent interaction

Three points where findings flow:

1. **Each slow-agent call emits `findings` natively**, inline with its
   `analysis` + `followups`, pulling relevant symbols/file-sections
   from the `symbols` and `context` it received. The slow agent is
   the most expensive call in the pipeline and has the richest
   context — making it produce the canonical structured output
   directly avoids a later extraction pass over prose. The slow
   agent's system prompt carries a PROMOTION RULE: every bug
   described in prose MUST also appear as a Finding, because
   downstream merge passes read ONLY the findings array.

2. **After all sibling lens calls return, a single fast-agent pass
   consolidates the per-lens findings for that task**: deduplicates
   across lenses (a bug seen by three lenses becomes one finding with
   extended `relevant_symbols`), composes a unified prose narrative
   that replaces the per-lens concatenation, performs a COMPLETENESS
   CHECK that promotes prose-only bugs (described by a lens but never
   emitted as a Finding by any lens) to new Findings, and returns
   `(unified_analysis, unified_findings)` for the task.

3. **After each task is reaped, the task's unified findings are
   applied to the running list as a delta by
   `kres_core::findings::FindingsStore::apply_delta`**: incoming
   records with a matching `id` update the existing finding in
   place (union relevant_symbols / relevant_file_sections /
   related_finding_ids / open_questions, prefer incoming non-empty
   prose, max severity, stamp `last_updated_task`); a matching id
   with `status: invalidated` flips the existing entry to
   invalidated; a new id is appended with `first_seen_task` stamped.
   The store is backed by jsondb and rewrites the canonical
   `findings.json` atomically on every apply. There is no per-turn
   snapshot history and no LLM round-trip during apply.

4. **Before each slow-agent call**, the slow-agent request includes a
   `previous_findings` field carrying the current list. The slow
   agent's system prompt tells it to (a) not rediscover things already
   in the list, (b) actively look for chains where this task's target
   combined with a listed finding produces a larger bug.
