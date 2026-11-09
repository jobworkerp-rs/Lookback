#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)
VERIFY="${SCRIPT_DIR}/verify-migration-release.mjs"
FIXTURE="${SCRIPT_DIR}/lib/migration-test-fixture.mjs"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

make_fixture() { node "${FIXTURE}" "$1" "${2:-valid}"; }

node - "${AGENT_APP}/package.json" "${AGENT_APP}/src-tauri/tauri.conf.json" <<'NODE'
const fs = require("node:fs");
const [packagePath, tauriPath] = process.argv.slice(2);
const pkg = JSON.parse(fs.readFileSync(packagePath, "utf8"));
const tauri = JSON.parse(fs.readFileSync(tauriPath, "utf8"));
if (pkg.scripts?.["verify:migration-bundle"] !==
    "node scripts/verify-migration-release.mjs src-tauri/migration-bundle") {
  throw new Error("package script must verify the staged migration bundle");
}
if (!tauri.build?.beforeBuildCommand?.includes("pnpm verify:migration-bundle")) {
  throw new Error("Tauri beforeBuildCommand must verify the migration bundle");
}
NODE

make_fixture "${TMP_ROOT}/valid" valid
node "${VERIFY}" "${TMP_ROOT}/valid" >/dev/null
node "${VERIFY}" "${TMP_ROOT}/valid" --require-provenance \
  --front "${TMP_ROOT}/valid/test-artifacts/front" \
  --import "${TMP_ROOT}/valid/test-artifacts/memories-import" \
  --app-assets "${TMP_ROOT}/valid.app-assets" >/dev/null

mkdir -p "${TMP_ROOT}/distributed-assets"
cp -R "${TMP_ROOT}/valid.app-assets/workers" "${TMP_ROOT}/distributed-assets/"
node "${VERIFY}" "${TMP_ROOT}/valid" --require-provenance \
  --front "${TMP_ROOT}/valid/test-artifacts/front" \
  --import "${TMP_ROOT}/valid/test-artifacts/memories-import" \
  --app-assets "${TMP_ROOT}/valid.app-assets" \
  --distributed-assets "${TMP_ROOT}/distributed-assets" \
  --atlas-platform darwin-arm64 >/dev/null
printf '\n# distributed-only drift\n' \
  >>"${TMP_ROOT}/distributed-assets/workers/workflows/auto-embedding.yaml"
if node "${VERIFY}" "${TMP_ROOT}/valid" --require-provenance \
  --front "${TMP_ROOT}/valid/test-artifacts/front" \
  --import "${TMP_ROOT}/valid/test-artifacts/memories-import" \
  --app-assets "${TMP_ROOT}/valid.app-assets" \
  --distributed-assets "${TMP_ROOT}/distributed-assets" \
  --atlas-platform darwin-arm64 >/dev/null 2>&1; then
  echo "verification accepted a packaged workflow that differs from the source contract" >&2
  exit 1
fi

printf 'wrong source binary\n' >"${TMP_ROOT}/wrong-front"
if node "${VERIFY}" "${TMP_ROOT}/valid" --require-provenance --front "${TMP_ROOT}/wrong-front" \
  --import "${TMP_ROOT}/valid/test-artifacts/memories-import" \
  --app-assets "${TMP_ROOT}/valid.app-assets" >/dev/null 2>&1; then
  echo "verification accepted a front binary from a different source revision" >&2
  exit 1
fi
cp -R "${TMP_ROOT}/valid" "${TMP_ROOT}/missing-provenance"
rm "${TMP_ROOT}/missing-provenance/memories-source-provenance.json"
if node "${VERIFY}" "${TMP_ROOT}/missing-provenance" --require-provenance \
  --app-assets "${TMP_ROOT}/valid.app-assets" >/dev/null 2>&1; then
  echo "release verification accepted a bundle with no source provenance" >&2
  exit 1
fi

make_fixture "${TMP_ROOT}/dev" dev
node "${VERIFY}" "${TMP_ROOT}/dev" >/dev/null

# Without provenance the bundle must vouch for itself through `bundle verify`;
# a CLI that rejects its bundle or reports another digest must not pass.
for mode in broken-cli stale-digest provenance-mismatch; do
  make_fixture "${TMP_ROOT}/${mode}" "${mode}"
  if node "${VERIFY}" "${TMP_ROOT}/${mode}" >/dev/null 2>&1; then
    echo "verification accepted invalid migration fixture: ${mode}" >&2
    exit 1
  fi
done

cp -R "${TMP_ROOT}/valid" "${TMP_ROOT}/edited-catalog"
printf '{}\n' >"${TMP_ROOT}/edited-catalog/atlas/post-migration-tasks.json"
if node "${VERIFY}" "${TMP_ROOT}/edited-catalog" >/dev/null 2>&1; then
  echo "verification accepted a release bundle whose catalog differs from its provenance" >&2
  exit 1
fi

echo "migration build guard tests passed"
