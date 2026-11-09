#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AGENT_APP=$(cd "${SCRIPT_DIR}/.." && pwd)
VERIFY_PACKAGING="${SCRIPT_DIR}/verify-packaging-migration-provenance.sh"
FIXTURE="${SCRIPT_DIR}/lib/migration-test-fixture.mjs"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

CONTRACT_SOURCE="${TMP_ROOT}/contract/memories"
CONTRACT_APP="${TMP_ROOT}/contract/agent-app"
ensure_contract_source() {
  [[ -d "${CONTRACT_SOURCE}/.git" ]] && return 0
  mkdir -p "${CONTRACT_SOURCE}/protobuf/protobuf/llm_memory/data" \
    "${CONTRACT_SOURCE}/protobuf/protobuf/llm_memory/service" \
    "${CONTRACT_SOURCE}/grpc-admin/src/bin/memories_db_migrate" \
    "${CONTRACT_APP}/proto/llm_memory/data" "${CONTRACT_APP}/proto/llm_memory/service" \
    "${CONTRACT_APP}/src-tauri/src/commands" "${CONTRACT_APP}/workers/workflows/rag"
  printf 'data embedding-space proto fixture\n' \
    >"${CONTRACT_SOURCE}/protobuf/protobuf/llm_memory/data/embedding_space.proto"
  printf 'service embedding-space proto fixture\n' \
    >"${CONTRACT_SOURCE}/protobuf/protobuf/llm_memory/service/embedding_space.proto"
  cp "${CONTRACT_SOURCE}/protobuf/protobuf/llm_memory/data/embedding_space.proto" \
    "${CONTRACT_APP}/proto/llm_memory/data/embedding_space.proto"
  cp "${CONTRACT_SOURCE}/protobuf/protobuf/llm_memory/service/embedding_space.proto" \
    "${CONTRACT_APP}/proto/llm_memory/service/embedding_space.proto"
  cat >"${CONTRACT_SOURCE}/grpc-admin/src/bin/memories_db_migrate.rs" <<'EOF'
enum Command { Embedding { command: EmbeddingCommand } }
Command::Embedding { command } => run_embedding(command)
EOF
  printf 'enum EmbeddingCommand { Inspect, Plan, Switch, Abandon, Finalize, Restore }\n' \
    >"${CONTRACT_SOURCE}/grpc-admin/src/bin/memories_db_migrate/embedding_command.rs"
  printf 'run switch migration\n' \
    >"${CONTRACT_SOURCE}/grpc-admin/src/bin/memories_db_migrate/embedding_migrate.rs"
  printf 'run finalize migration\n' \
    >"${CONTRACT_SOURCE}/grpc-admin/src/bin/memories_db_migrate/embedding_finish.rs"

  local relative body
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
    mkdir -p "${CONTRACT_SOURCE}/workflows/$(dirname "${relative}")" \
      "${CONTRACT_APP}/workers/workflows/$(dirname "${relative}")"
    printf '%s\n' "${body}" >"${CONTRACT_SOURCE}/workflows/${relative}"
    cp "${CONTRACT_SOURCE}/workflows/${relative}" "${CONTRACT_APP}/workers/workflows/${relative}"
  done
  cat >"${CONTRACT_APP}/src-tauri/src/commands/embedding_workers_yaml.rs" <<'EOF'
const TEMPLATE: &str = include_str!("../../../workers/workflows/auto-embedding-workers.yaml");
EOF
  cat >"${CONTRACT_APP}/workers/workflows/rag/lookback-recall.yaml" <<'EOF'
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
  printf 'app RAG get-context overlay\n' >"${CONTRACT_APP}/workers/workflows/rag/lookback-get-context.yaml"
  printf 'app RAG get-reflections overlay\n' >"${CONTRACT_APP}/workers/workflows/rag/lookback-get-reflections.yaml"
  git -C "${CONTRACT_SOURCE}" init -q
  git -C "${CONTRACT_SOURCE}" config user.name "Packaging contract test"
  git -C "${CONTRACT_SOURCE}" config user.email "packaging-contract@example.invalid"
  git -C "${CONTRACT_SOURCE}" add .
  git -C "${CONTRACT_SOURCE}" commit -qm "embedding contract fixture"
}

stage_contract_assets() {
  local app=$1
  mkdir -p "${app}/proto" "${app}/workers" "${app}/src-tauri/src/commands"
  cp -R "${CONTRACT_APP}/proto/." "${app}/proto/"
  cp -R "${CONTRACT_APP}/workers/." "${app}/workers/"
  cp "${CONTRACT_APP}/src-tauri/src/commands/embedding_workers_yaml.rs" \
    "${app}/src-tauri/src/commands/embedding_workers_yaml.rs"
}

