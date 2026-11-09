#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT="${SCRIPT_DIR}/ci-stage-placeholders.sh"
CONFIG="${SCRIPT_DIR}/../src-tauri/tauri.conf.json"
TMP_ROOT=$(mktemp -d)

cleanup() {
  rm -rf "${TMP_ROOT}"
}
trap cleanup EXIT

test_stages_every_linux_external_binary_placeholder() {
  local app="${TMP_ROOT}/agent-app"
  mkdir -p "${app}/scripts" "${app}/src-tauri"
  cp "${SCRIPT}" "${app}/scripts/ci-stage-placeholders.sh"
  cp "${CONFIG}" "${app}/src-tauri/tauri.conf.json"

  bash "${app}/scripts/ci-stage-placeholders.sh" x86_64-unknown-linux-gnu >/dev/null

  local name
  while IFS= read -r name; do
    [[ -f "${app}/src-tauri/bin/${name}-x86_64-unknown-linux-gnu" ]] || {
      echo "missing placeholder for ${name}" >&2
      exit 1
    }
  done < <(
    node -e 'const config = require(process.argv[1]); for (const bin of config.bundle.externalBin) console.log(bin.replace(/^bin\//, ""));' \
      "${app}/src-tauri/tauri.conf.json"
  )
}

# Resource directories inside src-tauri are build outputs (gitignored, not
# in a fresh checkout); tauri-build still requires them to exist.
test_stages_every_src_tauri_resource_directory() {
  local app="${TMP_ROOT}/agent-app-resources"
  mkdir -p "${app}/scripts" "${app}/src-tauri"
  cp "${SCRIPT}" "${app}/scripts/ci-stage-placeholders.sh"
  cp "${CONFIG}" "${app}/src-tauri/tauri.conf.json"

  bash "${app}/scripts/ci-stage-placeholders.sh" x86_64-unknown-linux-gnu >/dev/null

  local resource count=0
  while IFS= read -r resource; do
    count=$((count + 1))
    [[ -d "${app}/src-tauri/${resource}" ]] || {
      echo "missing resource directory ${resource}" >&2
      exit 1
    }
  done < <(
    node -e 'const config = require(process.argv[1]); for (const src of Object.keys(config.bundle.resources)) if (!src.startsWith("../")) console.log(src);' \
      "${app}/src-tauri/tauri.conf.json"
  )
  (( count > 0 )) || {
    echo "expected at least one src-tauri resource directory" >&2
    exit 1
  }
}

test_stages_every_linux_external_binary_placeholder
test_stages_every_src_tauri_resource_directory

echo "CI placeholder staging tests passed"
