#!/usr/bin/env bash
#
# setup.sh — initialize a kres config directory.
#
# Copies the agent configs, system prompts, and skills shipped in this
# repo into the destination directory (default ~/.kres) and substitutes
# the slow/fast API keys into each config file. Existing destination
# files are left untouched unless --overwrite is passed.
#
# Usage:
#   setup.sh [--dest DIR] [--slow-key KEY] [--fast-key KEY] [--overwrite]
#
# --slow-key / --fast-key accept either a path to an existing key file
# (whose trimmed contents become the key) or a literal key string.
# The argument's value is substituted inline for the @SLOW_KEY@ /
# @FAST_KEY@ placeholder in the installed config JSON — no separate
# key files are written.
#
# --fast-key feeds fast-code-agent, main-agent, and todo-agent.
# --slow-key feeds every slow-code-agent variant (opus, sonnet).
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
  --slow-key KEY         Slow-agent API key (path to existing file OR literal)
  --fast-key KEY         Fast / main / todo agent API key (path OR literal)
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

say() { printf '  %s\n' "$*"; }

# resolve_key KEY — returns the literal API key string that should be
# substituted for @FAST_KEY@ / @SLOW_KEY@ placeholders. The argument
# may be either a path to an existing file (whose trimmed contents
# become the key) or a literal key string. An empty argument returns
# the empty string, signalling 'leave placeholder in place'.
resolve_key() {
  local val="$1"
  if [[ -z "$val" ]]; then
    printf ''
    return 0
  fi
  if [[ -f "$val" ]]; then
    # Trim leading/trailing whitespace (newlines especially) so a
    # multiline key file doesn't smuggle an unintended newline into
    # the installed JSON.
    awk 'BEGIN{ORS=""} {gsub(/^[[:space:]]+|[[:space:]]+$/,""); print}' "$val"
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

# install_config SRC DST PLACEHOLDER KEY_VALUE — copy a JSON config,
# substituting the literal token PLACEHOLDER (e.g. `@FAST_KEY@`) with
# KEY_VALUE. If KEY_VALUE is empty, the file is copied verbatim so
# the operator can edit it later. Same overwrite semantics as
# install_file.
install_config() {
  local src="$1" dst="$2" placeholder="$3" key_value="$4"
  if [[ ! -e "$src" ]]; then
    echo "error: source missing: $src" >&2
    return 1
  fi
  if [[ -e "$dst" ]] && [[ "${OVERWRITE}" -ne 1 ]]; then
    say "keep: ${dst}"
    return 0
  fi
  if [[ -z "${key_value}" ]]; then
    install -m 0644 "$src" "$dst"
    say "wrote: ${dst} (${placeholder} left in place — edit before running)"
    return 0
  fi
  # Literal string substitution via awk — no regex metachar concerns
  # on either side. jq would also work but isn't guaranteed present;
  # awk is in POSIX and handles every reasonable key value.
  local tmp
  tmp="$(mktemp "${dst}.tmp.XXXXXX")"
  awk -v ph="${placeholder}" -v val="${key_value}" '
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
  ' "$src" > "$tmp"
  mv "$tmp" "$dst"
  chmod 0640 "$dst"
  say "wrote: ${dst} (${placeholder}=$(redact "${key_value}"))"
}

echo "kres setup"
say "dest:         ${DEST}"
say "overwrite:    $([[ ${OVERWRITE} -eq 1 ]] && echo yes || echo no)"
say "slow key:     $(redact "${SLOW_KEY_VALUE}")"
say "fast key:     $(redact "${FAST_KEY_VALUE}")"
say "slow model:   ${SLOW_MODEL}"
say "model:        ${MODEL}"

echo "system prompts and agent configs:"
# Every prompt/template the shipped kres binary uses is embedded
# via include_str!: agent `*.system.md` prompts go through
# kres-agents::embedded_prompts, slash-command templates
# (/review, /summary, /summary-markdown) go through
# kres-agents::user_commands. None of these files are installed
# on disk by default — rebuilding kres refreshes the lot.
#
# Override directories (both empty on a fresh install, both
# honoured by the respective loaders when populated):
#
#   ~/.kres/system-prompts/<agent>.system.md
#     → override an agent system prompt. AgentConfig::load reads
#       this ahead of the embedded copy.
#
#   ~/.kres/commands/<name>.md
#     → override (or add) a slash-command template. Consulted by
#       user_commands::lookup, which drives --prompt "word: extra",
#       --prompt "/word extra", and the /review / /summary /
#       /summary-markdown REPL paths.
#
# Both override directories are new; older installs populated
# ~/.kres/prompts/ directly. The rename prevents stale files
# from an earlier install shadowing embedded defaults after an
# upgrade — leftover files under ~/.kres/prompts/ are safe to
# delete.
#
# The only files that still install to ~/.kres/prompts/ are
# operator-authored `<word>-template.md` templates used by the
# legacy --prompt "word: extra" lookup. Those are user content,
# not kres-shipped content.
mkdir -p "${DEST}/prompts"
shopt -s nullglob
for src in "${CONFIGS_SRC}/prompts"/*.md; do
  case "$(basename "$src")" in
    *.system.md | bug-summary.md | bug-summary-markdown.md | review-template.md | triage-template.md)
      # Embedded in the binary; skip.
      ;;
    *)
      install_file "$src" "${DEST}/prompts/$(basename "$src")"
      ;;
  esac
done
shopt -u nullglob

# Fast / main / todo agent configs → fast key (placeholder
# @FAST_KEY@).
install_config "${CONFIGS_SRC}/fast-code-agent.json" \
               "${DEST}/fast-code-agent.json" "@FAST_KEY@" "${FAST_KEY_VALUE}"
install_config "${CONFIGS_SRC}/main-agent.json" \
               "${DEST}/main-agent.json" "@FAST_KEY@" "${FAST_KEY_VALUE}"
install_config "${CONFIGS_SRC}/todo-agent.json" \
               "${DEST}/todo-agent.json" "@FAST_KEY@" "${FAST_KEY_VALUE}"

# Slow agent variants → slow key (placeholder @SLOW_KEY@). Two tags
# ship; kres picks one via `--slow <tag>` (default: sonnet).
for tag in opus sonnet; do
  install_config "${CONFIGS_SRC}/slow-code-agent-${tag}.json" \
                 "${DEST}/slow-code-agent-${tag}.json" \
                 "@SLOW_KEY@" "${SLOW_KEY_VALUE}"
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
# fast/main/todo); we substitute them with --slow / --model values
# (or the built-in defaults) the same way install_config handles
# API-key placeholders.
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
elif [[ -z "${REVIEW_PROMPTS_PATH}" ]]; then
  say "kernel skill NOT installed: review-prompts directory could not be located."
  say "  Provide it with --review-prompts PATH (e.g. /home/you/local/src/review-prompts),"
  say "  or populate ~/.claude/skills/kernel/SKILL.md with a reference to your"
  say "  review-prompts tree and re-run setup.sh."
else
  say "kernel skill: @REVIEW_PROMPTS@ = ${REVIEW_PROMPTS_PATH} (from ${REVIEW_PROMPTS_SRC})"
  shopt -s nullglob
  for s in "${SKILLS_SRC}"/*.md; do
    bn="$(basename "$s")"
    dst="${DEST}/skills/${bn}"
    if [[ "${bn}" == "kernel.md" ]]; then
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
  echo "note: one or both agent-config placeholders were not substituted."
  echo "      edit ${DEST}/*.json and replace @FAST_KEY@ / @SLOW_KEY@ with"
  echo "      your API key strings, or re-run setup.sh with --fast-key /"
  echo "      --slow-key (either a literal key or a path to a key file)."
fi
