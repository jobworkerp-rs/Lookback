#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)
workflow="${AGENT_APP}/.gitea/workflows/build-and-release-cuda.yml"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

grep -Fq 'bash scripts/build-release.sh --profile linux-cuda --bundle appimage' "${workflow}" \
  || fail "CUDA workflow should build only the AppImage bundle"
if grep -Fq '[ ${#debs[@]} -gt 0 ]' "${workflow}"; then
  fail "CUDA workflow must not require a .deb artifact"
fi
grep -Fq '"$out"/appimage/*-cuda.AppImage' "${workflow}" \
  || fail "CUDA workflow should upload the CUDA AppImage"

upload_block=$(
  awk '
    /gh release upload/ { in_upload = 1 }
    in_upload { print }
    in_upload && /AppImage/ { exit }
  ' "${workflow}"
)

printf '%s\n' "${upload_block}" | grep -Fq '*-cuda.AppImage' \
  || fail "release upload command should include the CUDA AppImage"
if printf '%s\n' "${upload_block}" | grep -Fq '*-cuda.deb'; then
  fail "release upload command must not include the oversized CUDA .deb"
fi

printf 'PASS: CUDA GitHub Release upload excludes oversized .deb assets\n'
