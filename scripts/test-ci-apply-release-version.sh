#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
tmpdir="$(mktemp -d)"
trap 'rm -rf "${tmpdir}"' EXIT

conf="${tmpdir}/tauri.conf.json"
cp "${ROOT_DIR}/src-tauri/tauri.conf.json" "${conf}"

node "${ROOT_DIR}/scripts/ci-apply-release-version.mjs" v0.0.3 "${conf}"
node -e '
  const { readFileSync } = require("node:fs");
  const conf = JSON.parse(readFileSync(process.argv[1], "utf8"));
  if (conf.version !== "0.0.3") {
    throw new Error(`expected 0.0.3, got ${conf.version}`);
  }
' "${conf}"

node "${ROOT_DIR}/scripts/ci-apply-release-version.mjs" 1.2.3-rc.1 "${conf}"
node -e '
  const { readFileSync } = require("node:fs");
  const conf = JSON.parse(readFileSync(process.argv[1], "utf8"));
  if (conf.version !== "1.2.3-rc.1") {
    throw new Error(`expected 1.2.3-rc.1, got ${conf.version}`);
  }
' "${conf}"

if node "${ROOT_DIR}/scripts/ci-apply-release-version.mjs" release-003 "${conf}" 2>"${tmpdir}/invalid.log"; then
  echo "expected invalid tag to fail" >&2
  exit 1
fi
grep -Fq "release tag must be a semver version" "${tmpdir}/invalid.log"

echo "release version script tests passed"
