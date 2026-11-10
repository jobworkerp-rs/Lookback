#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="${1:-${ROOT_DIR}/.github/workflows/release.yml}"

assert_contains() {
  local needle="$1"
  if ! grep -Fq "${needle}" "${WORKFLOW}"; then
    echo "expected release workflow to contain: ${needle}" >&2
    exit 1
  fi
}

assert_order() {
  local first="$1"
  local second="$2"
  local first_line
  local second_line
  first_line="$(grep -nF "${first}" "${WORKFLOW}" | head -n 1 | cut -d: -f1 || true)"
  second_line="$(grep -nF "${second}" "${WORKFLOW}" | head -n 1 | cut -d: -f1 || true)"
  if [[ -z "${first_line}" || -z "${second_line}" || "${first_line}" -ge "${second_line}" ]]; then
    echo "expected '${first}' to appear before '${second}'" >&2
    exit 1
  fi
}

[[ -f "${WORKFLOW}" ]] || { echo "release workflow not found: ${WORKFLOW}" >&2; exit 1; }

assert_contains "build-macos:"
assert_contains "name: Build macOS app (unsigned)"
assert_contains "runs-on: [self-hosted, macOS, lookback-macos]"
assert_contains "targets: aarch64-apple-darwin"
assert_contains "bash scripts/build-release.sh --profile mac --macos-unsigned-intermediate --bundle app"
assert_contains "brew install protobuf cmake pkgconf jq"
assert_contains "command -v jq >/dev/null || { echo \"jq missing\" >&2; exit 1; }"
assert_contains "name: macos-unsigned-app"
assert_contains "sign-macos:"
assert_contains "name: Sign and notarize macOS DMG"
assert_contains "environment: release-macos"
assert_contains "APPLE_SIGNING_IDENTITY: \${{ secrets.APPLE_SIGNING_IDENTITY }}"
assert_contains "APPLE_NOTARY_KEY: \${{ secrets.APPLE_NOTARY_KEY }}"
assert_contains "APPLE_NOTARY_KEY_ID: \${{ secrets.APPLE_NOTARY_KEY_ID }}"
assert_contains "APPLE_NOTARY_ISSUER_ID: \${{ secrets.APPLE_NOTARY_ISSUER_ID }}"
assert_contains "Write the notarization API key"
assert_contains "uses: apple-actions/import-codesign-certs@"
assert_contains "p12-file-base64: \${{ secrets.APPLE_CERTIFICATE }}"
assert_contains "p12-password: \${{ secrets.APPLE_CERTIFICATE_PASSWORD }}"
assert_contains "Verify Apple signing identity"
assert_contains "security find-identity -v -p codesigning"
assert_contains "grep -F -- \"\${APPLE_SIGNING_IDENTITY}\""
assert_contains "Verify Apple notarization credentials"
assert_contains "xcrun notarytool history"
assert_contains "bash scripts/finalize-macos-bundle.sh"
assert_contains "Notarize macOS DMG"
assert_contains "xcrun notarytool submit"
assert_contains "xcrun stapler staple"
assert_contains "codesign --verify --deep --strict --verbose=2"
assert_contains 'Contents/Resources/memories-db-migrate/memories-db-migrate'
assert_contains 'Contents/Resources/memories-db-migrate/atlas/bin/atlas'
assert_contains 'node scripts/verify-atlas-lock.mjs'
assert_contains 'Contents/Resources/memories-db-migrate/atlas" darwin-arm64'
assert_contains "find \"\$app/Contents/Resources/plugins\" -name '*.dylib'"
assert_contains "xcrun stapler validate"
assert_contains "target/release/bundle/dmg/*.dmg"
assert_contains 'LOOKBACK_TAURI_VERBOSE: "1"'
assert_contains "needs: [test, build-cuda]"
assert_order "build-macos:" "sign-macos:"
assert_order "bash scripts/build-release.sh --profile mac --macos-unsigned-intermediate --bundle app" "name: Package the unsigned app"
assert_order "name: Verify and unpack the unsigned app" "uses: apple-actions/import-codesign-certs@"
assert_order "uses: apple-actions/import-codesign-certs@" "bash scripts/finalize-macos-bundle.sh"
assert_order "Verify Apple signing identity" "bash scripts/finalize-macos-bundle.sh"
assert_order "Verify Apple notarization credentials" "bash scripts/finalize-macos-bundle.sh"
assert_order "Write the notarization API key" "Verify Apple notarization credentials"
assert_order "bash scripts/finalize-macos-bundle.sh" "xcrun notarytool submit"
assert_order "xcrun notarytool submit" "spctl -a -t open --context context:primary-signature"
assert_order "sign-macos:" "build:"

echo "release workflow tests passed"
