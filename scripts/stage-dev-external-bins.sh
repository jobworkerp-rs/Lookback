#!/usr/bin/env bash
# Stage real local binaries with the target-triple suffix required by Tauri
# externalBin validation during `tauri dev`.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=${LOOKBACK_AGENT_APP:-$(cd "${SCRIPT_DIR}/.." && pwd)}
TRIPLE=""
TRIPLE_EXPLICIT=0
DRY_RUN=0

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/protoc-fetch.sh
source "${SCRIPT_DIR}/lib/protoc-fetch.sh"
# shellcheck source=lib/verified-migration-bundle.sh
source "${SCRIPT_DIR}/lib/verified-migration-bundle.sh"
# shellcheck source=lib/stage-coding-agent-package.sh
source "${SCRIPT_DIR}/lib/stage-coding-agent-package.sh"

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

`LOOKBACK_ISSUE_TRACKER_SERVER_BIN` may provide a prebuilt issue-tracker-server.
When it is unset, this script builds the sibling `issue-tracker-server` checkout
and stages its debug server binary. `LOOKBACK_ISSUE_TRACKER_SERVER_DIR` overrides
that checkout path; the former `LOOKBACK_ISSUE_TRACKER_RUNNER_DIR` is accepted
as a compatibility fallback.

`LOOKBACK_MEMORIES_SOURCE_DIR` can override the sibling Memories checkout. Its
embedding protobuf/workflow assets must match the app's explicitly mapped files.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --agent-app) AGENT_APP=$2; shift 2 ;;
    --triple) TRIPLE=$2; TRIPLE_EXPLICIT=1; shift 2 ;;
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
PLUGINS_DIR="${AGENT_APP}/src-tauri/plugins"
case "${TRIPLE}" in
  *darwin) LIBEXT=dylib ;;
  *) LIBEXT=so ;;
esac

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

resolve_issue_tracker_server() {
  local server_dir candidate
  if [[ -n "${LOOKBACK_ISSUE_TRACKER_SERVER_BIN:-}" ]]; then
    candidate="${LOOKBACK_ISSUE_TRACKER_SERVER_BIN}"
  else
    server_dir="${LOOKBACK_ISSUE_TRACKER_SERVER_DIR:-${LOOKBACK_ISSUE_TRACKER_RUNNER_DIR:-${AGENT_APP_PARENT}/issue-tracker-server}}"
    [[ -f "${server_dir}/Cargo.toml" ]] || die "issue-tracker-server source not found at ${server_dir}; set LOOKBACK_ISSUE_TRACKER_SERVER_BIN or LOOKBACK_ISSUE_TRACKER_SERVER_DIR"
    if [[ "${DRY_RUN}" == "1" ]]; then
      printf '%s\n' "${server_dir}/target/debug/issue-tracker-server"
      return 0
    fi
    cargo build --manifest-path "${server_dir}/Cargo.toml" -p issue-tracker-front --bin issue-tracker-server
    candidate="${server_dir}/target/debug/issue-tracker-server"
  fi
  [[ -f "${candidate}" ]] || die "issue-tracker-server not found at ${candidate}"
  printf '%s\n' "${candidate}"
}

stage_issue_tracker_server() {
  local source
  source=$(resolve_issue_tracker_server)
  if [[ "${DRY_RUN}" == "1" ]]; then
    printf 'would stage issue-tracker-server %s -> %s\n' "${source}" "${BIN_DIR}/issue-tracker-server-${TRIPLE}"
    return 0
  fi
  stage_one "${source}" issue-tracker-server
}

MIGRATION_LOCK_DIR=""
MIGRATION_STAGING=""
MIGRATION_BUILD_ROOT=""
MIGRATION_DESTINATION="${AGENT_APP}/src-tauri/migration-bundle"
MIGRATION_BACKUP="${AGENT_APP}/src-tauri/.migration-bundle-previous"
MIGRATION_VERIFIER="${SCRIPT_DIR}/verify-migration-release.mjs"

cleanup_migration_stage() {
  if [[ -n "${MIGRATION_BACKUP}" && -n "${MIGRATION_DESTINATION}" \
    && -n "${MIGRATION_LOCK_DIR}" ]]; then
    migration_bundle_recover_previous "${MIGRATION_DESTINATION}" "${MIGRATION_BACKUP}" \
      "${MIGRATION_VERIFIER}" 0 \
      || warn "migration bundle interrupted state was preserved for manual recovery"
  fi
  [[ -z "${MIGRATION_STAGING}" ]] || rm -rf "${MIGRATION_STAGING}"
  [[ -z "${MIGRATION_BUILD_ROOT}" ]] || rm -rf "${MIGRATION_BUILD_ROOT}"
  [[ -z "${MIGRATION_LOCK_DIR}" ]] || migration_bundle_lock_release "${MIGRATION_LOCK_DIR}"
}
trap cleanup_migration_stage EXIT
trap 'exit 130' INT TERM

acquire_migration_lock() {
  local lock=$1
  migration_bundle_lock_acquire "${lock}" "${MIGRATION_DESTINATION}" \
    "${MIGRATION_BACKUP}" "${MIGRATION_VERIFIER}" 0 \
    || die "could not acquire or recover migration bundle staging lock ${lock}"
  MIGRATION_LOCK_DIR=${lock}
}

prepare_migration_staging() {
  [[ "${DRY_RUN}" != "1" ]] || return 0
  mkdir -p "${AGENT_APP}/src-tauri"
  acquire_migration_lock "${MIGRATION_DESTINATION}.lock"
  migration_bundle_recover_previous "${MIGRATION_DESTINATION}" "${MIGRATION_BACKUP}" \
    "${MIGRATION_VERIFIER}" 0 \
    || die "could not recover the previous migration bundle activation"
}