case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64) HOST_TRIPLE=aarch64-apple-darwin ;;
  Darwin:x86_64) HOST_TRIPLE=x86_64-apple-darwin ;;
  Linux:x86_64) HOST_TRIPLE=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) HOST_TRIPLE=aarch64-unknown-linux-gnu ;;
  MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64) HOST_TRIPLE=x86_64-pc-windows-msvc ;;
  *) echo "unsupported test host: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

make_fixture() {
  local fixture=$1 mode=${2:-valid}
  node "${FIXTURE}" "${fixture}" "${mode}"
  node - "${fixture}" <<'NODE'
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
  if [[ -f "${fixture}/memories-source-provenance.json" ]]; then
    ensure_contract_source
    node - "${fixture}/memories-source-provenance.json" \
      "$(git -C "${CONTRACT_SOURCE}" rev-parse HEAD)" <<'NODE'
const fs = require("node:fs");
const [file, revision] = process.argv.slice(2);
const provenance = JSON.parse(fs.readFileSync(file, "utf8"));
provenance.source_revision = revision;
fs.writeFileSync(file, JSON.stringify(provenance, null, 2) + "\n");
NODE
    node "${SCRIPT_DIR}/verify-embedding-contract.mjs" "${CONTRACT_SOURCE}" "${CONTRACT_APP}" \
      --record-provenance "${fixture}/memories-source-provenance.json" >/dev/null
  fi
}
stage_fixture_artifacts() {
  local fixture=$1 app=$2 triple=$3
  local suffix=""
  [[ "${triple}" != *-windows-* ]] || suffix=.exe
  stage_contract_assets "${app}"
  mkdir -p "${app}/src-tauri/bin"
  cp "${fixture}/test-artifacts/front" "${app}/src-tauri/bin/front-${triple}${suffix}"
  cp "${fixture}/test-artifacts/memories-import" "${app}/src-tauri/bin/memories-import-${triple}${suffix}"
}
make_bin() {
  local path=$1
  mkdir -p "$(dirname "${path}")"
  printf '#!/usr/bin/env bash\nexit 0\n' >"${path}"
  chmod +x "${path}"
}

# Explicit target resolution must select the target-suffixed externalBin files,
# not whichever host artifacts happen to be found on PATH.
for triple in \
  x86_64-unknown-linux-gnu \
  aarch64-apple-darwin \
  x86_64-pc-windows-msvc; do
  fixture="${TMP_ROOT}/fixture-${triple}"
  app="${TMP_ROOT}/app-${triple}"
  make_fixture "${fixture}"
  mkdir -p "${app}/src-tauri"
  cp -R "${fixture}" "${app}/src-tauri/migration-bundle"
  stage_fixture_artifacts "${fixture}" "${app}" "${triple}"
  if [[ "${triple}" == x86_64-pc-windows-msvc ]]; then
    LOOKBACK_AGENT_APP="${app}" TAURI_ENV_PLATFORM=windows TAURI_ENV_ARCH=x86_64 \
      bash "${VERIFY_PACKAGING}"
  else
    LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${triple}"
  fi
done

valid="${TMP_ROOT}/valid"
missing_manifest="${TMP_ROOT}/missing-manifest"
app="${TMP_ROOT}/packaging-app"
make_fixture "${valid}"
cp -R "${valid}" "${missing_manifest}"
rm "${missing_manifest}/memories-source-provenance.json"
mkdir -p "${app}/src-tauri"
cp -R "${missing_manifest}" "${app}/src-tauri/migration-bundle"
stage_fixture_artifacts "${valid}" "${app}" "${HOST_TRIPLE}"

if LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${HOST_TRIPLE}" \
  >"${TMP_ROOT}/missing-manifest.log" 2>&1; then
  echo "packaging accepted a migration bundle without source provenance" >&2
  exit 1
fi
grep -qi 'regenerate.*pinned\|pinned.*regenerate' "${TMP_ROOT}/missing-manifest.log"

legacy_contract="${TMP_ROOT}/legacy-contract"
make_fixture "${legacy_contract}"
node - "${legacy_contract}/memories-source-provenance.json" <<'NODE'
const fs = require("node:fs");
const file = process.argv[2];
const provenance = JSON.parse(fs.readFileSync(file, "utf8"));
delete provenance.embedding_contract;
fs.writeFileSync(file, JSON.stringify(provenance, null, 2) + "\n");
NODE
rm -rf "${app}/src-tauri/migration-bundle"
cp -R "${legacy_contract}" "${app}/src-tauri/migration-bundle"
stage_contract_assets "${app}"
stage_fixture_artifacts "${valid}" "${app}" "${HOST_TRIPLE}"
if LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${HOST_TRIPLE}" \
  >"${TMP_ROOT}/missing-embedding-contract.log" 2>&1; then
  echo "packaging accepted a provenance manifest without the embedding asset contract" >&2
  exit 1
