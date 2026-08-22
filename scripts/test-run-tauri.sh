#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WRAPPER="${SCRIPT_DIR}/run-tauri.sh"

case "$(uname -s)" in
  Darwin) PLUGIN_EXT=dylib ;;
  Linux) PLUGIN_EXT=so ;;
  *)
    echo "unsupported host OS: $(uname -s)" >&2
    exit 1
    ;;
esac

TMP_ROOT=$(mktemp -d)
cleanup() {
  rm -rf "${TMP_ROOT}"
}
trap cleanup EXIT

make_bin() {
  local path=$1
  mkdir -p "$(dirname "${path}")"
  printf '#!/usr/bin/env bash\nexit 0\n' >"${path}"
  chmod +x "${path}"
}

make_migration_bundle() {
  local root=$1
  make_bin "${root}/memories-db-migrate"
  make_bin "${root}/atlas/bin/atlas"
  mkdir -p "${root}/atlas/sqlite/migrations"
  printf "INSERT INTO memories_schema_contract (contract_key, version) VALUES ('rdb_schema', '20260803000003');\n" >"${root}/atlas/sqlite/migrations/20260803000003_schema_contract.sql"
  printf 'catalog\n' >"${root}/atlas/sqlite/migrations/atlas.sum"
  cat >"${root}/atlas/post-migration-tasks.json" <<'EOF'
{"tasks":[{"id":"thread-message-times-v1","generation":1,"lifecycle":"active","completion_required_by_schema_version":"20260803000003"}]}
EOF
}

make_bin "${TMP_ROOT}/src/all-in-one"
make_bin "${TMP_ROOT}/src/front"
make_bin "${TMP_ROOT}/src/conductor-main"
make_bin "${TMP_ROOT}/src/memories-import"
make_bin "${TMP_ROOT}/src/protoc"
make_migration_bundle "${TMP_ROOT}/migration-bundle"
mkdir -p "${TMP_ROOT}/plugins/cuda_runner" "${TMP_ROOT}/github"
printf 'plugin\n' >"${TMP_ROOT}/plugins/cuda_runner/libcuda_runner.${PLUGIN_EXT}"

mkdir -p "${TMP_ROOT}/toolbin"
cat >"${TMP_ROOT}/toolbin/tauri" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${TAURI_ARG_LOG}"
printf '%s\n' "${GDK_BACKEND:-}" >"${TAURI_GDK_LOG}"
printf '%s\n' "${WEBKIT_DISABLE_DMABUF_RENDERER:-}" >"${TAURI_WEBKIT_LOG}"
EOF
chmod +x "${TMP_ROOT}/toolbin/tauri"

LOOKBACK_AGENT_APP="${TMP_ROOT}/github/agent-app" \
  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
  LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
  LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
  LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
  LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
  LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/plugins" \
  PROTOC="${TMP_ROOT}/src/protoc" \
  TAURI_ARG_LOG="${TMP_ROOT}/args.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" dev --no-watch

grep -q '^dev --no-watch$' "${TMP_ROOT}/args.log"
if [[ "$(uname -s)" == "Linux" ]]; then
  grep -q '^x11$' "${TMP_ROOT}/gdk.log"
  grep -q '^1$' "${TMP_ROOT}/webkit.log"
else
  grep -q '^$' "${TMP_ROOT}/gdk.log"
  grep -q '^$' "${TMP_ROOT}/webkit.log"
fi
find "${TMP_ROOT}/github/agent-app/src-tauri/bin" -maxdepth 1 -type f -name 'all-in-one-*' | grep -q . || {
  echo "expected dev wrapper to stage external bins" >&2
  exit 1
}
[[ -f "${TMP_ROOT}/github/agent-app/src-tauri/plugins/libcuda_runner.${PLUGIN_EXT}" ]] || {
  echo "expected dev wrapper to stage cuda_runner plugins" >&2
  exit 1
}
[[ ! -e "${TMP_ROOT}/github/plugins/libcuda_runner.${PLUGIN_EXT}" ]] || {
  echo "dev wrapper wrote plugins outside agent-app" >&2
  exit 1
}

LOOKBACK_AGENT_APP="${TMP_ROOT}/github/agent-app-explicit" \
  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
  LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
  LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
  LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
  LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
  LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/plugins" \
  PROTOC="${TMP_ROOT}/src/protoc" \
  GDK_BACKEND=wayland \
  WEBKIT_DISABLE_DMABUF_RENDERER=0 \
  TAURI_ARG_LOG="${TMP_ROOT}/args-explicit.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk-explicit.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit-explicit.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" dev --no-watch

grep -q '^wayland$' "${TMP_ROOT}/gdk-explicit.log"
grep -q '^0$' "${TMP_ROOT}/webkit-explicit.log"

LOOKBACK_AGENT_APP="${TMP_ROOT}/github/agent-app-build" \
  TAURI_ARG_LOG="${TMP_ROOT}/args-build.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk-build.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit-build.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" build

grep -q '^build$' "${TMP_ROOT}/args-build.log"
[[ ! -e "${TMP_ROOT}/github/agent-app-build/src-tauri" ]] || {
  echo "build wrapper unexpectedly ran dev external binary staging" >&2
  exit 1
}

echo "run-tauri tests passed"
