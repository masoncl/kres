#!/usr/bin/env python3
"""Walk a `kres --export` findings tree and validate every finding.

For each subdirectory of the bug-root whose workflow-owned outputs are
missing or incomplete, run kres with the embedded `validate` slash-command
against that finding directory and source workspace. The kres invocation runs from
`--workspace` so the workspace + git head match the tree the findings
reference.

The validate workflow owns false-positive elimination and classification. It writes `summary.md`,
updates `metadata.yaml` and `FINDING.md`, and retries internally when
its structured outputs or file updates are incomplete. This wrapper
only schedules finding directories and verifies the workflow-owned file
outputs; it does not do a second prose/regex classification pass.

Modeled on review-prompts/scripts/claude_xargs.py: argparse-driven
CLI, ThreadPoolExecutor when --parallel > 1, a SIGINT/SIGTERM handler
that walks the active subprocess list and tears down each process
group cleanly, and a per-finding --timeout escalation (SIGTERM →
SIGKILL).
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from threading import Event, Lock


# Global state for signal handling — same shape as claude_xargs.py.
shutdown_event = Event()
active_processes: list[subprocess.Popen] = []
processes_lock = Lock()
signal_received = False
SEVERITIES = {"high", "medium", "low"}


def configured_fast_model():
    """Read the fast-role model selector from ~/.kres/settings.json."""
    settings_path = Path.home() / ".kres" / "settings.json"
    try:
        settings = json.loads(settings_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"cannot read {settings_path}: {exc}") from exc
    model = settings.get("models", {}).get("fast")
    if not isinstance(model, str) or not model.strip():
        raise RuntimeError(f"{settings_path} has no non-empty models.fast setting")
    return model.strip()


def signal_handler(signum, frame):
    """Ctrl-C / SIGTERM: tell every active subprocess group to die."""
    global signal_received
    if signal_received:
        return
    signal_received = True
    print("\n\nInterrupted! Shutting down processes...", file=sys.stderr)
    shutdown_event.set()
    kill_all_processes()


def kill_all_processes():
    """Kill every active subprocess group, SIGTERM then SIGKILL."""
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
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait(timeout=5)
            except (ProcessLookupError, OSError, subprocess.TimeoutExpired):
                pass
    print("All processes terminated.", file=sys.stderr)


def unquote_yaml_scalar(value):
    value = value.strip()
    if len(value) >= 2 and value.startswith('"') and value.endswith('"'):
        return bytes(value[1:-1], "utf-8").decode("unicode_escape")
    if len(value) >= 2 and value.startswith("'") and value.endswith("'"):
        return value[1:-1]
    return value


def parse_top_level_scalar(yaml_text, key):
    for line in yaml_text.splitlines():
        if line.startswith((" ", "\t")):
            continue
        stripped = line.strip()
        if stripped.startswith(key + ":"):
            return unquote_yaml_scalar(stripped[len(key) + 1:])
    return ""


def extract_section(markdown, name):
    pattern = r"^# " + re.escape(name) + r"\s*\n(.*?)(?=^# |\Z)"
    match = re.search(pattern, markdown, re.MULTILINE | re.DOTALL | re.IGNORECASE)
    return match.group(1).strip() if match else ""


def summary_severity(summary_text):
    severity = extract_section(summary_text, "Severity").splitlines()
    for line in severity:
        value = line.strip().lower()
        if value in SEVERITIES:
            return value
    return ""


def finding_header_severity(finding_text):
    for line in finding_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("**Severity:**"):
            return unquote_yaml_scalar(stripped[len("**Severity:**"):]).lower()
    return ""


def metadata_validation_run(metadata_text):
    return parse_top_level_scalar(metadata_text, "validation_run").lower() in {
        "true",
        "yes",
        "1",
    }


def has_validation_marker(bug_dir):
    metadata_path = bug_dir / "metadata.yaml"
    if not metadata_path.is_file():
        return False
    metadata = metadata_path.read_text(encoding="utf-8", errors="replace")
    return metadata_validation_run(metadata)


def add_validation_marker(metadata_path):
    metadata = metadata_path.read_text(encoding="utf-8", errors="replace")
    if metadata_validation_run(metadata):
        return False

    lines = metadata.splitlines()
    insert_at = None
    for idx, line in enumerate(lines):
        if line.startswith("status:"):
            insert_at = idx + 1
            break
    if insert_at is None:
        for idx, line in enumerate(lines):
            if line.startswith("severity:"):
                insert_at = idx + 1
                break
    if insert_at is None:
        insert_at = len(lines)
    lines.insert(insert_at, "validation_run: true")
    trailing_newline = "\n" if metadata.endswith("\n") else ""
    metadata_path.write_text(
        "\n".join(lines) + trailing_newline,
        encoding="utf-8",
    )
    return True


def validate_state(bug_dir, require_marker=True):
    """Return (complete, reason) for the current validate workflow contract."""
    metadata_path = bug_dir / "metadata.yaml"
    finding_path = bug_dir / "FINDING.md"
    summary_path = bug_dir / "summary.md"
    missing = [
        path.name
        for path in (metadata_path, finding_path, summary_path)
        if not path.is_file()
    ]
    if missing:
        return (False, "missing " + ", ".join(missing))
    metadata = metadata_path.read_text(encoding="utf-8", errors="replace")
    finding = finding_path.read_text(encoding="utf-8", errors="replace")
    summary = summary_path.read_text(encoding="utf-8", errors="replace")
    metadata_sev = parse_top_level_scalar(metadata, "severity").lower()
    summary_sev = summary_severity(summary)
    finding_sev = finding_header_severity(finding)
    if require_marker and not metadata_validation_run(metadata):
        return (False, "metadata.yaml missing validation_run: true")
    if metadata_sev not in SEVERITIES:
        return (False, "metadata.yaml missing valid severity")
    if summary_sev not in SEVERITIES:
        return (False, "summary.md missing # Severity")
    if finding_sev not in SEVERITIES:
        return (False, "FINDING.md missing **Severity:**")
    if len({metadata_sev, summary_sev, finding_sev}) != 1:
        return (
            False,
            "severity mismatch "
            f"(metadata={metadata_sev}, summary={summary_sev}, finding={finding_sev})",
        )
    return (True, f"severity={metadata_sev}")


def has_auto_generated_fix(bug_dir):
    """True when /fix already published a patch for this finding.

    The fix workflow's publish step writes `auto-generated-fix.diff`
    into the finding directory (series runs add `-2.diff`, etc., but the
    base name is always present). Once a fix has landed there is nothing
    left for validation to decide, so the batch skips the finding.
    """
    return (bug_dir / "auto-generated-fix.diff").is_file()


def skip_reason(bug_dir, ignore_patches):
    if ignore_patches:
        if has_validation_marker(bug_dir):
            return "metadata.yaml has validation_run: true"
        return ""
    if has_auto_generated_fix(bug_dir):
        return "auto-generated-fix.diff present"
    complete, reason = validate_state(bug_dir)
    if complete:
        return reason
    return ""


def mark_successful_validation_if_needed(bug_dir):
    complete, reason = validate_state(bug_dir)
    if complete or reason != "metadata.yaml missing validation_run: true":
        return (complete, reason)

    otherwise_complete, other_reason = validate_state(bug_dir, require_marker=False)
    if not otherwise_complete:
        return (False, other_reason)

    if add_validation_marker(bug_dir / "metadata.yaml"):
        return (True, other_reason + ", validation_run=true")
    return validate_state(bug_dir)


def validate_one(kres_bin, slow_model, workspace, bug_dir, timeout):
    """Run kres --prompt 'validate: <bug_dir> <workspace>' with cwd = workspace.

    Returns (bug_dir, returncode, combined_output).
    """
    if shutdown_event.is_set():
        return (bug_dir, -1, "shutdown requested before start")
    # No --results: each kres run gets a default ~/.kres/sessions/
    # <ts>-<pid>/ dir, which is unique across concurrent processes.
    # Sharing a single --results across the parallel batch races on
    # session.json / findings.json / report.md / prompt.md and crashes
    # the Rust side with exit 101.
    cmd = [
        kres_bin,
        "--slow-model", slow_model,
        "--prompt", f"validate: {bug_dir} {workspace}",
        "--stdio",
        "--one",
    ]
    proc = None
    try:
        proc = subprocess.Popen(
            cmd,
            cwd=str(workspace),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            preexec_fn=os.setsid,  # new process group → clean group kill
        )
        with processes_lock:
            active_processes.append(proc)
        try:
            stdout, _ = proc.communicate(timeout=timeout)
            output = stdout or ""
            if proc.returncode == 0:
                complete, reason = mark_successful_validation_if_needed(bug_dir)
                if complete:
                    output += f"\n[validate-all] workflow outputs complete: {reason}\n"
                else:
                    output += f"\n[validate-all] incomplete workflow outputs: {reason}\n"
                    return (bug_dir, -1, output)
            return (bug_dir, proc.returncode, output)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                time.sleep(1)
                if proc.poll() is None:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait()
            except (ProcessLookupError, OSError):
                pass
            return (bug_dir, -1, f"timeout after {timeout} seconds")
    except Exception as exc:
        return (bug_dir, -1, str(exc))
    finally:
        if proc is not None:
            with processes_lock:
                if proc in active_processes:
                    active_processes.remove(proc)


def main():
    parser = argparse.ArgumentParser(
        description="Run the embedded kres validate prompt over every "
                    "finding directory under the bug-root.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Validate every finding in the cwd, 20 in flight at once (default).
  cd ~/local/kernel-bugs && %(prog)s --workspace ~/local/linux

  # Force sequential, with a 30-minute per-finding cap.
  cd ~/local/kernel-bugs && %(prog)s --workspace ~/local/linux \\
      --parallel 1 --timeout 1800

  # Re-validate every finding, even those that already have summary.md.
  %(prog)s --workspace ~/local/linux --force

  # Validate the exact set of finding dirs listed in a file (one per line).
  %(prog)s --workspace ~/local/linux -i /tmp/findings.txt

  # Validate listed findings even if they already have generated patches;
  # skip only findings already marked as validation runs.
  %(prog)s --workspace ~/local/linux -i /tmp/findings.txt --ignore-patches
        """,
    )
    parser.add_argument(
        "--workspace",
        required=True,
        type=Path,
        help="kernel source tree to cd into before each kres run",
    )
    parser.add_argument(
        "--bug-root",
        type=Path,
        default=Path.cwd(),
        help="root of the per-finding directories (default: cwd)",
    )
    parser.add_argument(
        "-i", "--input",
        type=Path,
        help="file containing a list of finding directories, one per "
             "line. When set, skip bug-root walking and validate exactly "
             "the listed directories. Blank lines and lines starting "
             "with '#' are ignored.",
    )
    parser.add_argument(
        "--kres-bin",
        default=shutil.which("kres") or "",
        help="kres binary to invoke (default: $(which kres))",
    )
    parser.add_argument(
        "-n", "--parallel",
        type=int,
        default=20,
        help="number of parallel validate runs (default: 20)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        help="per-finding timeout in seconds",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="re-validate even if current workflow outputs are complete",
    )
    parser.add_argument(
        "--ignore-patches",
        action="store_true",
        help="do not skip findings just because auto-generated-fix.diff is "
             "present. Without --force, skip only findings whose "
             "metadata.yaml already has validation_run: true.",
    )
    parser.add_argument(
        "-v", "--verbose",
        action="store_true",
        help="also print each validate run's stdout/stderr to the terminal",
    )
    args = parser.parse_args()

    try:
        slow_model = configured_fast_model()
    except RuntimeError as exc:
        print(f"model configuration error: {exc}", file=sys.stderr)
        return 1

    # Validate paths.
    if not args.kres_bin or not os.access(args.kres_bin, os.X_OK):
        target = args.kres_bin or "(none on $PATH)"
        print(f"kres binary not executable: {target}", file=sys.stderr)
        return 1
    workspace = args.workspace.resolve()
    if not workspace.is_dir():
        print(f"--workspace not a directory: {workspace}", file=sys.stderr)
        return 1

    candidates = []
    skipped = []

    if args.input is not None:
        # Explicit list mode: read one finding directory per line.
        input_path = args.input.resolve()
        if not input_path.is_file():
            print(f"-i not a file: {input_path}", file=sys.stderr)
            return 1
        seen = set()
        with input_path.open() as fh:
            for lineno, raw in enumerate(fh, start=1):
                line = raw.strip()
                if not line or line.startswith("#"):
                    continue
                entry = Path(line).expanduser().resolve()
                if not entry.is_dir():
                    print(
                        f"{input_path}:{lineno}: not a directory: {entry}",
                        file=sys.stderr,
                    )
                    return 1
                if entry in seen:
                    continue
                seen.add(entry)
                if not args.force and skip_reason(entry, args.ignore_patches):
                    skipped.append(entry)
                    continue
                candidates.append(entry)
        scan_root = input_path
    else:
        bug_root = args.bug_root.resolve()
        if not bug_root.is_dir():
            print(f"--bug-root not a directory: {bug_root}", file=sys.stderr)
            return 1

        # kres --export now writes per-finding folders under
        # <export-dir>/findings/<tag>/. Older exports laid them flat at
        # <export-dir>/<tag>/. Auto-detect: if the bug-root contains a
        # `findings/` subdir, descend into it; otherwise scan the
        # bug-root directly.
        findings_subtree = bug_root / "findings"
        if findings_subtree.is_dir():
            scan_root = findings_subtree
            print(f"using findings subtree: {scan_root}", file=sys.stderr)
        else:
            scan_root = bug_root

        # Collect finding directories under scan_root. `*/` already skips
        # dotfiles, but keep the explicit .git guard so a future change
        # can't sneak it through.
        for entry in sorted(scan_root.iterdir()):
            if not entry.is_dir():
                continue
            if entry.name == ".git":
                continue
            if not args.force and skip_reason(entry, args.ignore_patches):
                skipped.append(entry)
                continue
            candidates.append(entry)

    for d in skipped:
        reason = skip_reason(d, args.ignore_patches)
        print(f"skip {d.name} ({reason})")
    if not candidates:
        print(f"nothing to validate under {scan_root}", file=sys.stderr)
        return 0
    print(
        f"validate queue: {len(candidates)} finding(s) "
        f"(skipped {len(skipped)})",
        file=sys.stderr,
    )

    # Signal handlers — must be installed BEFORE submitting work so
    # an early Ctrl-C can still tear processes down.
    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    completed = 0
    failed = 0
    try:
        with ThreadPoolExecutor(max_workers=max(1, args.parallel)) as executor:
            futures = {
                executor.submit(
                    validate_one,
                    args.kres_bin,
                    slow_model,
                    workspace,
                    d,
                    args.timeout,
                ): d
                for d in candidates
            }
            for future in as_completed(futures):
                if shutdown_event.is_set():
                    break
                bug_dir, returncode, output = future.result()
                completed += 1
                bar = "=" * 60
                if returncode == 0:
                    print(f"\n{bar}\ncompleted: {bug_dir.name}\n{bar}")
                    if args.verbose and output:
                        print(output)
                else:
                    failed += 1
                    print(f"\n{bar}", file=sys.stderr)
                    print(
                        f"FAILED: {bug_dir.name} (exit {returncode})",
                        file=sys.stderr,
                    )
                    print(f"{bar}", file=sys.stderr)
                    if args.verbose and output:
                        print(output, file=sys.stderr)
                print(
                    f"progress: {completed}/{len(candidates)} "
                    f"(failed: {failed})",
                    file=sys.stderr,
                )
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
    finally:
        if shutdown_event.is_set() and not signal_received:
            kill_all_processes()

    print(
        f"\ncompleted: {completed}/{len(candidates)}, failed: {failed}, "
        f"skipped complete: {len(skipped)}",
        file=sys.stderr,
    )
    return 1 if failed > 0 or shutdown_event.is_set() else 0


if __name__ == "__main__":
    sys.exit(main())
