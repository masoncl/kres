=====================================================================
HARD RULE — 72 CHARACTER LINE LIMIT ON EVERY LINE OF PROSE

BEFORE YOU EMIT ANY LINE, COUNT ITS CHARACTERS.  IF THE COUNT IS GREATER
THAN 72, INSERT A NEWLINE AND WORD-WRAP BEFORE EMITTING.  THIS IS A HARD
LIMIT, NOT A SUGGESTION.  THE ONLY LINES ALLOWED TO EXCEED 72 CHARACTERS
ARE VERBATIM CODE FRAGMENTS QUOTED FROM SOURCE (function prototypes,
struct definitions, identifiers where breaking would change meaning).
EVERY PROSE LINE — FRAMING, SUBJECT:, QUESTIONS, CALL CHAINS,
OBSERVATIONS — WRAPS AT 72.  IF A Subject: LINE WOULD EXCEED 72
CHARACTERS, TIGHTEN THE WORDING UNTIL IT FITS; NEVER BREAK A Subject:
LINE ACROSS TWO LINES.
=====================================================================

Produce a bug report about existing code based on this template.

The inputs describe a research run: an optional original_prompt (the
top-level question that drove the run), a findings list sorted by
severity (most severe first), and a task_observations string — a
condensed, already-merged block of observations drawn from every
analysis task that contributed to a finding.  Your job is to turn
those inputs into a single, plain-text bug report covering every
bug that was found.  Treat the task_observations text as supporting
detail to fold into the relevant bug's section — quote from it when
it sharpens the question, do not attribute observations to tasks.

- If original_prompt is non-empty, open the report with one or two
sentences of plain-text context that restates what the run was looking
into, phrased as a lead-in to the sections that follow.  Do not quote the
original prompt verbatim and do not label the sentences as "original
prompt" or "context" — just make the first paragraph read naturally.
  - If original_prompt is empty, skip this framing entirely and start
    with the first bug section.

