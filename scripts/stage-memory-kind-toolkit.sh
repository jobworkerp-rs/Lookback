#!/usr/bin/env bash
# Stage the migration material from one memories checkout for a manual bundle.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)
MEMORIES_DIR=${1:?"usage: scripts/stage-memory-kind-toolkit.sh <memories-checkout>"}
TOOLKIT_DIR="${AGENT_APP}/src-tauri/migration-toolkit"

for source in \
  "${MEMORIES_DIR}/infra/sql/sqlite/manual/011_add_memory_kind.sql" \
  "${MEMORIES_DIR}/infra/sql/sqlite/manual/012_contract_memory_kind.sql" \
  "${MEMORIES_DIR}/infra/sql/postgres/manual/010_add_memory_kind.sql" \
  "${MEMORIES_DIR}/infra/sql/postgres/manual/011_contract_memory_kind.sql"; do
  [[ -f "${source}" ]] || { echo "missing migration toolkit source: ${source}" >&2; exit 1; }
done

# This document belongs to Lookback: it describes its UI, sidecar lifecycle,
# and local data root. Do not source it from memories, whose responsibility is
# the CLI and SQL contract only.
[[ -f "${TOOLKIT_DIR}/memory-kind-client-migration_ja.md" ]] || {
  echo "missing Lookback migration runbook: ${TOOLKIT_DIR}/memory-kind-client-migration_ja.md" >&2
  exit 1
}
[[ -f "${TOOLKIT_DIR}/vectordb-rebuild-runbook_ja.md" ]] || {
  echo "missing Lookback vector rebuild runbook: ${TOOLKIT_DIR}/vectordb-rebuild-runbook_ja.md" >&2
  exit 1
}

mkdir -p "${TOOLKIT_DIR}/sqlite" "${TOOLKIT_DIR}/postgres"
install -m644 "${MEMORIES_DIR}/infra/sql/sqlite/manual/011_add_memory_kind.sql" "${TOOLKIT_DIR}/sqlite/011_add_memory_kind.sql"
install -m644 "${MEMORIES_DIR}/infra/sql/sqlite/manual/012_contract_memory_kind.sql" "${TOOLKIT_DIR}/sqlite/012_contract_memory_kind.sql"
install -m644 "${MEMORIES_DIR}/infra/sql/postgres/manual/010_add_memory_kind.sql" "${TOOLKIT_DIR}/postgres/010_add_memory_kind.sql"
install -m644 "${MEMORIES_DIR}/infra/sql/postgres/manual/011_contract_memory_kind.sql" "${TOOLKIT_DIR}/postgres/011_contract_memory_kind.sql"

echo "staged memory-kind migration toolkit" >&2
