#!/bin/bash
#
# usage: fix_one.sh <findings-dir> <sha>
#
# Sets up a git worktree at the given SHA and runs
# `kres --prompt "fix: <findings-dir>" --one --follow` from inside it.
#
# The findings directory is the shape produced by `kres --export` —
# a directory with FINDING.md, summary.md, and metadata.yaml at top
# level. The SHA picks the tree state to apply the fix against.
#
# Before running this, you need a base linux clone that contains
# the SHA. semcode indexing for that range is also useful so the
# main agent can serve symbol lookups via MCP.

set -e

usage() {
    echo "usage: fix_one.sh [--linux <linux_dir>] [--turns <n>] [--slow <tag>] [--results <dir>] [--config <.config>] [--append <string>] [--skip-existing] <findings-dir> <sha>"
    echo "  findings-dir:  absolute or relative path to a kres finding"
    echo "                 directory (must contain FINDING.md and"
    echo "                 metadata.yaml)"
    echo "  sha:           git SHA to check out the worktree at"
    echo "  --linux:       path to the base linux directory"
    echo "                 (default: \$PWD/linux)"
    echo "  --turns:       turn budget passed through to kres"
    echo "                 (default: kres's own default)"
    echo "  --slow:        slow-agent tag passed through to kres --slow"
    echo "                 (e.g. sonnet, opus)"
    echo "  --results:     results directory passed to kres --results"
    echo "                 (default: kres's own default under ~/.kres/sessions)"
    echo "  --config:      path to a kernel .config file to install"
    echo "                 into the worktree (runs make olddefconfig"
    echo "                 after copying)"
    echo "  --append:      string to append to the prompt"
    echo "  --skip-existing: exit 0 without running kres when the finding"
    echo "                 already carries a fix (an auto-generated-fix*.diff"
    echo "                 file, or an auto_generated_fixes entry in"
    echo "                 metadata.yaml). One finding may need a series, so"
    echo "                 auto-generated-fix-2.diff is a second patch rather"
    echo "                 than a second attempt; any of them means the fix"
    echo "                 workflow already ran."
    echo "  --help:        show this help message"
}

if [ $# -lt 1 ]; then
    usage
    exit 1
fi

BASE_LINUX=""
TURNS=""
SLOW_TAG=""
RESULTS_DIR=""
APPEND_STRING=""
CONFIG_FILE=""
SKIP_EXISTING=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help)
            usage
            exit 0
            ;;
        --linux)
            BASE_LINUX="$2"
            shift 2
            ;;
        --turns)
            TURNS="$2"
            shift 2
            ;;
        --slow)
            SLOW_TAG="$2"
            shift 2
            ;;
        --results)
            RESULTS_DIR="$2"
            shift 2
            ;;
        --config)
            CONFIG_FILE="$2"
            shift 2
            ;;
        --append)
            APPEND_STRING="$2"
            shift 2
            ;;
        --skip-existing)
            SKIP_EXISTING=1
            shift
            ;;
        *)
            break
            ;;
    esac
done

FINDINGS_DIR="$1"
SHA="$2"

if [ -z "$FINDINGS_DIR" ] || [ -z "$SHA" ]; then
    usage
    exit 1
fi

if [ -z "$BASE_LINUX" ]; then
    BASE_LINUX="$(pwd -P)/linux"
fi

if [ ! -d "$FINDINGS_DIR" ]; then
    echo "Error: findings directory does not exist: $FINDINGS_DIR" >&2
    exit 1
fi

# Resolve to an absolute path. The fix-template tells the slow agent
# to read FINDING.md / summary.md / metadata.yaml from the BUG INPUT
# path verbatim, so a relative path here would break once we cd into
# the worktree.
FINDINGS_DIR="$(cd "$FINDINGS_DIR" && pwd -P)"

if [ ! -f "$FINDINGS_DIR/FINDING.md" ] || [ ! -f "$FINDINGS_DIR/metadata.yaml" ]; then
    echo "Error: $FINDINGS_DIR is not a kres finding directory" >&2
    echo "       (missing FINDING.md or metadata.yaml)" >&2
    exit 1
fi

