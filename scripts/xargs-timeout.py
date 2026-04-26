#!/usr/bin/env python3
"""Run a command template against many inputs in parallel, with a
per-invocation timeout and clean-shutdown signal handling.

Reads one input per line from a file, substitutes the line into the
command template, and runs the resulting shell command. The template
either contains the xargs-style placeholder `{}` (substituted at
every occurrence) or omits it (the input is appended to the end with
a single space). Up to `--parallel` invocations execute concurrently.
SIGINT or SIGTERM on this script tears down every active subprocess
group with a SIGTERM → grace → SIGKILL escalation, so an aborted
batch doesn't leave orphaned processes behind.

Lines starting with `#` and blank lines in the input file are
skipped. The command runs through `/bin/sh -c` (`shell=True`) so
shell metacharacters in the template work; treat the input file as
trusted for the same reason.
"""

import argparse
import os
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from threading import Event, Lock


# Global state for signal handling.
shutdown_event = Event()
active_processes = []
processes_lock = Lock()
signal_received = False


def signal_handler(signum, frame):
    """Handle Ctrl-C / SIGTERM by killing every active subprocess."""
    global signal_received
    if signal_received:
        return  # Avoid multiple invocations
    signal_received = True

    print("\n\nInterrupted! Shutting down processes...", file=sys.stderr)
    shutdown_event.set()
    kill_all_processes()


def kill_all_processes():
    """Kill all active processes, first with SIGTERM, then SIGKILL."""
    with processes_lock:
        procs = list(active_processes)

    if not procs:
        return

    print(
        f"Sending SIGTERM to {len(procs)} process group(s)...",
        file=sys.stderr,
    )
    for proc in procs:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, OSError):
            pass

    # Wait up to 5 seconds for graceful termination.
    deadline = time.time() + 5
    while time.time() < deadline:
        with processes_lock:
            still_running = [p for p in active_processes if p.poll() is None]
        if not still_running:
            break
        time.sleep(0.1)

    with processes_lock:
        still_running = [p for p in active_processes if p.poll() is None]

    if still_running:
        print(
            f"Sending SIGKILL to {len(still_running)} process group(s)...",
            file=sys.stderr,
        )
        for proc in still_running:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, OSError):
                pass

    print("Waiting for all processes to exit...", file=sys.stderr)
    for proc in procs:
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            print(
                f"Process {proc.pid} did not exit, forcing...",
                file=sys.stderr,
            )
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait(timeout=5)
            except (ProcessLookupError, OSError, subprocess.TimeoutExpired):
                pass

    print("All processes terminated.", file=sys.stderr)


PLACEHOLDER_ARG = "{}"
PLACEHOLDER_NUM = "{#}"  # GNU parallel convention for "job number"


def expand_template(cmd_template, arg, line_no):
    """Substitute the input + line number into the command template.

    Recognised placeholders (any number of occurrences each):

      * `{}`  — replaced with the input line (xargs `-I{}` style).
      * `{#}` — replaced with the 1-based line number, counted after
                blanks and `#` comments are filtered out (so it lines
                up with the order the runner submits work). Useful
                for per-input output dirs: `--results runs/{#}`.

    If neither placeholder is present, `arg` is appended to the end of
    the template with a single space — the legacy append form.
    """
    has_arg = PLACEHOLDER_ARG in cmd_template
    has_num = PLACEHOLDER_NUM in cmd_template
    if not has_arg and not has_num:
        return f"{cmd_template} {arg}"
    out = cmd_template
    if has_num:
        out = out.replace(PLACEHOLDER_NUM, str(line_no))
    if has_arg:
        out = out.replace(PLACEHOLDER_ARG, arg)
    return out


def run_one(cmd_template, arg, line_no, timeout):
    """Run a single shell command formed by substituting `arg` and
    `line_no` into `cmd_template`.

    Returns (arg, return_code, stdout, stderr).
    """
    if shutdown_event.is_set():
        return (arg, -1, "", "Shutdown requested before start")

    cmd = expand_template(cmd_template, arg, line_no)
    proc = None
    try:
        proc = subprocess.Popen(
            cmd,
            shell=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            preexec_fn=os.setsid,  # new pgrp → kill_all can take it down cleanly
        )

        with processes_lock:
            active_processes.append(proc)

        try:
            stdout, stderr = proc.communicate(timeout=timeout)
            return (arg, proc.returncode, stdout, stderr)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                time.sleep(1)
                if proc.poll() is None:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait()
            except (ProcessLookupError, OSError):
                pass
            return (arg, -1, "", f"Timeout after {timeout} seconds")
    except Exception as exc:
        return (arg, -1, "", str(exc))
    finally:
        if proc is not None:
            with processes_lock:
                if proc in active_processes:
                    active_processes.remove(proc)


