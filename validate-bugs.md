# Validation / summary pipeline defects

Found 2026-08-04 while converting a kres review run into a mailing-list
review. Four separate defects, three structural and one prompt-level.

Evidence lives in review worktrees under `~/working/review/linux.<sha>/`.
Those are transient — copy anything needed before they are recycled.

Primary example throughout: run `linux.7e321d857b5b`
(`io_uring/rsrc: add regbuf import flags`), finding id
`regbuf_import_flags_unimplemented_claim`.

---

## 1. Finding records drift across fields; id, title and summary can name different bugs

`findings.json` entry `regbuf_import_flags_unimplemented_claim` holds three
unrelated defects in one record:

| field | bug it describes |
|---|---|
| `id` | commit message overclaims what `import_flags` does |
| `title` | KBUF imu is cloneable into another ring (`IO_REGBUF_F_UNCLONEABLE` never set) |
| `summary` | `1 << imu->folio_shift` int-literal shift overflow in `io_vec_fill_bvec()` |

The id is the original finding. `title` and `summary` were each overwritten by
later delta-applies without re-keying the record or invalidating it.

Second instance, independently observed the same day: run `linux.e6d10832ff8b`,
finding `dmabuf_sgl_pool_object_overflow`. Its `title` was revised to a UAF
claim ("dereferences exporter-owned map->sgt from lockless submission
context") while `impact` kept the superseded text describing an out-of-bounds
write past a 4096-byte dma_pool object. Title and impact describe different
bugs.

Reproduce:

    python3 -c "
    import json
    d=json.load(open('kres-<sha>/findings.json'))
    fl=d if isinstance(d,list) else d.get('findings',[])
    for x in fl: print(x['id']); print(' T:',x.get('title')); print(' S:',str(x.get('summary'))[:200])
    "

Fix direction: a delta-apply that rewrites `title` must either rewrite the
whole record coherently or split off a new finding with its own id. A record
whose fields disagree cannot be validated coherently — see defect 4, where the
validator was asked to assess exactly this record.

Note the validator *did* detect this drift and repaired its exported
`FINDING.md`, but the canonical `findings.json` was left inconsistent. The
repair path only touches the export.

## 2. Validation verdict is prose-only; no structured field carries it

`summary-validation/findings/<id>/summary.md` contains:

    # Status

    Plausible

`summary-validation/findings/<id>/metadata.yaml` contains `status: active` and
`validation_run: true` — and no verdict field at all. So "Plausible" exists
only as a markdown heading in free text.

This is the failure mode AGENTS.md warns about from the other direction: the
rule says Rust must not classify AI prose, but here the only place the verdict
exists *is* prose, so no consumer can act on it without classifying prose.

Fix direction: the validate workflow should emit a typed verdict
(`confirmed` / `plausible` / `refuted`) into `metadata.yaml` and into the
findings record, and downstream consumers should read that field. Prose stays
for humans.

## 3. `/summary` render drops hedges and scope caveats

The validator's `summary.md` for `regbuf_import_flags_unimplemented_claim`
ends its Impact section with:

> This is a real defect in the current tree; it is simply not introduced by,
> or materially connected to, the commit this finding is filed against.

and carries `Status: Plausible`.

Neither survived into `kres-<sha>/summary.txt`:

    grep -icE 'plausible|pre-existing|predates|not introduced|latent' summary.txt
    0

The rendered summary states the bug as fact, attributes it to the reviewed
commit by placement, and says nothing about it predating that commit. A human
reading only `summary.txt` — which is the intended consumer — gets a
confident, misattributed report.

Fix direction: the summary template must carry the verdict and the
attribution note through to the rendered output. If a finding is not
attributable to the reviewed commit, that belongs in the rendered summary, not
just in the validation artifact.

## 4. Validator asserted a conclusion it had logged as untraced

Same finding. `summary.md` Impact states:

> This is a real use-after-free window, not merely an indefinite pin: the
> unregister-then-complete sequence in ublk_drv.c happens synchronously in
> the same request-completion path, with no wait for `imu->release(priv)`.

`metadata.yaml` `open_questions` for the same finding states:

> Whether io_free_imu()/imu->release(priv) callback ordering interacts with
> the described race in a way that shortens or lengthens the UAF window was
> not traced in this validation pass.

The untraced item decides the verdict. The validator considered the correct
answer ("indefinite pin"), rejected it, and recorded the reason it could not
have known as an open question.

The actual behaviour, traced in `linux.7e321d857b5b`:

- `io_uring/rsrc.c:io_buffer_unmap()` returns before `imu->release()` when
  `refcount_read(&imu->refs) > 1`, so `ublk_io_release()` never runs and
  `io->task_registered_buffers` stays 1.
- `drivers/block/ublk_drv.c` `UBLK_IO_COMMIT_AND_FETCH_REQ` does not call
  `__ublk_complete_rq()` unconditionally. There is a gate the validator read
  past:

      if (buf_idx != UBLK_INVALID_BUF_IDX)
              io_buffer_unregister_bvec(cmd, buf_idx, issue_flags);
      compl = ublk_need_complete_req(ub, io);
      ...
      if (compl)
              __ublk_complete_rq(req, io, ublk_dev_need_map_io(ub), NULL);

- `ublk_need_complete_req()` -> `ublk_sub_req_ref()` computes
  `sub_refs = UBLK_REFCOUNT_INIT - io->task_registered_buffers`. With
  `task_registered_buffers` still 1 and `io->ref` still `UBLK_REFCOUNT_INIT`,
  `refcount_sub_and_test()` leaves 1 and returns false.
- Registration is gated on `ublk_dev_support_zero_copy()`, which also makes
  `ublk_dev_need_req_ref()` true, so that path is always taken for these
  buffers.

Result: the request is not completed. No use-after-free. What remains is a
deferred completion — `io_buffer_unregister_bvec()` returns success while the
buffer is still live in the cloning ring.

Fix direction (prompt, not code): a validator must not state a consequence as
fact when its own open_questions list the step that would establish it.
Either trace it or downgrade the claim. Generic invariant, no bug-class
specifics: if the finding's harm depends on an ordering, refcount, lock, or
caller-supplied value, that guard must be read before the harm is asserted;
otherwise the finding is reported as mechanism-only.

## 5. summary.txt attributes another commit's code to the reviewed commit

Run `linux.b32dd786296e` ("block: add dma-buf support for raw bdev", touching
only `block/fops.c` and `include/linux/blkdev.h`).

Its `summary.txt` introduces a block of twelve findings with the claim that
this commit "adds dma_buf_io_ctx / dma_buf_io_map core code". It does not.
`drivers/dma-buf/dma-buf-io.c` has exactly one commit touching it,
`ff5dccf71e8c`, two commits earlier in the same series — confirmed with
`git log --oneline -- drivers/dma-buf/dma-buf-io.c` and
`git merge-base --is-ancestor ff5dccf71e8c b32dd786296e^`.

This is worse than the dropped-caveat problem in defect 3. There the
attribution note existed in the validation artifact and was lost in the
render. Here the rendered summary makes a positive false claim about what the
commit contains, which a reader cannot detect without running blame
themselves.

Related, same run: kres filed the `-EINVAL` vs `-EOPNOTSUPP` mismatch in
`blkdev_init_dma_buf_io_ctx()` under its latent bucket. That whole function is
new in this commit, so it is introduced. Misclassification in both directions
inside one summary.

Fix direction: the summary render should derive the "introduced by" claim from
`git blame`/`git log` on the cited files rather than from model prose, and
should refuse to state that a commit adds a file the commit does not touch.
This is checkable mechanically: cross the finding's cited filenames against
`git show --name-only <sha>` before rendering any attribution sentence.

## 6. A finding can be incomplete about its own chain

Same run. kres's ctx use-after-free finding tied the mechanism to the
zero-timeout `dma_resv_wait_timeout(...) < 0` recheck bug. Verification in
`linux.b32dd786296e` surfaced a second, unstated mechanism:
`dma_resv_add_fence()` replaces a fence with the same context, and every map
of a ctx shares `ctx->fence_ctx`, so a newer map's fence evicts an older
still-unsignalled one.

**Correction, from a later pass in `linux.ff5dccf71e8c`:** I first recorded
this as kres naming the wrong mechanism, with the UAF independent of the
recheck bug. That is not right. `dma_buf_io_map_release_work()` does
`refcount_inc(&ctx->refs)` *before* `dma_fence_signal()`, so any map whose
fence has signalled already holds a ctx reference. Two unsignalled
same-context fences can only coexist if the recheck lets a new map be built
while a teardown is still pending. So the recheck is a genuine precondition
and kres's chain was incomplete, not wrong: fence replacement is the
mechanism, the recheck is what makes it reachable.

The lesson survives the correction, in weaker form. A finding can state a
chain that is missing a link, and a maintainer who probes the stated link will
form the wrong impression of the whole finding. It is invisible to any check
that only asks "is the conclusion true".

Fix direction: nothing mechanical. Worth noting in the validate prompt that
confirming a conclusion is not confirming the stated chain, and that the chain
is what gets reviewed on a mailing list.

## 7. /summary silently drops most active findings, including highs

Run `linux.9e3dd1b2d71d` ("nvme-pci: implement dma-buf backed requests").

    findings.json:  53 findings, 43 active — 14 high, 20 medium, 9 low
    summary.txt:    11 sections

So 32 active findings never reached the rendered summary, and the drop is not
severity-ordered: at least three active *high* findings were dropped
(`nvme_dmabuf_page_granular_sync_race`, `nvme_dmabuf_req_index_unbounded`,
`nvme_dmabuf_stale_prp2_free`), while lower-severity items were kept.

Nothing in `summary.txt` or `summary-validation/` records that a drop
happened or why. A reader of `summary.txt` cannot tell whether the run found
11 things or 43. Downstream consumers that treat `summary.txt` as the run's
output — including the review-conversion procedure in
`kres-review-inline-template.md`, whose step 1 says to work only what
summary.txt kept — inherit the loss silently.

At least one drop looks correct on inspection
(`nvme_dmabuf_map_publish_before_init` is a false positive:
`dma_buf_io_init_map()` publishes nothing, `ctx->map` is assigned by
`dma_buf_io_create_map()` after `->map()` returns). So the filter is doing
real work. The defect is that it does it invisibly.

Fix direction: `/summary` should emit a manifest of every finding it
considered with a kept/dropped flag and a typed reason, either in
`summary.txt` or beside it in `summary-validation/`. Without that, "not in
summary.txt" is indistinguishable from "never found".

## 8. A relative `code_output` path aborts the whole summary run

Run `linux.ff507b489552`. `/summary` has now failed twice and never produced a
`summary.txt`. The second failure is fully diagnosed.

### What happens

The validate workflow writes each finding's triage output via `code_output`
with a model-supplied `path`. For 19 of 20 findings in this run the path was
absolute:

    /home/clm/working/review/linux.ff507b489552/kres-ff507b489552/
      summary-validation/findings/<id>/summary.md

For `dmabuf_dma_list_index_unbounded` it was relative:

    review/linux.ff507b489552/kres-ff507b489552/
      summary-validation/findings/dmabuf_dma_list_index_unbounded/summary.md

kres resolved that against the process cwd — the worktree root
`/home/clm/working/review/linux.ff507b489552` — and created a nested duplicate
tree at `<worktree>/review/linux.ff507b489552/kres-.../`. All three files
(`FINDING.md`, `metadata.yaml`, `summary.md`) landed there, complete and
well-formed.

Nothing in the workflow noticed. That finding's `workflow-validate.json` shows
both steps `done`, with `summary_written: true`, `severity_written: true`,
`verdict: Unconfirmed`, `severity: medium`. From the workflow's point of view
the write succeeded, because it did.

### Why one bad path loses all twenty

`kres-repl/src/summary.rs:validate_summary_finding()`:

    let validated_summary = std::fs::read_to_string(job.exported.dir.join("summary.md"))
        .with_context(|| format!("reading validated summary for {}", job.exported.id))?;

The `?` propagates out through `run_bounded_ordered` to `cmd_summary`, which
prints `/summary: validation failed: {error:#}` and returns. So a single
unreadable `summary.md` discards the other nineteen completed validations and
produces no output at all. The error does name the finding, but only on the
terminal — nothing is written to disk, and the summary phase produces no JSONL
logs, so after the fact there is no record of what went wrong.

### Reruns cannot recover, and cannot be hand-repaired around

`validate_findings_for_summary()` opens with:

    if inputs.validation_dir.exists() {
        std::fs::remove_dir_all(&inputs.validation_dir)...

so every `/summary` deletes `summary-validation/` and re-validates all
findings from scratch. `workflow-state/*/workflow-validate.json` is written
but never read back as input — there is no resume. Moving the misplaced files
into the canonical location does not help: the next run wipes them.

At `SUMMARY_VALIDATION_CONCURRENCY = 20` a rerun is a full re-validation of
every finding (about 11 minutes for this run, 17:30:56 to 17:41:42) and
re-rolls the same dice. Any one of 20 findings emitting a relative path fails
the whole thing again.

That concurrency value also explains the *first* failure's shape: all 20 run
at once, so when that process stopped, the 6 findings then in flight were all
left at `validate-reachability: pending` together. Run 2 overwrote every
`workflow-state` file, so run 1's cause is no longer inspectable and may or
may not have been this same bug.

### Fix directions

1. Resolve a relative `code_output` path against the results/validation
   directory, not the process cwd — or reject any path that escapes the
   expected tree. A model-supplied path landing outside it should fail loudly
   rather than silently create a parallel directory tree.
2. Make the `summary.md` read non-fatal: skip that finding, log it, render the
   other nineteen. A 95% summary beats a total loss.
3. Consider making validation resumable, or at least not deleting prior
   artifacts until the new run succeeds. Today a failed rerun destroys the
   evidence from the previous one.

### Correction to defect 7

Defect 7 above attributes `9e3dd1b2d71d`'s 43-active-to-11-section drop to a
silent filter in this code path. That is wrong: as shown here,
`validate_findings_for_summary()` either returns every validated finding or
fails outright — it cannot silently drop any. The drop therefore happens
downstream in `run_summary()`'s rendering, which I have not read. The observed
numbers in defect 7 stand; the located cause does not.

## 9. Note on this document's own provenance

Two facts in earlier revisions of this file, and two facts I handed to
verification passes, were wrong in the same way: true at one commit in the
series, asserted at another. The `bio_iov_vecs_to_alloc()` short-circuit is
real at `9e3dd1b2d71d` and does not exist at `c1ab6c0db385`. The fence-chain
claim above held until the code one commit earlier was read.

That is the same defect as #5, committed by hand rather than by the pipeline.
Any fix that makes kres check attribution mechanically should also apply to
notes carried between worktrees.

---

## Cross-cutting

Defects 1 and 4 compound. The validator was handed a record whose id, title,
and summary named three different bugs, and produced a verdict of "Plausible"
— which reads less like calibrated uncertainty than like the predictable
result of assessing an incoherent record. Fixing 1 may improve 4 on its own.

Defects 2 and 3 are the reason none of this was visible downstream. The
validator's caution and its scope correction were both present and both
discarded before reaching the only file a human reads.
