#!/usr/bin/env bash
# Stage empty placeholder files so `cargo clippy` / `cargo test` can run without
# the real sidecar binaries and plugins.
#
# tauri-build validates that every `externalBin` (with the target-triple suffix)
# and every `resources` glob exists at build time — even for `cargo clippy` /
# `cargo test`, which compile the crate via build.rs but never execute the
# binaries. CI lint/test jobs don't run build-release.sh, so without these
# placeholders the build script fails with:
#   resource path `bin/all-in-one-<triple>` doesn't exist
#
# The placeholders are EMPTY: they satisfy the existence check only. Never use
# them for an actual bundle — the real artifacts come from build-release.sh.
#
# Usage: scripts/ci-stage-placeholders.sh [target-triple]
#   default triple: auto-detected for the host platform.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)

# Resolve the target triple (arg overrides auto-detect).
if [[ $# -ge 1 && -n "$1" ]]; then
  TRIPLE=$1
else
  case "$(uname -s)" in
    Darwin)
      case "$(uname -m)" in
        arm64|aarch64) TRIPLE=aarch64-apple-darwin ;;
        *)             TRIPLE=x86_64-apple-darwin ;;
      esac ;;
    Linux) TRIPLE=x86_64-unknown-linux-gnu ;;
    *) echo "unsupported host OS: $(uname -s)" >&2; exit 1 ;;
  esac
fi
LIBEXT=$([[ "${TRIPLE}" == *darwin ]] && echo dylib || echo so)

BIN_DIR="${AGENT_APP}/src-tauri/bin"
PLUGINS_DIR="${AGENT_APP}/src-tauri/plugins"
DICT_DIR="${AGENT_APP}/dict"

mkdir -p "${BIN_DIR}" "${PLUGINS_DIR}" "${DICT_DIR}"

# externalBin entries (tauri.conf.json) with the platform-triple suffix.
for name in all-in-one front conductor-main memories-import protoc; do
  dest="${BIN_DIR}/${name}-${TRIPLE}"
  [[ -e "${dest}" ]] || : > "${dest}"
done

# One placeholder so the platform plugin glob (tauri.<platform>.conf.json)
# matches at least one file. Real plugins overwrite/join these later.
placeholder="${PLUGINS_DIR}/libplaceholder_ci.${LIBEXT}"
[[ -e "${placeholder}" ]] || : > "${placeholder}"

echo "staged CI placeholders for ${TRIPLE} (bin/*, plugins/*.${LIBEXT}*, dict/)" >&2
