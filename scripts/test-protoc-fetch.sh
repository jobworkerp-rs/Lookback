#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/protoc-fetch.sh
source "${SCRIPT_DIR}/lib/protoc-fetch.sh"

TMP_ROOT=$(mktemp -d)
cleanup() { rm -rf "${TMP_ROOT}"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# protoc_asset_name maps every supported triple to the official release asset.
# Pure string logic, so this always runs (no network).
test_asset_name_mapping() {
  [[ "$(protoc_asset_name aarch64-apple-darwin)" == "protoc-${PROTOC_VERSION}-osx-aarch_64.zip" ]] \
    || fail "macos arm64 asset name"
  [[ "$(protoc_asset_name x86_64-apple-darwin)" == "protoc-${PROTOC_VERSION}-osx-x86_64.zip" ]] \
    || fail "macos x86_64 asset name"
  [[ "$(protoc_asset_name x86_64-unknown-linux-gnu)" == "protoc-${PROTOC_VERSION}-linux-x86_64.zip" ]] \
    || fail "linux x86_64 asset name"
  [[ "$(protoc_asset_name aarch64-unknown-linux-gnu)" == "protoc-${PROTOC_VERSION}-linux-aarch_64.zip" ]] \
    || fail "linux aarch64 asset name"
}

test_asset_name_unknown_triple_fails() {
  if ( protoc_asset_name some-unknown-triple ) >/dev/null 2>&1; then
    fail "unknown triple should die"
  fi
}

# fetch_protoc_bin --dry-run must not touch the filesystem.
test_dry_run_writes_nothing() {
  local dest="${TMP_ROOT}/dry/protoc-aarch64-apple-darwin"
  DRY_RUN=1 fetch_protoc_bin aarch64-apple-darwin "${dest}" >/dev/null
  [[ ! -e "${dest}" ]] || fail "dry-run wrote ${dest}"
}

# Network + Darwin only: the staged protoc must be self-contained (no @rpath /
# Homebrew dylib refs) and run with NO DYLD_* set. This is the core regression
# for the worker-registration dyld crash.
test_official_protoc_self_contained() {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "skip self-contained test (not macOS)" >&2
    return 0
  fi
  local triple
  case "$(uname -m)" in
    arm64|aarch64) triple=aarch64-apple-darwin ;;
    x86_64)        triple=x86_64-apple-darwin ;;
    *) echo "skip self-contained test (unknown arch)" >&2; return 0 ;;
  esac

  local dest="${TMP_ROOT}/staged/protoc-${triple}"
  if ! fetch_protoc_bin "${triple}" "${dest}" >/dev/null 2>"${TMP_ROOT}/fetch.err"; then
    echo "skip self-contained test (fetch failed, likely offline):" >&2
    cat "${TMP_ROOT}/fetch.err" >&2
    return 0
  fi

  otool -L "${dest}" | tail -n +2 | grep -E '@rpath/|/opt/homebrew/' \
    && fail "staged protoc still references @rpath / Homebrew dylibs"

  # No DYLD_* env: the binary must resolve its deps entirely on its own.
  env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH "${dest}" --version \
    | grep -qx "libprotoc ${PROTOC_VERSION}" \
    || fail "staged protoc did not run standalone"
}

# Second fetch over an already-correct binary must skip the download.
test_fetch_is_idempotent() {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "skip idempotency test (not macOS)" >&2
    return 0
  fi
  local triple
  case "$(uname -m)" in
    arm64|aarch64) triple=aarch64-apple-darwin ;;
    x86_64)        triple=x86_64-apple-darwin ;;
    *) return 0 ;;
  esac
  local dest="${TMP_ROOT}/idem/protoc-${triple}"
  fetch_protoc_bin "${triple}" "${dest}" >/dev/null 2>/dev/null \
    || { echo "skip idempotency test (fetch failed, likely offline)" >&2; return 0; }

  fetch_protoc_bin "${triple}" "${dest}" 2>"${TMP_ROOT}/idem.err" >/dev/null
  grep -q "already staged" "${TMP_ROOT}/idem.err" \
    || fail "second fetch did not skip (expected 'already staged')"
}

test_asset_name_mapping
test_asset_name_unknown_triple_fails
test_dry_run_writes_nothing
test_official_protoc_self_contained
test_fetch_is_idempotent

echo "protoc-fetch tests passed"
