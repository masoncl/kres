## Markdown validated finding summary

Apply the kernel problem-description rules supplied before this section to
every validated finding in the user JSON. Cover each real finding exactly
once and merge entries only when they describe the same underlying defect.
Treat task_observations as supporting evidence without attributing text to
tasks.

Emit a flat sequence of Markdown problem descriptions. Do not propose or
describe a fix. Do not emit a Fixes trailer, sign-off, severity metadata,
finding ID, report preamble, numbering, or closing summary.

Each entry has this shape:

## <subsystem>: <concise lowercase problem description, no period>

<problem and mechanism paragraphs>

Use the source-area prefix established by the validated evidence. The
heading describes the existing defect rather than an imperative patch
action. Keep its text, excluding the leading `## `, within 75 columns.

Use inline backticks for identifiers and fenced blocks for source excerpts
or ASCII diagrams. For this Markdown output, that fenced-block rule overrides
the shared plain-text backtrace formatting sentence. Separate sections with
one blank line and end with a newline. Do not restore findings filtered out
before rendering or invent facts absent from the validated inputs.
