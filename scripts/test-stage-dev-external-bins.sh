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

make_stage_inputs() {
  local root=$1
  make_bin "${root}/bin/all-in-one"
  make_bin "${root}/bin/front"
  make_bin "${root}/bin/conductor-main"
  make_bin "${root}/bin/memories-import"
  make_bin "${root}/bin/protoc"
  make_plugin "${root}/plugins/libtest.so"
}

make_fake_memories_bundle_builder() {
  local memories=$1 fixture=$2 marker=$3 outcome=${4:-success}
  local script="${memories}/scripts/build-memories-db-migrate-sqlite.sh"
  mkdir -p "$(dirname "${script}")"
  if [[ "${outcome}" == "failure" ]]; then
    printf '#!/usr/bin/env bash\nprintf x >>%q\nexit 42\n' "${marker}" >"${script}"
  else
    printf '#!/usr/bin/env bash\nset -euo pipefail\nprintf x >>%q\nsleep "${LOOKBACK_TEST_MIGRATION_BUILD_DELAY:-0}"\ncp -R %q "$1"\n' \
      "${marker}" "${fixture}" >"${script}"
  fi
  chmod +x "${script}"
}

run_fixture_stage() {
  local app=$1 inputs=$2
  shift 2
  env \
    LOOKBACK_JOBWORKERP_BIN="${inputs}/bin/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${inputs}/bin/front" \
    LOOKBACK_CONDUCTOR_BIN="${inputs}/bin/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${inputs}/bin/memories-import" \
    LOOKBACK_PLUGINS_SRC="${inputs}/plugins" \
    PROTOC="${inputs}/bin/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple "$@"
}

test_missing_default_bundle_is_built_from_sibling_memories() {
  local root="${TMP_ROOT}/auto-build" app="${TMP_ROOT}/auto-build/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-auto-build.out

  assert_file "${marker}"
  assert_file "${app}/src-tauri/migration-bundle/memories-db-migrate"
  node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${AGENT_APP}/src-tauri/src/sidecar/migration_gate.rs" \
    "${app}/src-tauri/migration-bundle" >/dev/null
}

test_default_bundle_build_failure_preserves_existing_destination() {
  local root="${TMP_ROOT}/build-failure" app="${TMP_ROOT}/build-failure/agent-app"
  local inputs="${root}/inputs" fixture="${root}/old" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  mkdir -p "${app}/src-tauri"
  cp -R "${fixture}" "${app}/src-tauri/migration-bundle"
  printf '{}\n' >"${app}/src-tauri/migration-bundle/atlas/post-migration-tasks.json"
  printf 'keep\n' >"${app}/src-tauri/migration-bundle/sentinel"
  mkdir -p "${root}/memories/target/memories-db-migrate-sqlite"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}" failure

  if run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-build-failure.out 2>/tmp/stage-dev-build-failure.err; then
    echo "expected migration bundle build failure" >&2
    exit 1
  fi
  assert_file "${marker}"
  assert_file "${app}/src-tauri/migration-bundle/sentinel"
}

test_invalid_explicit_bundle_does_not_fall_back_to_builder() {
  local root="${TMP_ROOT}/invalid-explicit" app="${TMP_ROOT}/invalid-explicit/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  local invalid="${root}/invalid"
  make_migration_bundle "${invalid}"
  printf '{}\n' >"${invalid}/atlas/post-migration-tasks.json"

  if LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${invalid}" \
    run_fixture_stage "${app}" "${inputs}" \
    >/tmp/stage-dev-invalid-explicit.out 2>/tmp/stage-dev-invalid-explicit.err; then
    echo "expected invalid explicit migration bundle failure" >&2
    exit 1
  fi
  [[ ! -e "${marker}" ]] || {
    echo "explicit override failure unexpectedly invoked the sibling builder" >&2
    exit 1
  }
  [[ ! -e "${app}/src-tauri/migration-bundle" ]] || {
    echo "invalid explicit override staged a destination" >&2
    exit 1
  }
}

test_missing_default_dry_run_only_prints_build_plan() {
  local root="${TMP_ROOT}/dry-auto" app="${TMP_ROOT}/dry-auto/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"

  run_fixture_stage "${app}" "${inputs}" --dry-run >/tmp/stage-dev-dry-auto.out

  grep -q "would build migration bundle" /tmp/stage-dev-dry-auto.out
  [[ ! -e "${marker}" && ! -e "${app}" ]] || {
    echo "dry-run built or staged migration artifacts" >&2
    exit 1
  }
}

test_invalid_default_bundle_is_rebuilt() {
  local root="${TMP_ROOT}/invalid-default" app="${TMP_ROOT}/invalid-default/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  make_migration_bundle "${root}/memories/target/memories-db-migrate-sqlite"
  printf '{}\n' >"${root}/memories/target/memories-db-migrate-sqlite/atlas/post-migration-tasks.json"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-invalid-default.out

  assert_file "${marker}"
  node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${AGENT_APP}/src-tauri/src/sidecar/migration_gate.rs" \
    "${app}/src-tauri/migration-bundle" >/dev/null
}

test_invalid_default_reuses_valid_staged_bundle() {
  local root="${TMP_ROOT}/invalid-default-cache" app="${TMP_ROOT}/invalid-default-cache/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  make_migration_bundle "${root}/memories/target/memories-db-migrate-sqlite"
  printf '{}\n' >"${root}/memories/target/memories-db-migrate-sqlite/atlas/post-migration-tasks.json"
  mkdir -p "${app}/src-tauri"
  cp -R "${fixture}" "${app}/src-tauri/migration-bundle"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-invalid-default-cache.out 2>&1

  [[ ! -e "${marker}" ]] || {
    echo "valid staged migration bundle was rebuilt unnecessarily" >&2
    exit 1
  }
  grep -q "reuse verified staged migration bundle" /tmp/stage-dev-invalid-default-cache.out
}

