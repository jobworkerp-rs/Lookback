#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WRAPPER="${SCRIPT_DIR}/run-tauri.sh"
MIGRATION_TEST_FIXTURE="${SCRIPT_DIR}/lib/migration-test-fixture.mjs"

case "$(uname -s)" in
  Darwin) PLUGIN_EXT=dylib ;;
  Linux) PLUGIN_EXT=so ;;
  *)
    echo "unsupported host OS: $(uname -s)" >&2
    exit 1
    ;;
esac

case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64) HOST_TRIPLE=aarch64-apple-darwin ;;
  Darwin:x86_64) HOST_TRIPLE=x86_64-apple-darwin ;;
  Linux:x86_64|Linux:amd64) HOST_TRIPLE=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) HOST_TRIPLE=aarch64-unknown-linux-gnu ;;
  *) echo "unsupported host target: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

# Pick a target that cannot be the current host so this assertion remains
# meaningful on both supported desktop platforms.
case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64) CROSS_TARGET=x86_64-apple-darwin ;;
  *) CROSS_TARGET=aarch64-apple-darwin ;;
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

make_migration_bundle() { node "${MIGRATION_TEST_FIXTURE}" "$1" "${2:-valid}"; }

prepare_pre_staged_package() {
  local app=$1 triple=$2
  local tauri_root="${app}/src-tauri"
  mkdir -p "${tauri_root}/bin" "${tauri_root}/plugins"
  cp -R "${TMP_ROOT}/migration-bundle" "${tauri_root}/migration-bundle"
  cp -R "${TMP_ROOT}/migration-bundle.app-assets/." "${app}/"
  cp "${TMP_ROOT}/migration-bundle/test-artifacts/front" \
    "${tauri_root}/bin/front-${triple}"
  cp "${TMP_ROOT}/migration-bundle/test-artifacts/memories-import" \
    "${tauri_root}/bin/memories-import-${triple}"
  for name in all-in-one conductor-main issue-tracker-server protoc; do
    make_bin "${tauri_root}/bin/${name}-${triple}"
  done
  printf 'fixture plugin\n' >"${tauri_root}/plugins/libcuda_runner.${PLUGIN_EXT}"
}

make_bin "${TMP_ROOT}/src/all-in-one"
make_bin "${TMP_ROOT}/src/front"
make_bin "${TMP_ROOT}/src/conductor-main"
make_bin "${TMP_ROOT}/src/memories-import"
make_bin "${TMP_ROOT}/src/protoc"
make_migration_bundle "${TMP_ROOT}/migration-bundle"
node - "${TMP_ROOT}/migration-bundle/atlas/atlas-tool.lock.json" <<'NODE'
const fs = require("node:fs");
const lock = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (!lock.platforms?.["darwin-arm64"] || !lock.platforms?.["linux-amd64"]) {
  throw new Error("shared migration fixture must cover macOS ARM and Linux Atlas targets");
}
NODE
mkdir -p "${TMP_ROOT}/plugins/cuda_runner" "${TMP_ROOT}/github"
printf 'plugin\n' >"${TMP_ROOT}/plugins/cuda_runner/libcuda_runner.${PLUGIN_EXT}"
make_bin "${TMP_ROOT}/src/issue-tracker-server"

mkdir -p "${TMP_ROOT}/toolbin"
cat >"${TMP_ROOT}/toolbin/tauri" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${TAURI_ARG_LOG}"
printf '%s\n' "${GDK_BACKEND:-}" >"${TAURI_GDK_LOG}"
printf '%s\n' "${WEBKIT_DISABLE_DMABUF_RENDERER:-}" >"${TAURI_WEBKIT_LOG}"
printf '%s\n' "${NO_STRIP:-}" >"${TAURI_NO_STRIP_LOG}"
EOF
chmod +x "${TMP_ROOT}/toolbin/tauri"

LOOKBACK_AGENT_APP="${TMP_ROOT}/github/agent-app" \
  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
  LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
  LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
  LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
  LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
  LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/plugins" \
  LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${TMP_ROOT}/src/issue-tracker-server" \
  PROTOC="${TMP_ROOT}/src/protoc" \
  TAURI_ARG_LOG="${TMP_ROOT}/args.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit.log" \
  TAURI_NO_STRIP_LOG="${TMP_ROOT}/no-strip.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" dev --no-watch

