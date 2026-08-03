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

<optional shortest possible setup sentence>

<the smallest applicable non-prose descriptors carrying the problem and
mechanism; omit when no catalog form improves on one short sentence>

<optional shortest possible sentence describing why the descriptor matters>

Use the source-area prefix established by the validated evidence. The
subject describes the existing defect rather than an imperative patch
action and must fit on one 75-column line. Separate entries with a line
containing `---`, with one blank line on either side.

Indent descriptors and source excerpts by four spaces. Do not use
backticks, headings, free-form bullets, bold text, or fenced code blocks.
Every code excerpt must be copied verbatim from source, preceded by
filename:function, and include enough contiguous enclosing control flow to
locate the failing branch. Unrelated contiguous code may be replaced by a
standalone `[ ... ] // omitted: <reason>` marker only when at least two
consecutive lines are omitted. Never omit a single source line or use a
source-language comment as an omission marker. All retained source spelling,
capitalization, punctuation, and indentation must remain exact; the lowercase
subject rule does not apply to source. Never emit pseudocode in a bug summary.
End with a newline. Do not restore findings filtered out before rendering or
invent facts absent from the validated inputs.
