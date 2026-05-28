#!/usr/bin/env bash
#
# setup.sh — initialize a kres config directory.
#
# Copies model configs, optional MCP config, and skills shipped in this
# repo into the destination directory (default ~/.kres) and substitutes
# API-key placeholders in each model file. Existing destination files
# are left untouched unless --overwrite is passed.
#
# Usage:
#   setup.sh [--dest DIR] [--slow-key KEY] [--fast-key KEY] [--overwrite]
#
# --slow-key / --fast-key accept literal key strings. The argument's
# value is substituted inline for the @SLOW_KEY@ / @FAST_KEY@
# placeholder in the installed config JSON — no external credential
# files are read or written.
#
# --fast-key feeds the default Sonnet model config.
# --slow-key feeds the default Opus model config.
#
# Without --overwrite, any destination file that already exists is
# reported and skipped. The script is idempotent in that mode.

set -euo pipefail

usage() {
  cat <<USAGE
Usage: $0 [--dest DIR] [--slow-key KEY] [--fast-key KEY]
          [--slow MODEL] [--model MODEL]
          [--semcode PATH] [--review-prompts PATH] [--overwrite]

Options:
  --dest DIR             Destination directory (default: \$HOME/.kres)
  --slow-key KEY         Slow-agent API key literal
  --fast-key KEY         Fast / main / todo agent API key literal
  --slow MODEL           Model id used for the slow agent in settings.json.
                         Default: claude-opus-4-7.
  --model MODEL          Model id used for fast/main/todo agents in
                         settings.json. Default: claude-sonnet-4-6.
  --semcode PATH         Path to a semcode-mcp binary. Installs mcp.json
                         pointing at it. If omitted, mcp.json is only
                         installed when semcode-mcp is found on PATH (and
                         the bare name is used). Pass --semcode \"\" to
                         force-skip even if semcode-mcp is on PATH.
  --review-prompts PATH  Path to a kernel-review-prompts tree (the directory
                         that contains kernel/technical-patterns.md etc.).
                         Used as the value of @REVIEW_PROMPTS@ in the kernel
                         skill. If omitted, setup.sh reads
                         ~/.claude/skills/kernel/SKILL.md and pulls the
                         first review-prompts path it finds. When neither
                         is available the skill is not installed.
  --overwrite            Replace existing files instead of leaving them alone
  -h, --help             Print this help and exit
USAGE
}

DEST="${HOME}/.kres"
SLOW_KEY=""
FAST_KEY=""
SLOW_MODEL="claude-opus-4-7"
MODEL="claude-sonnet-4-6"
# SEMCODE states: unset (auto-detect via PATH), empty-after-flag
# (explicit skip), or a non-empty string (use as the binary path).
SEMCODE_ARG=""
SEMCODE_FLAG_SEEN=0
REVIEW_PROMPTS_ARG=""
REVIEW_PROMPTS_FLAG_SEEN=0
OVERWRITE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dest)                DEST="$2"; shift 2 ;;
    --dest=*)              DEST="${1#*=}"; shift ;;
    --slow-key)            SLOW_KEY="$2"; shift 2 ;;
    --slow-key=*)          SLOW_KEY="${1#*=}"; shift ;;
    --fast-key)            FAST_KEY="$2"; shift 2 ;;
    --fast-key=*)          FAST_KEY="${1#*=}"; shift ;;
    --slow)                SLOW_MODEL="$2"; shift 2 ;;
    --slow=*)              SLOW_MODEL="${1#*=}"; shift ;;
    --model)               MODEL="$2"; shift 2 ;;
    --model=*)             MODEL="${1#*=}"; shift ;;
    --semcode)             SEMCODE_ARG="$2"; SEMCODE_FLAG_SEEN=1; shift 2 ;;
    --semcode=*)           SEMCODE_ARG="${1#*=}"; SEMCODE_FLAG_SEEN=1; shift ;;
    --review-prompts)      REVIEW_PROMPTS_ARG="$2"; REVIEW_PROMPTS_FLAG_SEEN=1; shift 2 ;;
    --review-prompts=*)    REVIEW_PROMPTS_ARG="${1#*=}"; REVIEW_PROMPTS_FLAG_SEEN=1; shift ;;
    --overwrite)           OVERWRITE=1; shift ;;
    -h|--help)             usage; exit 0 ;;
    *)                     echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# Resolve the repo root from the script's own location so setup.sh
