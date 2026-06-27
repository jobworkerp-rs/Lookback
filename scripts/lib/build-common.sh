#!/usr/bin/env bash
# Shared helpers for the Lookback release build scripts.
#
# Sourced (not executed) by scripts/build-release.sh and its sub-libs.
# Provides logging, a command runner that honours DRY_RUN, prerequisite
# checks, platform-triple detection, and an idempotent file installer.

# Logging -------------------------------------------------------------------
# Colours degrade gracefully when stdout is not a TTY.
if [[ -t 1 ]]; then
  _C_RESET=$'\033[0m'; _C_INFO=$'\033[36m'; _C_WARN=$'\033[33m'; _C_ERR=$'\033[31m'
else
  _C_RESET=""; _C_INFO=""; _C_WARN=""; _C_ERR=""
fi

log()  { printf '%s==>%s %s\n' "${_C_INFO}" "${_C_RESET}" "$*" >&2; }
warn() { printf '%swarn:%s %s\n' "${_C_WARN}" "${_C_RESET}" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "${_C_ERR}" "${_C_RESET}" "$*" >&2; exit 1; }

# run COMMAND...
# Echoes the command and runs it, unless DRY_RUN=1 in which case it only
# echoes. Keeps every side-effecting step visible and dry-runnable.
run() {
  printf '%s   $%s %s\n' "${_C_INFO}" "${_C_RESET}" "$*" >&2
  [[ "${DRY_RUN:-0}" == "1" ]] && return 0
  "$@"
}

# require_cmd CMD...
# Fails with a single message listing everything missing.
require_cmd() {
  local missing=()
  local c
  for c in "$@"; do
    command -v "$c" >/dev/null 2>&1 || missing+=("$c")
  done
  ((${#missing[@]} == 0)) || die "missing required command(s): ${missing[*]}"
}

# detect_triple PLATFORM
# Echoes the Rust/Tauri target triple for the given platform on this host.
# Cross-arch builds are out of scope: the arch is taken from the host.
detect_triple() {
  local platform=$1
  case "${platform}" in
    mac)
      case "$(uname -m)" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64)        echo "x86_64-apple-darwin" ;;
        *) die "unsupported macOS arch: $(uname -m)" ;;
      esac
      ;;
    linux)
      # Only the gnu x86_64 target is supported for distribution today.
      echo "x86_64-unknown-linux-gnu"
      ;;
    *) die "unknown platform: ${platform}" ;;
  esac
}

# rustc_at_least MAJOR MINOR
# edition 2024 requires rustc >= 1.85.
rustc_at_least() {
  local want_major=$1 want_minor=$2 ver major minor
  ver=$(rustc --version | awk '{print $2}')
  major=${ver%%.*}
  minor=${ver#*.}; minor=${minor%%.*}
  ((major > want_major)) && return 0
  ((major == want_major && minor >= want_minor)) && return 0
  return 1
}

# install_file SRC DEST
# Idempotent copy preserving exec bit. Skips when DEST already has identical
# bytes so re-runs are cheap and leave mtimes untouched.
install_file() {
  local src=$1 dest=$2
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    run install -m755 "${src}" "${dest}"
    return 0
  fi
  [[ -f "${src}" ]] || die "expected build output not found: ${src}"
  if [[ -f "${dest}" ]] && cmp -s "${src}" "${dest}"; then
    log "unchanged: ${dest}"
    return 0
  fi
  run install -m755 "${src}" "${dest}"
}
