# How findings move through kres

Narrative companion to [`findings-json-format.md`](findings-json-format.md).

## The pipeline

```
prompt → fast agent → slow agent → consolidator → reaper → findings.json
                      (analysis +   (per-lens →  (applies   (jsondb-backed)
                       findings)     unified)    delta)
                                                     ↓
                                                  report.md
```

The slow agent emits a JSON envelope; the reaper turns each reaped
task into storage.

## The slow agent's `findings` array is a delta

```json
{"analysis": "...", "findings": [{"id": "race_in_cq_ack", ...}], "followups": [...]}
```

Per entry, keyed by `id`:

- **New `id`** → append.
- **Existing `id`** → merge in place. Populate only the fields that
  change.
- **Existing `id` + `"status": "invalidated"`** → mark as negative
  evidence; record stays in the store.
- **Existing `id` + `"reactivate": true`** → reverse a prior
  invalidation.

The agent emits only what it's adding, extending, invalidating, or
reactivating this turn — never the whole list.

## The reaper, per reaped task

1. Append `effective_analysis` to `report.md` (before the `/stop`
   latch check, so a stopped task still captures prose).
2. If `/stop` is latched, bail.
3. Run the **promoter**: a one-shot fast-agent audit that reads
   the analysis prose and a prose-narrowed slice of existing
   findings, then emits any bugs the prose names that the slow
   agent didn't promote to a Finding. Extras are appended to the
   delta.
4. `FindingsStore::apply_delta(delta, stamp, Some(&analysis))`.
5. If the promoter contributed entries, append a
   `_promoted-from-prose: id1, id2_` trailer to `report.md`.

The promoter covers two silent-loss paths: a lens describing a bug
in prose without emitting the matching Finding, and a slow-agent
reply whose JSON didn't parse (`ParseStrategy::RawText`). It uses
its own judge-mode system prompt (`PROMOTE_SYSTEM`) and is
cancellable by `/stop` via `tokio::sync::Notify`.

## Apply rules (deterministic Rust)

`kres_core::findings::apply_delta_to_list`:

- **Update (matching id):** union `relevant_symbols`,
  `relevant_file_sections`, `related_finding_ids`, `open_questions`.
  Prose fields (`title`, `summary`, `reproducer_sketch`, `impact`,
  `mechanism_detail`, `fix_sketch`) use **longer-wins** — a shorter
  incoming is ignored, so a later turn that mentions the id in
  passing can't clobber a detailed earlier body. Severity escalates
  only. `reactivate: true` beats contradictory
  `status: invalidated` on the same delta.
- **Add (new id):** append with stamps; strip any wire-level
  `reactivate` / `details` fields the agent tried to send.
- **Collision at the promoter:** `filter_net_new` sees the full
  store ∪ delta universe. A colliding id is **renamed** to
  `<id>__promoted_<n>`, never dropped — losing a record is worse
  than storing a duplicate.

## Provenance

Each task gets a `Uuid::new_v4()` at spawn. Pipeline-dispatched
tasks (`cmd_next` / `cmd_continue`) also carry the dispatching
`TodoItem.id` (or `.name`). `first_seen_task` /
`last_updated_task` are stamped as `"<uuid-simple>/<todo-tag>"` or
bare `"<uuid-simple>"` for operator-typed prompts.

## The `details` field

Every `apply_delta` touch attaches a `{task, analysis}` entry to
the finding's `details`. Same task_id overwrites; different
task_ids append. This is how `/summary` reaches the per-task
narrative that would otherwise live only in `report.md`.

**`details` never goes back to an agent.** Agent-bound slices go
through `kres_core::redact_findings_for_agent` first — applied in
the slow-agent `previous_findings` path and in the promoter's
inputs. An incoming delta that tries to populate `details` itself
is stripped at add-time.

## Storage

`FindingsStore` wraps `JsonDb<FindingsFile>`. Every write-guard
drop atomically rewrites `<results>/findings.json` (tmp + fsync +
rename). One canonical file, no snapshots, no LLM round-trip on
apply. Legacy unversioned `findings.json` files load via
`SchemaV0::VERSION_OPTIONAL = true`.

## Observability

The reaper logs per apply:

```
[findings] N total (added=A updated=U invalidated=I reactivated=R changed=C quiescent=Q)
```

`tasks_since_change` resets only on a structural change (not on
details-only updates) and drives the `--turns 0` quiescence stop.
