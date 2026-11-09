#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)
SCRIPT="${SCRIPT_DIR}/stage-dev-external-bins.sh"
MIGRATION_TEST_FIXTURE="${SCRIPT_DIR}/lib/migration-test-fixture.mjs"

TMP_ROOT=$(mktemp -d)
CONCURRENT_TEST_CONTROL=""
CONCURRENT_STAGE_PIDS=()
cleanup() {
  if [[ -n "${CONCURRENT_TEST_CONTROL}" ]]; then
    : >"${CONCURRENT_TEST_CONTROL}/release-install" 2>/dev/null || true
    local pid
    for pid in "${CONCURRENT_STAGE_PIDS[@]}"; do
      wait "${pid}" 2>/dev/null || true
    done
  fi
  [[ "${PRESERVE_TMP_ROOT:-0}" != "1" ]] || {
    echo "preserved stage-dev test workspace: ${TMP_ROOT}" >&2
    return
  }
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
  local bundle=$1
  node "${MIGRATION_TEST_FIXTURE}" "${bundle}" "${2:-valid}" "${3:-fixture}"
  node - "${bundle}" <<'NODE'
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const root = process.argv[2];
const cliPath = path.join(root, "memories-db-migrate");
const cli = fs.readFileSync(cliPath, "utf8").replace(
  "#!/usr/bin/env node\n",
  `#!/usr/bin/env node
if (process.argv[2] === "embedding") {
  const command = process.argv[3];
  const commands = "inspect plan switch abandon finalize restore";
  if (command === "--help") console.log("Usage: memories-db-migrate embedding <COMMAND>\\n" + commands);
  else if (process.argv[4] === "--help") console.log("Usage: memories-db-migrate embedding " + command);
  else process.exit(2);
  process.exit(0);
}
`,
);
fs.writeFileSync(cliPath, cli);
fs.chmodSync(cliPath, 0o755);
const provenancePath = path.join(root, "memories-source-provenance.json");
if (fs.existsSync(provenancePath)) {
  const digest = crypto.createHash("sha256").update(fs.readFileSync(cliPath)).digest("hex");
  const provenance = JSON.parse(fs.readFileSync(provenancePath, "utf8"));
  provenance.source_artifact_sha256.memories_db_migrate = digest;
  provenance.distributed_artifact_sha256.memories_db_migrate = digest;
  fs.writeFileSync(provenancePath, JSON.stringify(provenance, null, 2) + "\n");
}
NODE
  if [[ -f "${bundle}/memories-source-provenance.json" ]]; then
    ensure_test_contract_source
    node - "${bundle}/memories-source-provenance.json" \
      "$(git -C "${TEST_CONTRACT_SOURCE}" rev-parse HEAD)" <<'NODE'
const fs = require("node:fs");
const [file, revision] = process.argv.slice(2);
const provenance = JSON.parse(fs.readFileSync(file, "utf8"));
provenance.source_revision = revision;
fs.writeFileSync(file, JSON.stringify(provenance, null, 2) + "\n");
NODE
    node "${SCRIPT_DIR}/verify-embedding-contract.mjs" \
      "${TEST_CONTRACT_SOURCE}" "${TEST_CONTRACT_APP}" \
      --record-provenance "${bundle}/memories-source-provenance.json" >/dev/null
  fi
}

TEST_CONTRACT_SOURCE="${TMP_ROOT}/shared-contract/memories"
TEST_CONTRACT_APP="${TMP_ROOT}/shared-contract/agent-app"
ensure_test_contract_source() {
  [[ -d "${TEST_CONTRACT_SOURCE}/.git" ]] && return 0
  make_fake_embedding_contract_assets "${TEST_CONTRACT_SOURCE}" "${TEST_CONTRACT_APP}"
  git -C "${TEST_CONTRACT_SOURCE}" init -q
  git -C "${TEST_CONTRACT_SOURCE}" config user.name "Stage contract test"
  git -C "${TEST_CONTRACT_SOURCE}" config user.email "stage-contract@example.invalid"
  git -C "${TEST_CONTRACT_SOURCE}" add .
  git -C "${TEST_CONTRACT_SOURCE}" commit -qm "embedding contract fixture"
}

migration_bundle_digest() {
  node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).content_digest)' \
    "$1/bundle-manifest.json"
}

make_stage_inputs() {
  local root=$1
  make_bin "${root}/bin/all-in-one"
  make_bin "${root}/bin/front"
  make_bin "${root}/bin/conductor-main"
  make_bin "${root}/bin/memories-import"
  make_bin "${root}/bin/protoc"
  make_plugin "${root}/plugins/libtest.so"
  make_issue_tracker_server "${root}/issue-tracker/issue-tracker-server"
}

