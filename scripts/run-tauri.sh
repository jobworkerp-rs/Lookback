#!/usr/bin/env bash
# Keep local Tauri dev aligned with the current external sidecar/plugin
# artifacts, and require strict migration provenance for every packaging build.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP="${LOOKBACK_AGENT_APP:-$(cd "${SCRIPT_DIR}/.." && pwd)}"

host_triple() {
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64) printf '%s\n' aarch64-apple-darwin ;;
    Darwin:x86_64) printf '%s\n' x86_64-apple-darwin ;;
    Linux:x86_64|Linux:amd64) printf '%s\n' x86_64-unknown-linux-gnu ;;
    Linux:arm64|Linux:aarch64) printf '%s\n' aarch64-unknown-linux-gnu ;;
    MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64) printf '%s\n' x86_64-pc-windows-msvc ;;
    MINGW*:aarch64|MSYS*:aarch64|CYGWIN*:aarch64) printf '%s\n' aarch64-pc-windows-msvc ;;
    *) printf '%s\n' "" ;;
  esac
}

requested_target() {
  [[ $# -gt 0 ]] || return 0
  shift
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --target) [[ $# -ge 2 ]] && printf '%s\n' "$2"; return ;;
      --target=*) printf '%s\n' "${1#--target=}"; return ;;
    esac
    shift
  done
}

COMMAND="${1:-}"
TARGET="$(requested_target "$@" || true)"
HOST_TARGET="$(host_triple)"
if [[ -n "${TARGET}" && "${TARGET}" != "${HOST_TARGET}" && "${LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED:-0}" != "1" ]]; then
  echo "cross-target Tauri builds require target-specific pre-staged sidecars and plugins; set LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1 after staging ${TARGET} artifacts" >&2
  exit 2
fi

if [[ "${COMMAND}" == "dev" ]]; then
  stage_args=()
  [[ -z "${TARGET}" ]] || stage_args=(--triple "${TARGET}")
  bash "${SCRIPT_DIR}/stage-dev-external-bins.sh" "${stage_args[@]}"
fi

case "${COMMAND}" in
  build|bundle)
    verify_args=()
    [[ -z "${TARGET}" ]] || verify_args=(--triple "${TARGET}")
    bash "${SCRIPT_DIR}/verify-packaging-migration-provenance.sh" "${verify_args[@]}"
    ;;
esac

if [[ "${COMMAND}" == "dev" ]]; then
  if [[ "$(uname -s)" == "Linux" ]]; then
    export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"
    export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"
  fi
fi

if [[ "${COMMAND}" == "build" && "$(uname -s)" == "Linux" ]]; then
  # linuxdeploy's bundled strip cannot parse modern ELF RELR relocations.
  # The system libraries it copies are already stripped, so skipping that
  # redundant pass preserves AppImage bundling on current Linux hosts.
  export NO_STRIP=true
  # Tauri searches this project-local tool cache before invoking linuxdeploy.
  # Keep the GTK hook under version control so a freshly created cache does
  # not regress to linuxdeploy's X11-only default on Wayland sessions.
  tools_dir="${AGENT_APP}/target/.tauri"
  mkdir -p "${tools_dir}"
  install -m755 "${SCRIPT_DIR}/assets/linuxdeploy-plugin-gtk.sh" \
    "${tools_dir}/linuxdeploy-plugin-gtk.sh"
fi

exec tauri "$@"
