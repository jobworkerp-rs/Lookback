#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP_SOURCE=$(cd "${SCRIPT_DIR}/.." && pwd)
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

prepare_release_fixture() {
  local root=$1
  local bundle="${root}/app/src-tauri/migration-bundle"
  local app_assets="${bundle}.app-assets"
  mkdir -p "${root}/app/scripts/lib" "${root}/bin"
  node "${SCRIPT_DIR}/lib/migration-test-fixture.mjs" "${bundle}" valid
  cp -R "${app_assets}/." "${root}/app/"
  cp "${SCRIPT_DIR}/verify-migration-release.mjs" "${root}/app/scripts/"
  cp "${SCRIPT_DIR}/verify-embedding-contract.mjs" "${root}/app/scripts/"
  cp "${SCRIPT_DIR}/record-signed-atlas-lock.mjs" "${root}/app/scripts/"
  cp "${SCRIPT_DIR}/lib/record-signed-migration-provenance.mjs" "${root}/app/scripts/lib/"
  cp "${bundle}/test-artifacts/front" "${root}/bin/front-test-triple"
  cp "${bundle}/test-artifacts/memories-import" "${root}/bin/memories-import-test-triple"
}

test_provenance_writer_separates_source_and_distributed_hashes() {
  local root="${TMP_ROOT}/writer"
  local bundle="${root}/bundle"
  local source="${TMP_ROOT}/writer/memories" revision
  node "${SCRIPT_DIR}/lib/migration-test-fixture.mjs" "${bundle}" valid
  mkdir -p "${source}/infra/atlas/sqlite/migrations" \
    "${source}/infra/atlas/postgres/migrations" "${source}/grpc-admin/src/bin" \
    "${source}/agent-chat-import"
  cp "${bundle}/atlas/post-migration-tasks.json" "${source}/infra/atlas/post-migration-tasks.json"
  cp "${bundle}/atlas/post-migration-task-history.json" "${source}/infra/atlas/post-migration-task-history.json"
  cp "${bundle}/atlas/sqlite/migrations/20260930000001_fixture.sql" \
    "${source}/infra/atlas/sqlite/migrations/"
  cp "${bundle}/atlas/postgres/migrations/20260930000001_fixture.sql" \
    "${source}/infra/atlas/postgres/migrations/"
  : >"${source}/grpc-admin/Cargo.toml"
  : >"${source}/grpc-admin/src/bin/front.rs"
  : >"${source}/grpc-admin/src/bin/memories_db_migrate.rs"
  : >"${source}/agent-chat-import/Cargo.toml"
  git -C "${source}" init -q -b test-ref
  git -C "${source}" remote add origin https://github.com/jobworkerp-rs/memory-store.git
  git -C "${source}" -c user.name=fixture -c user.email=fixture@example.invalid \
    add infra grpc-admin agent-chat-import
  git -C "${source}" -c user.name=fixture -c user.email=fixture@example.invalid \
    commit -qm "verified migration source fixture"
  revision=$(git -C "${source}" rev-parse HEAD)
  git -C "${source}" tag -a v0.0.19 -m "v0.0.19"

  node "${SCRIPT_DIR}/lib/write-migration-source-provenance.mjs" \
    "${source}" "${bundle}" v0.0.19 "${revision}" \
    "${bundle}/test-artifacts/front" "${bundle}/test-artifacts/memories-import" >/dev/null
  node - "${bundle}/memories-source-provenance.json" \
    "${bundle}.app-assets/embedding-contract.json" "${revision}" <<'NODE'
const fs = require("node:fs");
const [provenancePath, contractPath, revision] = process.argv.slice(2);
const provenance = JSON.parse(fs.readFileSync(provenancePath, "utf8"));
const contract = JSON.parse(fs.readFileSync(contractPath, "utf8"));
if (provenance.source_repository !== "https://github.com/jobworkerp-rs/memory-store.git" ||
    provenance.source_ref !== "v0.0.19" || provenance.source_revision !== revision) {
  throw new Error("source provenance must preserve the real public origin, release tag, and peeled SHA");
}
contract.source_revision = revision;
provenance.embedding_contract = contract;
fs.writeFileSync(provenancePath, JSON.stringify(provenance, null, 2) + "\n");
NODE
  node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${bundle}" \
    --require-provenance --front "${bundle}/test-artifacts/front" \
    --import "${bundle}/test-artifacts/memories-import" \
    --app-assets "${bundle}.app-assets" >/dev/null
  node - "${bundle}/memories-source-provenance.json" <<'NODE'
const fs = require("node:fs");
const provenance = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (provenance.format_version !== 3 ||
    JSON.stringify(provenance.source_artifact_sha256) !==
      JSON.stringify(provenance.distributed_artifact_sha256)) {
  throw new Error("source provenance must initialize distinct source and distributed digest maps equally");
}
NODE
}

