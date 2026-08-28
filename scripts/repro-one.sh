#!/usr/bin/env bash
#
# repro-one.sh -- set up a linux worktree and run the kres repro.md prompt.
#
# usage: repro-one.sh <findings-dir> <sha>
#
# Sets up a git worktree at the given SHA and runs the repro prompt from
# inside it.  Writes the full stream-json transcript to
# ./repro-one-<finding-tag>.json and prints a human-readable trace on stdout.
#
# Optional env:
#   KRES_SRC          kres checkout; the repro prompt is read from
#                     $KRES_SRC/prompts/repro.md
#   REPRO_PROMPT      path to repro.md, overriding KRES_SRC
#   REPRO_TIMEOUT     wall-clock seconds for the whole run (default 3600)
#
# One of --prompt, REPRO_PROMPT, or KRES_SRC must name a readable
# repro.md; the script has no built-in location for it.

set -euo pipefail

usage() {
    echo "usage: repro-one.sh [--linux <linux_dir>] [--config <.config>] [--append <string>] [--timeout <seconds>] [--prompt <repro.md>] <findings-dir> <sha>"
    echo "  findings-dir:  absolute or relative path to a kres finding"
    echo "                 directory (must contain FINDING.md and"
    echo "                 metadata.yaml)"
    echo "  sha:           git SHA to check out the worktree at"
    echo "  --linux:       path to the base linux directory"
    echo "                 (default: \$PWD/linux)"
    echo "  --config:      path to a kernel .config file to install"
    echo "                 into the worktree (runs make olddefconfig"
    echo "                 after copying)"
    echo "  --append:      string to append to the repro system prompt"
    echo "  --timeout:     wall-clock seconds for the whole run"
    echo "                 (default: \$REPRO_TIMEOUT or 3600)"
    echo "  --prompt:      path to repro.md"
    echo "                 (default: \$REPRO_PROMPT, else"
    echo "                 \$KRES_SRC/prompts/repro.md; one of the three"
    echo "                 must be set)"
    echo "  --help:        show this help message"
}

if [ $# -lt 1 ]; then
    usage
    exit 1
fi

BASE_LINUX=""
CONFIG_FILE=""
APPEND_STRING=""
PROMPT="${REPRO_PROMPT:-}"
TIMEOUT="${REPRO_TIMEOUT:-3600}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        --linux)
            BASE_LINUX="$2"
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
        --timeout)
            TIMEOUT="$2"
            shift 2
            ;;
        --prompt)
            PROMPT="$2"
            shift 2
            ;;
        *)
            break
            ;;
    esac
done

FINDINGS_DIR="${1:-}"
SHA="${2:-}"

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

FINDINGS_DIR="$(cd "$FINDINGS_DIR" && pwd -P)"

if [ ! -f "$FINDINGS_DIR/FINDING.md" ] || [ ! -f "$FINDINGS_DIR/metadata.yaml" ]; then
    echo "Error: $FINDINGS_DIR is not a kres finding directory" >&2
    echo "       (missing FINDING.md or metadata.yaml)" >&2
    exit 1
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

# Locate repro.md. It ships in the kres repo as prompts/repro.md, so
# KRES_SRC points at the checkout rather than at the file. Nothing is
# guessed: a run with none of the three set stops here.
if [ -z "$PROMPT" ]; then
    if [ -n "${KRES_SRC:-}" ]; then
        PROMPT="$KRES_SRC/prompts/repro.md"
    else
        echo "Error: no repro prompt configured." >&2
        echo "       Pass --prompt PATH, set REPRO_PROMPT to a repro.md," >&2
        echo "       or set KRES_SRC to your kres checkout (the prompt is" >&2
        echo "       read from \$KRES_SRC/prompts/repro.md)." >&2
        exit 2
    fi
fi

[[ -r "$PROMPT" ]] || { echo "missing prompt: $PROMPT" >&2; exit 2; }

