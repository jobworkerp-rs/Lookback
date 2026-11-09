#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP_SOURCE=$(cd "${SCRIPT_DIR}/.." && pwd)
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

make_tools() {
  local tools=$1 log=$2 real_file
  real_file=$(command -v file)
  mkdir -p "${tools}"
  cat >"${tools}/file" <<EOF
#!/usr/bin/env bash
target=\${!#}
case "\${target}" in
  */Contents/MacOS/Lookback|*/Contents/MacOS/all-in-one|*/Contents/MacOS/front|*/Contents/MacOS/conductor-main|*/Contents/MacOS/issue-tracker-server|*/Contents/MacOS/memories-import|*/Contents/MacOS/protoc|*/Contents/Resources/plugins/*.dylib|*/Contents/Resources/memories-db-migrate/memories-db-migrate|*/Contents/Resources/memories-db-migrate/atlas/bin/atlas|*/Contents/Resources/UnexpectedMachO)
    echo 'Mach-O 64-bit executable arm64'
    ;;
  *) exec "${real_file}" "\$@" ;;
esac
EOF
  cat >"${tools}/codesign" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
target="${!#}"
if [[ " $* " == *" --verify "* ]]; then
  printf 'verify %s\n' "${target}" >>"${CODESIGN_LOG}"
  if [[ -d "${target}" ]]; then
    [[ -f "${target}/_CodeSignature/mock-signature" ]]
  else
    [[ -f "${target}.mock-signature" ]] || [[ "$(tail -n 1 "${target}" 2>/dev/null || true)" == 'SIGNED:fixture-identity' ]]
    [[ "$(tail -n 1 "${target}" 2>/dev/null || true)" != 'TAMPERED:after-sign' ]]
  fi
  exit
fi
printf 'sign %s\n' "${target}" >>"${CODESIGN_LOG}"
[[ "${target}" != "${FAIL_CODESIGN_TARGET:-}" ]] || exit 17
if [[ -d "${target}" ]]; then
  mkdir -p "${target}/_CodeSignature"
  printf 'fixture bundle seal\n' >"${target}/_CodeSignature/mock-signature"
else
  printf 'SIGNED:fixture-identity\n' >>"${target}"
  [[ "${target}" != "${TAMPER_AFTER_SIGN_TARGET:-}" ]] \
    || printf 'TAMPERED:after-sign\n' >>"${target}"
fi
EOF
  cat >"${tools}/hdiutil" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
target="${!#}"
mkdir -p "$(dirname "${target}")"
printf 'fixture disk image\n' >"${target}"
printf 'create %s\n' "${target}" >>"${HDIUTIL_LOG}"
EOF
  cat >"${tools}/spctl" <<'EOF'
#!/usr/bin/env bash
printf 'spctl %s\n' "$*" >>"${SPCTL_LOG}"
EOF
  cat >"${tools}/xcrun" <<'EOF'
#!/usr/bin/env bash
printf 'xcrun %s\n' "$*" >>"${XCRUN_LOG}"
EOF
  chmod +x "${tools}/file" "${tools}/codesign" "${tools}/hdiutil" "${tools}/spctl" "${tools}/xcrun"
  export PATH="${tools}:${PATH}"
  export CODESIGN_LOG="${log}"
  export HDIUTIL_LOG="${TMP_ROOT}/hdiutil.log"
  export SPCTL_LOG="${TMP_ROOT}/spctl.log"
  export XCRUN_LOG="${TMP_ROOT}/xcrun.log"
  : >"${CODESIGN_LOG}"
  : >"${HDIUTIL_LOG}"
  : >"${SPCTL_LOG}"
  : >"${XCRUN_LOG}"
}

copy_tree_if_present() {
  local source=$1 target=$2
  if [[ -d "${source}" ]]; then
    mkdir -p "${target}"
    cp -R "${source}/." "${target}/"
  fi
}

