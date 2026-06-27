#!/usr/bin/env bash
set -euo pipefail

ROOT=""
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: scripts/ci-free-disk-space.sh [--root DIR] [--dry-run]

Free disk space on GitHub-hosted Ubuntu runners before large release builds.

Options:
  --root DIR   Prefix cleanup targets with DIR. Intended for tests.
  --dry-run    Print the actions without deleting files.
  -h, --help   Show this help.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      [[ $# -ge 2 ]] || { echo "--root requires a directory" >&2; exit 2; }
      ROOT="${2%/}"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -n "${ROOT}" && "${ROOT}" != /* ]]; then
  echo "--root must be an absolute path" >&2
  exit 2
fi

cleanup_paths=(
  /usr/share/dotnet
  /usr/local/lib/android
  /usr/local/share/boost
  /usr/local/share/powershell
  /opt/ghc
  /opt/hostedtoolcache/CodeQL
  /opt/hostedtoolcache/go
  /opt/hostedtoolcache/Python
  /opt/hostedtoolcache/Ruby
)

if [[ -n "${AGENT_TOOLSDIRECTORY:-}" && "${AGENT_TOOLSDIRECTORY}" == /* ]]; then
  cleanup_paths+=("${AGENT_TOOLSDIRECTORY}")
fi

target_path() {
  local path="$1"
  if [[ -n "${ROOT}" ]]; then
    printf '%s%s\n' "${ROOT}" "${path}"
  else
    printf '%s\n' "${path}"
  fi
}

run_privileged() {
  if [[ "${DRY_RUN}" == "1" ]]; then
    printf 'dry-run: %q' "$1"
    shift
    for arg in "$@"; do
      printf ' %q' "${arg}"
    done
    printf '\n'
    return 0
  fi

  if [[ "${EUID}" -eq 0 || -n "${ROOT}" ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

show_disk() {
  local label="$1"
  echo "::group::Disk usage (${label})"
  df -h /
  if [[ -d /mnt ]]; then
    df -h /mnt || true
  fi
  echo "::endgroup::"
}

remove_path() {
  local original="$1"
  local target
  target="$(target_path "${original}")"

  if [[ ! -e "${target}" && ! -L "${target}" ]]; then
    echo "skip missing ${original}"
    return 0
  fi

  echo "::group::Remove ${original}"
  du -sh "${target}" 2>/dev/null || true
  run_privileged rm -rf "${target}"
  echo "::endgroup::"
}

show_disk before

for path in "${cleanup_paths[@]}"; do
  remove_path "${path}"
done

if [[ -z "${ROOT}" ]]; then
  if command -v docker >/dev/null 2>&1; then
    echo "::group::Prune Docker data"
    run_privileged docker system prune -af || true
    echo "::endgroup::"
  fi

  if command -v apt-get >/dev/null 2>&1; then
    echo "::group::Clean apt cache"
    run_privileged apt-get clean || true
    echo "::endgroup::"
  fi

  if [[ -f /mnt/swapfile ]]; then
    echo "::group::Remove swapfile"
    run_privileged swapoff /mnt/swapfile || true
    run_privileged rm -f /mnt/swapfile || true
    echo "::endgroup::"
  fi
fi

show_disk after
