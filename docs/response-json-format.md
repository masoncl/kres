# response JSON format

The code agent responds with JSON containing its analysis and optional
requests for additional data.

## Schema

```json
{
  "analysis": "string — the code agent's analysis and answer",
  "followups": [
    {
      "type": "string — source|type|callers|callees|search|file|read|git|question",
      "name": "string — symbol name, regex pattern, glob, file:line+count, or question text",
      "reason": "string — why this data is needed",
      "path": "string — optional directory scope for search/file types"
    }
  ],
  "skill_reads": [
    "string — absolute file path referenced in a skill that needs loading"
  ]
}
```

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

The optional `path` field scopes `search` and `file` to a subdirectory.

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
