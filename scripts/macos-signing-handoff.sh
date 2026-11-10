#!/usr/bin/env bash
# Hands the unsigned macOS app from the build job, which runs third-party
# dependency code on the self-hosted runner, to the signing job, which holds
# the Apple secrets on a GitHub-hosted runner.
#
#   macos-signing-handoff.sh pack ARCHIVE                  (run from the repo root)
#   macos-signing-handoff.sh unpack ARCHIVE SHA256 DEST
#
# pack prints the archive's SHA-256. unpack checks it, then refuses any member
# outside HANDOFF_PATHS so the archive cannot replace the scripts the signing
# job runs. The checksum only catches a mixed-up or damaged hand-off; it cannot
# vouch for what the build produced.
set -euo pipefail

# The app plus the build inputs finalize-macos-bundle.sh checks it against.
# Tracked inputs (workers/, entitlements, scripts/) come from the signer's own
# checkout instead.
HANDOFF_PATHS=(
  target/release/bundle/macos/Lookback.app
  src-tauri/bin
  src-tauri/plugins
  src-tauri/migration-bundle
  dict
  resources/coding-agent
)

die() {
  echo "macos-signing-handoff: $*" >&2
  exit 1
}

is_allowed_member() {
  local member="${1%/}" allowed
  case "/${member}/" in
    */../*) return 1 ;;
  esac
  for allowed in "${HANDOFF_PATHS[@]}"; do
    [[ "${member}" == "${allowed}" || "${member}" == "${allowed}/"* ]] && return 0
  done
  return 1
}

pack() {
  local archive="$1"
  # Artifacts drop symlinks and file modes, so everything travels as one tar.
  tar -cf "${archive}" "${HANDOFF_PATHS[@]}"
  shasum -a 256 "${archive}" | cut -d' ' -f1
}

unpack() {
  local archive="$1" expected="$2" dest="$3" member
  echo "${expected}  ${archive}" | shasum -a 256 -c --status \
    || die "checksum mismatch for ${archive}"
  while IFS= read -r member; do
    is_allowed_member "${member}" || die "unexpected archive member: ${member}"
  done < <(tar -tf "${archive}")
  tar -xf "${archive}" -C "${dest}"
}

case "${1:-}" in
  pack) [[ $# -eq 2 ]] || die "usage: $0 pack ARCHIVE"; pack "$2" ;;
  unpack) [[ $# -eq 4 ]] || die "usage: $0 unpack ARCHIVE SHA256 DEST"; unpack "$2" "$3" "$4" ;;
  *) die "usage: $0 pack ARCHIVE | unpack ARCHIVE SHA256 DEST" ;;
esac