fi
grep -q 'required embedding contract' "${TMP_ROOT}/missing-embedding-contract.log"

rm -rf "${app}/src-tauri/migration-bundle"
cp -R "${valid}" "${app}/src-tauri/migration-bundle"
printf 'front from another source revision\n' >"${app}/src-tauri/bin/front-${HOST_TRIPLE}"
if LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${HOST_TRIPLE}" \
  >"${TMP_ROOT}/mismatched-front.log" 2>&1; then
  echo "packaging accepted a front binary that differs from migration provenance" >&2
  exit 1
fi
grep -q 'front artifact does not match' "${TMP_ROOT}/mismatched-front.log"

stage_fixture_artifacts "${valid}" "${app}" "${HOST_TRIPLE}"
printf 'import from another source revision\n' >"${app}/src-tauri/bin/memories-import-${HOST_TRIPLE}"
if LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${HOST_TRIPLE}" \
  >"${TMP_ROOT}/mismatched-import.log" 2>&1; then
  echo "packaging accepted a memories-import binary that differs from migration provenance" >&2
  exit 1
fi
grep -q 'memories_import artifact does not match' "${TMP_ROOT}/mismatched-import.log"

stage_contract_assets "${app}"
printf '\n# stale workflow worker suffix\n' >>"${app}/workers/workflows/auto-image-embedding-workers.yaml"
if LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${HOST_TRIPLE}" \
  >"${TMP_ROOT}/mismatched-embedding-worker.log" 2>&1; then
  echo "packaging accepted a worker template that differs from its Memories source contract" >&2
  exit 1
fi
grep -q 'auto-image-embedding-workers.yaml' "${TMP_ROOT}/mismatched-embedding-worker.log"

stage_contract_assets "${app}"
node - "${app}/workers/workflows/rag/lookback-recall.yaml" \
  "${app}/src-tauri/migration-bundle/memories-source-provenance.json" <<'NODE'
const crypto = require("node:crypto");
const fs = require("node:fs");
const [overlayPath, provenancePath] = process.argv.slice(2);
const overlay = fs.readFileSync(overlayPath, "utf8")
  .replace('name: "%{LOOKBACK_MM_EMBEDDING_WORKER}"',
    'name: "memories-mm-embedding%{MEMORY_EMBEDDING_SPACE_SUFFIX}"') +
  '\n# name: "%{LOOKBACK_MM_EMBEDDING_WORKER}"\n' +
  '# prefix: "%{MEMORY_EMBEDDING_QUERY_PREFIX:-}"\n';
fs.writeFileSync(overlayPath, overlay);
const provenance = JSON.parse(fs.readFileSync(provenancePath, "utf8"));
provenance.embedding_contract.app_sha256["workers/workflows/rag/lookback-recall.yaml"] =
  crypto.createHash("sha256").update(overlay).digest("hex");
fs.writeFileSync(provenancePath, JSON.stringify(provenance, null, 2) + "\n");
NODE
if LOOKBACK_AGENT_APP="${app}" bash "${VERIFY_PACKAGING}" --triple "${HOST_TRIPLE}" \
  >"${TMP_ROOT}/mismatched-rag-overlay.log" 2>&1; then
  echo "packaging accepted an unresolved RAG worker despite matching provenance digests" >&2
  exit 1
fi
grep -q 'RAG query embedding overlay' "${TMP_ROOT}/mismatched-rag-overlay.log"

# The Tauri wrapper preserves the non-strict dev staging flow, but refuses to
# package its output unless the strict provenance check succeeds first.
case "$(uname -s)" in
  Darwin) plugin_ext=dylib ;;
  Linux) plugin_ext=so ;;
  MINGW*|MSYS*|CYGWIN*) plugin_ext=dll ;;
  *) echo "unsupported test host: $(uname -s)" >&2; exit 1 ;;
esac
if [[ "${plugin_ext}" != dll ]]; then
dev_app="${TMP_ROOT}/dev/github/agent-app"
dev_inputs="${TMP_ROOT}/dev-inputs"
dev_bundle="${TMP_ROOT}/dev-bundle"
mkdir -p "$(dirname "${dev_app}")"
make_fixture "${dev_bundle}"
rm "${dev_bundle}/memories-source-provenance.json"
make_bin "${dev_inputs}/all-in-one"
make_bin "${dev_inputs}/front"
make_bin "${dev_inputs}/conductor-main"
make_bin "${dev_inputs}/memories-import"
make_bin "${dev_inputs}/issue-tracker-server"
make_bin "${dev_inputs}/protoc"
mkdir -p "${dev_inputs}/plugins"
printf 'dev plugin\n' >"${dev_inputs}/plugins/libcuda_runner.${plugin_ext}"
mkdir -p "${TMP_ROOT}/toolbin"
cat >"${TMP_ROOT}/toolbin/tauri" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${TAURI_ARG_LOG}"
EOF
chmod +x "${TMP_ROOT}/toolbin/tauri"

