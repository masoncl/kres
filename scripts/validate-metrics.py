#!/usr/bin/env python3
"""Log-derived metrics for `validate` runs, computed identically before
and after a change.

Every number the validate-workflow rework was justified by came from an
ad-hoc script. This is that script, checked in, so a before/after
comparison is not comparing two different definitions.

Point it at the `.kres/logs` directory of the workspace the validations
ran against — `validate-all.py` runs with `cwd = <source workspace>`, so
the JSONL lives next to the source tree, not next to the findings.

  scripts/validate-metrics.py ~/local/linux.kres/.kres/logs
  scripts/validate-metrics.py ~/local/linux.kres/.kres/logs --since 2026-08-07
  scripts/validate-metrics.py before/logs --compare after/logs

What it reports, and why each number is here:

  reject rate      fraction of a step's synthesis calls whose response
                   did not carry the step's declared output. Each one
                   re-runs the whole step.
  retry wall       wall-clock inside the window from the first rejected
                   synthesis to the accepted one.
  dropped requests typed followups emitted by a rejected attempt that no
                   fetch record in the run ever served.
  cache            fresh vs created vs read input tokens per phase. The
                   large synthesis prompts should not be all-fresh.
  unparseable      slow responses that were not JSON at all.
"""

import argparse
import collections
import datetime
import json
import os
import statistics
import sys
from pathlib import Path

# The declared output that proves a step's synthesis actually answered
# the step, rather than replying with the gather envelope.
STEP_REQUIRED_OUTPUT = {
    "validate-claims": "claim_validation",
    "validate-conjunction": "conjunction",
    "validate-reachability": "triage_coding",
}


def parse_ts(record):
    return datetime.datetime.fromisoformat(record["timestamp"].replace("Z", "+00:00"))


def label_parts(record):
    label = record.get("label") or ""
    tokens = label.split()
    # Most labels are `phase=X task=Y ...`; a few (json-repair,
    # json-normalization) are a bare kind. Fall back to the first token
    # so those still get a row instead of an unnamed one.
    phase = tokens[0] if tokens else ""
    task = ""
    for token in tokens:
        if token.startswith("phase="):
            phase = token[6:]
        elif token.startswith("task="):
            task = token[5:]
    return phase, task


def loads_lenient(text):
    """Parse an assistant response, tolerating literal control characters.

    Returns None when the response is not JSON at all — that is itself a
    measured quantity, so it must not raise.
    """
    try:
        return json.loads(text, strict=False)
    except Exception:
        return None


