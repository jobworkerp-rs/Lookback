#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
VERIFY="${SCRIPT_DIR}/verify-embedding-contract.mjs"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

SOURCE="${TMP_ROOT}/memories"
APP="${TMP_ROOT}/agent-app"
PROVENANCE="${TMP_ROOT}/memories-source-provenance.json"

mkdir -p "${SOURCE}/protobuf/protobuf/llm_memory/data" \
  "${SOURCE}/protobuf/protobuf/llm_memory/service" \
  "${SOURCE}/grpc-admin/src/bin/memories_db_migrate" \
  "${SOURCE}/grpc-admin/src/bin" "${SOURCE}/agent-chat-import" \
  "${SOURCE}/infra/atlas" "${SOURCE}/scripts" \
  "${APP}/src-tauri/src/commands" "${APP}/workers/workflows/rag" \
  "${APP}/proto/llm_memory/data" "${APP}/proto/llm_memory/service"

printf 'syntax = "proto3";\npackage llm_memory.data;\n' \
  >"${SOURCE}/protobuf/protobuf/llm_memory/data/embedding_space.proto"
printf 'syntax = "proto3";\npackage llm_memory.service;\n' \
  >"${SOURCE}/protobuf/protobuf/llm_memory/service/embedding_space.proto"
printf '[package]\nname = "grpc-admin"\n' >"${SOURCE}/grpc-admin/Cargo.toml"
printf 'fn main() {}\n' >"${SOURCE}/grpc-admin/src/bin/front.rs"
printf '[package]\nname = "agent-chat-import"\n' >"${SOURCE}/agent-chat-import/Cargo.toml"
printf '{}\n' >"${SOURCE}/infra/atlas/post-migration-tasks.json"
printf '{}\n' >"${SOURCE}/infra/atlas/post-migration-task-history.json"
printf '#!/usr/bin/env bash\n' >"${SOURCE}/scripts/build-memories-db-migrate-sqlite.sh"

write_workflow() {
  local source_path=$1 body=$2 app_path=${1#workflows/}
  mkdir -p "${SOURCE}/$(dirname "${source_path}")" \
    "${APP}/workers/workflows/$(dirname "${app_path}")"
  printf '%b\n' "${body}" >"${SOURCE}/${source_path}"
  cp "${SOURCE}/${source_path}" "${APP}/workers/workflows/${app_path}"
}

write_workflow workflows/auto-embedding.yaml 'document: auto-embedding'
write_workflow workflows/auto-embedding-workers.yaml \
  $'workers:\n  - name: "memories-auto-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-embedding.yaml'
write_workflow workflows/auto-image-embedding.yaml 'document: auto-image-embedding'
write_workflow workflows/auto-image-embedding-workers.yaml \
  $'workers:\n  - name: "memories-auto-image-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-image-embedding.yaml\n  - name: memories-auto-image-caption\n    settings:\n      workflow_data:\n        $file: auto-image-caption.yaml'
write_workflow workflows/auto-image-caption.yaml 'document: auto-image-caption'
write_workflow workflows/auto-thread-embedding.yaml 'document: auto-thread-embedding'
write_workflow workflows/auto-thread-embedding-workers.yaml \
  $'workers:\n  - name: "memories-auto-thread-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-thread-embedding.yaml'
write_workflow workflows/thread-reflection/auto-reflection-intent-embedding.yaml \
  'document: auto-reflection-intent-embedding'
write_workflow workflows/thread-reflection/auto-reflection-intent-embedding-workers.yaml \
  $'workers:\n  - name: "memories-auto-reflection-intent-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-reflection-intent-embedding.yaml'
write_workflow workflows/thread-reflection/auto-reflection-summary-embedding.yaml \
  'document: auto-reflection-summary-embedding'
write_workflow workflows/thread-reflection/auto-reflection-summary-embedding-workers.yaml \
  $'workers:\n  - name: "memories-auto-reflection-summary-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-reflection-summary-embedding.yaml'

# The app-only overlays are explicit: these files are not upstream workflows.
cat >"${APP}/src-tauri/src/commands/embedding_workers_yaml.rs" <<'EOF'
const TEMPLATE: &str = include_str!("../../../workers/workflows/auto-embedding-workers.yaml");
EOF
write_rag_overlay() {
  local worker_name=${1:-%{LOOKBACK_MM_EMBEDDING_WORKER}} include_prefix=${2:-yes}
  local overlay="${APP}/workers/workflows/rag/lookback-recall.yaml"
  cat >"${APP}/workers/workflows/rag/lookback-recall.yaml" <<'EOF'
# Comments cannot satisfy the worker or query-prefix contract:
# name: "%{LOOKBACK_MM_EMBEDDING_WORKER}"
# prefix: "%{MEMORY_EMBEDDING_QUERY_PREFIX:-}"
  - embedQuery:
      run:
        worker:
EOF
  printf '          name: "%s"\n' "${worker_name}" >>"${overlay}"
  cat >>"${overlay}" <<'EOF'
          using: embed_text
          arguments:
            text: "${ $workflow.input.query }"
EOF
  if [[ "${include_prefix}" == yes ]]; then
    printf '            prefix: "%%{MEMORY_EMBEDDING_QUERY_PREFIX:-}"\n' >>"${overlay}"
  fi
}

expect_rag_overlay_rejected() {
  local fixture=$1
  case "${fixture}" in
    missing-prefix)
      write_rag_overlay "%{LOOKBACK_MM_EMBEDDING_WORKER}" no
      ;;
    literal-legacy-worker)
      write_rag_overlay "memories-mm-embedding"
      ;;
    old-unresolved-suffix)
      write_rag_overlay 'memories-mm-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}'
      ;;
    empty-placeholder)
      write_rag_overlay '%{LOOKBACK_MM_EMBEDDING_WORKER:-}'
      ;;
    unsafe-default)
      write_rag_overlay '%{LOOKBACK_MM_EMBEDDING_WORKER:-memories-mm-embedding}'
      ;;
    *) echo "unknown RAG overlay fixture: ${fixture}" >&2; exit 1 ;;
  esac

  if node "${VERIFY}" "${SOURCE}" "${APP}" >"${TMP_ROOT}/${fixture}.log" 2>&1; then
    echo "embedding contract accepted invalid RAG overlay fixture: ${fixture}" >&2
    exit 1
  fi
  grep -q 'RAG query embedding overlay' "${TMP_ROOT}/${fixture}.log"
}

