#!/usr/bin/env python3
"""Concurrency and reaper metrics for one kres run's code.jsonl.

Two independent measures, both computed the same way for every run so
comparisons are valid. Neither reproduces the exact numbers quoted in
todo-updates.md for kres-aug6-2 — that method was not recorded in
enough detail to reconstruct — so compare runs measured by THIS
script, not against those figures.

  in-flight: union of model-call intervals. A call is a user record
    paired with the next assistant record carrying the same label.
    Unambiguous, but reads a task as idle while its main agent is
    running tools between calls.

  task-side in-flight: the same, restricted to the phases that run
    inside a task (gather, lens, probe, fetch, consolidate). This is
    the one that answers "are the slots full?", because it excludes
    the reaper-side calls (promote, todo, goal) that keep the
    all-calls measure busy precisely while every task is stalled
    waiting for the drain.

Task-span concurrency is deliberately NOT measured here. Records only
appear at call start and end, so a slow lens leaves a multi-minute
hole in its task's record stream that is indistinguishable from the
task being finished.

Usage: measure-run.py <logdir> [<logdir> ...]
"""
import json
import re
import sys
from collections import defaultdict
from datetime import datetime

# Phases that run inside a task, as opposed to in the reaper.
TASK_SIDE = {
    "phase=fast-gather",
    "phase=slow-lens",
    "phase=slow",
    "phase=cache-probe",
    "phase=fetch",
    "phase=consolidate",
}


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def phase_of(label):
    return re.sub(r"\s*task=.*", "", label or "")


def task_of(label):
    m = re.search(r"task=(.*)$", label or "")
    return m.group(1).strip() if m else None


def load(logdir):
    records = []
    for line in open(f"{logdir}/code.jsonl"):
        r = json.loads(line)
        records.append((parse_ts(r["timestamp"]), r))
    records.sort(key=lambda x: x[0])
    return records


def call_intervals(records, phases=None):
    """User record -> next assistant record with the same label."""
    pending = {}
    out = []
    for ts, r in records:
        label = r.get("label") or ""
        if phases is not None and phase_of(label) not in phases:
            continue
        if r.get("role") == "user":
            pending.setdefault(label, []).append(ts)
        elif r.get("role") == "assistant":
            starts = pending.get(label)
            if starts:
                out.append((starts.pop(0), ts))
    return out


def sweep(spans, lo, hi):
    events = sorted([(s, 1) for s, _ in spans] + [(e, -1) for _, e in spans])
    live, prev = 0, lo
    at = defaultdict(float)
    for ts, delta in events:
        at[live] += (ts - prev).total_seconds()
        prev, live = ts, live + delta
    at[live] += (hi - prev).total_seconds()
    return at


def show(title, at, total):
    weighted = sum(n * s for n, s in at.items())
    idle = at.get(0, 0.0)
    print(f"  {title}: mean {weighted/total:.2f}, peak {max(at)}, "
          f"idle {idle/60:.1f} min ({idle/total*100:.1f}%)")
    for n in sorted(at):
        if at[n] / total >= 0.005:
            print(f"      {n:3d} concurrent | {at[n]/60:6.1f} min | {at[n]/total*100:5.1f}%")


CONSOLE_PATTERNS = [
    ("dispatch", r"^\[dispatch"),
    ("post-reap skipped", r"^\[dispatch post-reap\] skipped"),
    ("todo update", r"^\[todo update\] before"),
    ("goal check", r"^\[goal check\] met="),
    ("goal met", r"^\[goal met"),
    ("prioritize", r"^\[prioritize\]"),
    ("promote", r"^\[promote\] \d+ prose"),
    ("rate limit", r"rate-limit"),
]


def console_summary(logdir):
    """Scheduling narrative, from the console transcript."""
    path = f"{logdir}/console.jsonl"
    try:
        lines = [json.loads(l)["line"] for l in open(path)]
    except FileNotFoundError:
        print("  console.jsonl: absent (run predates the transcript tee)")
        return
    print(f"  console.jsonl: {len(lines)} line(s)")
    for name, pat in CONSOLE_PATTERNS:
        hits = [l for l in lines if re.search(pat, l)]
        if hits:
            print(f"      {len(hits):5d}  {name}")
    # Batch sizes are the whole point of the reap-batch rework.
    sizes = [
        int(m.group(1))
        for l in lines
        if (m := re.search(r"(\d+) completed task\(s\)", l))
    ]
    if sizes:
        print(f"      reap batch sizes: {sizes} (mean {sum(sizes)/len(sizes):.1f})")
    starts = [
        int(m.group(1))
        for l in lines
        if (m := re.search(r"starting (\d+) task\(s\)", l))
    ]
    if starts:
        print(f"      dispatch sizes:   {starts} (total {sum(starts)})")


def report(logdir):
    records = load(logdir)
    if not records:
        print(f"\n=== {logdir} ===\n  (no records yet)")
        return
    lo, hi = records[0][0], records[-1][0]
    total = (hi - lo).total_seconds()
    print(f"\n=== {logdir} ===")
    print(f"  wall {total/60:.1f} min, {len(records)} records")
    show("all calls    ", sweep(call_intervals(records), lo, hi), total)
    show("task-side    ", sweep(call_intervals(records, TASK_SIDE), lo, hi), total)

    phases = defaultdict(int)
    usage = defaultdict(int)
    for _, r in records:
        if r.get("role") != "assistant":
            continue
        phases[phase_of(r.get("label"))] += 1
        for k, v in (r.get("usage") or {}).items():
            usage[k] += v or 0
    print("  assistant calls by phase:")
    for k, v in sorted(phases.items(), key=lambda kv: -kv[1]):
        print(f"      {v:5d}  {k}")
    if usage:
        print("  usage:", {k: v for k, v in sorted(usage.items())})
    console_summary(logdir)


if __name__ == "__main__":
    for d in sys.argv[1:]:
        report(d)