make_issue_tracker_server() {
  make_bin "$1"
}

make_fake_memories_bundle_builder() {
  local memories=$1 fixture=$2 marker=$3 outcome=${4:-success}
  local script="${memories}/scripts/build-memories-db-migrate-sqlite.sh"
  local app
  app="$(dirname "${memories}")/agent-app"
  mkdir -p "$(dirname "${script}")"
  if [[ "${outcome}" == "failure" ]]; then
    printf '#!/usr/bin/env bash\nprintf x >>%q\nexit 42\n' "${marker}" >"${script}"
  else
    printf '#!/usr/bin/env bash\nset -euo pipefail\nprintf x >>%q\ncp -R %q "$1"\n' \
      "${marker}" "${fixture}" >"${script}"
  fi
  chmod +x "${script}"
  make_fake_embedding_contract_assets "${memories}" "${app}"
  git -C "${memories}" init -q
  git -C "${memories}" config user.name "Stage contract test"
  git -C "${memories}" config user.email "stage-contract@example.invalid"
  git -C "${memories}" add .
  git -C "${memories}" commit -qm "embedding contract fixture"
  if [[ -f "${fixture}/memories-source-provenance.json" ]]; then
    node - "${fixture}/memories-source-provenance.json" "$(git -C "${memories}" rev-parse HEAD)" <<'NODE'
const fs = require("node:fs");
const [file, revision] = process.argv.slice(2);
const provenance = JSON.parse(fs.readFileSync(file, "utf8"));
provenance.source_revision = revision;
fs.writeFileSync(file, JSON.stringify(provenance, null, 2) + "\n");
NODE
    node "${SCRIPT_DIR}/verify-embedding-contract.mjs" "${memories}" "${app}" \
      --record-provenance "${fixture}/memories-source-provenance.json" >/dev/null
  fi
}

make_fake_embedding_contract_assets() {
  local memories=$1 app=$2 relative body
  mkdir -p "${memories}/protobuf/protobuf/llm_memory/data" \
    "${memories}/protobuf/protobuf/llm_memory/service" \
    "${memories}/grpc-admin/src/bin/memories_db_migrate" \
    "${app}/proto/llm_memory/data" "${app}/proto/llm_memory/service" \
    "${app}/src-tauri/src/commands" "${app}/workers/workflows/rag"
  printf 'data embedding-space proto fixture\n' \
    >"${memories}/protobuf/protobuf/llm_memory/data/embedding_space.proto"
  printf 'service embedding-space proto fixture\n' \
    >"${memories}/protobuf/protobuf/llm_memory/service/embedding_space.proto"
  cp "${memories}/protobuf/protobuf/llm_memory/data/embedding_space.proto" \
    "${app}/proto/llm_memory/data/embedding_space.proto"
  cp "${memories}/protobuf/protobuf/llm_memory/service/embedding_space.proto" \
    "${app}/proto/llm_memory/service/embedding_space.proto"

  cat >"${memories}/grpc-admin/src/bin/memories_db_migrate.rs" <<'EOF'
enum Command { Embedding { command: EmbeddingCommand } }
Command::Embedding { command } => run_embedding(command)
EOF
  cat >"${memories}/grpc-admin/src/bin/memories_db_migrate/embedding_command.rs" <<'EOF'
enum EmbeddingCommand { Inspect, Plan, Switch, Abandon, Finalize, Restore }
EOF
  printf 'pub async fn run_switch() {}\n' \
    >"${memories}/grpc-admin/src/bin/memories_db_migrate/embedding_migrate.rs"
  printf 'pub async fn run_finalize() {}\n' \
    >"${memories}/grpc-admin/src/bin/memories_db_migrate/embedding_finish.rs"

  for relative in \
    auto-embedding.yaml auto-embedding-workers.yaml \
    auto-image-embedding.yaml auto-image-embedding-workers.yaml auto-image-caption.yaml \
    auto-thread-embedding.yaml auto-thread-embedding-workers.yaml \
    thread-reflection/auto-reflection-intent-embedding.yaml \
    thread-reflection/auto-reflection-intent-embedding-workers.yaml \
    thread-reflection/auto-reflection-summary-embedding.yaml \
    thread-reflection/auto-reflection-summary-embedding-workers.yaml; do
    body="document: ${relative}"
    case "${relative}" in
      auto-embedding-workers.yaml)
        body=$'workers:\n  - name: "memories-auto-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-embedding.yaml' ;;
      auto-image-embedding-workers.yaml)
        body=$'workers:\n  - name: "memories-auto-image-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-image-embedding.yaml\n  - name: memories-auto-image-caption\n    settings:\n      workflow_data:\n        $file: auto-image-caption.yaml' ;;
      auto-thread-embedding-workers.yaml)
        body=$'workers:\n  - name: "memories-auto-thread-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-thread-embedding.yaml' ;;
      thread-reflection/auto-reflection-intent-embedding-workers.yaml)
        body=$'workers:\n  - name: "memories-auto-reflection-intent-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-reflection-intent-embedding.yaml' ;;
      thread-reflection/auto-reflection-summary-embedding-workers.yaml)
        body=$'workers:\n  - name: "memories-auto-reflection-summary-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"\n    settings:\n      workflow_data:\n        $file: auto-reflection-summary-embedding.yaml' ;;
    esac
    mkdir -p "${memories}/workflows/$(dirname "${relative}")" \
      "${app}/workers/workflows/$(dirname "${relative}")"
    printf '%s\n' "${body}" >"${memories}/workflows/${relative}"
    cp "${memories}/workflows/${relative}" "${app}/workers/workflows/${relative}"
  done

  cat >"${app}/src-tauri/src/commands/embedding_workers_yaml.rs" <<'EOF'