write_rag_overlay
printf 'document: lookback-get-context\n' >"${APP}/workers/workflows/rag/lookback-get-context.yaml"
printf 'document: lookback-get-reflections\n' >"${APP}/workers/workflows/rag/lookback-get-reflections.yaml"

cat >"${SOURCE}/grpc-admin/src/bin/memories_db_migrate.rs" <<'EOF'
use embedding_command::EmbeddingCommand;
Command::Embedding { command } => run_embedding(command),
EOF
cat >"${SOURCE}/grpc-admin/src/bin/memories_db_migrate/embedding_command.rs" <<'EOF'
enum EmbeddingCommand { Inspect, Plan, Switch, Abandon, Finalize, Restore }
EOF
printf 'pub async fn run_switch() {}\n' >"${SOURCE}/grpc-admin/src/bin/memories_db_migrate/embedding_migrate.rs"
printf 'pub async fn run_finalize() {}\n' >"${SOURCE}/grpc-admin/src/bin/memories_db_migrate/embedding_finish.rs"

cp "${SOURCE}/protobuf/protobuf/llm_memory/data/embedding_space.proto" \
  "${APP}/proto/llm_memory/data/embedding_space.proto"
cp "${SOURCE}/protobuf/protobuf/llm_memory/service/embedding_space.proto" \
  "${APP}/proto/llm_memory/service/embedding_space.proto"

git -C "${SOURCE}" init -q
git -C "${SOURCE}" config user.name "Contract Test"
git -C "${SOURCE}" config user.email "contract@example.invalid"
git -C "${SOURCE}" add .
git -C "${SOURCE}" commit -qm "embedding contract fixture"
SOURCE_REVISION=$(git -C "${SOURCE}" rev-parse HEAD)
node - "${PROVENANCE}" "${SOURCE_REVISION}" <<'NODE'
const fs = require("node:fs");
fs.writeFileSync(process.argv[2], JSON.stringify({
  format_version: 3,
  source_revision: process.argv[3],
}, null, 2) + "\n");
NODE

