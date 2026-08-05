# prompt JSON format

The prompt JSON is the user message sent to fast and slow code-agent turns. It
provides source context and a question for the current phase to analyze. Fast
gather uses one conversation: its first turn carries seed evidence and later
turns append only new records. The final slow turn receives the complete
canonical accumulated evidence.

## Wire framing: one or two documents

The schema below describes the logical prompt. On the wire it is sent as one or
two consecutive JSON documents in separate cacheable text blocks:

- a **stable** document with the fields that do not change across related calls
  — the gather conversation's first turn, or every lens in one fan-out;
- a **delta** document with the fields specific to this call.

Every field lands in exactly one document, and the union is the logical prompt
above. An empty delta is `{}`. The stable document ends with a newline, so the
pair reads as two whitespace-separated JSON values.

The split exists so the stable bytes can be cached once and reused: all lenses
over one task cache-read identical evidence, and gather round 2 reuses round
1's task scope. `CodePrompt::to_split_documents` produces both halves;
`to_delta_document` produces just the delta for a caller that already holds the
stable bytes. When a prompt has no stable field the stable document is empty
and the caller sends the whole prompt as one block.

Each half is a complete JSON document that parses on its own. An earlier
version instead spliced one object across the block boundary — chopping the
closing brace off the first half and the opening brace off the second — which
made neither half independently parseable and required an `_empty_tail`
sentinel key to keep an empty second half syntactically legal. Log tooling must
therefore read a payload as a *stream* of JSON values, not a single one; see
`turn_documents` in `kres-core/src/log.rs`.

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
  "previous_findings": [],
  "parallel_lenses": [],
  "lens_instruction": "string",
  "common_skills": {},
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
| `common_skills` | object | Byte-stable skill scaffold/common files; combine with task-selected `skills`. |
| `previous_findings` | array | Every current session finding, in full, redacted only to remove store-owned narrative/provenance fields. Sent once in the shared cached prefix so parallel lenses cache-read the same bytes. |
| `parallel_lenses` | value | Workflow-defined lens metadata for lensed slow calls. |
| `lens_instruction` | string | Instruction for the current lens. |
| `plan` | object | Compact session plan with goal, mode, explicit `active_step_id`, and steps. |
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
