#!/usr/bin/env bash
# Exercises scripts/macos-signing-handoff.sh with real archives.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HANDOFF="${SCRIPT_DIR}/macos-signing-handoff.sh"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

# A build tree holding every handed-over path, with the symlink and the
# executable bit a real app carries.
make_build_tree() {
  local root="$1"
  local app="${root}/target/release/bundle/macos/Lookback.app"
  mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Frameworks/X.framework/Versions/A" \
    "${root}/src-tauri/bin" "${root}/src-tauri/plugins" "${root}/src-tauri/migration-bundle/atlas/bin" \
    "${root}/dict" "${root}/resources/coding-agent" "${root}/scripts"
  printf 'main\n' >"${app}/Contents/MacOS/Lookback"
  chmod 755 "${app}/Contents/MacOS/Lookback"
  printf 'lib\n' >"${app}/Contents/Frameworks/X.framework/Versions/A/X"
  ln -s A "${app}/Contents/Frameworks/X.framework/Versions/Current"
  printf 'front\n' >"${root}/src-tauri/bin/front-aarch64-apple-darwin"
  chmod 755 "${root}/src-tauri/bin/front-aarch64-apple-darwin"
  printf 'plugin\n' >"${root}/src-tauri/plugins/libx.dylib"
  printf 'atlas\n' >"${root}/src-tauri/migration-bundle/atlas/bin/atlas"
  printf 'dict\n' >"${root}/dict/word.csv"
  printf '{}\n' >"${root}/resources/coding-agent/manifest.json"
  # Present in the build tree but never part of the hand-off.
  printf 'untrusted\n' >"${root}/scripts/finalize-macos-bundle.sh"
}

build="${TMP_ROOT}/build"
make_build_tree "${build}"
archive="${TMP_ROOT}/handoff.tar"

sha=$(cd "${build}" && bash "${HANDOFF}" pack "${archive}")
[[ "${sha}" =~ ^[0-9a-f]{64}$ ]] || fail "pack must print the archive sha256, got: ${sha}"
[[ "${sha}" == "$(shasum -a 256 "${archive}" | cut -d' ' -f1)" ]] || fail "pack printed a wrong sha256"
! tar -tf "${archive}" | grep -q '^scripts/' || fail "pack must not include anything outside the hand-off paths"

# Unpacking reproduces bytes, symlinks, and modes.
dest="${TMP_ROOT}/sign"
mkdir -p "${dest}/scripts"
printf 'trusted\n' >"${dest}/scripts/finalize-macos-bundle.sh"
bash "${HANDOFF}" unpack "${archive}" "${sha}" "${dest}" || fail "unpack of a valid archive failed"
app="${dest}/target/release/bundle/macos/Lookback.app"
[[ -x "${app}/Contents/MacOS/Lookback" ]] || fail "the executable bit must survive the hand-off"
[[ "$(readlink "${app}/Contents/Frameworks/X.framework/Versions/Current")" == "A" ]] \
  || fail "framework symlinks must survive the hand-off"
cmp -s "${build}/src-tauri/bin/front-aarch64-apple-darwin" "${dest}/src-tauri/bin/front-aarch64-apple-darwin" \
  || fail "handed-over bytes must not change"
[[ "$(cat "${dest}/scripts/finalize-macos-bundle.sh")" == "trusted" ]] \
  || fail "unpacking must leave the signer's own scripts alone"

# A wrong checksum stops before anything is unpacked.
dest_bad="${TMP_ROOT}/bad-sha"
mkdir -p "${dest_bad}"
if bash "${HANDOFF}" unpack "${archive}" "$(printf '0%.0s' {1..64})" "${dest_bad}" 2>/dev/null; then
  fail "a checksum mismatch must fail"
fi
[[ -z "$(ls -A "${dest_bad}")" ]] || fail "a checksum mismatch must unpack nothing"

# reject_case NAME ARCHIVE: the archive must be refused and unpack nothing.
reject_case() {
  local name="$1" bad_archive="$2" out="${TMP_ROOT}/reject-${1}"
  mkdir -p "${out}"
  if bash "${HANDOFF}" unpack "${bad_archive}" "$(shasum -a 256 "${bad_archive}" | cut -d' ' -f1)" "${out}" 2>/dev/null; then
    fail "${name} must be refused"
  fi
  [[ -z "$(ls -A "${out}")" ]] || fail "${name} must unpack nothing"
}

# An entry outside the hand-off paths, e.g. one replacing a signer script.
(cd "${build}" && tar -cf "${TMP_ROOT}/extra.tar" dict scripts)
reject_case "an archive carrying scripts/" "${TMP_ROOT}/extra.tar"

# A look-alike of an allowed path.
mkdir -p "${build}/dictionary"
printf 'x\n' >"${build}/dictionary/x"
(cd "${build}" && tar -cf "${TMP_ROOT}/lookalike.tar" dict dictionary)
reject_case "a look-alike path" "${TMP_ROOT}/lookalike.tar"

# A path that starts inside an allowed directory and climbs out of it.
mkdir -p "${TMP_ROOT}/climb/dict"
printf 'x\n' >"${TMP_ROOT}/climb/escape"
# GNU tar drops ".." from member names unless -P is given (bsdtar keeps them
# either way), so -P builds the same hostile archive on Linux CI and macOS.
(cd "${TMP_ROOT}/climb" && tar -cPf "${TMP_ROOT}/climb.tar" dict/../escape)
tar -tf "${TMP_ROOT}/climb.tar" | grep -q '\.\.' || fail "fixture must keep the .. member"
reject_case "a member climbing out with .." "${TMP_ROOT}/climb.tar"

echo "macos signing handoff tests passed"