TAG="$(basename "$FINDINGS_DIR")"
DIR="$BASE_LINUX.$TAG"

echo "Findings directory: $FINDINGS_DIR" >&2
echo "Base linux:         $BASE_LINUX" >&2
echo "SHA:                $SHA" >&2
echo "Findings tag:       $TAG" >&2
echo "Repro prompt:       $PROMPT" >&2
echo "Timeout:            $TIMEOUT" >&2
if [ -n "$CONFIG_FILE" ]; then
    echo "Kernel config:      $CONFIG_FILE" >&2
fi

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

# Scratch files for git mailinfo, created per run by apply_auto_fixes.
# Fixed names under /tmp were shared by every concurrent run on the
# host, so one run's mailinfo output could satisfy another run's
# emptiness test and send it down the `git am` path with the wrong
# patch. Batch walks over a finding set run several of these at once.
MSG_TMP=""
PATCH_TMP=""
cleanup_tmp() {
    [ -n "$MSG_TMP" ] && rm -f "$MSG_TMP"
    [ -n "$PATCH_TMP" ] && rm -f "$PATCH_TMP"
    return 0
}
trap cleanup_tmp EXIT

# Apply every auto-generated-fix*.diff from $FINDINGS_DIR onto $DIR.
# Prefer `git am` so format-patch inputs become real commits with their
# original subject/metadata visible in the reflog. Fall back to
# `git apply --index` + one commit per patch for plain diffs. The repro
# prompt treats $SHA as the buggy base and HEAD as the fixed side.
apply_auto_fixes() {
    local fixes=()
    local f
    while IFS= read -r -d '' f; do
        fixes+=("$f")
    done < <(find "$FINDINGS_DIR" -maxdepth 1 -name 'auto-generated-fix*.diff' -print0 | sort -zV)

    if [ "${#fixes[@]}" -eq 0 ]; then
        echo "No auto-generated-fix*.diff in $FINDINGS_DIR; skipping fix application" >&2
        return 0
    fi

    echo "Applying ${#fixes[@]} auto-generated fix(es) to $DIR (base $SHA)" >&2
    MSG_TMP="$(mktemp -t repro-one-msg.XXXXXX)"
    PATCH_TMP="$(mktemp -t repro-one-patch.XXXXXX)"
    (
        cd "$DIR"
        for f in "${fixes[@]}"; do
            local name; name=$(basename "$f")
            if git apply --reverse --check "$f" 2>/dev/null; then
                echo "  $name already applied, skipping" >&2
            elif git apply --check "$f" 2>/dev/null; then
                if git mailinfo "$MSG_TMP" "$PATCH_TMP" <"$f" >/dev/null 2>&1 &&
                   [ -s "$PATCH_TMP" ] &&
                   git -c user.name='repro-one' -c user.email='repro-one@localhost' \
                       am --3way --no-gpg-sign "$f" >/dev/null 2>&1; then
                    echo "  git am $name -> $(git rev-parse --short HEAD)" >&2
                else
                    git am --abort >/dev/null 2>&1 || true
                    : >"$MSG_TMP"
                    : >"$PATCH_TMP"
                    echo "  git apply $name" >&2
                    git apply --index "$f"
                    local subject
                    subject=$(sed -n -E 's/^Subject: (\[PATCH[^]]*\] )?//p' "$f" | head -1)
                    if [ -z "$subject" ]; then
                        subject="repro-one: apply $name"
                    fi
                    git -c user.name='repro-one' -c user.email='repro-one@localhost' \
                        commit --no-verify --quiet -m "$subject" -m "Applied from $name"
                    echo "  committed $name -> $(git rev-parse --short HEAD)" >&2
                fi
            else
                echo "Error: $name does not apply cleanly to $SHA" >&2
                git apply "$f" || true   # surface the rejection details
                exit 1
            fi
        done
        echo "  fixed HEAD: $(git rev-parse --short HEAD)" >&2
    )
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
    git -C "$DIR" reset --hard "$SHA"
    git -C "$DIR" clean -fdx
    refresh_worktree_support_files
fi

cd "$DIR"

if [ -n "$CONFIG_FILE" ]; then
    cp "$CONFIG_FILE" .config
    make olddefconfig
fi

apply_auto_fixes

echo "Worktree ready at $DIR"

CWD="$DIR"
OUT=$CWD/repro-one-$TAG.json

if [ -n "$APPEND_STRING" ]; then
    APPEND_STRING=$'\n\n'"Additional caller instruction: $APPEND_STRING"
fi

if ! command -v timeout >/dev/null 2>&1; then
    echo "missing required command: timeout" >&2
    exit 2
fi
if ! command -v claude >/dev/null 2>&1; then
    echo "missing required command: claude" >&2
    exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "missing required command: jq" >&2
    exit 2
fi

SCHEMA='{
  "type": "object",
  "required": ["verdict","buggy_output","fixed_output","repro_src","repro_bin","repo_diff","repo_diff_summary","not_verified"],
  "additionalProperties": false,
  "properties": {
    "verdict":       {"enum": ["bug_reproduced","not_reproduced","not_triggerable","setup_failure"]},
    "buggy_output":  {"type": "string"},
    "fixed_output":  {"type": "string"},
    "repro_src":     {"type": "string"},
    "repro_bin":     {"type": "string"},
    "repo_diff":     {"type": "string"},
    "repo_diff_summary": {"type": "string"},
    "finding_edits": {"type": "array", "items": {"type": "string"}},
    "not_verified":  {"type": "array", "items": {"type": "string"}},
    "repro_notes":   {"type": "string"},
    "claim_mismatches": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["source","wrong_claim","evidence"],
        "additionalProperties": false,
        "properties": {
          "source":      {"enum": ["summary.md","FINDING.md","metadata.yaml","auto-generated-fix.diff"]},
          "wrong_claim": {"type": "string"},
          "evidence":    {"type": "string"}
        }
      }
    }
  }
}'