grep -q '^dev --no-watch$' "${TMP_ROOT}/args.log"
if [[ "$(uname -s)" == "Linux" ]]; then
  grep -q '^wayland,x11$' "${TMP_ROOT}/gdk.log"
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
find "${TMP_ROOT}/github/agent-app/src-tauri/bin" -maxdepth 1 -type f -name 'issue-tracker-server-*' | grep -q . || {
  echo "expected dev wrapper to stage issue-tracker-server" >&2
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
  LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${TMP_ROOT}/src/issue-tracker-server" \
  PROTOC="${TMP_ROOT}/src/protoc" \
  GDK_BACKEND=wayland \
  WEBKIT_DISABLE_DMABUF_RENDERER=0 \
  TAURI_ARG_LOG="${TMP_ROOT}/args-explicit.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk-explicit.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit-explicit.log" \
  TAURI_NO_STRIP_LOG="${TMP_ROOT}/no-strip-explicit.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" dev --no-watch

grep -q '^wayland$' "${TMP_ROOT}/gdk-explicit.log"
grep -q '^0$' "${TMP_ROOT}/webkit-explicit.log"

BUILD_APP="${TMP_ROOT}/github/agent-app-build"
prepare_pre_staged_package "${BUILD_APP}" "${HOST_TRIPLE}"
LOOKBACK_AGENT_APP="${BUILD_APP}" \
  LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1 \
  TAURI_ARG_LOG="${TMP_ROOT}/args-build.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk-build.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit-build.log" \
  TAURI_NO_STRIP_LOG="${TMP_ROOT}/no-strip-build.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" build

grep -q '^build$' "${TMP_ROOT}/args-build.log"
if [[ "$(uname -s)" == "Linux" ]]; then
  grep -q '^true$' "${TMP_ROOT}/no-strip-build.log"
  staged_gtk_plugin="${TMP_ROOT}/github/agent-app-build/target/.tauri/linuxdeploy-plugin-gtk.sh"
  [[ -x "${staged_gtk_plugin}" ]] || {
    echo "expected build wrapper to stage the Linux GTK bundler plugin" >&2
    exit 1
  }
  grep -Fq "export GDK_BACKEND=\"\${GDK_BACKEND:-wayland,x11}\"" "${staged_gtk_plugin}" || {
    echo "expected staged GTK bundler plugin to prefer Wayland with X11 fallback" >&2
    exit 1
  }
else
  grep -q '^$' "${TMP_ROOT}/no-strip-build.log"
fi
[[ -f "${BUILD_APP}/src-tauri/bin/all-in-one-${HOST_TRIPLE}" ]] || {
  echo "expected build fixture to include its pre-staged external bins" >&2
  exit 1
}
[[ -f "${TMP_ROOT}/github/agent-app-build/src-tauri/plugins/libcuda_runner.${PLUGIN_EXT}" ]] || {
  echo "expected build wrapper to stage cuda_runner plugins" >&2
  exit 1
}

if LOOKBACK_AGENT_APP="${TMP_ROOT}/github/agent-app-cross" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" build --target "${CROSS_TARGET}" >"${TMP_ROOT}/cross-target.log" 2>&1; then
  echo "cross-target build unexpectedly staged host artifacts" >&2
  exit 1
fi
grep -q 'target-specific pre-staged sidecars and plugins' "${TMP_ROOT}/cross-target.log"
[[ ! -e "${TMP_ROOT}/github/agent-app-cross/src-tauri" ]] || {
  echo "cross-target build wrote host artifacts into the target bundle" >&2
  exit 1
}

RELEASE_APP="${TMP_ROOT}/github/agent-app-release"
prepare_pre_staged_package "${RELEASE_APP}" "${HOST_TRIPLE}"
printf 'do not replace pre-staged release inputs\n' >"${RELEASE_APP}/src-tauri/release-fixture-sentinel"
LOOKBACK_AGENT_APP="${RELEASE_APP}" \
  LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1 \
  TAURI_ARG_LOG="${TMP_ROOT}/args-release.log" \
  TAURI_GDK_LOG="${TMP_ROOT}/gdk-release.log" \
  TAURI_WEBKIT_LOG="${TMP_ROOT}/webkit-release.log" \
  TAURI_NO_STRIP_LOG="${TMP_ROOT}/no-strip-release.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${WRAPPER}" build

grep -q '^build$' "${TMP_ROOT}/args-release.log"
[[ -f "${RELEASE_APP}/src-tauri/release-fixture-sentinel" ]] || {
  echo "release build wrapper unexpectedly replaced pre-staged artifacts" >&2
  exit 1
}

echo "run-tauri tests passed"
