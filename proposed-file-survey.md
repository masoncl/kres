# Semcode file survey

The `file_survey` command gives an agent enough structural information
to plan targeted source reads without placing an entire large source file in
context.

## Command

```text
file_survey(path: string)
```

It is available as both a semcode query command (`file_survey PATH`, with
`survey` as an alias) and an MCP tool (`file_survey` with a `path` argument).

The command parses one file with Tree-sitter. Most fields are syntactic facts.
For every function and type defined in the file, semcode also searches the
current Git-aware database view and reports the number of distinct definitions
that call or reference the same spelling. These counts are not symbol
resolution and do not prove that two uses with the same spelling refer to the
same symbol.

## Output

```json
{"file":"mm/filemap.c","functions_defined":[["filemap_fault",14],["filemap_map_pages",2]],"calls":[["filemap_get_folio",6],["folio_put",31],["mapping->a_ops->read_folio",2]],"types_defined":[["struct filemap_folio_batch",3]],"types_mentioned":[["struct address_space",19],["struct folio",143],["vm_fault_t",4]],"parse_errors":0,"truncated":false}
```

Tuple meanings:

- `functions_defined`: `[name, distinct_caller_count]`
- `calls`: `[callee_expression_text, occurrence_count]`
- `types_defined`: `[declaration_name, distinct_referencer_count]`
- `types_mentioned`: `[type_syntax_text, occurrence_count]`

Line numbers are not included. Definitions retain source order. Aggregated calls
and type mentions sort lexically so output is deterministic. Serialize the
entire response as compact JSON without indentation or insignificant
whitespace.

The count in each definition tuple counts distinct stored function/type definitions whose
deduplicated relationship arrays contain the name, not the total number of
source occurrences. A definition that uses a name several times contributes
one. Counts include indexed definitions at the current Git commit plus modified
and untracked working-tree definitions, while excluding stale versions of dirty
or deleted files. Outside a Git repository, the command still returns the
syntactic survey and sets these counts to zero.

## Extraction rules

- Record function definitions that Tree-sitter recognizes. Do not report
  prototypes or other function declarations.
- Aggregate `call_expression` nodes by the exact normalized text of their
  callee expression. Preserve indirect forms such as
  `mapping->a_ops->read_folio`; do not label them as resolved callbacks.
- Record syntactic struct, union, enum, typedef, and type-identifier definitions.
- Aggregate syntactic type mentions by normalized source text.
- Omit basic scalar types according to the filtering rules below.
- Collapse whitespace in extracted names and expressions to a single space.
- Do not include function bodies, signatures, individual callsites, enclosing
  function relationships, or type-reference locations.
- Do not report resolved callees, linkage, callback registrations, or external
  definition locations. The caller/referencer counts are spelling-based
  database relationship matches, not Tree-sitter symbol resolution.
- Macro-generated or macro-hidden constructs may be absent. Parse failures must
  increment `parse_errors`; absence from this output is not proof that a symbol
  is absent from the program.

## Basic type filtering

`types_mentioned` excludes exact normalized spellings in these categories:

- C/C++ built-in scalar spellings and combinations: `void`, signed and
  unsigned `char`, `short`, `int`, `long`, and `long long`, `float`, `double`,
  `long double`, `_Bool`, `bool`, `wchar_t`, `char8_t`, `char16_t`, `char32_t`,
  and signed or unsigned `__int128`. This includes spellings such as
  `unsigned long long` and `signed short int`.
- Common fixed-width C and Linux integer aliases: `u8`/`s8` through
  `u128`/`s128`, their `__u32`/`__s32` forms, and `uint8_t`/`int8_t` through
  `uint128_t`/`int128_t`.
- Standard scalar C aliases: `size_t`, `ssize_t`, `off_t`, `loff_t`,
  `ptrdiff_t`, `intptr_t`, `uintptr_t`, `intmax_t`, and `uintmax_t`.
- Rust primitives: `()`, `!`, `bool`, `char`, `str`, all signed and unsigned
  integer primitives including `isize` and `usize`, and `f32`/`f64`.
- Python scalar annotations: `None`, `bool`, `int`, `float`, `complex`, `str`,
  `bytes`, and `bytearray`.

Filtering is exact after whitespace normalization. Domain-specific typedefs
such as `vm_fault_t`, `sector_t`, and named structs, unions, and enums remain in
the output.

## Limits

Result sections are not size-limited; the command returns every unique entry.
The retained `truncated` compatibility field is always false. The requested
file must be a supported C, C++, Rust, or Python source file within the
configured workspace.

This implementation requires no semcode database schema changes. Exact global
occurrence counts would require storing non-deduplicated relationship data (or
reparsing all source), because the existing `calls` and `types` arrays retain
unique names only.