APPEND_SYS="\
Finding tag: $TAG
Finding directory: $FINDINGS_DIR
Linux worktree (cwd): $CWD
Buggy base SHA: $SHA

Before each major action (build, run, edit file, revert), print a single line of the form
    STAGE: <short verb-phrase>
to stdout so an external follower can see progress.

Your FINAL assistant message must populate the supplied JSON schema. Fill
buggy_output / fixed_output with 1-3 lines each of the actual command
output that distinguishes the two sides. Fill repro_src and repro_bin
with absolute paths. Fill repo_diff and repo_diff_summary with absolute
paths to the Linux worktree's repo.diff and repo-diff-summary.md audit
files. Set verdict to setup_failure (and put the reason in not_verified[0])
if you could not build, boot, or run the reproducer."
APPEND_SYS="$APPEND_SYS$APPEND_STRING"

human_filter='
  if .type == "system" and .subtype == "init" then
    "INIT: model=" + (.model // "?") + " cwd=" + (.cwd // "?")
  elif .type == "assistant" then
    (.message.content // [])[] | (
      if .type == "text" then
        "TEXT: " + ((.text // "") | gsub("\n"; " ") | .[0:240])
      elif .type == "tool_use" then
        "TOOL: " + (.name // "?") + " " + ((.input // {}) | tojson | .[0:200])
      elif .type == "thinking" then
        empty
      else empty end
    )
  elif .type == "user" then
    (.message.content // [])[] | (
      if .type == "tool_result" then
        "RESULT: " + ((.content // "") | (if type=="array" then map(.text? // "") | join(" ") else . end) | tostring | gsub("\n"; " ") | .[0:240])
      else empty end
    )
  elif .type == "result" then
    "VERDICT: " + ((.structured_output // {verdict:"<missing>"}) | tojson) + "  (turns=" + (.num_turns|tostring) + ", cost=$" + (.total_cost_usd|tostring) + ")"
  else empty end
'

run_make_clean() {
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
}

# Run claude. Wrap with timeout(1) -- the CLI has no built-in timeout
# (verified against claude -p --help in this build).  Use pipefail so
# claude's exit code propagates through the pipeline.
set +e
timeout "$TIMEOUT" claude -p "$(cat "$PROMPT")" \
        --output-format stream-json \
        --verbose \
        --include-partial-messages \
        --no-session-persistence \
        --dangerously-skip-permissions \
        --append-system-prompt "$APPEND_SYS" \
        --json-schema "$SCHEMA" \
        </dev/null \
    | tee "$OUT" \
    | jq -r --unbuffered "$human_filter"
RC=${PIPESTATUS[0]}
set -e

if [[ $RC -eq 124 ]]; then
    echo "TIMEOUT after ${TIMEOUT}s -- transcript at $OUT" >&2
    run_make_clean
    exit "$RC"
fi

# Pull the structured verdict back out of the transcript.  If the agent
# never produced a result event the next jq returns empty -- treat that
# as "no verdict".
VERDICT=$(jq -r 'select(.type=="result") | .structured_output.verdict // empty' "$OUT" | tail -1)
echo "verdict: ${VERDICT:-<missing>}" >&2

# Verdict-driven post-processing.  Three buckets:
#   bug_reproduced            -> copy artifacts, status confirmed[_with_caveats]
#   not_reproduced |
#   not_triggerable           -> status invalidated; repro_notes is the
#                                refutation evidence
#   setup_failure | <missing> -> leave metadata.yaml alone (tooling broke,
#                                no determination made)
# Regardless of verdict, if there are repro_notes or claim_mismatches we
# prepend a dated "Repro notes" section to FINDING.md so the audit trail
# is preserved even when the run was negative.

MISMATCH_COUNT=$(jq 'select(.type=="result") | .structured_output.claim_mismatches // [] | length' "$OUT" | tail -1)
MISMATCH_COUNT=${MISMATCH_COUNT:-0}
FIX_DIFF_COUNT=$(jq '[ select(.type=="result") | .structured_output.claim_mismatches // [] | .[] | select(.source=="auto-generated-fix.diff") ] | length' "$OUT" | tail -1)
FIX_DIFF_COUNT=${FIX_DIFF_COUNT:-0}
NOTES=$(jq -r 'select(.type=="result") | .structured_output.repro_notes // ""' "$OUT" | tail -1)

NEW_STATUS=
case "$VERDICT" in
    bug_reproduced)
        REPRO_SRC=$(jq -r 'select(.type=="result") | .structured_output.repro_src // empty' "$OUT" | tail -1)
        REPRO_BIN=$(jq -r 'select(.type=="result") | .structured_output.repro_bin // empty' "$OUT" | tail -1)
        for f in "$REPRO_SRC" "$REPRO_BIN"; do
            if [[ -n "$f" && -e "$f" ]]; then
                cp -p "$f" "$FINDINGS_DIR/" && echo "copied $f -> $FINDINGS_DIR/" >&2
            else
                echo "skip: not a file: '$f'" >&2
            fi
        done
        if [[ "$MISMATCH_COUNT" -gt 0 ]]; then
            NEW_STATUS=confirmed_with_caveats
        else
            NEW_STATUS=confirmed
        fi
        ;;
    not_reproduced)
        NEW_STATUS=invalidated
        ;;
    not_triggerable)
        NEW_STATUS=confirmed_latent
        ;;
    setup_failure|"")
        : # leave metadata.yaml untouched
        ;;
    *)
        echo "warn: unknown verdict '$VERDICT' -- leaving metadata.yaml alone" >&2
        ;;
esac

if [[ "$MISMATCH_COUNT" -gt 0 || -n "$NOTES" ]]; then
    STAMP=$(date -Iseconds)
    SECTION=$(mktemp)
    {
        echo "## Repro notes ($STAMP, tag=$TAG, verdict=${VERDICT:-<missing>})"
        echo
        case "$VERDICT" in
            not_reproduced)
                echo "> **Finding invalidated by reproduction.** The reproducer ran on"
                echo "> the buggy kernel but the claimed BUG did not fire. \`metadata.yaml\`"
                echo "> \`status:\` has been set to \`invalidated\`."
                echo
                ;;
            not_triggerable)
                echo "> **Latent bug: no in-tree trigger path.** The buggy code exists"
                echo "> and the mechanism is confirmed by analysis, but no in-tree caller"
                echo "> can drive it with attacker-controlled inputs (see call-graph"
                echo "> evidence below). \`metadata.yaml\` \`status:\` has been set to"
                echo "> \`confirmed_latent\` — the fix is still worthwhile as defensive"
                echo "> hardening, but the finding does not describe a presently"
                echo "> reachable vulnerability."
                echo
                ;;
        esac
        if [[ "$FIX_DIFF_COUNT" -gt 0 ]]; then
            echo "> **ACTION REQUIRED: \`auto-generated-fix.diff\` needs updating.** The"
            echo "> reproduction contradicts $FIX_DIFF_COUNT prose claim(s) inside the fix"
            echo "> patch's commit message (see the Claim mismatches list below where"
            echo "> \`source\` is \`auto-generated-fix.diff\`). The patch code itself may"
            echo "> still be correct -- the commit-message wording is what needs to be"
            echo "> regenerated/edited before this fix is sent upstream."
            echo
        fi
        if [[ -n "$NOTES" ]]; then
            echo "$NOTES"
            echo
        fi
        if [[ "$MISMATCH_COUNT" -gt 0 ]]; then
            echo "**Claim mismatches** ($MISMATCH_COUNT): the run contradicts these"
            echo "prose claims in the finding text:"
            echo
            jq -r 'select(.type=="result") | .structured_output.claim_mismatches[]?
                   | "- **source**: " + .source + "  \n"
                   + "  **wrong claim**: " + (.wrong_claim | gsub("\n";" ")) + "  \n"
                   + "  **evidence**: " + (.evidence | gsub("\n";" "))' "$OUT"
            echo
        fi
    } > "$SECTION"

    FMD=$FINDINGS_DIR/FINDING.md
    if [[ -f "$FMD" ]]; then
        # Insert AFTER the first line if it looks like a markdown
        # title (`# ...`), else prepend to the very top.
        cp -p "$FMD" "$FMD.bak"
        if head -1 "$FMD" | grep -qE '^# '; then
            { head -1 "$FMD"; echo; cat "$SECTION"; tail -n +2 "$FMD"; } > "$FMD.tmp"
        else
            { cat "$SECTION"; echo; cat "$FMD"; } > "$FMD.tmp"
        fi
        mv "$FMD.tmp" "$FMD"
        echo "prepended Repro notes to $FMD (backup: $FMD.bak)" >&2
    else
        cat "$SECTION" > "$FMD"
        echo "created $FMD with Repro notes" >&2
    fi
    rm -f "$SECTION"
fi

if [[ -n "$NEW_STATUS" ]]; then
    YAML=$FINDINGS_DIR/metadata.yaml
    if [[ -f "$YAML" ]] && grep -qE '^status:[[:space:]]' "$YAML"; then
        # Idempotent: only rewrite if the line doesn't already match.
        if ! grep -qE "^status:[[:space:]]+${NEW_STATUS}([[:space:]]|\$)" "$YAML"; then
            sed -i.bak -E "s/^status:[[:space:]]*.*\$/status: ${NEW_STATUS}/" "$YAML" \
                && echo "updated $YAML: status -> ${NEW_STATUS} (backup: $YAML.bak)" >&2
        else
            echo "$YAML already status: ${NEW_STATUS} -- no change" >&2
        fi
    else
        echo "warn: $YAML missing or has no top-level 'status:' field" >&2
    fi
fi

run_make_clean
exit "$RC"