mkdir -p "${TMP_ROOT}/mock-bin"
cat >"${TMP_ROOT}/mock-bin/codesign" <<'MOCK_CODESIGN'
#!/usr/bin/env bash
set -euo pipefail
target=""
for argument in "$@"; do target=${argument}; done
[[ -f "${target}" ]] || { echo "mock codesign target missing" >&2; exit 2; }
printf '\n// mock signed bytes\n' >>"${target}"
printf '%s\n' "${target}" >>"${MOCK_CODESIGN_LOG}"
MOCK_CODESIGN
chmod +x "${TMP_ROOT}/mock-bin/codesign"
export MOCK_CODESIGN_LOG="${TMP_ROOT}/codesign.log"
export PATH="${TMP_ROOT}/mock-bin:${PATH}"

source "${SCRIPT_DIR}/lib/build-common.sh"
source "${SCRIPT_DIR}/lib/build-deps.sh"

test_provenance_writer_separates_source_and_distributed_hashes

prepare_release_fixture "${TMP_ROOT}/tampered-before-sign"
AGENT_APP="${TMP_ROOT}/tampered-before-sign/app"
BIN_DIR="${TMP_ROOT}/tampered-before-sign/bin"
TRIPLE=test-triple
PLATFORM=mac
DRY_RUN=0
APPLE_SIGNING_IDENTITY=fixture-identity
printf '\n// unauthorized pre-sign modification\n' \
  >>"${AGENT_APP}/src-tauri/migration-bundle/memories-db-migrate"
if (sign_macos_migration_binaries) >/dev/null 2>&1; then
  echo "signing accepted a migration CLI changed after source verification" >&2
  exit 1
fi
[[ ! -e "${MOCK_CODESIGN_LOG}" ]] || {
  echo "codesign ran before migration provenance verification" >&2
  exit 1
}

prepare_release_fixture "${TMP_ROOT}/signed"
AGENT_APP="${TMP_ROOT}/signed/app"
BIN_DIR="${TMP_ROOT}/signed/bin"
TRIPLE=test-triple
bundle="${AGENT_APP}/src-tauri/migration-bundle"
node "${SCRIPT_DIR}/verify-migration-release.mjs" \
  "${bundle}" \
  --require-provenance --front "${BIN_DIR}/front-test-triple" \
  --import "${BIN_DIR}/memories-import-test-triple" \
  --app-assets "${AGENT_APP}" >/dev/null

sign_macos_migration_binaries
record_signed_atlas_lock
# The test runs on Linux while exercising the macOS release path; keep the
# host-selected lock entry aligned with the bytes altered by the codesign mock.
node "${AGENT_APP}/scripts/record-signed-atlas-lock.mjs" \
  "${bundle}/atlas" linux-amd64 >/dev/null
cp "${BIN_DIR}/front-test-triple" "${TMP_ROOT}/front-pre-record"
printf '\n// unauthorized post-sign change\n' >>"${BIN_DIR}/front-test-triple"
if (record_signed_migration_provenance) >/dev/null 2>&1; then
  echo "provenance recorder accepted an unsigned front binary change" >&2
  exit 1
fi
mv "${TMP_ROOT}/front-pre-record" "${BIN_DIR}/front-test-triple"
sign_macos_files "verified Memories sidecars" \
  "${BIN_DIR}/front-test-triple" "${BIN_DIR}/memories-import-test-triple"
record_signed_migration_provenance memories_db_migrate front memories_import

node - "${bundle}/memories-source-provenance.json" <<'NODE'
const fs = require("node:fs");
const provenance = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (provenance.format_version !== 3 ||
    provenance.source_artifact_sha256.memories_db_migrate ===
      provenance.distributed_artifact_sha256.memories_db_migrate ||
    provenance.source_artifact_sha256.front === provenance.distributed_artifact_sha256.front ||
    provenance.source_artifact_sha256.memories_import === provenance.distributed_artifact_sha256.memories_import ||
    JSON.stringify(provenance.signed_artifacts) !==
      JSON.stringify(["front", "memories_db_migrate", "memories_import"])) {
  throw new Error("signed provenance must preserve source hashes and record every signed distributable hash");
}
NODE

node "${SCRIPT_DIR}/verify-migration-release.mjs" \
  "${bundle}" \
  --require-provenance --front "${BIN_DIR}/front-test-triple" \
  --import "${BIN_DIR}/memories-import-test-triple" \
  --app-assets "${AGENT_APP}" >/dev/null

for artifact in memories-db-migrate front memories-import; do
  case "${artifact}" in
    memories-db-migrate) target="${bundle}/memories-db-migrate" ;;
    front) target="${BIN_DIR}/front-test-triple" ;;
    memories-import) target="${BIN_DIR}/memories-import-test-triple" ;;
  esac
  cp "${target}" "${TMP_ROOT}/${artifact}.signed"
  printf '\n// post-sign tamper\n' >>"${target}"
  if node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${bundle}" \
    --require-provenance --front "${BIN_DIR}/front-test-triple" \
    --import "${BIN_DIR}/memories-import-test-triple" \
    --app-assets "${AGENT_APP}" >/dev/null 2>&1; then
    echo "release verification accepted post-sign tampering of ${artifact}" >&2
    exit 1
  fi
  mv "${TMP_ROOT}/${artifact}.signed" "${target}"
done

echo "migration signing provenance tests passed"
