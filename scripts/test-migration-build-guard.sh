#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)

# This verifies the source checkout layout for clean CI clones. It is not
# intended to run after release staging replaces this directory with a real bundle.
test -f "${AGENT_APP}/src-tauri/migration-bundle/.gitkeep"

node - "${AGENT_APP}/package.json" "${AGENT_APP}/src-tauri/tauri.conf.json" <<'NODE'
const fs = require("node:fs");
const [packagePath, tauriPath] = process.argv.slice(2);
const pkg = JSON.parse(fs.readFileSync(packagePath, "utf8"));
const tauri = JSON.parse(fs.readFileSync(tauriPath, "utf8"));
if (pkg.scripts?.["verify:migration-bundle"] !==
    "node scripts/verify-migration-release.mjs src-tauri/src/sidecar/migration_gate.rs src-tauri/migration-bundle") {
  throw new Error("package script must verify the staged migration bundle");
}
if (!tauri.build?.beforeBuildCommand?.includes("pnpm verify:migration-bundle")) {
  throw new Error("Tauri beforeBuildCommand must verify the migration bundle");
}
NODE

tmp_root=$(mktemp -d)
trap 'rm -rf "${tmp_root}"' EXIT
mkdir -p "${tmp_root}/bundle/atlas/sqlite/migrations"
printf '%s\n' \
  'pub const STARTUP_MIGRATION: MigrationRelease = MigrationRelease {' \
  '    migration_id: "thread-message-times-v1",' \
  '    expected_schema_contract: "20260803000003",' \
  '};' >"${tmp_root}/release.rs"
printf '%s\n' \
  "INSERT INTO memories_schema_contract (contract_key, version) VALUES ('rdb_schema', '20260803000003');" \
  >"${tmp_root}/bundle/atlas/sqlite/migrations/20260803000003_schema_contract.sql"
printf 'sum\n' >"${tmp_root}/bundle/atlas/sqlite/migrations/atlas.sum"
printf '%s\n' \
  '{"tasks":[{"id":"thread-message-times-v1","generation":1,"lifecycle":"active","completion_required_by_schema_version":"20260803000003"}]}' \
  >"${tmp_root}/bundle/atlas/post-migration-tasks.json"

if node "${SCRIPT_DIR}/verify-migration-release.mjs" \
  "${tmp_root}/release.rs" "${tmp_root}/bundle" >/dev/null 2>&1; then
  echo "verification accepted a bundle without memories-db-migrate" >&2
  exit 1
fi

echo "migration build guard tests passed"
