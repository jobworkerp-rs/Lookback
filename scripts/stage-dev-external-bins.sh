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

MIGRATION_LOCK_DIR=""
MIGRATION_STAGING=""
MIGRATION_BUILD_ROOT=""
MIGRATION_BACKUP=""
MIGRATION_DESTINATION=""

cleanup_migration_stage() {
  if [[ -n "${MIGRATION_BACKUP}" && -e "${MIGRATION_BACKUP}" ]]; then
    if [[ -n "${MIGRATION_DESTINATION}" && ! -e "${MIGRATION_DESTINATION}" ]]; then
      mv "${MIGRATION_BACKUP}" "${MIGRATION_DESTINATION}" 2>/dev/null || true
    else
      rm -rf "${MIGRATION_BACKUP}"
    fi
  fi
  [[ -z "${MIGRATION_STAGING}" ]] || rm -rf "${MIGRATION_STAGING}"
  [[ -z "${MIGRATION_BUILD_ROOT}" ]] || rm -rf "${MIGRATION_BUILD_ROOT}"
  if [[ -n "${MIGRATION_LOCK_DIR}" && -d "${MIGRATION_LOCK_DIR}" ]]; then
    local owner=""
    [[ ! -f "${MIGRATION_LOCK_DIR}/owner-pid" ]] || owner=$(cat "${MIGRATION_LOCK_DIR}/owner-pid")
    [[ "${owner}" != "$$" ]] || rm -rf "${MIGRATION_LOCK_DIR}"
  fi
}
trap cleanup_migration_stage EXIT
trap 'exit 130' INT TERM

acquire_migration_lock() {
  local lock=$1 attempts=0 owner=""
  while ! mkdir "${lock}" 2>/dev/null; do
    attempts=$((attempts + 1))
    if [[ -f "${lock}/owner-pid" ]]; then
      owner=$(cat "${lock}/owner-pid" 2>/dev/null || true)
      if [[ "${owner}" =~ ^[0-9]+$ ]] && ! kill -0 "${owner}" 2>/dev/null; then
        rm -rf "${lock}"
        continue
      fi
    elif (( attempts >= 5 )); then
      # A process can die between mkdir and writing owner-pid. Only rmdir an
      # empty lock so a concurrently-published owner file wins the race.
      rmdir "${lock}" 2>/dev/null || true
    fi
    if (( attempts >= 300 )); then
      die "timed out waiting for migration bundle staging lock ${lock}; retry after the other dev launch finishes"
    fi
    sleep 0.2
  done
  MIGRATION_LOCK_DIR=${lock}
  printf '%s\n' "$$" >"${lock}/owner-pid"
}

stage_migration_bundle() {
  local memories_repo="${AGENT_APP_PARENT}/memories"
  local default_source="${memories_repo}/target/memories-db-migrate-sqlite"
  local build_script="${memories_repo}/scripts/build-memories-db-migrate-sqlite.sh"
  local destination="${AGENT_APP}/src-tauri/migration-bundle"
  local release_source="${AGENT_APP}/src-tauri/src/sidecar/migration_gate.rs"
  local explicit=0 source=""
  [[ -f "${release_source}" ]] || release_source="${SCRIPT_DIR}/../src-tauri/src/sidecar/migration_gate.rs"

  if [[ "${LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE+x}" == "x" ]]; then
    explicit=1
    source=${LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE}
    node "${SCRIPT_DIR}/verify-migration-release.mjs" "${release_source}" "${source}" \
      || die "explicit LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE is invalid: ${source}"
  elif [[ -e "${default_source}" ]]; then
    if node "${SCRIPT_DIR}/verify-migration-release.mjs" "${release_source}" "${default_source}" >/dev/null 2>&1; then
      source=${default_source}
    else
      warn "default migration bundle is invalid; resolving a verified replacement"
    fi
  fi

  if [[ "${DRY_RUN}" == "1" ]]; then
    if [[ -n "${source}" ]]; then
      printf 'would stage migration bundle %s -> %s\n' "${source}" "${destination}"
    elif [[ "${explicit}" == "0" ]]; then
      [[ -x "${build_script}" ]] || die "official migration bundle builder not found at ${build_script}"
      printf 'would build migration bundle with %s in a temporary directory\n' "${build_script}"
      printf 'would verify and stage generated migration bundle -> %s\n' "${destination}"
    fi
    return
  fi

  mkdir -p "${AGENT_APP}/src-tauri"
  MIGRATION_DESTINATION=${destination}
  acquire_migration_lock "${destination}.lock"

  if [[ "${explicit}" == "0" && -z "${source}" && -e "${destination}" ]] \
    && node "${SCRIPT_DIR}/verify-migration-release.mjs" "${release_source}" "${destination}" >/dev/null 2>&1; then
    log "reuse verified staged migration bundle: ${destination}"
    return
  fi

  if [[ -z "${source}" ]]; then
    [[ -x "${build_script}" ]] || die "official migration bundle builder not found at ${build_script}"
    MIGRATION_BUILD_ROOT=$(mktemp -d "${AGENT_APP}/src-tauri/.migration-bundle-build.XXXXXX")
    source="${MIGRATION_BUILD_ROOT}/bundle"
    "${build_script}" "${source}" \
      || die "official migration bundle build failed: ${build_script}"
    node "${SCRIPT_DIR}/verify-migration-release.mjs" "${release_source}" "${source}" \
      || die "generated migration bundle failed release verification"
  fi

  if [[ "${source}" == "${destination}" ]]; then
    log "migration bundle already staged and verified: ${destination}"
    return
  fi

  MIGRATION_STAGING=$(mktemp -d "${AGENT_APP}/src-tauri/.migration-bundle-stage.XXXXXX")
  cp -R "${source}/." "${MIGRATION_STAGING}/"
  node "${SCRIPT_DIR}/verify-migration-release.mjs" "${release_source}" "${MIGRATION_STAGING}"

  if [[ -e "${destination}" ]]; then
    MIGRATION_BACKUP=$(mktemp -d "${AGENT_APP}/src-tauri/.migration-bundle-previous.XXXXXX")
    rmdir "${MIGRATION_BACKUP}"
    mv "${destination}" "${MIGRATION_BACKUP}"
  fi
  if ! mv "${MIGRATION_STAGING}" "${destination}"; then
    [[ -z "${MIGRATION_BACKUP}" || ! -e "${MIGRATION_BACKUP}" ]] \
      || mv "${MIGRATION_BACKUP}" "${destination}" 2>/dev/null || true
    die "could not activate verified migration bundle at ${destination}"
  fi
  MIGRATION_STAGING=""
  [[ -z "${MIGRATION_BACKUP}" ]] || rm -rf "${MIGRATION_BACKUP}"
  MIGRATION_BACKUP=""
}

stage_migration_bundle
# protoc: default to the official self-contained release binary. A developer can
# still point PROTOC at their own self-contained protoc to skip the download.
if [[ -n "${PROTOC:-}" ]]; then
  stage_one "$(resolve_required_bin protoc PROTOC protoc ../protobuf/bin/protoc)" protoc
else
  fetch_protoc_bin "${TRIPLE}" "${BIN_DIR}/protoc-${TRIPLE}"
fi
stage_plugins

log "staged dev externalBin files for ${TRIPLE}"