prepare_fixture() {
  local root=$1 source app triple=aarch64-apple-darwin name
  source="${root}/source"
  app="${root}/Lookback.app"
  mkdir -p "${source}/scripts/lib" "${source}/src-tauri/bin" \
    "${source}/src-tauri/plugins" "${source}/src-tauri" \
    "${source}/workers" "${source}/dict"
  cp "${AGENT_APP_SOURCE}/src-tauri/entitlements.plist" "${source}/src-tauri/entitlements.plist"
  node "${SCRIPT_DIR}/lib/migration-test-fixture.mjs" \
    "${source}/src-tauri/migration-bundle" valid
  cp -R "${source}/src-tauri/migration-bundle.app-assets/." "${source}/"
  cp "${source}/src-tauri/migration-bundle/test-artifacts/front" \
    "${source}/src-tauri/bin/front-${triple}"
  cp "${source}/src-tauri/migration-bundle/test-artifacts/memories-import" \
    "${source}/src-tauri/bin/memories-import-${triple}"
  for name in all-in-one conductor-main issue-tracker-server protoc; do
    printf '#!/bin/sh\nexit 0\n' >"${source}/src-tauri/bin/${name}-${triple}"
    chmod +x "${source}/src-tauri/bin/${name}-${triple}"
  done
  printf 'fixture plugin\n' >"${source}/src-tauri/plugins/libfixture.dylib"
  copy_tree_if_present "${AGENT_APP_SOURCE}/resources/coding-agent" \
    "${source}/resources/coding-agent"

  mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
  printf '<?xml version="1.0"?><plist><dict><key>CFBundleExecutable</key><string>Lookback</string></dict></plist>\n' \
    >"${app}/Contents/Info.plist"
  printf '#!/bin/sh\nexit 0\n' >"${app}/Contents/MacOS/Lookback"
  chmod +x "${app}/Contents/MacOS/Lookback"
  for name in all-in-one front conductor-main issue-tracker-server memories-import protoc; do
    cp "${source}/src-tauri/bin/${name}-${triple}" "${app}/Contents/MacOS/${name}"
  done
  cp -R "${source}/src-tauri/migration-bundle" \
    "${app}/Contents/Resources/memories-db-migrate"
  cp -R "${source}/src-tauri/plugins" "${app}/Contents/Resources/plugins"
  copy_tree_if_present "${source}/workers" "${app}/Contents/Resources/workers"
  copy_tree_if_present "${source}/dict" "${app}/Contents/Resources/dict"
  copy_tree_if_present "${source}/resources/coding-agent" \
    "${app}/Contents/Resources/coding-agent"
  mkdir -p "${app}/Contents/Frameworks/Fixture.framework/Versions/A"
  printf 'fixture framework resource\n' \
    >"${app}/Contents/Frameworks/Fixture.framework/Versions/A/Resources"
  ln -s A "${app}/Contents/Frameworks/Fixture.framework/Versions/Current"
  printf '%s\n%s\n' "${source}" "${app}"
}

run_finalizer() {
  local source=$1 app=$2
  shift 2
  bash "${SCRIPT_DIR}/finalize-macos-bundle.sh" \
    --app "${app}" --source-root "${source}" \
    --triple aarch64-apple-darwin "$@"
}

test_signs_and_records_final_macos_bytes() {
  local root="${TMP_ROOT}/valid" paths source app source_atlas source_front package bundle manifest
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  local log="${TMP_ROOT}/codesign-valid.log"
  make_tools "${TMP_ROOT}/tools-valid" "${log}"
  source_atlas=$(shasum -a 256 "${source}/src-tauri/migration-bundle/atlas/bin/atlas" | awk '{print $1}')
  source_front=$(shasum -a 256 "${source}/src-tauri/bin/front-aarch64-apple-darwin" | awk '{print $1}')
  export APPLE_SIGNING_IDENTITY=fixture-identity
  export APPLE_NOTARY_KEYCHAIN_PROFILE=fixture-profile
  run_finalizer "${source}" "${app}" --dmg "${root}/Lookback.dmg" --notarize

  for path in \
    "${app}/Contents/MacOS/Lookback" \
    "${app}/Contents/MacOS/all-in-one" \
    "${app}/Contents/MacOS/front" \
    "${app}/Contents/MacOS/conductor-main" \
    "${app}/Contents/MacOS/issue-tracker-server" \
    "${app}/Contents/MacOS/memories-import" \
    "${app}/Contents/MacOS/protoc" \
    "${app}/Contents/Resources/plugins/libfixture.dylib" \
    "${app}/Contents/Resources/memories-db-migrate/memories-db-migrate" \
    "${app}/Contents/Resources/memories-db-migrate/atlas/bin/atlas" \
    "${app}" "${root}/Lookback.dmg"; do
    grep -Fq "sign ${path}" "${log}" || fail "missing code-sign step for ${path}"
  done
  [[ "$(shasum -a 256 "${source}/src-tauri/migration-bundle/atlas/bin/atlas" | awk '{print $1}')" == "${source_atlas}" ]] \
    || fail "signing the app bundle must not mutate the trusted upstream Atlas source"
  [[ "$(shasum -a 256 "${source}/src-tauri/bin/front-aarch64-apple-darwin" | awk '{print $1}')" == "${source_front}" ]] \
    || fail "signing the app bundle must not mutate the verified source front binary"
  package="${app}/Contents/Resources/memories-db-migrate"
  bundle="${package}/memories-source-provenance.json"
  manifest="${root}/manifest.json"
  cp "${bundle}" "${manifest}"
  node - "${manifest}" "${package}/memories-db-migrate" \
    "${app}/Contents/MacOS/front" "${app}/Contents/MacOS/memories-import" \
    "${package}/atlas/bin/atlas" "${source_front}" "${source_atlas}" <<'NODE'
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const [manifestPath, cli, front, importer, atlas, sourceFront, sourceAtlas] = process.argv.slice(2);
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
const sha = (file) => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
if (manifest.source_artifact_sha256.front !== sourceFront ||
    manifest.source_artifact_sha256.memories_db_migrate === sha(cli) ||
    manifest.source_artifact_sha256.front === sha(front) ||
    manifest.source_artifact_sha256.memories_import === sha(importer) ||
    manifest.distributed_artifact_sha256.front !== sha(front) ||
    manifest.distributed_artifact_sha256.memories_import !== sha(importer) ||
    manifest.distributed_artifact_sha256.memories_db_migrate !== sha(cli)) {
  throw new Error("final packaged artifacts must retain source hashes and record mutated distributed bytes");
}
const lock = JSON.parse(fs.readFileSync(path.join(path.dirname(path.dirname(atlas)), "atlas-tool.lock.json"), "utf8"));
if (lock.platforms["darwin-arm64"].source_sha256 !== sourceAtlas ||
    lock.platforms["darwin-arm64"].sha256 !== sha(atlas)) {
  throw new Error("signed Atlas lock must preserve the upstream digest and record distributed bytes");
}
NODE
  grep -Fq 'xcrun notarytool submit' "${XCRUN_LOG}" \
    || fail "the explicit notarization option must submit the signed DMG"
  grep -Fq 'xcrun stapler staple' "${XCRUN_LOG}" \
    || fail "notarized DMGs must be stapled"
  grep -Fq 'xcrun stapler validate' "${XCRUN_LOG}" \
    || fail "stapled DMGs must be validated"
}

