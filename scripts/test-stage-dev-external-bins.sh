#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)
SCRIPT="${SCRIPT_DIR}/stage-dev-external-bins.sh"

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

assert_file() {
  local path=$1
  [[ -f "${path}" ]] || {
    echo "expected file: ${path}" >&2
    exit 1
  }
}

make_plugin() {
  local path=$1
  mkdir -p "$(dirname "${path}")"
  printf 'plugin\n' >"${path}"
}

test_env_overrides_stage_target_triple_bins() {
  local src="${TMP_ROOT}/src"
  make_bin "${src}/all-in-one"
  make_bin "${src}/front"
  make_bin "${src}/conductor-main"
  make_bin "${src}/memories-import"
  make_bin "${src}/migrate-memory-kind"
  make_bin "${src}/protoc"
  make_plugin "${TMP_ROOT}/plugins/libexisting.so"

  local app="${TMP_ROOT}/app"
  LOOKBACK_JOBWORKERP_BIN="${src}/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${src}/front" \
    LOOKBACK_CONDUCTOR_BIN="${src}/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${src}/memories-import" \
    LOOKBACK_MIGRATE_MEMORY_KIND_BIN="${src}/migrate-memory-kind" \
    LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/plugins" \
    PROTOC="${src}/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple >/tmp/stage-dev-test.out

  assert_file "${app}/src-tauri/bin/all-in-one-test-triple"
  assert_file "${app}/src-tauri/bin/front-test-triple"
  assert_file "${app}/src-tauri/bin/conductor-main-test-triple"
  assert_file "${app}/src-tauri/bin/memories-import-test-triple"
  assert_file "${app}/src-tauri/bin/migrate-memory-kind-test-triple"
  assert_file "${app}/src-tauri/bin/protoc-test-triple"
}

test_stages_cuda_runner_plugins_from_workspace_plugins_cuda_runner() {
  local workspace="${TMP_ROOT}/workspace"
  local src="${workspace}/github/agent-app"
  make_bin "${TMP_ROOT}/src/all-in-one"
  make_bin "${TMP_ROOT}/src/front"
  make_bin "${TMP_ROOT}/src/conductor-main"
  make_bin "${TMP_ROOT}/src/memories-import"
  make_bin "${TMP_ROOT}/src/migrate-memory-kind"
  make_bin "${TMP_ROOT}/src/protoc"
  make_plugin "${workspace}/plugins/cuda_runner/libcuda_runner.so"
  mkdir -p "$(dirname "${src}")"

  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
    LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
    LOOKBACK_MIGRATE_MEMORY_KIND_BIN="${TMP_ROOT}/src/migrate-memory-kind" \
    PROTOC="${TMP_ROOT}/src/protoc" \
    bash "${SCRIPT}" --agent-app "${src}" --triple test-triple >/tmp/stage-dev-plugin.out

  assert_file "${src}/src-tauri/plugins/libcuda_runner.so"
  [[ ! -e "${workspace}/github/plugins/libcuda_runner.so" ]] || {
    echo "staging wrote outside agent-app" >&2
    exit 1
  }
}

test_stages_nested_plugins_from_env_override() {
  local app="${TMP_ROOT}/env-plugin-app"
  local plugin_src="${TMP_ROOT}/custom-plugins/nested"
  make_bin "${TMP_ROOT}/src/all-in-one"
  make_bin "${TMP_ROOT}/src/front"
  make_bin "${TMP_ROOT}/src/conductor-main"
  make_bin "${TMP_ROOT}/src/memories-import"
  make_bin "${TMP_ROOT}/src/migrate-memory-kind"
  make_bin "${TMP_ROOT}/src/protoc"
  make_plugin "${plugin_src}/libcustom.so"

  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
    LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
    LOOKBACK_MIGRATE_MEMORY_KIND_BIN="${TMP_ROOT}/src/migrate-memory-kind" \
    LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/custom-plugins" \
    PROTOC="${TMP_ROOT}/src/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple >/tmp/stage-dev-plugin-env.out

  assert_file "${app}/src-tauri/plugins/libcustom.so"
}

test_missing_required_binary_fails() {
  local app="${TMP_ROOT}/missing-app"
  make_plugin "${TMP_ROOT}/plugins/libexisting.so"
  mkdir -p "${TMP_ROOT}/empty-path"
  if PATH="${TMP_ROOT}/empty-path:/usr/bin:/bin" bash "${SCRIPT}" --agent-app "${app}" --triple test-triple >/tmp/stage-dev-missing.out 2>/tmp/stage-dev-missing.err; then
    echo "expected missing binary failure" >&2
    exit 1
  fi

  grep -q "all-in-one" /tmp/stage-dev-missing.err
}

test_dry_run_does_not_write_files() {
  local src="${TMP_ROOT}/dry-src"
  make_bin "${src}/all-in-one"
  make_bin "${src}/front"
  make_bin "${src}/conductor-main"
  make_bin "${src}/memories-import"
  make_bin "${src}/migrate-memory-kind"
  make_bin "${src}/protoc"
  local app="${TMP_ROOT}/dry-app"
  local dry_parent
  dry_parent=$(dirname "${app}")
  make_plugin "${TMP_ROOT}/plugins/cuda_runner/libdry.so"

  LOOKBACK_JOBWORKERP_BIN="${src}/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${src}/front" \
    LOOKBACK_CONDUCTOR_BIN="${src}/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${src}/memories-import" \
    LOOKBACK_MIGRATE_MEMORY_KIND_BIN="${src}/migrate-memory-kind" \
    LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/plugins" \
    PROTOC="${src}/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple --dry-run >/tmp/stage-dev-dry.out

  [[ ! -e "${app}/src-tauri/bin/all-in-one-test-triple" ]] || {
    echo "dry-run wrote staged binary" >&2
    exit 1
  }
  [[ ! -e "${app}/src-tauri/plugins/libdry.so" ]] || {
    echo "dry-run wrote staged plugin" >&2
    exit 1
  }
}

test_env_overrides_stage_target_triple_bins
test_stages_cuda_runner_plugins_from_workspace_plugins_cuda_runner
test_stages_nested_plugins_from_env_override
test_missing_required_binary_fails
test_dry_run_does_not_write_files

echo "stage-dev-external-bins tests passed"