def main():
    parser = argparse.ArgumentParser(
        description="Run a command template against many inputs in parallel.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Append form: input goes at the end of the template.
  %(prog)s -n 4 -c 'kres --prompt "triage:"' -f bugs.txt --timeout 1800

  # `{}` placeholder: inserts the input where you want it. Useful when
  # the input has to land mid-command, e.g. as a `--results` value.
  %(prog)s -n 4 -c 'kres --results runs/{} --prompt {}' -f bugs.txt

  # `{#}` placeholder: 1-based line number (after `#`/blank lines are
  # filtered out). Pair with `{}` to keep per-input output dirs
  # short even when the inputs themselves are long paths.
  %(prog)s -n 4 -c 'kres --results runs/{#} --prompt {}' -f bugs.txt

  # Print each input verbatim (useful as a smoke test).
  %(prog)s -c 'echo' -f inputs.txt

  # Echo failing lines' stderr along with the progress markers.
  %(prog)s -n 8 -c './run-one.sh' -f tasks.txt -v
        """,
    )
    parser.add_argument(
        "-c", "--command",
        required=True,
        help="command template; each input line is appended to it before "
             "running the result through `/bin/sh -c`",
    )
    parser.add_argument(
        "-f", "--input-file",
        required=True,
        help="file with one input per line (blank lines and lines "
             "starting with `#` are skipped)",
    )
    parser.add_argument(
        "-n", "--parallel",
        type=int,
        default=24,
        help="number of parallel invocations (default: 24)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        help="per-invocation timeout in seconds (default: no timeout)",
    )
    parser.add_argument(
        "-v", "--verbose",
        action="store_true",
        help="print stderr from failed invocations",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the fully-expanded command that would run for each "
             "input, then exit. Nothing is executed, no signal handlers "
             "installed.",
    )

    args = parser.parse_args()

    # Read inputs.
    try:
        with open(args.input_file, "r") as f:
            inputs = [
                line.strip()
                for line in f
                if line.strip() and not line.lstrip().startswith("#")
            ]
    except OSError as exc:
        print(f"Error opening {args.input_file}: {exc}", file=sys.stderr)
        return 1
    if not inputs:
        print(
            f"Error: no inputs found in {args.input_file}",
            file=sys.stderr,
        )
        return 1
    print(
        f"Loaded {len(inputs)} input(s) from {args.input_file}",
        file=sys.stderr,
    )

    if args.dry_run:
        # Show the fully-expanded command per input on stdout, one
        # per line, so the operator can pipe / inspect it. No signal
        # handlers, no executor, no subprocesses.
        for line_no, arg in enumerate(inputs, start=1):
            print(expand_template(args.command, arg, line_no))
        print(
            f"--dry-run: {len(inputs)} command(s) would run",
            file=sys.stderr,
        )
        return 0

    # Wire signal handlers.
    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    completed = 0
    failed = 0
    try:
        with ThreadPoolExecutor(max_workers=max(1, args.parallel)) as executor:
            futures = {
                executor.submit(
                    run_one, args.command, arg, line_no, args.timeout
                ): arg
                for line_no, arg in enumerate(inputs, start=1)
            }
            for future in as_completed(futures):
                if shutdown_event.is_set():
                    break

                arg, returncode, stdout, stderr = future.result()
                completed += 1
                bar = "=" * 60
                if returncode == 0:
                    print(f"\n{bar}\nCompleted: {arg}\n{bar}")
                    print(stdout)
                else:
                    failed += 1
                    print(f"\n{bar}", file=sys.stderr)
                    print(
                        f"FAILED: {arg} (exit code: {returncode})",
                        file=sys.stderr,
                    )
                    print(f"{bar}", file=sys.stderr)
                    if args.verbose and stderr:
                        print(stderr, file=sys.stderr)

                print(
                    f"Progress: {completed}/{len(inputs)} (failed: {failed})",
                    file=sys.stderr,
                )
    except Exception as exc:
        print(f"Error: {exc}", file=sys.stderr)
    finally:
        if shutdown_event.is_set() and not signal_received:
            kill_all_processes()

    print(
        f"\nCompleted: {completed}/{len(inputs)}, Failed: {failed}",
        file=sys.stderr,
    )
    return 1 if failed > 0 or shutdown_event.is_set() else 0


if __name__ == "__main__":
    sys.exit(main())
