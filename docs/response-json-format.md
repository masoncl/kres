# response JSON format

The code agent responds with JSON containing its analysis and optional
requests for additional data.

## Schema

```json
{
  "analysis": "string — the code agent's analysis and answer",
  "followups": [
    {
      "type": "string — one of the typed followup kinds listed below",
      "name": "string — symbol name, regex pattern, glob, file:line+count, or question text",
      "reason": "string — why this data is needed",
      "path": "string — optional directory scope for search/file types"
    }
  ],
  "skill_reads": [
    "string — absolute file path referenced in a skill that needs loading"
  ],
  "findings": ["full Finding records or deltas"],
  "ready_for_slow": true,
  "code_output": [{"path": "relative/path", "content": "...", "purpose": "..."}],
  "code_edits": [{"file_path": "path", "old_string": "...", "new_string": "...", "replace_all": false}],
  "plan": {"steps": []}
}
```

The complete response must be one JSON object. Unknown top-level and nested
fields are rejected. Consumers may impose stricter required fields or allow
workflow-declared extension fields.

## Followup types

| `type`     | `name` contains                  | What it fetches                               |
|------------|----------------------------------|-----------------------------------------------|
| `source`   | symbol name                      | Full source definition for a function or macro  |
| `type`     | type name                        | Struct, union, or typedef definition            |
| `callers`  | function name                    | All functions that call it                     |
| `callees`  | function name                    | All functions it calls                         |
| `search`   | regex pattern                    | Grep across the codebase                       |
| `file`     | filename glob                    | Find files matching the pattern                |
| `read`     | `file.c:100+50`                  | Read specific file range (start line + count)  |
| `question` | question text                    | Free-form question for the orchestrator        |
| `survey`   | source filename                  | Compact semcode file inventory                 |
| `grep`     | regex pattern                    | Local grep fallback/search                     |
| `find`     | filename pattern                 | Local file discovery                           |
| `git`      | readonly git command             | Repository history or diff context             |
| `make` / `meson` / `cargo` | command          | Typed build or test command                    |
| `bash`     | shell command                    | Allowlist-gated workspace command              |
| `lore`     | query                            | Semcode lore/history search                    |

Every followup requires a non-empty `reason` that explains what decision the
requested evidence will unblock; this lets the fetcher and later agents retain
the purpose when requests are deduplicated or served from cache. The optional
`path` field scopes searches and file discovery. Optional
`nice_to_have: true` marks a non-blocking followup; omitted or false is
blocking.

`findings` is a delta, not the complete store. `code_output` and `code_edits`
are used in coding mode. `plan` is accepted only when the request set
`plan_rewrite_allowed`; review lenses cannot rewrite the global plan.

## Example

```json
{
  "analysis": "[NO SOURCE] Cannot verify UAF without source.",
  "followups": [
    {"type": "source", "name": "__mld_query_work", "reason": "need source to verify group pointer"},
    {"type": "type", "name": "inet6_dev", "reason": "need struct fields used by the source"},
    {"type": "callers", "name": "__mld_query_work", "reason": "trace entry path"},
    {"type": "search", "name": "IP6SKB_ROUTERALERT", "path": "net/ipv6/", "reason": "find flag checks"},
    {"type": "file", "name": "mcast.c", "path": "net/", "reason": "locate the file"},
    {"type": "read", "name": "net/ipv6/mcast.c:1460+50", "reason": "read around the stale pointer"}
  ]
}
```
