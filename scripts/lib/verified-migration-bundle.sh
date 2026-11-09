#!/usr/bin/env bash
# Shared locking and crash recovery for the verified migration bundle's
# destination -> backup -> destination rename sequence.

migration_bundle_verify() {
  local verifier=$1 bundle=$2 require_provenance=${3:-0}
  if [[ "${require_provenance}" == "1" ]]; then
    local app_assets
    app_assets=$(cd "$(dirname "${verifier}")/.." && pwd)
    node "${verifier}" "${bundle}" --require-provenance \
      --app-assets "${app_assets}" >/dev/null
  else
    node "${verifier}" "${bundle}" >/dev/null
  fi
}

migration_bundle_preserve_previous() {
  local destination=$1 backup=$2
  local snapshot="${backup}.snapshot.json"
  [[ -e "${destination}" ]] || return 0
  [[ ! -e "${backup}" && ! -L "${backup}" ]] \
    || { echo "migration bundle backup path already exists: ${backup}" >&2; return 1; }
  [[ ! -e "${snapshot}" && ! -L "${snapshot}" ]] \
    || { echo "migration bundle recovery snapshot already exists: ${snapshot}" >&2; return 1; }
  node "$(dirname "${BASH_SOURCE[0]}")/migration-bundle-snapshot.mjs" \
    create "${destination}" "${snapshot}" \
    || { echo "could not snapshot the currently staged migration bundle" >&2; return 1; }
}

migration_bundle_snapshot_matches() {
  local backup=$1
  local snapshot="${2:-${backup}.snapshot.json}"
  [[ -f "${snapshot}" && ! -L "${snapshot}" ]] \
    && node "$(dirname "${BASH_SOURCE[0]}")/migration-bundle-snapshot.mjs" \
      verify "${backup}" "${snapshot}" >/dev/null 2>&1
}

migration_bundle_backup_is_restorable() {
  local backup=$1 verifier=$2
  local snapshot="${backup}.snapshot.json"
  [[ -d "${backup}" && ! -L "${backup}" ]] || return 1
  if [[ -e "${snapshot}" || -L "${snapshot}" ]]; then
    migration_bundle_snapshot_matches "${backup}" "${snapshot}"
    return $?
  fi
  if migration_bundle_verify "${verifier}" "${backup}" 0 >/dev/null 2>&1; then
    return 0
  fi
  migration_bundle_snapshot_matches "${backup}"
}

migration_bundle_finish_restore() {
  local backup=$1 destination=$2
  local snapshot="${backup}.snapshot.json"
  mv "${backup}" "${destination}" \
    || { echo "could not restore migration bundle backup ${backup}" >&2; return 1; }
  rm -f "${snapshot}" \
    || { echo "restored migration bundle but could not remove its snapshot ${snapshot}" >&2; return 1; }
}

# If a prior process died between the two renames, either restore the verified
# backup or finish cleanup when the newly activated destination verifies. A
# snapshot permits byte-preserving rollback of an older generation that the
# current release verifier no longer accepts; it does not bless that bundle as
# a current release or authorize executing it.
migration_bundle_recover_previous() {
  local destination=$1 backup=$2 verifier=$3 strict=${4:-0}
  local snapshot="${backup}.snapshot.json"
  if [[ ! -e "${backup}" && ! -L "${backup}" ]]; then
    [[ ! -e "${snapshot}" && ! -L "${snapshot}" ]] || rm -f "${snapshot}"
    return 0
  fi
  if [[ ! -e "${destination}" ]]; then
    migration_bundle_backup_is_restorable "${backup}" "${verifier}" \
      || { echo "migration bundle recovery backup is not valid; preserved at ${backup}" >&2; return 1; }
    migration_bundle_finish_restore "${backup}" "${destination}"
    return $?
  fi
  if migration_bundle_verify "${verifier}" "${destination}" "${strict}"; then
    rm -rf "${backup}" \
      || { echo "verified migration bundle activated but backup cleanup failed: ${backup}" >&2; return 1; }
    rm -f "${snapshot}" \
      || { echo "verified migration bundle activated but snapshot cleanup failed: ${snapshot}" >&2; return 1; }
    return 0
  fi
  if migration_bundle_backup_is_restorable "${backup}" "${verifier}"; then
    rm -rf "${destination}" \
      || { echo "could not remove incomplete migration bundle destination; backup preserved at ${backup}" >&2; return 1; }
    migration_bundle_finish_restore "${backup}" "${destination}"
    return $?
  fi
  echo "neither migration bundle destination nor backup verifies; both were preserved" >&2
  return 1
}