# works whether invoked from a checkout, an installed tree, or via a
# symlink on the operator's PATH.
SRC_DIR="$(cd "$(dirname -- "${BASH_SOURCE[0]:-$0}")" && pwd)"
CONFIGS_SRC="${SRC_DIR}/configs"
SKILLS_SRC="${SRC_DIR}/skills"

if [[ ! -d "${CONFIGS_SRC}" ]]; then
  echo "error: configs/ not found at ${CONFIGS_SRC}" >&2
  echo "       run setup.sh from inside the kres repo checkout." >&2
  exit 1
fi

mkdir -p "${DEST}"
mkdir -p "${DEST}/skills"
mkdir -p "${DEST}/models"
mkdir -p "${DEST}/system-prompts"
mkdir -p "${DEST}/commands"
mkdir -p "${DEST}/workflows"

say() { printf '  %s\n' "$*"; }

# resolve_key KEY — returns the literal API key string that should be
# substituted for @FAST_KEY@ / @SLOW_KEY@ placeholders. An empty
# argument returns the empty string, signalling 'leave placeholder in
# place'.
resolve_key() {
  local val="$1"
  if [[ -z "$val" ]]; then
    printf ''
    return 0
  fi
  printf '%s' "$val"
}

SLOW_KEY_VALUE="$(resolve_key "${SLOW_KEY}")"
FAST_KEY_VALUE="$(resolve_key "${FAST_KEY}")"

# For logging/reporting: obscure the literal key so it doesn't leak
# to stdout. `set | grep` and `ps` still see the full value, but our
# own output stays clean.
redact() {
  local val="$1"
  if [[ -z "$val" ]]; then
    printf '<placeholder unchanged>'
  elif [[ "${#val}" -le 8 ]]; then
    printf '***'
  else
    printf '%s***%s' "${val:0:4}" "${val: -2}"
  fi
}

# install_file SRC DST — copy SRC to DST. Skip if DST exists and
# --overwrite was not passed. Create parent directory as needed.
install_file() {
  local src="$1" dst="$2"
  if [[ ! -e "$src" ]]; then
    echo "error: source missing: $src" >&2
    return 1
  fi
  if [[ -e "$dst" ]] && [[ "${OVERWRITE}" -ne 1 ]]; then
    say "keep: ${dst}"
    return 0
  fi
  install -m 0644 "$src" "$dst"
  say "wrote: ${dst}"
}

# install_model_config SRC DST — copy a model JSON config, replacing
# both @FAST_KEY@ and @SLOW_KEY@ placeholders. If a key value is empty,
# its placeholder is left in place so the operator can edit later.
install_model_config() {
  local src="$1" dst="$2"
  if [[ ! -e "$src" ]]; then
    echo "error: source missing: $src" >&2
    return 1
  fi
  if [[ -e "$dst" ]] && [[ "${OVERWRITE}" -ne 1 ]]; then
    say "keep: ${dst}"
    return 0
  fi
  local tmp
  tmp="$(mktemp "${dst}.tmp.XXXXXX")"
  KRES_FAST_KEY_VALUE="${FAST_KEY_VALUE}" \
  KRES_SLOW_KEY_VALUE="${SLOW_KEY_VALUE}" \
  awk \
    -v fast_ph="@FAST_KEY@" \
    -v slow_ph="@SLOW_KEY@" \
    '
    BEGIN {
      fast_val = ENVIRON["KRES_FAST_KEY_VALUE"]
      slow_val = ENVIRON["KRES_SLOW_KEY_VALUE"]
    }
    function json_escape(s,    out, i, c) {
      out = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\"") out = out "\\\""
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else if (c == "\t") out = out "\\t"
        else out = out c
      }
      return out
    }
    function subst(line, ph, val,    out, i, lp, replacement) {
      lp = length(ph)
      replacement = (val == "" ? ph : json_escape(val))
      out = ""
      while ((i = index(line, ph)) > 0) {
        out  = out substr(line, 1, i - 1) replacement
        line = substr(line, i + lp)
      }
      return out line
    }
    {
      line = subst($0, fast_ph, fast_val)
      line = subst(line, slow_ph, slow_val)
      print line
    }
    ' "$src" > "$tmp"
  mv "$tmp" "$dst"
  chmod 0640 "$dst"
  say "wrote: ${dst} (@FAST_KEY@=$(redact "${FAST_KEY_VALUE}"), @SLOW_KEY@=$(redact "${SLOW_KEY_VALUE}"))"
}

