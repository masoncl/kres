#!/usr/bin/env python3
"""Compare two kres runs at matched progress points.

Built for aug6-5 (file survey present, findings carry source bodies)
vs aug6-6 (file survey removed, `previous_findings` stripped of
source, lens fan-out degrades instead of failing). The runs differ in
three ways at once, so treat per-metric attribution with care — the
one clean single-variable measurement is `previous_findings` bytes at
a matched findings count, which only the stripping affects.

Matching is by turn number, not wall-clock: the runs were launched
hours apart against a shared API key, so elapsed time is contaminated.

Usage: compare-runs.py <logdir_a> <resultsdir_a> <logdir_b> <resultsdir_b>
"""
import json
import re
import sys
from collections import Counter
from datetime import datetime


def ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def console(logdir):
    for line in open(f"{logdir}/console.jsonl"):
        r = json.loads(line)
        yield ts(r["timestamp"]), r["line"]


def timeline(logdir):
    """(turn, findings_total) as the run progressed.

    "Turn" is the CHRONOLOGICAL index of completed tasks, not the
    `#N` in the console line — that is a task id, and tasks finish out
    of order, so matching on it compares unrelated points in the two
    runs.
    """
    turns, findings = [], []
    for t, line in console(logdir):
        if re.search(r"== done #\d+", line):
            turns.append((t, len(turns) + 1))
        m = re.search(r"\[findings\] (\d+) total", line)
        if m:
            findings.append((t, int(m.group(1))))
    return turns, findings


def findings_at_turn(turns, findings, n):
    """Findings recorded at or before the moment turn n completed."""
    at = [t for t, k in turns if k == n]
    if not at:
        return None
    cutoff = at[0]
    seen = [v for t, v in findings if t <= cutoff]
    return max(seen) if seen else 0


def history_bytes(logdir):
    """previous_findings size against how many findings it carried."""
    dec = json.JSONDecoder()
    out = []
    for line in open(f"{logdir}/code.jsonl"):
        r = json.loads(line)
        if r.get("role") != "user" or "slow-lens" not in (r.get("label") or ""):
            continue
        try:
            v, _ = dec.raw_decode(r.get("content") or "", 0)
        except ValueError:
            continue
        pf = v.get("previous_findings")
        if pf:
            out.append((len(pf), len(json.dumps(pf))))
    return out


def gather_rounds(logdir):
    """Fast-gather calls per task — a proxy for re-fetching evidence."""
    per_task = Counter()
    for line in open(f"{logdir}/code.jsonl"):
        r = json.loads(line)
        if r.get("role") != "assistant":
            continue
        lab = r.get("label") or ""
        if not lab.startswith("phase=fast-gather"):
            continue
        m = re.search(r"task=(.*)$", lab)
        if m:
            per_task[m.group(1)] += 1
    return per_task


def report(name, logdir, resultsdir):
    turns, findings = timeline(logdir)
    done = max((k for _, k in turns), default=0)
    try:
        d = json.load(open(f"{resultsdir}/findings.json"))
        fs = d.get("findings") if isinstance(d, dict) else d
    except Exception:
        fs = []
    sev = Counter(f.get("severity") for f in fs)
    hist = history_bytes(logdir)
    gr = gather_rounds(logdir)
    print(f"\n=== {name}")
    print(f"  turns completed : {done}")
    print(f"  findings now    : {len(fs)}  {dict(sev)}")
    if hist:
        big = max(hist, key=lambda x: x[1])
        print(
            f"  previous_findings: {len(hist)} lens request(s) carried it; "
            f"largest {big[1]/1024:.0f} KB at {big[0]} finding(s) "
            f"= {big[1]/max(big[0],1)/1024:.1f} KB/finding"
        )
    else:
        print("  previous_findings: none carried yet")
    if gr:
        print(
            f"  fast-gather      : {sum(gr.values())} call(s) over {len(gr)} task(s) "
            f"= {sum(gr.values())/len(gr):.2f} per task"
        )
    return turns, findings, done


if __name__ == "__main__":
    la, ra, lb, rb = sys.argv[1:5]
    ta, fa, da = report(ra, la, ra)
    tb, fb, db = report(rb, lb, rb)
    common = min(da, db)
    if common:
        print(f"\n=== findings at matched turns (up to turn {common})")
        print(f"  {'turn':>5} {ra:>14} {rb:>14}")
        step = max(1, common // 10)
        for n in list(range(step, common + 1, step)):
            print(
                f"  {n:>5} {str(findings_at_turn(ta, fa, n)):>14} "
                f"{str(findings_at_turn(tb, fb, n)):>14}"
            )
    else:
        print("\n(no completed turns in common yet)")
