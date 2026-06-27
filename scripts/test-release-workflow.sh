#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -n "${1:-}" ]]; then
  WORKFLOW="$1"
elif [[ -f "${ROOT_DIR}/.github/workflows/release.yml" ]]; then
  WORKFLOW="${ROOT_DIR}/.github/workflows/release.yml"
else
  WORKFLOW="${ROOT_DIR}/../agent-app-public/.github/workflows/release.yml"
fi

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
assert_contains "name: Build macOS DMG"
assert_contains "runs-on: macos-15"
assert_contains "targets: aarch64-apple-darwin"
assert_contains "APPLE_CERTIFICATE: \${{ secrets.APPLE_CERTIFICATE }}"
assert_contains "APPLE_CERTIFICATE_PASSWORD: \${{ secrets.APPLE_CERTIFICATE_PASSWORD }}"
assert_contains "APPLE_SIGNING_IDENTITY: \${{ secrets.APPLE_SIGNING_IDENTITY }}"
assert_contains "APPLE_ID: \${{ secrets.APPLE_ID }}"
assert_contains "APPLE_PASSWORD: \${{ secrets.APPLE_PASSWORD }}"
assert_contains "APPLE_TEAM_ID: \${{ secrets.APPLE_TEAM_ID }}"
assert_contains "uses: apple-actions/import-codesign-certs@v3"
assert_contains "p12-file-base64: \${{ secrets.APPLE_CERTIFICATE }}"
assert_contains "p12-password: \${{ secrets.APPLE_CERTIFICATE_PASSWORD }}"
assert_contains "Verify Apple signing identity"
assert_contains "security find-identity -v -p codesigning"
assert_contains "grep -F -- \"\${APPLE_SIGNING_IDENTITY}\""
assert_contains "Verify Apple notarization credentials"
assert_contains "xcrun notarytool history"
assert_contains "bash scripts/build-release.sh --profile mac"
assert_contains "Notarize macOS DMG"
assert_contains "xcrun notarytool submit"
assert_contains "xcrun stapler staple"
assert_contains "codesign --verify --deep --strict --verbose=2"
assert_contains "find \"\$app/Contents/Resources/plugins\" -name '*.dylib'"
assert_contains "xcrun stapler validate"
assert_contains "target/release/bundle/dmg/*.dmg"
assert_contains "needs: [test, build-macos]"
assert_order "uses: apple-actions/import-codesign-certs@v3" "bash scripts/build-release.sh --profile mac"
assert_order "Verify Apple signing identity" "bash scripts/build-release.sh --profile mac"
assert_order "Verify Apple notarization credentials" "bash scripts/build-release.sh --profile mac"
assert_order "bash scripts/build-release.sh --profile mac" "xcrun notarytool submit"
assert_order "xcrun notarytool submit" "spctl -a -t open --context context:primary-signature"
assert_order "build-macos:" "build:"

echo "release workflow tests passed"