LOOKBACK_AGENT_APP="${dev_app}" \
  LOOKBACK_JOBWORKERP_BIN="${dev_inputs}/all-in-one" \
  LOOKBACK_MEMORIES_BIN="${dev_inputs}/front" \
  LOOKBACK_CONDUCTOR_BIN="${dev_inputs}/conductor-main" \
  LOOKBACK_MEMORIES_IMPORT_BIN="${dev_inputs}/memories-import" \
  LOOKBACK_MEMORIES_DB_MIGRATE_BUNDLE="${dev_bundle}" \
  LOOKBACK_PLUGINS_SRC="${dev_inputs}/plugins" \
  LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${dev_inputs}/issue-tracker-server" \
  PROTOC="${dev_inputs}/protoc" \
  TAURI_ARG_LOG="${TMP_ROOT}/tauri-args.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${SCRIPT_DIR}/run-tauri.sh" dev --no-watch
grep -q '^dev --no-watch$' "${TMP_ROOT}/tauri-args.log"
[[ ! -e "${dev_app}/src-tauri/migration-bundle/memories-source-provenance.json" ]]

if LOOKBACK_AGENT_APP="${dev_app}" \
  LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1 \
  TAURI_ARG_LOG="${TMP_ROOT}/tauri-args.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${SCRIPT_DIR}/run-tauri.sh" build >"${TMP_ROOT}/run-tauri-rejected.log" 2>&1; then
  echo "Tauri wrapper packaged dev-staged artifacts without provenance" >&2
  exit 1
fi
grep -q 'pinned release inputs' "${TMP_ROOT}/run-tauri-rejected.log"
[[ "$(wc -l <"${TMP_ROOT}/tauri-args.log")" -eq 1 ]]
fi

packaged_app="${TMP_ROOT}/packaged-app"
mkdir -p "${packaged_app}/src-tauri"
cp -R "${valid}" "${packaged_app}/src-tauri/migration-bundle"
stage_fixture_artifacts "${valid}" "${packaged_app}" "${HOST_TRIPLE}"
LOOKBACK_AGENT_APP="${packaged_app}" \
  LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1 \
  TAURI_ARG_LOG="${TMP_ROOT}/tauri-args.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${SCRIPT_DIR}/run-tauri.sh" build
grep -q '^build$' "${TMP_ROOT}/tauri-args.log"

cross_app="${TMP_ROOT}/app-aarch64-apple-darwin"
LOOKBACK_AGENT_APP="${cross_app}" \
  LOOKBACK_EXTERNAL_ARTIFACTS_PRESTAGED=1 \
  TAURI_ARG_LOG="${TMP_ROOT}/tauri-args.log" \
  PATH="${TMP_ROOT}/toolbin:${PATH}" \
  bash "${SCRIPT_DIR}/run-tauri.sh" build --target aarch64-apple-darwin
grep -q '^build --target aarch64-apple-darwin$' "${TMP_ROOT}/tauri-args.log"

# The Tauri CLI beforeBuild hook must use the strict verifier, while local dev
# retains its explicitly non-strict staging path.
node - "${AGENT_APP}/package.json" "${AGENT_APP}/src-tauri/tauri.conf.json" <<'NODE'
const fs = require("node:fs");
const [packagePath, tauriPath] = process.argv.slice(2);
const pkg = JSON.parse(fs.readFileSync(packagePath, "utf8"));
const tauri = JSON.parse(fs.readFileSync(tauriPath, "utf8"));
if (!pkg.scripts?.["verify:packaging-migration"]?.includes("verify-packaging-migration-provenance.sh")) {
  throw new Error("package.json must expose the strict packaging provenance verifier");
}
if (pkg.scripts?.tauri !== "bash scripts/run-tauri.sh") {
  throw new Error("pnpm tauri must route through the packaging-aware wrapper");
}
if (!pkg.scripts?.["tauri:build"]?.includes("scripts/run-tauri.sh")) {
  throw new Error("pnpm tauri:build must route through the packaging-aware wrapper");
}
if (!tauri.build?.beforeBuildCommand?.includes("pnpm verify:packaging-migration")) {
  throw new Error("Tauri beforeBuildCommand must require packaging provenance");
}
if (!tauri.build?.beforeBundleCommand?.includes("pnpm verify:packaging-migration")) {
  throw new Error("Tauri beforeBundleCommand must require packaging provenance");
}
if (!tauri.build?.beforeDevCommand?.includes("pnpm dev")) {
  throw new Error("Tauri dev command must remain available for local development");
}
NODE

echo "packaging migration provenance tests passed"
