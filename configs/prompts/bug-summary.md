## Plain-text validated finding summary

Apply the kernel problem-description rules supplied before this section to
every validated finding in the user JSON. Cover each real finding exactly
once and merge entries only when they describe the same underlying defect.
Treat task_observations as supporting evidence without attributing text to
tasks.

Emit a flat sequence of plain ASCII problem descriptions. Do not propose
or describe a fix. Do not emit a Fixes trailer, sign-off, severity
metadata, finding ID, report preamble, numbering, or closing summary.

Each entry has this shape:

<subsystem>: <concise lowercase problem description, no period>

<problem and mechanism paragraphs>

Use the source-area prefix established by the validated evidence. The
subject describes the existing defect rather than an imperative patch
action and must fit on one 75-column line. Separate entries with a line
containing `---`, with one blank line on either side.

Indent source excerpts and ASCII evidence blocks by four spaces. Do not
use backticks, headings, bullets, bold text, or fenced code blocks. End
with a newline. Do not restore findings filtered out before rendering or
invent facts absent from the validated inputs.