const TEMPLATE: &str = include_str!("../../../workers/workflows/auto-embedding-workers.yaml");
EOF
  cat >"${app}/workers/workflows/rag/lookback-recall.yaml" <<'EOF'
do:
  - embedQuery:
      run:
        worker:
          name: "%{LOOKBACK_MM_EMBEDDING_WORKER}"
          using: embed_text
          arguments:
            text: "${ $workflow.input.query }"
            prefix: "%{MEMORY_EMBEDDING_QUERY_PREFIX:-}"
EOF
  printf 'app RAG get-context overlay\n' >"${app}/workers/workflows/rag/lookback-get-context.yaml"
  printf 'app RAG get-reflections overlay\n' >"${app}/workers/workflows/rag/lookback-get-reflections.yaml"
}

run_fixture_stage() {
  local app=$1 inputs=$2
  local use_explicit_bundle=0 explicit_bundle=""
  shift 2
  if [[ "${1:-}" == "--bundle-override" ]]; then
    [[ $# -ge 2 ]] || {
      echo "--bundle-override requires a bundle path" >&2
      return 2
    }
    use_explicit_bundle=1
    explicit_bundle=$2
    shift 2
  fi
  local issue_tracker_server="${inputs}/issue-tracker/issue-tracker-server"
  (
    # Most fixtures exercise default resolution; don't let the caller's local
    # release bundle silently turn them into explicit-override tests.
    unset LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE
    if [[ "${use_explicit_bundle}" == "1" ]]; then
      export LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${explicit_bundle}"
    fi
    if [[ -n "${LOOKBACK_TEST_STAGE_XTRACE:-}" ]]; then
      env \
        LOOKBACK_JOBWORKERP_BIN="${inputs}/bin/all-in-one" \
        LOOKBACK_MEMORIES_BIN="${inputs}/bin/front" \
        LOOKBACK_CONDUCTOR_BIN="${inputs}/bin/conductor-main" \
        LOOKBACK_MEMORIES_IMPORT_BIN="${inputs}/bin/memories-import" \
        LOOKBACK_PLUGINS_SRC="${inputs}/plugins" \
        LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${issue_tracker_server}" \
        PROTOC="${inputs}/bin/protoc" \
        bash -x "${SCRIPT}" --agent-app "${app}" --triple test-triple "$@"
    else
      env \
        LOOKBACK_JOBWORKERP_BIN="${inputs}/bin/all-in-one" \
        LOOKBACK_MEMORIES_BIN="${inputs}/bin/front" \
        LOOKBACK_CONDUCTOR_BIN="${inputs}/bin/conductor-main" \
        LOOKBACK_MEMORIES_IMPORT_BIN="${inputs}/bin/memories-import" \
        LOOKBACK_PLUGINS_SRC="${inputs}/plugins" \
        LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${issue_tracker_server}" \
        PROTOC="${inputs}/bin/protoc" \
        bash "${SCRIPT}" --agent-app "${app}" --triple test-triple "$@"
    fi
  )
}

test_inherited_migration_bundle_override_does_not_change_default_fixture() {
  local root="${TMP_ROOT}/inherited-bundle-override" app="${TMP_ROOT}/inherited-bundle-override/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"

  (
    export LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${fixture}"
    run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-inherited-bundle.out
  )

  assert_file "${marker}"
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
    "${app}/src-tauri/migration-bundle" >/dev/null
}

test_default_staging_rejects_memories_without_embedding_contract() {
  local root="${TMP_ROOT}/missing-embedding-contract" app="${TMP_ROOT}/missing-embedding-contract/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  rm "${root}/memories/protobuf/protobuf/llm_memory/service/embedding_space.proto"

  if run_fixture_stage "${app}" "${inputs}" \
    >"${root}/missing-contract.out" 2>"${root}/missing-contract.err"; then
    echo "dev staging accepted a Memories source without the embedding-space contract" >&2
    exit 1
  fi
  [[ ! -e "${marker}" && ! -e "${app}/src-tauri/migration-bundle" ]] || {
    echo "dev staging built or activated a bundle from an unsupported Memories source" >&2
    exit 1
  }
  grep -q 'embedding_space.proto' "${root}/missing-contract.err"
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
  make_migration_bundle "${invalid}" broken-cli

  if run_fixture_stage "${app}" "${inputs}" --bundle-override "${invalid}" \
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
  [[ ! -e "${marker}" && ! -e "${app}/src-tauri/bin" \
    && ! -e "${app}/src-tauri/plugins" \
    && ! -e "${app}/src-tauri/migration-bundle" ]] || {
    echo "dry-run built or staged migration artifacts" >&2
    exit 1
  }
}

test_changed_bundle_replaces_the_staged_bundle() {
  local root="${TMP_ROOT}/changed-bundle" app="${TMP_ROOT}/changed-bundle/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${root}/old" dev old-content
  make_migration_bundle "${fixture}" dev new-content
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  mkdir -p "${app}/src-tauri"
  cp -R "${root}/old" "${app}/src-tauri/migration-bundle"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-changed-bundle.out 2>&1

  assert_file "${marker}"
  [[ "$(migration_bundle_digest "${app}/src-tauri/migration-bundle")" \
    == "$(migration_bundle_digest "${fixture}")" ]] || {
    echo "a bundle built from changed Memories sources did not replace the staged one" >&2
    exit 1
  }
}

test_unchanged_bundle_keeps_the_staged_bundle() {
  local root="${TMP_ROOT}/unchanged-bundle" app="${TMP_ROOT}/unchanged-bundle/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}" dev
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  mkdir -p "${app}/src-tauri"
  cp -R "${fixture}" "${app}/src-tauri/migration-bundle"
  printf 'staged copy\n' >"${app}/src-tauri/migration-bundle/sentinel"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-unchanged-bundle.out 2>&1

  assert_file "${marker}"
  assert_file "${app}/src-tauri/migration-bundle/sentinel"
  grep -q "staged migration bundle is current" /tmp/stage-dev-unchanged-bundle.out
}

wait_for_file() {
  local path=$1 label=$2 attempts=0
  while [[ ! -e "${path}" ]]; do
    attempts=$((attempts + 1))
    if (( attempts >= 1000 )); then
      echo "timed out waiting for ${label}: ${path}" >&2
      return 1
    fi
    sleep 0.01
  done
}

wait_for_either_file() {
  local first=$1 second=$2 label=$3 attempts=0
  while [[ ! -e "${first}" && ! -e "${second}" ]]; do
    attempts=$((attempts + 1))
    if (( attempts >= 1000 )); then
      echo "timed out waiting for ${label}: ${first} or ${second}" >&2
      return 1
    fi
    sleep 0.01
  done
}

test_concurrent_default_builds_are_serialized() {
  local root="${TMP_ROOT}/concurrent" app="${TMP_ROOT}/concurrent/agent-app"
  local inputs="${root}/inputs" fixture="${root}/fixture" marker="${root}/build-called"
  local control="${root}/control" tools="${root}/tools" real_mkdir real_install real_rm
  local first_pid second_pid first_status=0 second_status=0 preserved second_raced=0
  make_stage_inputs "${inputs}"
  printf 'fixture Memories front\n' >"${inputs}/bin/front"
  printf 'fixture memories-import\n' >"${inputs}/bin/memories-import"
  make_migration_bundle "${fixture}"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"
  mkdir -p "${control}" "${tools}"
  CONCURRENT_TEST_CONTROL=${control}
  real_mkdir=$(command -v mkdir)
  real_install=$(command -v install)
  real_rm=$(command -v rm)
  cat >"${tools}/mkdir" <<'MOCK_MKDIR'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${LOOKBACK_TEST_STAGE_ID:-}" == "second" && $# == 1 \
  && "$1" == */src-tauri/migration-bundle.lock ]]; then
  : >"${LOOKBACK_TEST_CONTROL}/second-lock-attempt"
fi
exec "${LOOKBACK_TEST_REAL_MKDIR}" "$@"
MOCK_MKDIR
  cat >"${tools}/install" <<'MOCK_INSTALL'
#!/usr/bin/env bash
set -euo pipefail
target=""
for argument in "$@"; do target=${argument}; done
if [[ "${target}" == */all-in-one-test-triple ]]; then
  : >"${LOOKBACK_TEST_CONTROL}/${LOOKBACK_TEST_STAGE_ID}-install-entered"
  if ! "${LOOKBACK_TEST_REAL_MKDIR}" "${LOOKBACK_TEST_CONTROL}/install-claim" 2>/dev/null; then
    echo "install: cannot create regular file '${target}': File exists" >&2
    exit 1
  fi
  cleanup_install_claim() {
    "${LOOKBACK_TEST_REAL_RM}" -rf "${LOOKBACK_TEST_CONTROL}/install-claim"
  }
  trap cleanup_install_claim EXIT
  while [[ ! -e "${LOOKBACK_TEST_CONTROL}/release-install" ]]; do sleep 0.01; done
fi
"${LOOKBACK_TEST_REAL_INSTALL}" "$@"
MOCK_INSTALL
  chmod +x "${tools}/mkdir" "${tools}/install"

  LOOKBACK_TEST_STAGE_ID=first LOOKBACK_TEST_CONTROL="${control}" \
    LOOKBACK_TEST_REAL_MKDIR="${real_mkdir}" LOOKBACK_TEST_REAL_INSTALL="${real_install}" \
    LOOKBACK_TEST_REAL_RM="${real_rm}" PATH="${tools}:${PATH}" \
    run_fixture_stage "${app}" "${inputs}" >"${root}/first-child.log" 2>&1 &
  first_pid=$!
  CONCURRENT_STAGE_PIDS+=("${first_pid}")
  wait_for_file "${control}/first-install-entered" "first stage reached binary install"
  LOOKBACK_TEST_STAGE_ID=second LOOKBACK_TEST_CONTROL="${control}" \
    LOOKBACK_TEST_REAL_MKDIR="${real_mkdir}" LOOKBACK_TEST_REAL_INSTALL="${real_install}" \
    LOOKBACK_TEST_REAL_RM="${real_rm}" PATH="${tools}:${PATH}" \
    run_fixture_stage "${app}" "${inputs}" >"${root}/second-child.log" 2>&1 &
  second_pid=$!
  CONCURRENT_STAGE_PIDS+=("${second_pid}")
  wait_for_either_file "${control}/second-install-entered" "${control}/second-lock-attempt" \
    "second stage's serialization point"
  [[ ! -e "${control}/second-install-entered" ]] || second_raced=1
  if (( second_raced != 0 )); then
    echo "second stage reached binary installation before contending for the shared staging lock" >&2
  fi
  : >"${control}/release-install"
  if wait "${first_pid}"; then :; else first_status=$?; fi
  if wait "${second_pid}"; then :; else second_status=$?; fi
  if (( first_status != 0 || second_status != 0 || second_raced != 0 )); then
    PRESERVE_TMP_ROOT=1
    preserved=$(mktemp -d "/tmp/opencode/stage-dev-concurrent-failure.XXXXXX")
    cp "${root}/first-child.log" "${preserved}/first-child.log"
    cp "${root}/second-child.log" "${preserved}/second-child.log"
    echo "concurrent stage child exit statuses: first=${first_status} second=${second_status}" >&2
    echo "preserved child logs: ${preserved}" >&2
    echo "--- first child ---" >&2
    cat "${root}/first-child.log" >&2
    echo "--- second child ---" >&2
    cat "${root}/second-child.log" >&2
    return 1
  fi

  [[ "$(wc -c <"${marker}" | tr -d ' ')" == "2" ]] || {
    echo "each concurrent staging run must build the migration bundle exactly once" >&2
    exit 1
  }
  node "${SCRIPT_DIR}/verify-migration-release.mjs" \
    "${app}/src-tauri/migration-bundle" --require-provenance \
    --front "${app}/src-tauri/bin/front-test-triple" \
    --import "${app}/src-tauri/bin/memories-import-test-triple" \
    --app-assets "${app}" >/dev/null
  for binary in all-in-one front conductor-main memories-import protoc issue-tracker-server; do
    local input_path="${inputs}/bin/${binary}"
    [[ "${binary}" != "issue-tracker-server" ]] \
      || input_path="${inputs}/issue-tracker/issue-tracker-server"
    assert_file "${app}/src-tauri/bin/${binary}-test-triple"
    cmp -s "${input_path}" "${app}/src-tauri/bin/${binary}-test-triple" || {
      echo "concurrent staging left an incomplete or inconsistent ${binary} binary" >&2
      exit 1
    }
  done
  assert_file "${app}/src-tauri/plugins/libtest.so"
  cmp -s "${inputs}/plugins/libtest.so" "${app}/src-tauri/plugins/libtest.so" || {
    echo "concurrent staging left an incomplete or inconsistent plugin" >&2
    exit 1
  }
  [[ ! -e "${app}/src-tauri/.migration-bundle-previous" \
    && ! -e "${app}/src-tauri/.migration-bundle-previous.snapshot.json" ]] || {
    echo "concurrent staging left migration recovery artifacts behind" >&2
    exit 1
  }
  [[ -z "$(find "${app}/src-tauri" -maxdepth 1 \( \
    -name '.migration-bundle-stage.*' -o -name '.migration-bundle-build.*' \) -print -quit)" ]] || {
    echo "concurrent staging left a partial migration bundle directory" >&2
    exit 1
  }
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

test_interrupted_two_rename_activation_restores_previous_valid_bundle() {
  local root="${TMP_ROOT}/interrupted-activation" app="${TMP_ROOT}/interrupted-activation/agent-app"
  local inputs="${root}/inputs" fixture="${root}/old" marker="${root}/build-called"
  local backup="${app}/src-tauri/.migration-bundle-previous"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${fixture}"
  mkdir -p "${app}/src-tauri" "${app}/src-tauri/migration-bundle.lock"
  cp -R "${fixture}" "${backup}"
  printf 'keep-old-generation\n' >"${backup}/sentinel"
  printf '99999999\n' >"${app}/src-tauri/migration-bundle.lock/owner-pid"
  make_fake_memories_bundle_builder "${root}/memories" "${fixture}" "${marker}"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-interrupted-activation.out

  assert_file "${app}/src-tauri/migration-bundle/sentinel"
  [[ "$(cat "${app}/src-tauri/migration-bundle/sentinel")" == "keep-old-generation" ]] || {
    echo "interrupted activation did not restore the previous valid bundle" >&2
    exit 1
  }
  [[ ! -e "${backup}" && ! -e "${app}/src-tauri/migration-bundle.lock" ]] || {
    echo "activation recovery left stale backup or lock state" >&2
    exit 1
  }
}

test_interrupted_after_new_bundle_rename_keeps_new_bundle_and_cleans_old_backup() {
  local root="${TMP_ROOT}/interrupted-after-activation" app="${TMP_ROOT}/interrupted-after-activation/agent-app"
  local inputs="${root}/inputs" previous="${root}/previous" current="${root}/current"
  local destination="${app}/src-tauri/migration-bundle"
  local backup="${app}/src-tauri/.migration-bundle-previous"
  make_stage_inputs "${inputs}"
  make_migration_bundle "${previous}"
  make_migration_bundle "${current}"
  mkdir -p "${app}/src-tauri" "${destination}" "${app}/src-tauri/migration-bundle.lock"
  cp -R "${previous}/." "${backup}/"
  cp -R "${current}/." "${destination}/"
  printf 'new-generation\n' >"${destination}/sentinel"
  printf 'old-generation\n' >"${backup}/sentinel"
  printf '99999999\n' >"${app}/src-tauri/migration-bundle.lock/owner-pid"
  make_fake_memories_bundle_builder "${root}/memories" "${current}" "${root}/build-called"

  run_fixture_stage "${app}" "${inputs}" >/tmp/stage-dev-after-activation.out

  [[ "$(cat "${destination}/sentinel")" == "new-generation" ]] || {
    echo "recovery replaced a verified new bundle with its older backup" >&2
    exit 1
  }
  [[ ! -e "${backup}" && ! -e "${app}/src-tauri/migration-bundle.lock" ]] || {
    echo "post-activation recovery left backup or lock state" >&2
    exit 1
  }
}

test_interrupted_activation_restores_legacy_bundle_from_its_snapshot() {
  local root="${TMP_ROOT}/interrupted-legacy-activation" app="${TMP_ROOT}/interrupted-legacy-activation/agent-app"
  local inputs="${root}/inputs" marker="${root}/build-called"
  local destination="${app}/src-tauri/migration-bundle"
  local backup="${app}/src-tauri/.migration-bundle-previous"
  make_stage_inputs "${inputs}"
  mkdir -p "${destination}" "${app}/src-tauri/migration-bundle.lock"
  printf 'legacy-generation-1\n' >"${destination}/generation.txt"
  printf 'preserve-exact-old-bundle-bytes\n' >"${destination}/legacy-binary"
  source "${SCRIPT_DIR}/lib/verified-migration-bundle.sh"
  migration_bundle_preserve_previous "${destination}" "${backup}"
  mv "${destination}" "${backup}"
  printf '99999999\n' >"${app}/src-tauri/migration-bundle.lock/owner-pid"
  make_fake_memories_bundle_builder "${root}/memories" "${root}/unused" "${marker}" failure

  if run_fixture_stage "${app}" "${inputs}" \
    >/tmp/stage-dev-interrupted-legacy.out 2>/tmp/stage-dev-interrupted-legacy.err; then
    echo "expected replacement build failure after restoring old generation" >&2
    exit 1
  fi

  [[ "$(cat "${destination}/generation.txt")" == "legacy-generation-1" ]] || {
    echo "recovery did not restore the previous-generation bundle snapshot" >&2
    exit 1
  }
  [[ "$(cat "${destination}/legacy-binary")" == "preserve-exact-old-bundle-bytes" ]] || {
    echo "recovery changed bytes in the previous-generation bundle" >&2
    exit 1
  }
  [[ ! -e "${backup}" && ! -e "${backup}.snapshot.json" \
    && ! -e "${app}/src-tauri/migration-bundle.lock" ]] || {
    echo "legacy bundle snapshot recovery left stale backup or lock state" >&2
    exit 1
  }
  assert_file "${marker}"
}

test_interrupted_activation_does_not_restore_snapshot_mismatch() {
  local root="${TMP_ROOT}/interrupted-tampered-snapshot" app="${TMP_ROOT}/interrupted-tampered-snapshot/agent-app"
  local destination="${app}/src-tauri/migration-bundle"
  local backup="${app}/src-tauri/.migration-bundle-previous"
  mkdir -p "${destination}"
  printf 'legacy-generation-1\n' >"${destination}/generation.txt"
  source "${SCRIPT_DIR}/lib/verified-migration-bundle.sh"
  migration_bundle_preserve_previous "${destination}" "${backup}"
  mv "${destination}" "${backup}"
  printf 'tampered after activation snapshot\n' >"${backup}/generation.txt"

  if migration_bundle_recover_previous \
    "${destination}" "${backup}" "${SCRIPT_DIR}/verify-migration-release.mjs" 0; then
    echo "recovery accepted backup bytes that differ from the activation snapshot" >&2
    exit 1
  fi
  [[ ! -e "${destination}" && -e "${backup}" && -f "${backup}.snapshot.json" ]] || {
    echo "snapshot mismatch recovery did not preserve untrusted backup state" >&2
    exit 1
  }
}

test_interrupted_activation_snapshot_overrules_a_still_valid_current_bundle() {
  local root="${TMP_ROOT}/interrupted-valid-but-changed-snapshot" app="${TMP_ROOT}/interrupted-valid-but-changed-snapshot/agent-app"
  local destination="${app}/src-tauri/migration-bundle"
  local backup="${app}/src-tauri/.migration-bundle-previous"
  make_migration_bundle "${destination}"
  source "${SCRIPT_DIR}/lib/verified-migration-bundle.sh"
  migration_bundle_preserve_previous "${destination}" "${backup}"
  mv "${destination}" "${backup}"
  printf 'post-snapshot mutation\n' >>"${backup}/test-artifacts/front"

  if migration_bundle_recover_previous \
    "${destination}" "${backup}" "${SCRIPT_DIR}/verify-migration-release.mjs" 0; then
    echo "recovery used current-release validity instead of the original activation snapshot" >&2
    exit 1
  fi
  [[ ! -e "${destination}" && -e "${backup}" && -f "${backup}.snapshot.json" ]] || {
    echo "snapshot mismatch against a semantically valid bundle was not preserved" >&2
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
  make_issue_tracker_server "${src}/issue-tracker-server"
  make_plugin "${TMP_ROOT}/plugins/libexisting.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  local app="${TMP_ROOT}/app"
  LOOKBACK_JOBWORKERP_BIN="${src}/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${src}/front" \
    LOOKBACK_CONDUCTOR_BIN="${src}/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${src}/memories-import" \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${src}/issue-tracker-server" \
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
  make_issue_tracker_server "${TMP_ROOT}/src/issue-tracker-server"
  make_plugin "${workspace}/plugins/cuda_runner/libcuda_runner.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"
  mkdir -p "$(dirname "${src}")"

  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
    LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${TMP_ROOT}/src/issue-tracker-server" \
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
  make_issue_tracker_server "${TMP_ROOT}/src/issue-tracker-server"
  make_plugin "${plugin_src}/libcustom.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  LOOKBACK_JOBWORKERP_BIN="${TMP_ROOT}/src/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${TMP_ROOT}/src/front" \
    LOOKBACK_CONDUCTOR_BIN="${TMP_ROOT}/src/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${TMP_ROOT}/src/memories-import" \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${TMP_ROOT}/src/issue-tracker-server" \
    LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
    LOOKBACK_PLUGINS_SRC="${TMP_ROOT}/custom-plugins" \
    PROTOC="${TMP_ROOT}/src/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple >/tmp/stage-dev-plugin-env.out

  assert_file "${app}/src-tauri/plugins/libcustom.so"
}

test_stages_issue_tracker_server_from_explicit_binary() {
  local app="${TMP_ROOT}/issue-tracker-server-app" inputs="${TMP_ROOT}/issue-tracker-inputs"
  make_stage_inputs "${inputs}"
  make_issue_tracker_server "${inputs}/issue-tracker/issue-tracker-server"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  LOOKBACK_JOBWORKERP_BIN="${inputs}/bin/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${inputs}/bin/front" \
    LOOKBACK_CONDUCTOR_BIN="${inputs}/bin/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${inputs}/bin/memories-import" \
    LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${TMP_ROOT}/migration-bundle" \
    LOOKBACK_PLUGINS_SRC="${inputs}/plugins" \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${inputs}/issue-tracker/issue-tracker-server" \
    PROTOC="${inputs}/bin/protoc" \
    bash "${SCRIPT}" --agent-app "${app}" --triple test-triple >/tmp/stage-dev-issue-tracker.out

  assert_file "${app}/src-tauri/bin/issue-tracker-server-test-triple"
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
  make_issue_tracker_server "${src}/issue-tracker-server"
  local app="${TMP_ROOT}/dry-app"
  local dry_parent
  dry_parent=$(dirname "${app}")
  make_plugin "${TMP_ROOT}/plugins/cuda_runner/libdry.so"
  make_migration_bundle "${TMP_ROOT}/migration-bundle"

  LOOKBACK_JOBWORKERP_BIN="${src}/all-in-one" \
    LOOKBACK_MEMORIES_BIN="${src}/front" \
    LOOKBACK_CONDUCTOR_BIN="${src}/conductor-main" \
    LOOKBACK_MEMORIES_IMPORT_BIN="${src}/memories-import" \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${src}/issue-tracker-server" \
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

test_verifier_rejects_bundles_memories_does_not_vouch_for() {
  local mode
  for mode in broken-cli stale-digest provenance-mismatch; do
    local bundle="${TMP_ROOT}/rejected-${mode}"
    make_migration_bundle "${bundle}" "${mode}"
    if node "${SCRIPT_DIR}/verify-migration-release.mjs" "${bundle}" \
      >/tmp/stage-dev-rejected.out 2>/tmp/stage-dev-rejected.err; then
      echo "verifier accepted a ${mode} migration bundle" >&2
      exit 1
    fi
  done
}

test_env_overrides_stage_target_triple_bins
test_stages_cuda_runner_plugins_from_workspace_plugins_cuda_runner
test_stages_nested_plugins_from_env_override
test_stages_issue_tracker_server_from_explicit_binary
test_missing_required_binary_fails
test_dry_run_does_not_write_files
test_verifier_rejects_bundles_memories_does_not_vouch_for
test_missing_default_bundle_is_built_from_sibling_memories
test_default_staging_rejects_memories_without_embedding_contract
test_default_bundle_build_failure_preserves_existing_destination
test_invalid_explicit_bundle_does_not_fall_back_to_builder
test_missing_default_dry_run_only_prints_build_plan
test_changed_bundle_replaces_the_staged_bundle
test_unchanged_bundle_keeps_the_staged_bundle
test_concurrent_default_builds_are_serialized
test_ownerless_stale_lock_is_recovered
test_interrupted_two_rename_activation_restores_previous_valid_bundle
test_interrupted_after_new_bundle_rename_keeps_new_bundle_and_cleans_old_backup
test_inherited_migration_bundle_override_does_not_change_default_fixture
test_interrupted_activation_restores_legacy_bundle_from_its_snapshot
test_interrupted_activation_does_not_restore_snapshot_mismatch
test_interrupted_activation_snapshot_overrules_a_still_valid_current_bundle

echo "stage-dev-external-bins tests passed"