# Skip a finding that already carries a fix. Checked here, before the
# worktree is created or reset, so a skip costs nothing. Both markers
# are consulted: the diff files the publish step writes, and the
# auto_generated_fixes list it records in metadata.yaml. Either one
# alone can be stale if a file was deleted by hand.
#
# A finding can legitimately need a patch series, so
# auto-generated-fix-2.diff is the second patch of one fix rather than
# a second attempt at the first. Presence of ANY of them means the fix
# workflow has already run against this finding.
if [ -n "$SKIP_EXISTING" ]; then
    existing_diff="$(find "$FINDINGS_DIR" -maxdepth 1 -name 'auto-generated-fix*.diff' -print -quit 2>/dev/null || true)"
    if [ -n "$existing_diff" ]; then
        echo "skip: $(basename "$FINDINGS_DIR") already has $(basename "$existing_diff")" >&2
        exit 0
    fi
    if grep -q '^auto_generated_fixes:' "$FINDINGS_DIR/metadata.yaml" 2>/dev/null; then
        echo "skip: $(basename "$FINDINGS_DIR") has auto_generated_fixes in metadata.yaml" >&2
        exit 0
    fi
fi

if [ -n "$CONFIG_FILE" ] && [ ! -f "$CONFIG_FILE" ]; then
    echo "Error: config file does not exist: $CONFIG_FILE" >&2
    exit 1
fi

if [ ! -d "$BASE_LINUX" ]; then
    echo "Error: linux directory does not exist: $BASE_LINUX" >&2
    exit 1
fi
BASE_LINUX="$(cd "$BASE_LINUX" && pwd -P)"

if [ "$SHA" = "HEAD" ]; then
    SHA=$(cd "$BASE_LINUX" && git rev-parse --short HEAD)
fi

FINDINGS_TAG="$(basename "$FINDINGS_DIR")"

echo "Findings directory: $FINDINGS_DIR" >&2
echo "Base linux:         $BASE_LINUX" >&2
echo "SHA:                $SHA" >&2
echo "Findings tag:       $FINDINGS_TAG" >&2
if [ -n "$CONFIG_FILE" ]; then
    echo "Kernel config:      $CONFIG_FILE" >&2
fi

# Tag the worktree by the finding's basename rather than the SHA.
# Multiple findings share the same audit SHA in this workflow, so a
# SHA-suffixed worktree path would collide across parallel fix runs.
DIR="$BASE_LINUX.$FINDINGS_TAG"

refresh_worktree_support_files() {
    if [ -d "$BASE_LINUX/.semcode.db" ]; then
        rm -rf "$DIR/.semcode.db"
        cp -al "$BASE_LINUX/.semcode.db" "$DIR/.semcode.db"
    else
        echo "Warning: $BASE_LINUX/.semcode.db not found, skipping" >&2
    fi
    if [ -f "$BASE_LINUX/.config" ]; then
        cp "$BASE_LINUX/.config" "$DIR/.config"
        (cd "$DIR" && make olddefconfig)
    fi
}

if [ ! -d "$DIR" ]; then
    (cd "$BASE_LINUX" && git worktree add -d "$DIR" "$SHA")
    while true; do
        if [ -d "$DIR" ]; then
            break
        fi
        echo "waiting for $DIR to exist"
        sleep 1
    done
    refresh_worktree_support_files
else
    # Existing per-finding worktrees are disposable. Reusing one with an
    # old auto-fix commit makes kres see the bug as already fixed and
    # invalidates the finding incorrectly, so force it back to the
    # requested base before every run.
    git -C "$DIR" reset --hard "$SHA"
    git -C "$DIR" clean -fdx
    refresh_worktree_support_files
fi

cd "$DIR"

if [ -n "$CONFIG_FILE" ]; then
    cp "$CONFIG_FILE" .config
    make olddefconfig
fi

echo "Worktree ready at $DIR"

PROMPT="fix: $FINDINGS_DIR"
if [ -n "$APPEND_STRING" ]; then
    PROMPT="$PROMPT $APPEND_STRING"
fi

KRES_OPTS=(--one --follow)
if [ -n "$TURNS" ]; then
    KRES_OPTS+=(--turns "$TURNS")
fi
if [ -n "$SLOW_TAG" ]; then
    KRES_OPTS+=(--slow "$SLOW_TAG")
fi
if [ -n "$RESULTS_DIR" ]; then
    KRES_OPTS+=(--results "$RESULTS_DIR")
fi

start=$(date +%s)
set +e
kres --prompt "$PROMPT" "${KRES_OPTS[@]}"
status=$?
set -e
end=$(date +%s)
echo "Elapsed time: $((end - start)) seconds (sha $SHA)"

clean_start=$(date +%s)
echo "Running make clean in $DIR"
set +e
make clean
clean_status=$?
set -e
clean_end=$(date +%s)
if [ "$clean_status" -ne 0 ]; then
    echo "Warning: make clean failed with status $clean_status in $DIR" >&2
else
    echo "make clean completed in $((clean_end - clean_start)) seconds"
fi

exit $status
