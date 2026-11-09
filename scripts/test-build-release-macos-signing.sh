#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_APP_SOURCE="$(cd "${SCRIPT_DIR}/.." && pwd)"
MIGRATION_TEST_FIXTURE="${SCRIPT_DIR}/lib/migration-test-fixture.mjs"
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

prepare_migration_signing_fixture() {
  local app=$1
  local bundle="${app}/src-tauri/migration-bundle"
  local app_assets="${bundle}.app-assets"
  mkdir -p "${app}/scripts" "${app}/src-tauri/bin"
  node "${MIGRATION_TEST_FIXTURE}" "${bundle}" valid
  cp -R "${app_assets}/." "${app}/"
  cp "${SCRIPT_DIR}/verify-migration-release.mjs" "${app}/scripts/"
  cp "${SCRIPT_DIR}/verify-embedding-contract.mjs" "${app}/scripts/"
  cp "${bundle}/test-artifacts/front" "${app}/src-tauri/bin/front-test-triple"
  cp "${bundle}/test-artifacts/memories-import" "${app}/src-tauri/bin/memories-import-test-triple"
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
  BIN_DIR="${AGENT_APP}/src-tauri/bin"
  TRIPLE=test-triple
  export PLATFORM=mac
  export APPLE_SIGNING_IDENTITY="Developer ID Application: Example (TEAMID)"
  prepare_migration_signing_fixture "${AGENT_APP}"

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
  BIN_DIR="${AGENT_APP}/src-tauri/bin"
  TRIPLE=test-triple
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
  BIN_DIR="${AGENT_APP}/src-tauri/bin"
  TRIPLE=test-triple
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

test_release_build_assembles_app_before_deferred_finalization() {
  local plan="${TMP_DIR}/mac-release-plan.log"
  DRY_RUN=1 bash "${SCRIPT_DIR}/build-release.sh" \
    --profile mac --bundle dmg,app --workdir "${TMP_DIR}/release-plan" >"${plan}" 2>&1
  grep -Fq 'pnpm tauri build --bundles app' "${plan}" \
    || { echo "macOS Tauri build must assemble only an unsigned app" >&2; exit 1; }
  grep -Fq 'pnpm tauri build --bundles app --no-sign' "${plan}" \
    || { echo "macOS dry-run must show Tauri's explicit no-sign option" >&2; exit 1; }
  grep -Fq 'Tauri assembles an unsigned macOS app; signing is deferred' "${plan}" \
    || { echo "the signing identity must be deferred until verified app finalization" >&2; exit 1; }
  grep -Fq 'finalize unsigned macOS app and requested DMG after Tauri assembly' "${plan}" \
    || { echo "macOS release must run finalization only after Tauri app assembly" >&2; exit 1; }
  if grep -Fq 'codesign ' "${plan}"; then
    echo "release assembly must not codesign external binaries before Tauri packaging" >&2
    exit 1
  fi
  local assemble_line finalize_line
  assemble_line=$(grep -nF 'pnpm tauri build --bundles app' "${plan}" | head -n 1 | cut -d: -f1)
  finalize_line=$(grep -nF 'finalize unsigned macOS app and requested DMG after Tauri assembly' "${plan}" | head -n 1 | cut -d: -f1)
  [[ "${assemble_line}" -lt "${finalize_line}" ]] \
    || { echo "macOS signing finalization must follow Tauri assembly" >&2; exit 1; }
}

test_tauri_no_sign_option_is_explicit_even_with_imported_certificate_env() {
  local tools="${TMP_DIR}/tauri-no-sign-tools" args_log="${TMP_DIR}/tauri-no-sign-args.log"
  local env_log="${TMP_DIR}/tauri-no-sign-env.log"
  mkdir -p "${tools}" "${TMP_DIR}/tauri-no-sign-app"
  cat >"${tools}/pnpm" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${TAURI_ARGS_LOG}"
printf 'identity=%s\n' "${APPLE_SIGNING_IDENTITY-<unset>}" >>"${TAURI_ENV_LOG}"
printf 'tauri_identity=%s\n' "${TAURI_SIGNING_IDENTITY-<unset>}" >>"${TAURI_ENV_LOG}"
printf 'certificate_present=%s\n' "$([[ -n "${APPLE_CERTIFICATE:-}" ]] && echo yes || echo no)" >>"${TAURI_ENV_LOG}"
printf 'certificate_password_present=%s\n' "$([[ -n "${APPLE_CERTIFICATE_PASSWORD:-}" ]] && echo yes || echo no)" >>"${TAURI_ENV_LOG}"
EOF
  chmod +x "${tools}/pnpm"
  : >"${args_log}"
  : >"${env_log}"

  # Loading the build script exposes the exact command runner without invoking
  # the release pipeline. The mock keeps certificate fixtures out of output.
  source "${SCRIPT_DIR}/build-release.sh"
  AGENT_APP="${TMP_DIR}/tauri-no-sign-app"
  PLATFORM=mac
  export tauri_bundles=app LOOKBACK_TAURI_VERBOSE=0
  PATH="${tools}:${PATH}" \
    TAURI_ARGS_LOG="${args_log}" TAURI_ENV_LOG="${env_log}" \
    APPLE_SIGNING_IDENTITY="fixture-identity" \
    TAURI_SIGNING_IDENTITY="fixture-tauri-identity" \
    APPLE_CERTIFICATE="fixture-certificate" \
    APPLE_CERTIFICATE_PASSWORD="fixture-certificate-password" \
    run_tauri_build ""

  grep -Fq -- '--no-sign' "${args_log}" \
    || { echo "macOS Tauri assembly must pass its explicit --no-sign option" >&2; exit 1; }
  grep -Fxq 'identity=' "${env_log}" \
    || { echo "the signing identity must not be sent to Tauri app assembly" >&2; exit 1; }
  grep -Fxq 'tauri_identity=' "${env_log}" \
    || { echo "the Tauri signing identity must not be sent to app assembly" >&2; exit 1; }
  grep -Fxq 'certificate_present=no' "${env_log}" \
    || { echo "Tauri app assembly must not receive the inherited signing certificate" >&2; exit 1; }
  grep -Fxq 'certificate_password_present=no' "${env_log}" \
    || { echo "Tauri app assembly must not receive the inherited certificate password" >&2; exit 1; }
}

test_signs_macos_dylibs_with_runtime_options
test_signs_macos_migration_binaries_with_runtime_options
test_release_staging_removes_stale_macos_plugins
test_linux_does_not_codesign_plugins
test_linux_does_not_codesign_migration_binaries
test_missing_identity_skips_for_local_unsigned_builds
test_missing_identity_skips_migration_binary_signing
test_missing_migration_binary_fails_before_codesign
test_release_build_assembles_app_before_deferred_finalization
test_tauri_no_sign_option_is_explicit_even_with_imported_certificate_env

echo "build-release macOS signing tests passed"
