# prompt JSON format

The prompt JSON is sent to the code agent as the user message. It
provides kernel source context and a question for the agent to analyze.

## Schema

```json
{
  "question": "string — the question for the code agent to answer",
  "symbols": [
    {
      "name": "string — symbol name",
      "type": "string — function|struct|union|enum|typedef|macro_function|define",
      "filename": "string — source file path relative to kernel tree root",
      "line": "integer — line number where the symbol starts",
      "definition": "string — full text of the symbol, including leading comments",
      "callers": ["string — names of functions that call this symbol"],
      "callees": ["string — names of functions called by this symbol"]
    }
  ],
  "context": [
    {
      "source": "string — where this data came from (e.g. 'semcode/find_callers')",
      "content": "string — raw content"
    }
  ],
  "previously_fetched": {},
  "previous_findings": [],
  "parallel_lenses": [],
  "lens_instruction": "string",
  "skills": {},
  "plan": {},
  "plan_rewrite_allowed": true
}
```

## Fields

### Required

| Field      | Type   | Description                                          |
|------------|--------|------------------------------------------------------|
| `question` | string | The question for the code agent to answer.           |

### Optional

| Field     | Type   | Description                                           |
|-----------|--------|-------------------------------------------------------|
| `symbols` | array  | Array of symbol objects providing source code context. |
| `context` | array  | Array of general context objects from tool results.    |
| `skills`  | object | Dict of skill name → {content, files} for domain knowledge. |
| `previously_fetched` | object | Manifest of evidence already gathered for this task. |
| `previous_findings` | array | Current findings, redacted to remove store-owned narrative/provenance fields. |
| `parallel_lenses` | value | Workflow-defined lens metadata for lensed slow calls. |
| `lens_instruction` | string | Instruction for the current lens. |
| `plan` | object | Current session plan, forwarded to fast and slow turns. |
| `plan_rewrite_allowed` | boolean | Permits the first eligible non-review slow turn to return a replacement step list. |

## Symbol object

Each entry in `symbols` describes a kernel code symbol.

| Field        | Type    | Description                                                |
|--------------|---------|------------------------------------------------------------|
| `name`       | string  | Symbol name.                                               |
| `type`       | string  | One of: `function`, `struct`, `union`, `enum`, `typedef`, `macro_function`, `define`. |
| `filename`   | string  | Source file path relative to the kernel tree root.         |
| `line`       | integer | Line number where the symbol starts (including leading comment). |
| `definition` | string  | Full text of the symbol, including any leading comments.   |
| `callers`    | array   | Names of functions that call this symbol (optional).       |
| `callees`    | array   | Names of functions called by this symbol (optional).       |

## Context object

General-purpose context from tool results that don't map to a specific
symbol (e.g. call chains, grep results, lore search hits). Tool-specific
objects may carry additional structured fields; `source` and `content` are the
common form, not a closed serde schema.

| Field     | Type   | Description                                          |
|-----------|--------|------------------------------------------------------|
| `source`  | string | Where this data came from (tool name or description).|
| `content` | string | Raw content from the tool result.                    |

## Example

```json
{
  "question": "Is there a use-after-free in __mld_query_work?",
  "symbols": [
    {
      "name": "__mld_query_work",
      "type": "function",
      "filename": "net/ipv6/mcast.c",
      "line": 1424,
      "definition": "static void __mld_query_work(...) {\n\t...\n}\n",
      "callers": ["mld_query_work"],
      "callees": ["pskb_may_pull", "ipv6_addr_equal", "mld_marksources"]
    }
  ],
  "context": [
    {
      "source": "semcode/find_callers",
      "content": "mld_query_work calls __mld_query_work at net/ipv6/mcast.c:1540"
    }
  ]
}
```