test_missing_identity_fails_closed() {
  local root="${TMP_ROOT}/missing-identity" paths source app log
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  log="${TMP_ROOT}/codesign-missing.log"
  make_tools "${TMP_ROOT}/tools-missing" "${log}"
  unset APPLE_SIGNING_IDENTITY
  if run_finalizer "${source}" "${app}" --dmg "${root}/Lookback.dmg" \
    >"${TMP_ROOT}/missing-identity.out" 2>"${TMP_ROOT}/missing-identity.err"; then
    fail "macOS distribution finalization must reject a missing signing identity"
  fi
  grep -Fq "APPLE_SIGNING_IDENTITY is required" "${TMP_ROOT}/missing-identity.err" \
    || fail "missing identity failure must explain the release requirement"
  [[ ! -s "${log}" ]] || fail "codesign must not run without a release identity"
  [[ ! -e "${root}/Lookback.dmg" ]] || fail "a missing signing identity must not produce a DMG"
}

test_unsigned_intermediate_is_explicit_and_app_only() {
  local root="${TMP_ROOT}/unsigned" paths source app log
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  log="${TMP_ROOT}/codesign-unsigned.log"
  make_tools "${TMP_ROOT}/tools-unsigned" "${log}"
  unset APPLE_SIGNING_IDENTITY
  run_finalizer "${source}" "${app}" --unsigned-intermediate
  [[ ! -s "${log}" ]] || fail "explicit unsigned intermediate mode must not sign"
  if run_finalizer "${source}" "${app}" --unsigned-intermediate \
    --dmg "${root}/Lookback.dmg" >"${TMP_ROOT}/unsigned-dmg.out" 2>&1; then
    fail "unsigned intermediate mode must reject DMG creation"
  fi
  [[ ! -e "${root}/Lookback.dmg" ]] || fail "unsigned intermediate mode must not create a distributable DMG"
}

test_tampered_assembly_is_rejected_before_signing() {
  local root="${TMP_ROOT}/tampered" paths source app log
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  log="${TMP_ROOT}/codesign-tampered.log"
  make_tools "${TMP_ROOT}/tools-tampered" "${log}"
  export APPLE_SIGNING_IDENTITY=fixture-identity
  printf 'tampered\n' >>"${app}/Contents/MacOS/front"
  if run_finalizer "${source}" "${app}" --dmg "${root}/Lookback.dmg" \
    >"${TMP_ROOT}/tampered.out" 2>"${TMP_ROOT}/tampered.err"; then
    fail "post-assembly changes to an external binary must be rejected"
  fi
  grep -Fq "does not match trusted build input" "${TMP_ROOT}/tampered.err" \
    || fail "tampered assembly failure must identify the trusted input comparison"
  [[ ! -s "${log}" ]] || fail "assembly verification must precede all signing"
}