# Binary, plugin, protoc, issue-tracker sidecar, and migration-bundle staging
# share src-tauri targets. Hold one lock across every verifier and writer so a
# competing dev launch cannot observe or create a mixed sidecar generation.
prepare_migration_staging

# Memories identifies a bundle by the content digest in its manifest; Lookback
# compares it as an opaque value and never interprets the bundle's contents.
migration_bundle_digest() {
  node -e 'const m = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")); process.stdout.write(m.content_digest ?? "")' \
    "$1/bundle-manifest.json" 2>/dev/null || true
}

# The default source is always rebuilt from the sibling Memories checkout so a
# changed working tree can never leave an older bundle staged. The build is
# incremental, and an unchanged result keeps the staged bundle in place.
stage_migration_bundle() {
  local memories_repo="${LOOKBACK_MEMORIES_SOURCE_DIR:-${AGENT_APP_PARENT}/memories}"
  local build_script="${memories_repo}/scripts/build-memories-db-migrate-sqlite.sh"
  local destination="${MIGRATION_DESTINATION}"
  local backup="${MIGRATION_BACKUP}"
  local source="" digest=""

  if [[ "${LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE+x}" == "x" ]]; then
    source=${LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE}
    if [[ -n "${LOOKBACK_MEMORIES_SOURCE_DIR:-}" ]]; then
      node "${SCRIPT_DIR}/verify-embedding-contract.mjs" "${memories_repo}" "${AGENT_APP}" \
        || die "explicit Memories source does not match the app embedding contract"
    fi
    node "${MIGRATION_VERIFIER}" "${source}" \
      || die "explicit LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE is invalid: ${source}"
  else
    [[ -x "${build_script}" ]] || die "official migration bundle builder not found at ${build_script}"
    node "${SCRIPT_DIR}/verify-embedding-contract.mjs" "${memories_repo}" "${AGENT_APP}" \
      || die "sibling Memories source does not match the app embedding contract"
  fi

  if [[ "${DRY_RUN}" == "1" && "${LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE+x}" != "x" ]]; then
    printf 'would build migration bundle with %s in a temporary directory\n' "${build_script}"
    printf 'would stage it -> %s unless its content digest matches the staged bundle\n' "${destination}"
    return
  elif [[ "${LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE+x}" != "x" ]]; then
    MIGRATION_BUILD_ROOT=$(mktemp -d "${AGENT_APP}/src-tauri/.migration-bundle-build.XXXXXX")
    source="${MIGRATION_BUILD_ROOT}/bundle"
    "${build_script}" "${source}" \
      || die "official migration bundle build failed: ${build_script}"
    node "${MIGRATION_VERIFIER}" "${source}" \
      || die "generated migration bundle failed verification"
  fi

  if [[ "${DRY_RUN}" == "1" ]]; then
    printf 'would stage migration bundle %s -> %s\n' "${source}" "${destination}"
    return
  fi

  digest=$(migration_bundle_digest "${source}")
  [[ -n "${digest}" ]] || die "migration bundle has no content digest: ${source}"
  if [[ -e "${destination}" && "$(migration_bundle_digest "${destination}")" == "${digest}" ]] \
    && node "${MIGRATION_VERIFIER}" "${destination}" >/dev/null 2>&1; then
    log "staged migration bundle is current: ${digest}"
    return
  fi

  MIGRATION_STAGING=$(mktemp -d "${AGENT_APP}/src-tauri/.migration-bundle-stage.XXXXXX")
  cp -R "${source}/." "${MIGRATION_STAGING}/"
  node "${MIGRATION_VERIFIER}" "${MIGRATION_STAGING}"

  migration_bundle_activate "${MIGRATION_STAGING}" "${destination}" "${backup}" \
    "${MIGRATION_VERIFIER}" 0 \
    || die "could not activate verified migration bundle at ${destination}"
  MIGRATION_STAGING=""
  log "staged migration bundle: ${digest}"
}

JOBWORKERP_BIN=$(resolve_required_bin all-in-one LOOKBACK_JOBWORKERP_BIN all-in-one ../target/release/all-in-one)
stage_one "${JOBWORKERP_BIN}" all-in-one
# The package runtime must be byte-identical to the sidecar the app spawns:
# the native bootstrap compares its digest with the running child's executable.
stage_coding_agent_package
stage_one "$(resolve_required_bin front LOOKBACK_MEMORIES_BIN memories-front ../memories/target/release/front)" front
stage_one "$(resolve_required_bin conductor-main LOOKBACK_CONDUCTOR_BIN conductor-main ../conductor/target/release/conductor-main)" conductor-main
stage_one "$(resolve_required_bin memories-import LOOKBACK_MEMORIES_IMPORT_BIN memories-import ../memories/target/release/memories-import)" memories-import
stage_migration_bundle
# protoc: default to the official self-contained release binary. A developer can
# still point PROTOC at their own self-contained protoc to skip the download.
if [[ -n "${PROTOC:-}" ]]; then
  stage_one "$(resolve_required_bin protoc PROTOC protoc ../protobuf/bin/protoc)" protoc
else
  fetch_protoc_bin "${TRIPLE}" "${BIN_DIR}/protoc-${TRIPLE}"
fi
stage_plugins
stage_issue_tracker_server

log "staged dev externalBin files for ${TRIPLE}"