node "${VERIFY}" "${SOURCE}" "${APP}" --record-provenance "${PROVENANCE}"
node "${SCRIPT_DIR}/verify-migration-release.mjs" --source "${SOURCE}" \
  --app-assets "${APP}" >/dev/null
for fixture in missing-prefix literal-legacy-worker old-unresolved-suffix empty-placeholder unsafe-default; do
  expect_rag_overlay_rejected "${fixture}"
done
write_rag_overlay '%{LOOKBACK_MM_EMBEDDING_WORKER:-memories-mm-embedding}'
if node "${SCRIPT_DIR}/verify-migration-release.mjs" --source "${SOURCE}" \
  --app-assets "${APP}" >"${TMP_ROOT}/source-unsafe-worker-default.log" 2>&1; then
  echo "source staging accepted an unsafe RAG worker fallback" >&2
  exit 1
fi
grep -q 'RAG query embedding overlay' "${TMP_ROOT}/source-unsafe-worker-default.log"
write_rag_overlay
node - "${PROVENANCE}" "${SOURCE_REVISION}" <<'NODE'
const fs = require("node:fs");
const [file, revision] = process.argv.slice(2);
const manifest = JSON.parse(fs.readFileSync(file, "utf8"));
if (manifest.embedding_contract?.source_revision !== revision) {
  throw new Error("contract was not tied to the Memories source revision");
}
if (manifest.embedding_contract?.mappings?.["workflows/auto-image-caption.yaml"] !==
    "workers/workflows/auto-image-caption.yaml") {
  throw new Error("caption workflow is missing from the source-to-app contract");
}
NODE

mv "${APP}/workers/workflows/auto-image-caption.yaml" "${TMP_ROOT}/caption.yaml"
if node "${VERIFY}" "${SOURCE}" "${APP}" >"${TMP_ROOT}/missing-caption.log" 2>&1; then
  echo "embedding contract accepted an app missing the caption workflow referenced by its worker" >&2
  exit 1
fi
grep -q 'auto-image-caption.yaml' "${TMP_ROOT}/missing-caption.log"
mv "${TMP_ROOT}/caption.yaml" "${APP}/workers/workflows/auto-image-caption.yaml"

printf '\n// drift\n' >>"${APP}/proto/llm_memory/data/embedding_space.proto"
if node "${VERIFY}" "${SOURCE}" "${APP}" >"${TMP_ROOT}/proto-drift.log" 2>&1; then
  echo "embedding contract accepted an app protobuf that differs from its Memories source" >&2
  exit 1
fi
grep -q 'embedding_space.proto' "${TMP_ROOT}/proto-drift.log"
cp "${SOURCE}/protobuf/protobuf/llm_memory/data/embedding_space.proto" \
  "${APP}/proto/llm_memory/data/embedding_space.proto"

mv "${SOURCE}/protobuf/protobuf/llm_memory/service/embedding_space.proto" \
  "${TMP_ROOT}/old-source-embedding-space.proto"
if node "${VERIFY}" "${SOURCE}" "${APP}" >"${TMP_ROOT}/old-source.log" 2>&1; then
  echo "embedding contract accepted a Memories source without the embedding-space service proto" >&2
  exit 1
fi
grep -q 'embedding_space.proto' "${TMP_ROOT}/old-source.log"
mv "${TMP_ROOT}/old-source-embedding-space.proto" \
  "${SOURCE}/protobuf/protobuf/llm_memory/service/embedding_space.proto"

node - "${PROVENANCE}" <<'NODE'
const fs = require("node:fs");
const file = process.argv[2];
const manifest = JSON.parse(fs.readFileSync(file, "utf8"));
manifest.source_revision = "0".repeat(40);
fs.writeFileSync(file, JSON.stringify(manifest, null, 2) + "\n");
NODE
if node "${VERIFY}" "${SOURCE}" "${APP}" --record-provenance "${PROVENANCE}" \
  >"${TMP_ROOT}/revision-mismatch.log" 2>&1; then
  echo "embedding contract recorded assets against a different Memories source revision" >&2
  exit 1
fi
grep -q 'source revision' "${TMP_ROOT}/revision-mismatch.log"

echo "embedding contract tests passed"
