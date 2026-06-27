#!/usr/bin/env bash
# Keep `pnpm tauri dev` compatible with Tauri's externalBin validation without
# changing the release build path.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

if [[ "${1:-}" == "dev" ]]; then
  bash "${SCRIPT_DIR}/stage-dev-external-bins.sh"
  if [[ "$(uname -s)" == "Linux" ]]; then
    export GDK_BACKEND="${GDK_BACKEND:-x11}"
    export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"
  fi
fi

exec tauri "$@"