def read_run(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return [json.loads(line) for line in handle if line.strip()]
    except (OSError, json.JSONDecodeError):
        return []


def is_validate_run(records):
    return bool(records) and "validate-claims" in (records[0].get("label") or "")


def collect(logs_dir, since=None, until=None):
    runs = []
    for entry in sorted(Path(logs_dir).glob("*/code.jsonl")):
        records = read_run(entry)
        if not is_validate_run(records):
            continue
        day = records[0]["timestamp"][:10]
        if since and day < since:
            continue
        if until and day > until:
            continue
        runs.append((entry.parent.name, records))
    return runs


def analyse(runs):
    m = {
        "runs": len(runs),
        "wall": 0.0,
        "retry_wall": 0.0,
        "runs_with_retry": 0,
        "synth": collections.Counter(),
        "rejected": collections.Counter(),
        "unparseable_slow": 0,
        "dropped_requests": 0,
        "dropped_never_fetched": 0,
        "dropped_by_kind": collections.Counter(),
        "usage": collections.defaultdict(collections.Counter),
        "calls": collections.Counter(),
        "slow_fresh_in": [],
        "slow_out": [],
    }
    if not runs:
        return m

    for _, records in runs:
        m["wall"] += (parse_ts(records[-1]) - parse_ts(records[0])).total_seconds()

        fetched = set()
        for record in records:
            if record.get("role") == "fetch":
                for followup in json.loads(record["content"])["followups"]:
                    fetched.add(followup)

        first_reject = {}
        last_accept = {}
        run_slow_in = run_slow_out = 0

        for index, record in enumerate(records):
            phase, task = label_parts(record)
            if record.get("role") != "assistant":
                continue
            usage = record.get("usage") or {}
            if usage:
                m["calls"][phase] += 1
                for key, value in usage.items():
                    m["usage"][phase][key] += value
                if phase == "slow" and task == "validate-reachability":
                    run_slow_in += usage.get("input", 0)
                    run_slow_out += usage.get("output", 0)

            if phase not in ("fast-synth", "slow"):
                continue
            required = STEP_REQUIRED_OUTPUT.get(task)
            if required is None:
                continue

            parsed = loads_lenient(record.get("content", ""))
            if parsed is None:
                if phase == "slow":
                    m["unparseable_slow"] += 1
                m["synth"][task] += 1
                m["rejected"][task] += 1
                first_reject.setdefault(task, index)
                continue

            m["synth"][task] += 1
            if required in parsed:
                last_accept[task] = index
                continue

            m["rejected"][task] += 1
            first_reject.setdefault(task, index)
            for followup in parsed.get("followups") or []:
                kind = followup.get("type")
                key = f"{kind}:{followup.get('name')}"
                m["dropped_requests"] += 1
                m["dropped_by_kind"][kind] += 1
                if not any(key in served for served in fetched):
                    m["dropped_never_fetched"] += 1

        m["slow_fresh_in"].append(run_slow_in)
        m["slow_out"].append(run_slow_out)

        # Wall clock spent between the first rejected synthesis and the
        # accepted one, measured from the request that produced the
        # rejection so the wasted model time is included.
        for task, reject_index in first_reject.items():
            accept_index = last_accept.get(task)
            if accept_index is None or accept_index <= reject_index:
                continue
            request = reject_index
            while request >= 0 and records[request].get("role") != "user":
                request -= 1
            if request < 0:
                continue
            m["retry_wall"] += (
                parse_ts(records[accept_index]) - parse_ts(records[request])
            ).total_seconds()
            m["runs_with_retry"] += 1

    return m


def pct(numerator, denominator):
    return 0.0 if not denominator else 100.0 * numerator / denominator


def report(name, m, out=sys.stdout):
    def emit(line=""):
        print(line, file=out)

    emit(f"=== {name}")
    if not m["runs"]:
        emit("  no validate runs found")
        return
    emit(f"  runs {m['runs']}  wall {m['wall']:.0f}s  mean {m['wall'] / m['runs']:.0f}s/run")

    emit("  synthesis reject rate (response lacked the step's declared output):")
    for task in sorted(m["synth"]):
        total = m["synth"][task]
        bad = m["rejected"][task]
        emit(f"    {task:24s} {bad:5d}/{total:<5d} {pct(bad, total):5.1f}%")

    emit(
        f"  retry wall {m['retry_wall']:.0f}s = {pct(m['retry_wall'], m['wall']):.1f}% "
        f"of total, in {m['runs_with_retry']} run(s)"
    )
    emit(
        f"  requests emitted by rejected attempts: {m['dropped_requests']}"
        f"  never fetched: {m['dropped_never_fetched']}"
        f" ({pct(m['dropped_never_fetched'], m['dropped_requests']):.0f}%)"
    )
    if m["dropped_by_kind"]:
        kinds = ", ".join(f"{k}={v}" for k, v in m["dropped_by_kind"].most_common(6))
        emit(f"    by kind: {kinds}")
    emit(f"  unparseable slow responses: {m['unparseable_slow']}")

    emit("  tokens by phase:")
    emit(f"    {'phase':22s} {'calls':>6} {'fresh':>12} {'cache_create':>13} {'cache_read':>12} {'out':>11}")
    for phase in sorted(m["usage"], key=lambda p: -m["usage"][p]["input"]):
        u = m["usage"][phase]
        emit(
            f"    {phase:22s} {m['calls'][phase]:6d} {u['input']:12d} "
            f"{u['cache_creation']:13d} {u['cache_read']:12d} {u['output']:11d}"
        )
    if m["slow_fresh_in"]:
        emit(
            f"  slow/validate-reachability per run: fresh in median "
            f"{int(statistics.median(m['slow_fresh_in']))}, out median "
            f"{int(statistics.median(m['slow_out']))}"
        )


def main():
    parser = argparse.ArgumentParser(
        description="Log-derived metrics for kres validate runs.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("logs", help="a .kres/logs directory")
    parser.add_argument("--compare", help="a second .kres/logs directory to diff against")
    parser.add_argument("--since", help="only runs whose first record is on/after YYYY-MM-DD")
    parser.add_argument("--until", help="only runs whose first record is on/before YYYY-MM-DD")
    args = parser.parse_args()

    for path in filter(None, [args.logs, args.compare]):
        if not os.path.isdir(path):
            print(f"not a directory: {path}", file=sys.stderr)
            return 1

    baseline = analyse(collect(args.logs, args.since, args.until))
    report(args.logs, baseline)
    if args.compare:
        print()
        other = analyse(collect(args.compare, args.since, args.until))
        report(args.compare, other)
    return 0


if __name__ == "__main__":
    sys.exit(main())
