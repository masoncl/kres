#!/usr/bin/env python3
"""Per-phase prompt-cache accounting for one kres run.

Reads both code.jsonl and main.jsonl. For every assistant record the
provider reports input / output / cache_creation / cache_read; this
groups them by phase so a phase that WRITES cache nobody reads is
visible as such. That failure mode is the one the tree has hit
repeatedly — 6328c9f removed it from the single-lens probe, 4692adc
from the todo/goal prefixes, a7bbdbf from the prioritizer.

read/write below 1.0 means the phase spent more on populating the
cache than it recovered. For a phase that runs once per session that
can be correct (someone else reads it); for a repeating phase it is a
straight loss.

Usage: cache-audit.py <logdir> [<logdir> ...]
"""
import json
import re
import sys
from collections import defaultdict


def phase_of(label):
    return re.sub(r"\s*task=.*", "", label or "") or "(unlabelled)"


def collect(logdir):
    per_phase = defaultdict(lambda: defaultdict(int))
    for name in ("code.jsonl", "main.jsonl"):
        try:
            fh = open(f"{logdir}/{name}")
        except FileNotFoundError:
            continue
        for line in fh:
            r = json.loads(line)
            if r.get("role") != "assistant":
                continue
            usage = r.get("usage") or {}
            if not usage:
                continue
            key = per_phase[phase_of(r.get("label"))]
            key["calls"] += 1
            for field in ("input", "output", "cache_creation", "cache_read"):
                key[field] += usage.get(field) or 0
    return per_phase


def human(n):
    if n >= 1_000_000:
        return f"{n/1_000_000:.2f}M"
    if n >= 1_000:
        return f"{n/1_000:.1f}k"
    return str(n)


def report(logdir):
    per_phase = collect(logdir)
    if not per_phase:
        print(f"\n=== {logdir} ===\n  (no usage records)")
        return
    print(f"\n=== {logdir} ===")
    header = f"  {'phase':28} {'calls':>5} {'input':>8} {'output':>8} {'cr_write':>9} {'cr_read':>9} {'r/w':>6}"
    print(header)
    print("  " + "-" * (len(header) - 2))
    totals = defaultdict(int)
    for phase, u in sorted(per_phase.items(), key=lambda kv: -kv[1]["cache_creation"]):
        for k, v in u.items():
            totals[k] += v
        ratio = u["cache_read"] / u["cache_creation"] if u["cache_creation"] else None
        flag = ""
        if u["cache_creation"] > 50_000 and (ratio is None or ratio < 1.0):
            flag = "  <- writes more than it reads"
        print(
            f"  {phase:28} {u['calls']:>5} {human(u['input']):>8} {human(u['output']):>8} "
            f"{human(u['cache_creation']):>9} {human(u['cache_read']):>9} "
            f"{(f'{ratio:.2f}' if ratio is not None else '  -'):>6}{flag}"
        )
    print("  " + "-" * (len(header) - 2))
    ratio = totals["cache_read"] / totals["cache_creation"] if totals["cache_creation"] else 0
    print(
        f"  {'TOTAL':28} {totals['calls']:>5} {human(totals['input']):>8} "
        f"{human(totals['output']):>8} {human(totals['cache_creation']):>9} "
        f"{human(totals['cache_read']):>9} {ratio:>6.2f}"
    )
    # Uncached input is what the cache never got a chance at.
    billed = totals["input"] + totals["cache_creation"] + totals["cache_read"]
    if billed:
        print(
            f"  cached share of billed input: "
            f"{(totals['cache_creation']+totals['cache_read'])/billed*100:.1f}%"
            f"  (uncached {human(totals['input'])} of {human(billed)})"
        )


if __name__ == "__main__":
    for d in sys.argv[1:]:
        report(d)