test_unexpected_macho_is_rejected() {
  local root="${TMP_ROOT}/unexpected" paths source app log
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  log="${TMP_ROOT}/codesign-unexpected.log"
  make_tools "${TMP_ROOT}/tools-unexpected" "${log}"
  export APPLE_SIGNING_IDENTITY=fixture-identity
  printf 'unexpected Mach-O\n' >"${app}/Contents/Resources/UnexpectedMachO"
  if run_finalizer "${source}" "${app}" --dmg "${root}/Lookback.dmg" \
    >"${TMP_ROOT}/unexpected.out" 2>"${TMP_ROOT}/unexpected.err"; then
    fail "an unproven executable embedded in the app must be rejected"
  fi
  grep -Fq "unexpected Mach-O artifact" "${TMP_ROOT}/unexpected.err" \
    || fail "unexpected Mach-O rejection must identify the untrusted artifact"
  [[ ! -s "${log}" ]] || fail "unexpected app contents must be rejected before signing"
}

test_external_broken_and_cyclic_app_symlinks_are_rejected_before_signing() {
  local mode root paths source app log link outside target
  for mode in absolute relative broken cyclic; do
    root="${TMP_ROOT}/symlink-${mode}"
    paths=$(prepare_fixture "${root}")
    source=$(printf '%s\n' "${paths}" | sed -n '1p')
    app=$(printf '%s\n' "${paths}" | sed -n '2p')
    log="${TMP_ROOT}/codesign-symlink-${mode}.log"
    make_tools "${TMP_ROOT}/tools-symlink-${mode}" "${log}"
    export APPLE_SIGNING_IDENTITY=fixture-identity
    link="${app}/Contents/Frameworks/Fixture.framework/Assets"
    outside="${root}/outside"
    printf 'outside the app\n' >"${outside}"
    case "${mode}" in
      absolute) target=${outside} ;;
      relative) target=../../../../outside ;;
      broken) target=missing-target ;;
      cyclic) target=Assets ;;
    esac
    ln -s "${target}" "${link}"
    if run_finalizer "${source}" "${app}" \
      >"${TMP_ROOT}/symlink-${mode}.out" 2>"${TMP_ROOT}/symlink-${mode}.err"; then
      fail "${mode} app symlink must be rejected"
    fi
    grep -Fq 'app symlink' "${TMP_ROOT}/symlink-${mode}.err" \
      || fail "${mode} symlink rejection must identify the app link"
    [[ ! -s "${log}" ]] \
      || fail "${mode} app symlink must be rejected before signing"
  done
}

test_signing_failure_and_post_sign_tampering_are_rejected() {
  local root="${TMP_ROOT}/sign-failure" paths source app log
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  log="${TMP_ROOT}/codesign-failure.log"
  make_tools "${TMP_ROOT}/tools-failure" "${log}"
  export APPLE_SIGNING_IDENTITY=fixture-identity
  export FAIL_CODESIGN_TARGET="${app}/Contents/MacOS/front"
  if run_finalizer "${source}" "${app}" --dmg "${root}/Lookback.dmg" \
    >"${TMP_ROOT}/sign-failure.out" 2>"${TMP_ROOT}/sign-failure.err"; then
    fail "a nested codesign failure must stop bundle finalization"
  fi
  [[ ! -e "${root}/Lookback.dmg" ]] || fail "a failed sign must not produce a DMG"
  unset FAIL_CODESIGN_TARGET

  root="${TMP_ROOT}/post-sign-tamper"
  paths=$(prepare_fixture "${root}")
  source=$(printf '%s\n' "${paths}" | sed -n '1p')
  app=$(printf '%s\n' "${paths}" | sed -n '2p')
  log="${TMP_ROOT}/codesign-post-tamper.log"
  make_tools "${TMP_ROOT}/tools-post-tamper" "${log}"
  export TAMPER_AFTER_SIGN_TARGET="${app}/Contents/MacOS/front"
  if run_finalizer "${source}" "${app}" --dmg "${root}/Lookback.dmg" \
    >"${TMP_ROOT}/post-sign-tamper.out" 2>"${TMP_ROOT}/post-sign-tamper.err"; then
    fail "tampering after nested signing must fail signature verification"
  fi
  [[ ! -e "${root}/Lookback.dmg" ]] || fail "post-sign tampering must prevent DMG creation"
  unset TAMPER_AFTER_SIGN_TARGET
}

test_signs_and_records_final_macos_bytes
test_missing_identity_fails_closed
test_unsigned_intermediate_is_explicit_and_app_only
test_tampered_assembly_is_rejected_before_signing
test_unexpected_macho_is_rejected
test_external_broken_and_cyclic_app_symlinks_are_rejected_before_signing
test_signing_failure_and_post_sign_tampering_are_rejected

echo "macOS bundle finalization tests passed"
