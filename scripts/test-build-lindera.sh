#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

commands=()

log() { :; }
die() {
  echo "$*" >&2
  exit 1
}
run() {
  commands+=("$*")
}

AGENT_APP="${ROOT_DIR}"
WORKDIR="${ROOT_DIR}/.tmp-test-build-lindera"
DRY_RUN=1
export AGENT_APP WORKDIR DRY_RUN

# shellcheck source=scripts/lib/build-lindera.sh
source "${ROOT_DIR}/scripts/lib/build-lindera.sh"

stage_lindera release

joined="$(printf '%s\n' "${commands[@]}")"

assert_contains() {
  local needle="$1"
  if [[ "${joined}" != *"${needle}"* ]]; then
    echo "expected build-lindera command plan to contain: ${needle}" >&2
    printf '%s\n' "${joined}" >&2
    exit 1
  fi
}

assert_not_contains() {
  local needle="$1"
  if [[ "${joined}" == *"${needle}"* ]]; then
    echo "expected build-lindera command plan not to contain: ${needle}" >&2
    printf '%s\n' "${joined}" >&2
    exit 1
  fi
}

[[ "${LINDERA_VERSION}" == "3.0.7" ]] || {
  echo "expected LINDERA_VERSION=3.0.7, got ${LINDERA_VERSION}" >&2
  exit 1
}

assert_contains "lindera-ipadic-3.0.7.zip"
assert_contains "https://github.com/lindera/lindera/releases/download/v3.0.7/lindera-ipadic-3.0.7.zip"
assert_not_contains "lindera build"
assert_not_contains "lindera-ipadic-aarch64-apple-darwin-v3.0.7.zip"

echo "build-lindera tests passed"
