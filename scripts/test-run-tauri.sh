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

make_bin "${TMP_ROOT}/src/all-in-one"
make_bin "${TMP_ROOT}/src/front"
make_bin "${TMP_ROOT}/src/conductor-main"
make_bin "${TMP_ROOT}/src/memories-import"
make_bin "${TMP_ROOT}/src/migrate-memory-kind"
make_bin "${TMP_ROOT}/src/protoc"
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
  LOOKBACK_MIGRATE_MEMORY_KIND_BIN="${TMP_ROOT}/src/migrate-memory-kind" \
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
  LOOKBACK_MIGRATE_MEMORY_KIND_BIN="${TMP_ROOT}/src/migrate-memory-kind" \
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

echo "run-tauri tests passed"
