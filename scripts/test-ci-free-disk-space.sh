#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="${SCRIPT_DIR}/ci-free-disk-space.sh"
TMP_DIR="$(mktemp -d)"
OUTSIDE_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}" "${OUTSIDE_DIR}"' EXIT

assert_path_absent() {
  local path="$1"
  if [[ -e "${path}" || -L "${path}" ]]; then
    echo "expected path to be absent: ${path}" >&2
    exit 1
  fi
}

assert_path_present() {
  local path="$1"
  if [[ ! -e "${path}" && ! -L "${path}" ]]; then
    echo "expected path to be present: ${path}" >&2
    exit 1
  fi
}

mkdir -p \
  "${TMP_DIR}/usr/share/dotnet" \
  "${TMP_DIR}/usr/local/lib/android" \
  "${TMP_DIR}/usr/local/share/boost" \
  "${TMP_DIR}/usr/local/share/powershell" \
  "${TMP_DIR}/opt/ghc" \
  "${TMP_DIR}/opt/hostedtoolcache/CodeQL" \
  "${TMP_DIR}/opt/hostedtoolcache/go" \
  "${TMP_DIR}/opt/hostedtoolcache/Python" \
  "${TMP_DIR}/opt/hostedtoolcache/Ruby" \
  "${TMP_DIR}/keep"
touch "${TMP_DIR}/keep/file"
mkdir -p "${OUTSIDE_DIR}/usr/share/dotnet"
touch "${OUTSIDE_DIR}/usr/share/dotnet/file"

"${SCRIPT}" --root "${TMP_DIR}" >/tmp/lookback-ci-free-disk-space-test.log

assert_path_absent "${TMP_DIR}/usr/share/dotnet"
assert_path_absent "${TMP_DIR}/usr/local/lib/android"
assert_path_absent "${TMP_DIR}/opt/hostedtoolcache/CodeQL"
assert_path_present "${TMP_DIR}/keep/file"
assert_path_present "${OUTSIDE_DIR}/usr/share/dotnet/file"

mkdir -p "${TMP_DIR}/usr/share/dotnet"
"${SCRIPT}" --root "${TMP_DIR}" --dry-run >/tmp/lookback-ci-free-disk-space-test.log
assert_path_present "${TMP_DIR}/usr/share/dotnet"

"${SCRIPT}" --root "${TMP_DIR}" >/tmp/lookback-ci-free-disk-space-test.log
"${SCRIPT}" --root "${TMP_DIR}" >/tmp/lookback-ci-free-disk-space-test.log

echo "ci-free-disk-space tests passed"