echo "kres setup"
say "dest:         ${DEST}"
say "overwrite:    $([[ ${OVERWRITE} -eq 1 ]] && echo yes || echo no)"
say "slow key:     $(redact "${SLOW_KEY_VALUE}")"
say "fast key:     $(redact "${FAST_KEY_VALUE}")"
say "slow model:   ${SLOW_MODEL}"
say "model:        ${MODEL}"

echo "system prompts and model configs:"
# Every shipped prompt/template is embedded via include_str!:
# agent `*.system.md` prompts go through
# kres-agents::embedded_prompts, summary templates go through
# kres-agents::user_commands, and review/triage/fix live in
# configs/workflows/*.json. None of these files are installed on
# disk by default — rebuilding kres refreshes the lot.
#
# Override directories (empty on a fresh install, honoured by the
# respective loaders when populated):
#
#   ~/.kres/system-prompts/<agent>.system.md
#     → override an agent system prompt. AgentConfig::load reads
#       this ahead of the embedded copy.
#
#   ~/.kres/commands/<name>.md
#     → override (or add) a non-workflow slash-command template.
#       Summary rendering consults summary / summary-markdown here.
#       Workflow-owned commands (/fix, /review, /triage) do not.
#
#   ~/.kres/workflows/<name>.json
#     → override a shipped workflow such as review, triage, or fix.
#
# These override directories are separate from the old
# ~/.kres/prompts/ tree. The rename prevents stale files from an
# earlier install shadowing embedded defaults after an upgrade —
# leftover files under ~/.kres/prompts/ are safe to delete.
#
# No shipped command templates are installed to ~/.kres/prompts/.

