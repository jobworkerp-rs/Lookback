#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

bundle="${TMP_DIR}/atlas"
mkdir -p "${bundle}/bin"
printf 'official Atlas bytes\n' >"${bundle}/bin/atlas"
sha=$(shasum -a 256 "${bundle}/bin/atlas" | awk '{print $1}')
cat >"${bundle}/atlas-tool.lock.json" <<EOF
{
  "platforms": {
    "darwin-arm64": { "sha256": "${sha}" }
  }
}
EOF

node "${SCRIPT_DIR}/verify-atlas-lock.mjs" "${bundle}" darwin-arm64

printf 'modified after lock generation\n' >>"${bundle}/bin/atlas"
if node "${SCRIPT_DIR}/verify-atlas-lock.mjs" "${bundle}" darwin-arm64 \
  >"${TMP_DIR}/mismatch.out" 2>"${TMP_DIR}/mismatch.err"; then
  echo "expected a modified Atlas binary to fail lock verification" >&2
  exit 1
fi
grep -Fq "Fixed Atlas binary SHA-256 does not match atlas-tool.lock.json" \
  "${TMP_DIR}/mismatch.err"

node "${SCRIPT_DIR}/record-signed-atlas-lock.mjs" "${bundle}" darwin-arm64
node "${SCRIPT_DIR}/verify-atlas-lock.mjs" "${bundle}" darwin-arm64
grep -Fq "\"source_sha256\": \"${sha}\"" "${bundle}/atlas-tool.lock.json"

if node "${SCRIPT_DIR}/record-signed-atlas-lock.mjs" "${bundle}" darwin-arm64 \
  >"${TMP_DIR}/already-signed.out" 2>"${TMP_DIR}/already-signed.err"; then
  echo "expected recording an already signed Atlas lock to fail" >&2
  exit 1
fi
grep -Fq "already records a distinct source SHA-256" "${TMP_DIR}/already-signed.err"

echo "Atlas lock verification tests passed"