test_concurrent_default_builds_are_serialized() {
  local root="${TMP_ROOT}/concurrent" app="${TMP_ROOT}/concurrent/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  local first_pid second_pid
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"

  LOOKBACK_TEST_MIGRATION_BUILD_DELAY=1 \
    run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-concurrent-first.out 2>&1 &
  first_pid=$!
  LOOKBACK_TEST_MIGRATION_BUILD_DELAY=1 \
    run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-concurrent-second.out 2>&1 &
  second_pid=$!
  wait "${first_pid}"
  wait "${second_pid}"

  [[ "$(wc -c <"${marker}" | tr -d ' ')" == "1" ]] || {
    echo "concurrent staging invoked the migration builder more than once" >&2
    exit 1
  }
  node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${AGENT_APP}/src-tauri/src/sidecar/migration_gate.rs" \
    "${app}/src-tauri/migration-bundle" >/dev/null
  [[ ! -e "${app}/src-tauri/migration-bundle.lock" ]] || {
    echo "concurrent staging left the migration lock behind" >&2
    exit 1
  }
}

test_ownerless_stale_lock_is_recovered() {
  local root="${TMP_ROOT}/stale-lock" app="${TMP_ROOT}/stale-lock/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  mkdir -p "${app}/src-tauri/migration-bundle.lock"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-stale-lock.out

  assert_file "${marker}"
  assert_file "${app}/src-tauri/migration-bundle/memories-db-migrate"
  [[ ! -e "${app}/src-tauri/migration-bundle.lock" ]] || {
    echo "stale migration lock was not removed" >&2
    exit 1
  }
}

test_env_overrides_stage_target_triple_bins() {
  local src="${TMP_ROOT}/src"
  make_bin "${src}/all-in-one"
  make_bin "${src}/front"
  make_bin "${src}/conductor-main"
  make_bin "${src}/memories-import"
  make_bin "${src}/protoc"
  make_plugin "${TMP_ROOT}/plugins/libexisting.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  local app="${TMP_ROOT}/app"
  LOOKBACK_JOBWORKERP_BIN="${src}/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${src}/front" \
    LOOKBACK_CONDUCTOR_BIN="${src}/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${src}/memories-import" \
    LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
    LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/plugins" \
    PROTOC="${src}/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple >/tmp/stage-dev-test.out

  assert_file "${app}/src-tauri/bin/all-in-one-test-triple"
  assert_file "${app}/src-tauri/bin/front-test-triple"
  assert_file "${app}/src-tauri/bin/conductor-main-test-triple"
  assert_file "${app}/src-tauri/bin/memories-import-test-triple"
  assert_file "${app}/src-tauri/bin/protoc-test-triple"
  assert_file "${app}/src-tauri/migration-bundle/memories-db-migrate"
  assert_file "${app}/src-tauri/migration-bundle/atlas/sqlite/migrations/atlas.sum"
}

test_stages_cuda_runner_plugins_from_workspace_plugins_cuda_runner() {
  local workspace="${TMP_ROOT}/workspace"
  local src="${workspace}/github/agent-app"
  make_bin "${TMP_ROOT}/src/all-in-one"
  make_bin "${TMP_ROOT}/src/front"
  make_bin "${TMP_ROOT}/src/conductor-main"
  make_bin "${TMP_ROOT}/src/memories-import"
  make_bin "${TMP_ROOT}/src/protoc"
  make_plugin "${workspace}/plugins/cuda_runner/libcuda_runner.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"
  mkdir -p "$(dirname "${src}")"

  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
    LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
    LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
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
  make_bin "${TMP_ROOT}/src/protoc"
  make_plugin "${plugin_src}/libcustom.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
    LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
    LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
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
  make_bin "${src}/protoc"
  local app="${TMP_ROOT}/dry-app"
  local dry_parent
  dry_parent=$(dirname "${app}")
  make_plugin "${TMP_ROOT}/plugins/cuda_runner/libdry.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  LOOKBACK_JOBWORKERP_BIN="${src}/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${src}/front" \
    LOOKBACK_CONDUCTOR_BIN="${src}/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${src}/memories-import" \
    LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
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

test_release_contract_mismatch_is_rejected() {
  local bundle="${TMP_ROOT}/bad-migration-bundle"
  make_migration_bundle "${bundle}"
  sed -i.bak 's/20260803000003/20260803000004/g' "${bundle}/atlas/post-migration-tasks.json"
  if node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${AGENT_APP}/src-tauri/src/sidecar/migration_gate.rs" "${bundle}" \
    >/tmp/stage-dev-bad-migration.out 2>/tmp/stage-dev-bad-migration.err; then
    echo "expected migration release contract mismatch failure" >&2
    exit 1
  fi
  grep -q "lacks required thread-message-times-v1@1" /tmp/stage-dev-bad-migration.err
}

test_env_overrides_stage_target_triple_bins
test_stages_cuda_runner_plugins_from_workspace_plugins_cuda_runner
test_stages_nested_plugins_from_env_override
test_missing_required_binary_fails
test_dry_run_does_not_write_files
test_release_contract_mismatch_is_rejected
test_missing_default_bundle_is_built_from_sibling_memories
test_default_bundle_build_failure_preserves_existing_destination
test_invalid_explicit_bundle_does_not_fall_back_to_builder
test_missing_default_dry_run_only_prints_build_plan
test_invalid_default_bundle_is_rebuilt
test_invalid_default_reuses_valid_staged_bundle
test_concurrent_default_builds_are_serialized
test_ownerless_stale_lock_is_recovered

echo "stage-dev-external-bins tests passed"
