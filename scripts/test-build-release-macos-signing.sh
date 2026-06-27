#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/build-deps.sh
source "${SCRIPT_DIR}/lib/build-deps.sh"

make_codesign_mock() {
  local dir=$1 log=$2
  mkdir -p "${dir}"
  cat >"${dir}/codesign" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${CODESIGN_LOG}"
EOF
  chmod +x "${dir}/codesign"
  export PATH="${dir}:${PATH}"
  export CODESIGN_LOG="${log}"
}

assert_count() {
  local want=$1 file=$2
  local got
  got=$(wc -l <"${file}" | tr -d ' ')
  if [[ "${got}" != "${want}" ]]; then
    echo "expected ${want} codesign calls, got ${got}" >&2
    cat "${file}" >&2 || true
    exit 1
  fi
}

test_signs_macos_dylibs_with_runtime_options() {
  local log="${TMP_DIR}/codesign-mac.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-mac" "${log}"
  PLUGINS_DIR="${TMP_DIR}/mac-plugins"
  PLATFORM=mac
  APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"
  mkdir -p "${PLUGINS_DIR}"
  touch "${PLUGINS_DIR}/liba.dylib" "${PLUGINS_DIR}/libb.dylib"

  sign_macos_plugins

  assert_count 2 "${log}"
  grep -Fq -- '--force --options runtime --timestamp --sign Developer ID Application: Example (TEAMID)' "${log}"
  grep -Fq "${PLUGINS_DIR}/liba.dylib" "${log}"
  grep -Fq "${PLUGINS_DIR}/libb.dylib" "${log}"
}

test_linux_does_not_codesign_plugins() {
  local log="${TMP_DIR}/codesign-linux.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-linux" "${log}"
  PLUGINS_DIR="${TMP_DIR}/linux-plugins"
  PLATFORM=linux
  APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"
  mkdir -p "${PLUGINS_DIR}"
  touch "${PLUGINS_DIR}/liba.so"

  sign_macos_plugins

  assert_count 0 "${log}"
}

test_missing_identity_skips_for_local_unsigned_builds() {
  local log="${TMP_DIR}/codesign-missing-identity.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-missing" "${log}"
  PLUGINS_DIR="${TMP_DIR}/unsigned-mac-plugins"
  PLATFORM=mac
  unset APPLE_SIGNING_IDENTITY
  mkdir -p "${PLUGINS_DIR}"
  touch "${PLUGINS_DIR}/liba.dylib"

  sign_macos_plugins 2>/tmp/lookback-signing-test.err

  assert_count 0 "${log}"
  grep -Fq "skip explicit macOS plugin signing" /tmp/lookback-signing-test.err
}

test_signs_macos_dylibs_with_runtime_options
test_linux_does_not_codesign_plugins
test_missing_identity_skips_for_local_unsigned_builds

echo "build-release macOS signing tests passed"