# Model configs. kres no longer auto-loads legacy
# ~/.kres/*-agent.json files; normal startup resolves each role to
# ~/.kres/models/<resolved-model-id>.json.
for src in "${CONFIGS_SRC}/models"/*.json; do
  install_model_config "$src" "${DEST}/models/$(basename "$src")"
done

# MCP registry: install mcp.json only when we actually have a
# semcode-mcp binary to point at. Decision order:
#   1. --semcode PATH given with a non-empty value → use that path
#      verbatim (even if the file doesn't exist, so the operator can
#      set up the binary afterwards without re-running setup.sh).
#   2. --semcode "" given → explicit skip.
#   3. No --semcode → check PATH; install with the bare name if
#      `semcode-mcp` resolves.
# When none of those hit, mcp.json is skipped entirely and the
# operator can drop in their own config later.
echo "mcp:"
SEMCODE_CMD=""
if [[ "${SEMCODE_FLAG_SEEN}" -eq 1 ]]; then
  if [[ -n "${SEMCODE_ARG}" ]]; then
    SEMCODE_CMD="${SEMCODE_ARG}"
    say "semcode: using explicit --semcode path ${SEMCODE_CMD}"
  else
    say "semcode: --semcode \"\" passed; skipping mcp.json"
  fi
else
  if command -v semcode-mcp >/dev/null 2>&1; then
    SEMCODE_CMD="semcode-mcp"
    say "semcode: found semcode-mcp on PATH; installing mcp.json"
  else
    say "semcode: semcode-mcp not on PATH; skipping mcp.json (pass --semcode PATH to override)"
  fi
fi
if [[ -n "${SEMCODE_CMD}" ]]; then
  mcp_dst="${DEST}/mcp.json"
  if [[ -e "${mcp_dst}" ]] && [[ "${OVERWRITE}" -ne 1 ]]; then
    say "keep: ${mcp_dst}"
  else
    mcp_tmp="$(mktemp "${mcp_dst}.tmp.XXXXXX")"
    awk -v cmd="${SEMCODE_CMD}" '
      BEGIN { replaced = 0 }
      {
        # Replace the first "command": "…" value with cmd. Preserve
        # anything before and after the match so a future trailing
        # comma or extra fields on the same line survive.
        if (!replaced && match($0, /"command"[[:space:]]*:[[:space:]]*"[^"]*"/)) {
          prefix = substr($0, 1, RSTART - 1)
          suffix = substr($0, RSTART + RLENGTH)
          print prefix "\"command\": \"" cmd "\"" suffix
          replaced = 1
          next
        }
        print
      }
    ' "${CONFIGS_SRC}/mcp.json" > "${mcp_tmp}"
    mv "${mcp_tmp}" "${mcp_dst}"
    chmod 0644 "${mcp_dst}"
    say "wrote: ${mcp_dst} (command=${SEMCODE_CMD})"
  fi
fi

# Per-user settings — default model ids per agent role. kres reads
# ~/.kres/settings.json on every start. The shipped file has two
# placeholder tokens (@SLOW_MODEL@ for the slow role, @MODEL@ for
# fast/main/todo); the classifier role is pinned in the shipped file
# to the Haiku config. We substitute placeholders with --slow /
# --model values (or the built-in defaults).
settings_dst="${DEST}/settings.json"
if [[ -e "${settings_dst}" ]] && [[ "${OVERWRITE}" -ne 1 ]]; then
  say "keep: ${settings_dst}"
else
  settings_tmp="$(mktemp "${settings_dst}.tmp.XXXXXX")"
  awk \
    -v slow_ph="@SLOW_MODEL@" -v slow_val="${SLOW_MODEL}" \
    -v reg_ph="@MODEL@" -v reg_val="${MODEL}" \
    '
    function subst(line, ph, val,    out, i, lp) {
      lp = length(ph)
      out = ""
      while ((i = index(line, ph)) > 0) {
        out  = out substr(line, 1, i - 1) val
        line = substr(line, i + lp)
      }
      return out line
    }
    {
      line = subst($0, slow_ph, slow_val)
      line = subst(line, reg_ph, reg_val)
      print line
    }
    ' "${CONFIGS_SRC}/settings.json" > "${settings_tmp}"
  mv "${settings_tmp}" "${settings_dst}"
  chmod 0644 "${settings_dst}"
  say "wrote: ${settings_dst} (slow=${SLOW_MODEL}, model=${MODEL})"
fi

# Kernel skill: carries an @REVIEW_PROMPTS@ placeholder that we
# substitute with the path to a kernel review-prompts tree.
# Decision order:
#   1. --review-prompts PATH → use verbatim.
#   2. ~/.claude/skills/kernel/SKILL.md → extract the first path that
#      looks like a review-prompts root (strip /kernel/... suffix).
#   3. Nothing → don't install the kernel skill; explain how.
REVIEW_PROMPTS_PATH=""
REVIEW_PROMPTS_SRC=""
if [[ "${REVIEW_PROMPTS_FLAG_SEEN}" -eq 1 ]] && [[ -n "${REVIEW_PROMPTS_ARG}" ]]; then
  REVIEW_PROMPTS_PATH="${REVIEW_PROMPTS_ARG}"
  REVIEW_PROMPTS_SRC="--review-prompts"
else
  claude_skill="${HOME}/.claude/skills/kernel/SKILL.md"
  if [[ -r "${claude_skill}" ]]; then
    # Pull out the longest leading path ending in /review-prompts,
    # ignoring anything under kernel/ (we want the root). First hit
    # wins. grep's -o gives us just the matched path.
    hit=$(grep -oE '[^ `"'"'"']*review-prompts' "${claude_skill}" | head -n 1 || true)
    if [[ -n "${hit}" ]]; then
      # Ask the operator to confirm before we bake an auto-detected
      # path into the installed skill — the SKILL.md may have stale
      # or wrong locations. Only ask when stdin is a tty; in a
      # non-interactive setup (CI, piped input) we refuse to guess
      # and point at --review-prompts instead.
      if [[ -t 0 ]]; then
        echo "setup.sh: found a review-prompts path in ${claude_skill}:"
        echo "    ${hit}"
        printf "Use this path for the kernel skill's @REVIEW_PROMPTS@? [Y/n] "
        answer=""
        read -r answer || answer=""
        case "${answer}" in
          ""|y|Y|yes|YES)
            REVIEW_PROMPTS_PATH="${hit}"
            REVIEW_PROMPTS_SRC="${claude_skill}"
            ;;
          *)
            echo "setup.sh: declined. Pass --review-prompts PATH to specify one."
            ;;
        esac
      else
        echo "setup.sh: found ${hit} in ${claude_skill} but stdin is not a tty; not guessing. Pass --review-prompts PATH to confirm it." >&2
      fi
    fi
  fi
fi

echo "skills:"
if [[ ! -d "${SKILLS_SRC}" ]]; then
  say "(no skills/ directory in source tree)"
else
  if [[ -z "${REVIEW_PROMPTS_PATH}" ]]; then
    say "kernel skill NOT installed: review-prompts directory could not be located."
    say "  Provide it with --review-prompts PATH (e.g. /home/you/local/src/review-prompts),"
    say "  or populate ~/.claude/skills/kernel/SKILL.md with a reference to your"
    say "  review-prompts tree and re-run setup.sh."
  else
    say "kernel skill: @REVIEW_PROMPTS@ = ${REVIEW_PROMPTS_PATH} (from ${REVIEW_PROMPTS_SRC})"
  fi
  shopt -s nullglob
  for s in "${SKILLS_SRC}"/*.md; do
    bn="$(basename "$s")"
    dst="${DEST}/skills/${bn}"
    if [[ "${bn}" == "kernel.md" ]]; then
      if [[ -z "${REVIEW_PROMPTS_PATH}" ]]; then
        continue
      fi
      if [[ -e "${dst}" ]] && [[ "${OVERWRITE}" -ne 1 ]]; then
        say "keep: ${dst}"
        continue
      fi
      tmp="$(mktemp "${dst}.tmp.XXXXXX")"
      awk -v ph="@REVIEW_PROMPTS@" -v val="${REVIEW_PROMPTS_PATH}" '
        BEGIN { lp = length(ph) }
        {
          line = $0
          out = ""
          while ((i = index(line, ph)) > 0) {
            out  = out substr(line, 1, i - 1) val
            line = substr(line, i + lp)
          }
          print out line
        }
      ' "$s" > "$tmp"
      mv "$tmp" "$dst"
      chmod 0644 "$dst"
      say "wrote: ${dst} (@REVIEW_PROMPTS@=${REVIEW_PROMPTS_PATH})"
    else
      install_file "$s" "${dst}"
    fi
  done
  shopt -u nullglob
fi

echo "done."
if [[ -z "${FAST_KEY_VALUE}" ]] || [[ -z "${SLOW_KEY_VALUE}" ]]; then
  echo
  echo "note: one or both model-config placeholders were not substituted."
  echo "      edit ${DEST}/models/*.json and replace @FAST_KEY@ / @SLOW_KEY@ with"
  echo "      your API key strings, or re-run setup.sh with --fast-key /"
  echo "      --slow-key and literal key values."
fi