- The report must be in plain text only.  No markdown, no special characters,
absolutely and completely plain text fit for the linux kernel mailing list.
  - Do NOT wrap code snippets, examples, or anything else in triple-backtick
    (```) fences.  The examples in THIS template use ``` to delimit the
    avoid/use-instead samples for your benefit; your output must not contain
    any ``` at all.  Indent code with plain spaces if you need to set it
    apart — no fences, no inline backticks, no bold, no italics, no bullet
    markers in the report body.

- Never include bugs filtered out as false positives or as non-issues.

- Always end the report with a blank line.

- The report must be conversational with undramatic wording, fit for sending
as a bug report to the linux kernel mailing list.
  - Report must be factual.  just technical observations.
  - Report should be framed as questions, not accusations.
  - Call issues "bugs", never use the word critical.
  - NEVER EVER USE ALL CAPS.

- Explain the bugs as questions about the code, but do not mention
any specific author.
  - don't say: Did you corrupt memory here?
  - instead say: Can this corrupt memory? or Does this code ...

- Vary your question phrasing.  Don't start with "Does this code ..." every
time.

- Ask your question specifically about the sources you're referencing:
  - If the bug is a leak, don't call it a 'resource leak', ask specifically
    about the resource you think is leaking.  'Does this code leak the folio?'
  - Don't say: 'Does this loop have a bounds checking issue?' Name the
    variable you think is overflowing: "Does this code overflow xyz[]?"

- Do not add explanatory content about why something matters or what
benefits a fix would provide.  State the issue and the suggestion, nothing
more.

- Report only bugs.  Do not include general architectural observations,
"interesting" code, or background context.  If a finding does not describe
a bug that would cause incorrect behaviour, a crash, a leak, a race, a
memory safety issue, or similar, drop it.

- Each finding that describes a real bug must get its own section in the
output.

- Within a section, include every detail the research run learned about
that bug: the call chain that reaches it, the sequence of events that
triggers it, the resource or variable affected, the observable symptom, and
any pointers to related code.  If the findings.json captured a mechanism
detail, reproducer sketch, impact statement, or fix sketch, fold that
material into the prose without repeating it verbatim.

- If multiple findings describe the same underlying bug, merge them into
one section and cite every affected code site.

- Do not invent facts.  If the findings list and task_observations
do not support a claim, do not make it.  If a finding lacks the
detail you want to cite, drop that detail rather than guess.

## Ensure clear, concise paragraphs

Never make long or dense confusing paragraphs, ask short questions backed
up by code snippets (in plain text), or call chains if needed.

The examples below use a fictional `drivers/example/widget.c` so the
format is clear without tying the sample to any real bug.

### AVOID
```
Can this sequence actually occur?  Looking at widget_claim() in
drivers/example/widget.c, if CPU1 already called widget_release() which
sets w->owner = NULL, wouldn't CPU2 check owner, see it is NULL, take
the 'already released' path with mutex_unlock/put_widget/goto retry
instead of calling widget_release() again?
```

### USE INSTEAD
```
Can this sequence actually occur?  Looking at widget_claim() in
drivers/example/widget.c, if CPU1 already called widget_release() and set
w->owner = NULL:

CPU1
widget_release()
   w->owner = NULL;

CPU2 would see this in widget_claim():
    if (!w->owner) {
        pr_debug("widget %p already released\n", w);
        mutex_unlock(&w->lock);
        put_widget(w);
        ...
        goto retry;
    }

and take the goto retry path instead of calling widget_release() again?
```

Dense paragraphs are hard to read.  Spread the information out so
it is easier to follow.

If you have a series of factual sentences, break them up into logical
groups with a blank line between each group.

If you have a series of statements followed by a question, put a blank
line before the question.

## NEVER EVER ALL CAPS

The only time it is acceptable to use ALL CAPS in the report is when you're
directly quoting code that happens to contain it.

### AVOID
```
WIDGET-1: INCORRECT LOCK ORDERING IN widget_destroy
```

### USE INSTEAD
```
The ->slab_lock / ->ref_lock ordering in widget_destroy differs from
the nesting used elsewhere in drivers/example/widget.c:
```

## Don't over explain

Some bugs are extremely nuanced, and require a lot of details to explain.

Some bugs are just completely obvious, especially cutting-and-pasting
errors, or places where the code clearly missed an update.  If you expect
a reasonable maintainer to understand a short explanation, use a short
explanation.

## NEVER QUOTE LINE NUMBERS

- Never mention line numbers when referencing code locations.  Use the
function name, and a call chain if that makes the reference clearer.
  - The line numbers present in the findings are unique to the code base
    setup for this research run.  Your audience doesn't know exactly what
    tree you're reading, so line numbers are meaningless to them.
  - YOU MUST NOT REFERENCE LINE NUMBERS IN THIS REPORT.
  - Instead, use small code snippets any time you feel the urge to say a
    line number out loud.

### AVOID
```
While this path is rare because widget_cache_lookup() is called earlier in
widget_submit(), the entry can disappear if the LRU evicts it (see line
427 in widget_cache_lookup) or if the initial store failed (see lines
503-506 in widget_cache_lookup).
```

### USE INSTEAD
```
While this path is rare because widget_cache_lookup() is called earlier in
widget_submit(), the entry can disappear if the LRU evicts it:

drivers/example/widget_cache.c:widget_cache_lookup() {
    ...
    ent = cache_search(cache, id);
    if (ent) {
        if (ent->generation < cache->gen && ent->needs_refresh) {
            lru_cache_remove(&cache->lru, &ent->node);
            ent = NULL;
    ...
}

It can also happen if the initial store failed:

drivers/example/widget_cache.c:widget_cache_lookup() {
    ...
    ret = lru_cache_store(&cache->lru, &ent->node, GFP_KERNEL);
    if (ret < 0) {
        kfree(ent);
        return ret;
    }
    ...
}
```

### CRITICIAL! DON'T USE (```) backticks
You've just read an example in markdown where backticks were used.  You're
going to want to copy those backticks into the .txt file you're generating.
YOU ABSOLUTELY MUST NEVER USE (```) BACKTICKS IN THE .TXT FILE.

Kernel developers work in C, not in markdown.  backticks are markdown.
DO NOT, FOR ANY REASON, PUT MARKDOWN IN THIS FILE

## Structure

The report is a flat sequence of per-bug sections.  Do not emit any
top-level wrapper, preamble, markdown headings, or closing summary.

Each section must cover, in order:

1. A line of the exact form `Subject: <short summary of the bug>`, on its
   own line.  The prefix `Subject: ` is literal, the summary is one clause
   naming the affected function and the nature of the bug (e.g. "Subject:
   widget_destroy acquires ref_lock in the wrong order").  No ALL CAPS,
   no trailing period, no numbering.  72 character max, including the Subject:
2. A `bug-severity:` line, yaml-style, with exactly one value from the
   closed set `[high, medium, low, latent, unknown]`.  The value MUST be
   lower case — no ALL CAPS — with no surrounding quotes, brackets, or
   trailing punctuation.  Pick `unknown` only when the findings truly
   do not support any of the other four.  `latent` is for bugs that
   need an unusual path or config to trigger.
3. A `bug-impact:` line, yaml-style, whose value is a single sentence
   of free-form plain text stating the user-visible or kernel-state
   consequence (e.g. "kernel oops when the widget is released
   concurrently with destroy").  The whole line — key, colon, value —
   MUST fit on ONE line and MUST be 72 characters or fewer.  Never
   wrap a `bug-impact:` line.  If the sentence would not fit, tighten
   the wording.
4. A blank line.
5. A concise question or statement of the bug, phrased as a question where
   possible.
6. Any code snippets needed to make the question concrete.  Use the same
   snippet style shown above: filename:function() { ... } with the smallest
   excerpt that makes the point.  Plain indentation, no ``` fences.
7. The call chain, if relevant.  Write it inline as funcA() -> funcB() ->
   funcC().
8. Any additional observations about the triggering sequence, affected
   resource, or observable symptom.  Keep each paragraph short.

Separate sections with a single blank line.  Do not number the sections.

Order the sections by bug-severity, most severe first: every `high`
section comes before every `medium`, every `medium` before every `low`,
every `low` before every `latent`, and every `latent` before every
`unknown`.  Within a single severity, preserve the order the findings
appear in the input (do not re-sort alphabetically or by function name).
Do not add severity headings, dividers, or any other framing between
groups — the bug-severity line inside each section is the only marker.

Sample section for reference (the outer ``` fences are only here to mark
the sample boundary — do NOT include them in your output).  The code is
fictional and exists only to show the shape.

```
Subject: widget_destroy acquires slab_lock and ref_lock in the wrong order
bug-severity: high
bug-impact: deadlock between widget teardown and reinit on SMP systems

Can this sequence deadlock against a concurrent widget_reinit()?  In
drivers/example/widget.c:widget_destroy(), the cleanup path takes the
locks in this order:

drivers/example/widget.c:widget_destroy() {
    ...
    raw_spin_lock_irq(&w->ref_lock);
    raw_spin_lock(&w->slab_lock);
    ...
}

while every other site in drivers/example/widget.c takes them in the
opposite order:

drivers/example/widget.c:widget_reinit() {
    ...
    raw_spin_lock(&w->slab_lock);
    raw_spin_lock(&w->ref_lock);
    ...
}

Call chain reaching the bad ordering: module_exit() -> widget_teardown()
-> widget_destroy().

Does lockdep complain about this when CONFIG_PROVE_LOCKING is enabled?
```
