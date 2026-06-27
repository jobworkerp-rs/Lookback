#!/usr/bin/env bash
# Stage real local binaries with the target-triple suffix required by Tauri
# externalBin validation during `tauri dev`.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=${LOOKBACK_AGENT_APP:-$(cd "${SCRIPT_DIR}/.." && pwd)}
TRIPLE=""
DRY_RUN=0

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/protoc-fetch.sh
source "${SCRIPT_DIR}/lib/protoc-fetch.sh"

usage() {
  cat >&2 <<'EOF'
Usage: scripts/stage-dev-external-bins.sh [options]

Options:
  --agent-app DIR   Agent app repository root (default: auto-detected)
  --triple TRIPLE   Target triple (default: host triple)
  --dry-run         Print actions without writing files
  -h, --help        Show this help

Resolution order mirrors the app's runtime sidecar lookup:
  env override, PATH, workspace-relative fallback.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --agent-app) AGENT_APP=$2; shift 2 ;;
    --triple) TRIPLE=$2; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

if [[ -z "${TRIPLE}" ]]; then
  case "$(uname -s)" in
    Darwin) TRIPLE=$(detect_triple mac) ;;
    Linux) TRIPLE=$(detect_triple linux) ;;
    *) die "unsupported host OS: $(uname -s)" ;;
  esac
fi

BIN_DIR="${AGENT_APP}/src-tauri/bin"
AGENT_APP_PARENT=$(cd "$(dirname "${AGENT_APP}")" && pwd)
AGENT_APP_WORKSPACE=$(cd "$(dirname "${AGENT_APP_PARENT}")" && pwd)
case "${TRIPLE}" in
  *darwin) LIBEXT=dylib ;;
  *) LIBEXT=so ;;
esac
PLUGINS_DIR="${AGENT_APP}/src-tauri/plugins"

resolve_required_bin() {
  local label=$1 env_var=$2 path_name=$3 fallback_rel=$4
  local candidate=""

  if [[ -n "${!env_var:-}" ]]; then
    candidate=${!env_var}
  elif candidate=$(command -v "${path_name}" 2>/dev/null); then
    :
  else
    candidate="${AGENT_APP}/${fallback_rel}"
  fi

  [[ -f "${candidate}" ]] || die "${label} not found at ${candidate}; set ${env_var}"
  printf '%s\n' "${candidate}"
}

stage_one() {
  local src=$1 dest_name=$2
  local dest="${BIN_DIR}/${dest_name}-${TRIPLE}"
  if [[ "${DRY_RUN}" == "1" ]]; then
    printf 'would stage %s -> %s\n' "${src}" "${dest}"
    return 0
  fi
  mkdir -p "${BIN_DIR}"
  install_file "${src}" "${dest}"
}

plugin_sources() {
  if [[ -n "${LOOKBACK_PLUGINS_SRC:-}" ]]; then
    printf '%s\n' "${LOOKBACK_PLUGINS_SRC}"
    return 0
  fi
  printf '%s\n' "${AGENT_APP_WORKSPACE}/plugins/cuda_runner"
  printf '%s\n' "${AGENT_APP_WORKSPACE}/plugins"
  printf '%s\n' "${AGENT_APP_PARENT}/plugins"
}

stage_plugins() {
  local staged=0 source file dest
  if [[ "${DRY_RUN}" != "1" ]]; then
    mkdir -p "${PLUGINS_DIR}"
  fi
  while IFS= read -r source; do
    [[ -d "${source}" ]] || continue
    while IFS= read -r file; do
      [[ -f "${file}" ]] || continue
      dest="${PLUGINS_DIR}/$(basename "${file}")"
      if [[ "$(cd "$(dirname "${file}")" && pwd)/$(basename "${file}")" == "${dest}" ]]; then
        staged=1
        continue
      fi
      if [[ "${DRY_RUN}" == "1" ]]; then
        printf 'would stage plugin %s -> %s\n' "${file}" "${dest}"
      else
        install_file "${file}" "${dest}"
      fi
      staged=1
    done < <(find "${source}" -type f -name "*.${LIBEXT}" 2>/dev/null)
  done < <(plugin_sources)

  if [[ "${staged}" != "1" ]]; then
    die "no *.${LIBEXT} plugins found; set LOOKBACK_PLUGINS_SRC or place them under ../../plugins/cuda_runner/ from agent-app"
  fi
}

stage_one "$(resolve_required_bin all-in-one LOOKBACK_JOBWORKERP_BIN all-in-one ../target/release/all-in-one)" all-in-one
stage_one "$(resolve_required_bin front LOOKBACK_MEMORIES_BIN memories-front ../memories/target/release/front)" front
stage_one "$(resolve_required_bin conductor-main LOOKBACK_CONDUCTOR_BIN conductor-main ../conductor/target/release/conductor-main)" conductor-main
stage_one "$(resolve_required_bin memories-import LOOKBACK_MEMORIES_IMPORT_BIN memories-import ../memories/target/release/memories-import)" memories-import
# protoc: default to the official self-contained release binary. A developer can
# still point PROTOC at their own self-contained protoc to skip the download.
if [[ -n "${PROTOC:-}" ]]; then
  stage_one "$(resolve_required_bin protoc PROTOC protoc ../protobuf/bin/protoc)" protoc
else
  fetch_protoc_bin "${TRIPLE}" "${BIN_DIR}/protoc-${TRIPLE}"
fi
stage_plugins

log "staged dev externalBin files for ${TRIPLE}"