migration_bundle_lock_acquire() {
  local lock=$1 destination=$2 backup=$3 verifier=$4 strict=${5:-0}
  local attempts=0 owner=""
  while ! mkdir "${lock}" 2>/dev/null; do
    attempts=$((attempts + 1))
    if [[ -f "${lock}/owner-pid" ]]; then
      owner=$(cat "${lock}/owner-pid" 2>/dev/null || true)
      if [[ "${owner}" =~ ^[0-9]+$ ]] && ! kill -0 "${owner}" 2>/dev/null; then
        migration_bundle_recover_previous "${destination}" "${backup}" \
          "${verifier}" "${strict}" || return 1
        rm -rf "${lock}" || return 1
        continue
      fi
    elif (( attempts >= 5 )); then
      # A kill between mkdir and owner-pid creation leaves an empty stale lock.
      migration_bundle_recover_previous "${destination}" "${backup}" \
        "${verifier}" "${strict}" || return 1
      rmdir "${lock}" 2>/dev/null || true
    fi
    if (( attempts >= 300 )); then
      echo "timed out waiting for migration bundle lock ${lock}" >&2
      return 1
    fi
    sleep 0.2
  done
  if ! printf '%s\n' "$$" >"${lock}/owner-pid"; then
    rmdir "${lock}" 2>/dev/null || true
    echo "could not record migration bundle lock owner ${lock}" >&2
    return 1
  fi
  MIGRATION_BUNDLE_LOCK_DIR=${lock}
}

migration_bundle_lock_release() {
  local lock=${1:-${MIGRATION_BUNDLE_LOCK_DIR:-}} owner=""
  [[ -n "${lock}" && -d "${lock}" ]] || return 0
  [[ ! -f "${lock}/owner-pid" ]] || owner=$(cat "${lock}/owner-pid" 2>/dev/null || true)
  if [[ "${owner}" == "$$" ]]; then
    rm -rf "${lock}"
  fi
  [[ "${MIGRATION_BUNDLE_LOCK_DIR:-}" != "${lock}" ]] || MIGRATION_BUNDLE_LOCK_DIR=""
}

migration_bundle_activate() {
  local staging=$1 destination=$2 backup=$3 verifier=$4 strict=${5:-0}
  local snapshot="${backup}.snapshot.json"
  [[ ! -e "${backup}" && ! -L "${backup}" ]] \
    || { echo "migration bundle backup path already exists: ${backup}" >&2; return 1; }
  [[ ! -e "${snapshot}" && ! -L "${snapshot}" ]] \
    || { echo "migration bundle recovery snapshot already exists: ${snapshot}" >&2; return 1; }
  if [[ -e "${destination}" ]]; then
    migration_bundle_preserve_previous "${destination}" "${backup}" \
      || return 1
    mv "${destination}" "${backup}" \
      || { rm -f "${snapshot}"; echo "could not preserve the currently staged migration bundle" >&2; return 1; }
  fi
  if ! mv "${staging}" "${destination}"; then
    migration_bundle_recover_previous "${destination}" "${backup}" \
      "${verifier}" "${strict}" || true
    echo "could not activate verified migration bundle at ${destination}" >&2
    return 1
  fi
  if ! migration_bundle_verify "${verifier}" "${destination}" "${strict}"; then
    migration_bundle_recover_previous "${destination}" "${backup}" \
      "${verifier}" "${strict}" || true
    echo "activated migration bundle did not verify; previous valid bundle recovery was attempted" >&2
    return 1
  fi
  if [[ -e "${backup}" ]]; then
    rm -rf "${backup}" \
      || { echo "new migration bundle is active but backup cleanup failed: ${backup}" >&2; return 1; }
    rm -f "${snapshot}" \
      || { echo "new migration bundle is active but snapshot cleanup failed: ${snapshot}" >&2; return 1; }
  fi
}
