#!/usr/bin/env bash
# Validate the migration bundle and the exact Memories binaries Tauri will
# package. This is intentionally stricter than the development staging check.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=${LOOKBACK_AGENT_APP:-$(cd "${SCRIPT_DIR}/.." && pwd)}
TRIPLE=""

usage() {
  cat >&2 <<'EOF'
Usage: scripts/verify-packaging-migration-provenance.sh [--triple TARGET]

Verifies the staged migration bundle and its target-specific front and
memories-import artifacts using the required source provenance manifest.
Without --triple, the target is resolved from Tauri's target environment or
the current host.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --triple|--target)
      [[ $# -ge 2 && -n "$2" ]] || { usage; exit 2; }
      TRIPLE=$2
      shift 2
      ;;
    --triple=*|--target=*)
      TRIPLE=${1#*=}
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage
      exit 2
      ;;
  esac
done

normalize_arch() {
  case "$1" in
    arm64|aarch64) printf '%s\n' aarch64 ;;
    amd64|x86_64) printf '%s\n' x86_64 ;;
    i386|i486|i586|i686) printf '%s\n' i686 ;;
    *) return 1 ;;
  esac
}

triple_for_platform() {
  local platform=$1 arch
  arch=$(normalize_arch "$2") || {
    echo "unsupported target architecture for packaging: $2" >&2
    return 1
  }
  case "$platform" in
    darwin|macos|mac)
      case "$arch" in
        aarch64|x86_64) printf '%s-apple-darwin\n' "$arch" ;;
        *) echo "unsupported macOS packaging architecture: $arch" >&2; return 1 ;;
      esac
      ;;
    linux)
      printf '%s-unknown-linux-gnu\n' "$arch"
      ;;
    windows|win32)
      printf '%s-pc-windows-msvc\n' "$arch"
      ;;
    *)
      echo "unsupported packaging platform: $platform" >&2
      return 1
      ;;
  esac
}

host_triple() {
  local system arch
  system=$(uname -s)
  arch=$(uname -m)
  case "$system" in
    Darwin)
      triple_for_platform darwin "$arch"
      ;;
    Linux)
      triple_for_platform linux "$arch"
      ;;
    MINGW*|MSYS*|CYGWIN*)
      triple_for_platform windows "$arch"
      ;;
    *)
      echo "unsupported packaging host platform: $system" >&2
      return 1
      ;;
  esac
}

if [[ -z "$TRIPLE" && -n "${TAURI_ENV_PLATFORM:-}" && -n "${TAURI_ENV_ARCH:-}" ]]; then
  TRIPLE=$(triple_for_platform "${TAURI_ENV_PLATFORM}" "${TAURI_ENV_ARCH}")
fi
if [[ -z "$TRIPLE" && -n "${CARGO_BUILD_TARGET:-}" ]]; then
  TRIPLE=${CARGO_BUILD_TARGET}
fi
if [[ -z "$TRIPLE" ]]; then
  TRIPLE=$(host_triple)
fi

# Target triples become filenames. Reject path syntax rather than allowing an
# environment override to make the verifier inspect files outside src-tauri.
if [[ "$TRIPLE" == *'/'* || "$TRIPLE" == *\\* || -z "$TRIPLE" ]]; then
  echo "invalid packaging target triple: ${TRIPLE}" >&2
  exit 2
fi

BUNDLE=${AGENT_APP}/src-tauri/migration-bundle
TARGET_BIN_SUFFIX=""
case "${TRIPLE}" in
  *-windows-*) TARGET_BIN_SUFFIX=.exe ;;
esac
FRONT=${AGENT_APP}/src-tauri/bin/front-${TRIPLE}${TARGET_BIN_SUFFIX}
IMPORT=${AGENT_APP}/src-tauri/bin/memories-import-${TRIPLE}${TARGET_BIN_SUFFIX}

if ! node "${SCRIPT_DIR}/verify-migration-release.mjs" "${BUNDLE}" --require-provenance \
  --app-assets "${AGENT_APP}" \
  --front "${FRONT}" --import "${IMPORT}"; then
  echo "Packaging requires a provenance-verified pinned migration bundle and matching staged Memories binaries for ${TRIPLE}." >&2
  echo "Regenerate/re-stage the pinned release inputs for ${TRIPLE}, then retry; development-only staging does not satisfy this check." >&2
  exit 1
fi

echo "Packaging migration provenance verified for ${TRIPLE}."
