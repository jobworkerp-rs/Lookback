#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
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
  export PLATFORM=mac
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"
  mkdir -p "${PLUGINS_DIR}"
  touch "${PLUGINS_DIR}/liba.dylib" "${PLUGINS_DIR}/libb.dylib"

  sign_macos_plugins

  assert_count 2 "${log}"
  grep -Fq -- '--force --options runtime --timestamp --sign Developer ID Application: Example (TEAMID)' "${log}"
  grep -Fq "${PLUGINS_DIR}/liba.dylib" "${log}"
  grep -Fq "${PLUGINS_DIR}/libb.dylib" "${log}"
}

test_signs_macos_migration_binaries_with_runtime_options() {
  local log="${TMP_DIR}/codesign-migration.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-migration" "${log}"
  AGENT_APP="${TMP_DIR}/migration-app"
  export PLATFORM=mac
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"
  mkdir -p "${AGENT_APP}/src-tauri/migration-bundle/atlas/bin"
  touch "${AGENT_APP}/src-tauri/migration-bundle/memories-db-migrate"
  touch "${AGENT_APP}/src-tauri/migration-bundle/atlas/bin/atlas"

  sign_macos_migration_binaries

  assert_count 2 "${log}"
  grep -Fq -- '--force --options runtime --timestamp --sign Developer ID Application: Example (TEAMID)' "${log}"
  grep -Fq "${AGENT_APP}/src-tauri/migration-bundle/memories-db-migrate" "${log}"
  grep -Fq "${AGENT_APP}/src-tauri/migration-bundle/atlas/bin/atlas" "${log}"
}

test_release_staging_removes_stale_macos_plugins() {
  WORKDIR="${TMP_DIR}/staging-deps"
  PLUGINS_DIR="${TMP_DIR}/staging-plugins"
  export PLATFORM=mac LIBEXT=dylib GPU=metal ONLY_REPOS="llama,mm"
  mkdir -p "${WORKDIR}/llama-cpp-runner/target/release"
  mkdir -p "${WORKDIR}/mm-embedding-runner/target/release"
  mkdir -p "${PLUGINS_DIR}"
  touch "${WORKDIR}/llama-cpp-runner/target/release/libjobworkerp_llama_cpp_plugin.dylib"
  touch "${WORKDIR}/mm-embedding-runner/target/release/libmm_embedding_runner.dylib"
  touch "${PLUGINS_DIR}/libplaceholder_ci.dylib"

  stage_plugins

  [[ -f "${PLUGINS_DIR}/libjobworkerp_llama_cpp_plugin.dylib" ]]
  [[ -f "${PLUGINS_DIR}/libmm_embedding_runner.dylib" ]]
  [[ ! -e "${PLUGINS_DIR}/libplaceholder_ci.dylib" ]]
}

test_linux_does_not_codesign_migration_binaries() {
  local log="${TMP_DIR}/codesign-migration-linux.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-migration-linux" "${log}"
  AGENT_APP="${TMP_DIR}/migration-linux-app"
  export PLATFORM=linux
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"

  sign_macos_migration_binaries

  assert_count 0 "${log}"
}

test_missing_identity_skips_migration_binary_signing() {
  local log="${TMP_DIR}/codesign-migration-missing-identity.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-migration-missing-identity" "${log}"
  AGENT_APP="${TMP_DIR}/migration-unsigned-app"
  export PLATFORM=mac
  unset APPLE_SIGNING_IDENTITY
  mkdir -p "${AGENT_APP}/src-tauri/migration-bundle/atlas/bin"
  touch "${AGENT_APP}/src-tauri/migration-bundle/memories-db-migrate"
  touch "${AGENT_APP}/src-tauri/migration-bundle/atlas/bin/atlas"

  sign_macos_migration_binaries 2>"${TMP_DIR}/migration-missing-identity.err"

  assert_count 0 "${log}"
  grep -Fq "skip explicit macOS migration bundle executables signing" "${TMP_DIR}/migration-missing-identity.err"
}

test_missing_migration_binary_fails_before_codesign() {
  local log="${TMP_DIR}/codesign-migration-missing-binary.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-migration-missing-binary" "${log}"
  AGENT_APP="${TMP_DIR}/migration-incomplete-app"
  export PLATFORM=mac
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"

  if (sign_macos_migration_binaries) 2>"${TMP_DIR}/migration-missing-binary.err"; then
    echo "expected a missing migration binary to fail signing" >&2
    exit 1
  fi

  assert_count 0 "${log}"
  grep -Fq "macOS migration bundle executable is missing" "${TMP_DIR}/migration-missing-binary.err"
}

test_linux_does_not_codesign_plugins() {
  local log="${TMP_DIR}/codesign-linux.log"
  : >"${log}"
  make_codesign_mock "${TMP_DIR}/mockbin-linux" "${log}"
  PLUGINS_DIR="${TMP_DIR}/linux-plugins"
  export PLATFORM=linux
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"
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
  export PLATFORM=mac
  unset APPLE_SIGNING_IDENTITY
  mkdir -p "${PLUGINS_DIR}"
  touch "${PLUGINS_DIR}/liba.dylib"

  sign_macos_plugins 2>/tmp/lookback-signing-test.err

  assert_count 0 "${log}"
  grep -Fq "skip explicit macOS plugin dylibs signing" /tmp/lookback-signing-test.err
}

test_signs_macos_dylibs_with_runtime_options
test_signs_macos_migration_binaries_with_runtime_options
test_release_staging_removes_stale_macos_plugins
test_linux_does_not_codesign_plugins
test_linux_does_not_codesign_migration_binaries
test_missing_identity_skips_for_local_unsigned_builds
test_missing_identity_skips_migration_binary_signing
test_missing_migration_binary_fails_before_codesign

echo "build-release macOS signing tests passed"
