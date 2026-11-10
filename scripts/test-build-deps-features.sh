#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/build-deps.sh
source "${SCRIPT_DIR}/lib/build-deps.sh"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

GPU=metal
[[ "$(gpu_features)" == "--features metal" ]] || fail "llama metal features changed"
[[ "$(mm_gpu_features)" == "--features metal,onnx-coreml" ]] \
  || fail "mm metal build must include CoreML"

GPU=cuda
[[ "$(gpu_features)" == "--features cuda" ]] || fail "llama cuda features changed"
[[ "$(mm_gpu_features)" == "--features cuda,onnx-cuda" ]] \
  || fail "mm cuda build must include ONNX CUDA"

GPU=cpu
[[ -z "$(gpu_features)" ]] || fail "llama CPU build must not enable GPU features"
[[ -z "$(mm_gpu_features)" ]] || fail "mm CPU build must not enable GPU features"

[[ "$(repo_url memory-store)" == "https://github.com/jobworkerp-rs/memory-store.git" ]] \
  || fail "Memories must build from its public GitHub repository"
[[ "$(repo_branch memory-store)" == "v0.0.19" ]] \
  || fail "Memories must select the published v0.0.19 tag"
[[ "$(repo_revision memory-store)" == "8253e725708264592e131af3b92d1b4c7719054a" ]] \
  || fail "Memories must pin the peeled v0.0.19 commit"
[[ "$(repo_ref_kind memory-store)" == "tag" ]] \
  || fail "the default Memories release ref must be fetched as a tag"

LOOKBACK_MEMORY_STORE_REPO_URL="https://example.invalid/memory-store.git"
[[ "$(repo_url memory-store)" == "${LOOKBACK_MEMORY_STORE_REPO_URL}" ]] \
  || fail "memory-store repository URL override must be honoured"
if (
  export LOOKBACK_MEMORY_STORE_REPO_URL="https://example.invalid/memory-store.git"
  unset LOOKBACK_MEMORY_STORE_REF LOOKBACK_MEMORY_STORE_REVISION
  validate_memory_store_source_config
) \
  >/tmp/lookback-missing-memories-pin.out 2>/tmp/lookback-missing-memories-pin.err; then
  fail "custom Memories remotes must require explicit ref and revision pins"
fi
grep -Fq "LOOKBACK_MEMORY_STORE_REF and LOOKBACK_MEMORY_STORE_REVISION" \
  /tmp/lookback-missing-memories-pin.err \
  || fail "unpinned custom Memories remote error must name both required pin overrides"
LOOKBACK_MEMORY_STORE_REF="reviewed-ref"
LOOKBACK_MEMORY_STORE_REVISION="0123456789abcdef0123456789abcdef01234567"
[[ "$(repo_branch memory-store)" == "${LOOKBACK_MEMORY_STORE_REF}" ]] \
  || fail "Memories ref override must be honoured"
[[ "$(repo_revision memory-store)" == "${LOOKBACK_MEMORY_STORE_REVISION}" ]] \
  || fail "Memories revision override must be honoured"
validate_memory_store_source_config
if (
  export LOOKBACK_MEMORY_STORE_REPO_URL="https://token:secret@example.invalid/LLM-Memories.git"
  export LOOKBACK_MEMORY_STORE_REF=reviewed-ref
  export LOOKBACK_MEMORY_STORE_REVISION=0123456789abcdef0123456789abcdef01234567
  validate_memory_store_source_config
) >/tmp/lookback-credential-url.out 2>/tmp/lookback-credential-url.err; then
  fail "credential-bearing Memories URL overrides must be rejected"
fi
if grep -Fq "secret" /tmp/lookback-credential-url.err; then
  fail "credential validation must not echo embedded URL secrets"
fi
if (
  export LOOKBACK_MEMORY_STORE_REPO_URL="git:secret@gitea.example.invalid/team/LLM-Memories.git"
  export LOOKBACK_MEMORY_STORE_REF=reviewed-ref
  export LOOKBACK_MEMORY_STORE_REVISION=0123456789abcdef0123456789abcdef01234567
  validate_memory_store_source_config
) >/tmp/lookback-ssh-token.out 2>/tmp/lookback-ssh-token.err; then
  fail "nonstandard SSH usernames must not be accepted as source credentials"
fi
if grep -Fq "secret" /tmp/lookback-ssh-token.err; then
  fail "SSH credential validation must not echo embedded URL secrets"
fi
unset LOOKBACK_MEMORY_STORE_REF LOOKBACK_MEMORY_STORE_REVISION LOOKBACK_MEMORY_STORE_REPO_URL

[[ "$(repo_url issue-tracker-server)" == "https://github.com/sutr-app/issue-tracker-server.git" ]] \
|| fail "Issue Tracker server must use its public GitHub repository"
[[ "$(repo_branch issue-tracker-server)" == "v0.0.1" ]] \
|| fail "Issue Tracker server must select its published v0.0.1 tag"
[[ "$(repo_revision issue-tracker-server)" == "5974330a31ae19fb04db99841be73f305958d0d8" ]] \
|| fail "Issue Tracker server must pin the peeled v0.0.1 commit"
[[ "$(repo_ref_kind issue-tracker-server)" == "tag" ]] \
|| fail "the default Issue Tracker release ref must be fetched as a tag"
LOOKBACK_ISSUE_TRACKER_SERVER_REF="reviewed-release"
[[ "$(repo_branch issue-tracker-server)" == "${LOOKBACK_ISSUE_TRACKER_SERVER_REF}" ]] \
  || fail "Issue Tracker server branch override must be honoured"
[[ "$(repo_ref_kind issue-tracker-server)" == "branch" ]] \
  || fail "an explicit Issue Tracker ref override must retain branch selection"
unset LOOKBACK_ISSUE_TRACKER_SERVER_REF

[[ "$(repo_branch jobworkerp)" == "main" ]] \
  || fail "jobworkerp must default to upstream main"
grep -Fq 'repo_branch jobworkerp' "${SCRIPT_DIR}/sync-jobworkerp-vendored.sh" \
  || fail "vendored jobworkerp sync must use the sidecar jobworkerp revision"
LOOKBACK_JOBWORKERP_REF="reviewed-jobworkerp-release"
[[ "$(repo_branch jobworkerp)" == "${LOOKBACK_JOBWORKERP_REF}" ]] \
  || fail "jobworkerp branch override must be honoured"
unset LOOKBACK_JOBWORKERP_REF

# A release without --experimental-issues neither builds nor bundles the
# Issue Tracker server, so packaging must not ask for it.
for only in "" issue-tracker-server; do
  if (ONLY_REPOS="${only}" EXPERIMENTAL_ISSUES=0 want_repo issue-tracker-server); then
    fail "the Issue Tracker server must not be built without --experimental-issues (--only '${only}')"
  fi
done
(ONLY_REPOS="" EXPERIMENTAL_ISSUES=1 want_repo issue-tracker-server) \
  || fail "--experimental-issues must build the Issue Tracker server"
ONLY_REPOS="jobworkerp,llama" SKIP_FRONTEND=0 EXPERIMENTAL_ISSUES=0 require_bundle_issue_tracker_dependencies

if (ONLY_REPOS="jobworkerp,llama" SKIP_FRONTEND=0 EXPERIMENTAL_ISSUES=1 require_bundle_issue_tracker_dependencies) \
  >/tmp/lookback-missing-issue-tracker-server.out 2>/tmp/lookback-missing-issue-tracker-server.err; then
  fail "an --experimental-issues bundle build must reject --only without the Issue Tracker server"
fi
grep -Fq "issue-tracker-server" /tmp/lookback-missing-issue-tracker-server.err \
  || fail "missing Issue Tracker server rejection must name the required repository"

ONLY_REPOS="jobworkerp,llama" SKIP_FRONTEND=1 EXPERIMENTAL_ISSUES=1 require_bundle_issue_tracker_dependencies
ONLY_REPOS="jobworkerp,llama,issue-tracker-server" SKIP_FRONTEND=0 EXPERIMENTAL_ISSUES=1 require_bundle_issue_tracker_dependencies
ONLY_REPOS="none" SKIP_FRONTEND=0 EXPERIMENTAL_ISSUES=1 DRY_RUN=1 require_bundle_issue_tracker_dependencies

incompatible_source="${TMP_ROOT}/incompatible-memories"
mkdir -p "${incompatible_source}"
git -C "${incompatible_source}" init -q -b reviewed-local-ref
git -C "${incompatible_source}" remote add origin "https://github.com/jobworkerp-rs/memory-store.git"
git -C "${incompatible_source}" -c user.name=fixture -c user.email=fixture@example.invalid \
  commit --allow-empty -qm "incompatible fixture"
incompatible_revision=$(git -C "${incompatible_source}" rev-parse HEAD)
if (
  export AGENT_APP="$(cd "${SCRIPT_DIR}/.." && pwd)"
  export WORKDIR="${TMP_ROOT}/build-work"
  export SKIP_CLONE=1
  export LOOKBACK_MEMORY_STORE_SOURCE_DIR="${incompatible_source}"
  export LOOKBACK_MEMORY_STORE_REF=reviewed-local-ref
  export LOOKBACK_MEMORY_STORE_REVISION="${incompatible_revision}"
  clone_or_pull memory-store
) >/tmp/lookback-incompatible-memories.out 2>/tmp/lookback-incompatible-memories.err; then
  fail "an incompatible explicit local Memories source must fail before Cargo build"
fi
grep -Fq "does not implement the expected schema/task contract" /tmp/lookback-incompatible-memories.err \
  || fail "incompatible local Memories source must fail at the migration-contract preflight"

make_fake_remote() {
  local root=$1 public_url=$2 name=$3
  local seed="${root}/${name}-seed" remote="${root}/${name}.git"
  mkdir -p "${root}"
  git init -q --bare "${remote}"
  git init -q -b main "${seed}"
  git -C "${seed}" config user.name fixture
  git -C "${seed}" config user.email fixture@example.invalid
  printf 'release source\n' >"${seed}/source.txt"
  git -C "${seed}" add source.txt
  git -C "${seed}" commit -qm "release source"
  local release_revision
  release_revision=$(git -C "${seed}" rev-parse HEAD)
  git -C "${seed}" tag -a v0.0.19 -m "v0.0.19"
  git -C "${seed}" checkout -q -b reviewed-branch
  printf 'branch addition\n' >>"${seed}/source.txt"
  git -C "${seed}" commit -qam "branch addition"
  git -C "${seed}" tag -a v0.0.1 -m "v0.0.1"
  git -C "${seed}" push -q "${remote}" main reviewed-branch --tags
  printf '%s\n%s\n%s\n' "${remote}" "${public_url}" "${release_revision}"
}

make_fetch_redirect_git() {
  local tools=$1 real_git
  real_git=$(command -v git)
  mkdir -p "${tools}"
  cat >"${tools}/git" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == "-C" && "\$3" == "fetch" && -n "\${PIN_TEST_REMOTE:-}" ]]; then
  export GIT_CONFIG_COUNT=1
  export GIT_CONFIG_KEY_0="url.file://\${PIN_TEST_REMOTE}.insteadOf"
  export GIT_CONFIG_VALUE_0="\${PIN_TEST_URL}"
fi
exec "${real_git}" "\$@"
EOF
  chmod +x "${tools}/git"
}

test_pinned_public_git_ref_checkout() {
  local root="${TMP_ROOT}/public-pins" memories_values issue_values
  memories_values=$(make_fake_remote "${root}" "https://github.com/jobworkerp-rs/memory-store.git" memories)
  local memories_remote memories_url memories_revision
  memories_remote=$(printf '%s\n' "${memories_values}" | sed -n '1p')
  memories_url=$(printf '%s\n' "${memories_values}" | sed -n '2p')
  memories_revision=$(printf '%s\n' "${memories_values}" | sed -n '3p')
  local memories_checkout="${root}/memory-checkout"
  local git_tools="${root}/tools"
  make_fetch_redirect_git "${git_tools}"
  (
    export PATH="${git_tools}:${PATH}"
    export PIN_TEST_REMOTE="${memories_remote}"
    export PIN_TEST_URL="${memories_url}"
    checkout_pinned_dependency "${memories_checkout}" "${memories_url}" \
      v0.0.19 "${memories_revision}" tag 0
    verify_pinned_dependency_checkout "${memories_checkout}" "${memories_url}" \
      v0.0.19 "${memories_revision}" tag
  )
  [[ "$(git -C "${memories_checkout}" rev-parse HEAD)" == "${memories_revision}" ]] \
    || fail "annotated public release tag must check out its peeled commit SHA"
  [[ "$(git -C "${memories_checkout}" cat-file -t refs/tags/v0.0.19)" == tag ]] \
    || fail "the public release selector must retain and verify the annotated tag object"

  issue_values=$(make_fake_remote "${root}" "https://github.com/sutr-app/issue-tracker-server.git" issue)
  local issue_remote issue_url issue_revision issue_branch_revision
  issue_remote=$(printf '%s\n' "${issue_values}" | sed -n '1p')
  issue_url=$(printf '%s\n' "${issue_values}" | sed -n '2p')
  issue_revision=$(printf '%s\n' "${issue_values}" | sed -n '3p')
  local issue_tag_revision issue_tag_checkout
  issue_tag_revision=$(git -C "${root}/issue-seed" rev-parse 'refs/tags/v0.0.1^{commit}')
  issue_tag_checkout="${root}/issue-tag-checkout"
  (
    export PATH="${git_tools}:${PATH}"
    export PIN_TEST_REMOTE="${issue_remote}"
    export PIN_TEST_URL="${issue_url}"
    checkout_pinned_dependency "${issue_tag_checkout}" "${issue_url}" \
      v0.0.1 "${issue_tag_revision}" tag 0
    verify_pinned_dependency_checkout "${issue_tag_checkout}" "${issue_url}" \
      v0.0.1 "${issue_tag_revision}" tag
  )
  [[ "$(git -C "${issue_tag_checkout}" rev-parse HEAD)" == "${issue_tag_revision}" ]] \
    || fail "Issue Tracker v0.0.1 tag must select its peeled release commit"
  issue_branch_revision=$(git -C "${root}/issue-seed" rev-parse reviewed-branch)
  local issue_checkout="${root}/issue-checkout"
  (
    export PATH="${git_tools}:${PATH}"
    export PIN_TEST_REMOTE="${issue_remote}"
    export PIN_TEST_URL="${issue_url}"
    checkout_pinned_dependency "${issue_checkout}" "${issue_url}" \
      reviewed-branch "${issue_revision}" branch 0
    verify_pinned_dependency_checkout "${issue_checkout}" "${issue_url}" \
      reviewed-branch "${issue_revision}" branch
  )
  [[ "$(git -C "${issue_checkout}" rev-parse HEAD)" == "${issue_revision}" ]] \
    || fail "an explicit branch override must still build the pinned SHA, not the branch tip"
  git -C "${issue_checkout}" status --porcelain | grep -q . \
    && fail "a verified pinned checkout must remain clean"
  [[ "${issue_branch_revision}" != "${issue_revision}" ]] \
    || fail "branch fixture must distinguish the selected revision from branch tip"

  if (
    export PATH="${git_tools}:${PATH}"
    export PIN_TEST_REMOTE="${memories_remote}"
    export PIN_TEST_URL="${memories_url}"
    checkout_pinned_dependency "${root}/wrong-sha" "${memories_url}" \
      v0.0.19 "0000000000000000000000000000000000000000" tag 0
  ) >"${TMP_ROOT}/wrong-sha.out" 2>"${TMP_ROOT}/wrong-sha.err"; then
    fail "a public release tag that does not resolve to the pinned SHA must fail"
  fi
  if (
    export PATH="${git_tools}:${PATH}"
    export PIN_TEST_REMOTE="${memories_remote}"
    export PIN_TEST_URL="${memories_url}"
    checkout_pinned_dependency "${root}/missing-tag" "${memories_url}" \
      v9.9.9 "${memories_revision}" tag 0
  ) >"${TMP_ROOT}/missing-tag.out" 2>"${TMP_ROOT}/missing-tag.err"; then
    fail "a missing public tag must fail before compilation"
  fi

  printf 'dirty\n' >>"${memories_checkout}/source.txt"
  if (verify_pinned_dependency_checkout "${memories_checkout}" "${memories_url}" \
    v0.0.19 "${memories_revision}" tag) \
    >"${TMP_ROOT}/dirty.out" 2>"${TMP_ROOT}/dirty.err"; then
    fail "a dirty pinned source checkout must fail"
  fi

  local wrong_origin="${root}/wrong-origin"
  git clone -q "${memories_remote}" "${wrong_origin}"
  git -C "${wrong_origin}" remote set-url origin https://gitea.example.invalid/jobworkerp-rs/memory-store.git
  if (verify_pinned_dependency_checkout "${wrong_origin}" "${memories_url}" \
    v0.0.19 "${memories_revision}" tag) \
    >"${TMP_ROOT}/wrong-origin.out" 2>"${TMP_ROOT}/wrong-origin.err"; then
    fail "a cached private-host clone must not be accepted as a public source"
  fi
  [[ "$(git -C "${wrong_origin}" remote get-url origin)" == \
    "https://gitea.example.invalid/jobworkerp-rs/memory-store.git" ]] \
    || fail "pin validation must not rewrite an existing clone origin"
  git -C "${wrong_origin}" remote set-url origin \
    "https://token:secret@github.com/jobworkerp-rs/memory-store.git"
  if (verify_pinned_dependency_checkout "${wrong_origin}" "${memories_url}" \
    v0.0.19 "${memories_revision}" tag) \
    >"${TMP_ROOT}/credential-origin.out" 2>"${TMP_ROOT}/credential-origin.err"; then
    fail "a credential-bearing cached origin must be rejected"
  fi
  if grep -Fq secret "${TMP_ROOT}/credential-origin.err"; then
    fail "credential-bearing origin failures must not echo the embedded token"
  fi
}

test_cached_local_branch_does_not_shadow_fetched_remote_provenance() {
  local root="${TMP_ROOT}/cached-branch-provenance"
  local public_url="https://github.com/jobworkerp-rs/memory-store.git"
  local remote="${root}/memory-store.git" source="${root}/memory-store"
  local bundle="${root}/migration-bundle" old_revision revision tools
  mkdir -p "${root}"
  git init -q --bare "${remote}"
  git init -q -b reviewed-branch "${source}"
  git -C "${source}" config user.name fixture
  git -C "${source}" config user.email fixture@example.invalid
  printf 'cached old branch state\n' >"${source}/source.txt"
  git -C "${source}" add source.txt
  git -C "${source}" commit -qm 'old cached branch'
  old_revision=$(git -C "${source}" rev-parse HEAD)

  node "${SCRIPT_DIR}/lib/migration-test-fixture.mjs" "${bundle}" valid
  mkdir -p "${source}/infra/atlas/sqlite/migrations" \
    "${source}/infra/atlas/postgres/migrations"
  cp "${bundle}/atlas/post-migration-tasks.json" \
    "${source}/infra/atlas/post-migration-tasks.json"
  cp "${bundle}/atlas/post-migration-task-history.json" \
    "${source}/infra/atlas/post-migration-task-history.json"
  cp "${bundle}/atlas/sqlite/migrations/20260930000001_fixture.sql" \
    "${source}/infra/atlas/sqlite/migrations/"
  cp "${bundle}/atlas/postgres/migrations/20260930000001_fixture.sql" \
    "${source}/infra/atlas/postgres/migrations/"
  git -C "${source}" add infra
  git -C "${source}" commit -qm 'publish migration source on reviewed branch'
  revision=$(git -C "${source}" rev-parse HEAD)
  git -C "${source}" push -q "${remote}" \
    refs/heads/reviewed-branch:refs/heads/reviewed-branch
  git -C "${source}" remote add origin "${public_url}"
  # Model a restored clone whose local branch predates the fetched upstream
  # branch. The checkout remains clean and its origin is already public.
  git -C "${source}" reset -q --hard "${old_revision}"
  tools="${root}/tools"
  make_fetch_redirect_git "${tools}"

  (
    export PATH="${tools}:${PATH}"
    export PIN_TEST_REMOTE="${remote}"
    export PIN_TEST_URL="${public_url}"
    checkout_pinned_dependency "${source}" "${public_url}" \
      reviewed-branch "${revision}" branch 0
    [[ "$(git -C "${source}" rev-parse refs/heads/reviewed-branch)" == "${old_revision}" ]] \
      || fail "the fixture must retain its stale local branch after pinned checkout"
    [[ "$(git -C "${source}" rev-parse refs/remotes/origin/reviewed-branch)" == "${revision}" ]] \
      || fail "the fetched remote branch must point at the requested pinned revision"
    node "${SCRIPT_DIR}/lib/write-migration-source-provenance.mjs" \
      "${source}" "${bundle}" reviewed-branch "${revision}" \
      "${bundle}/test-artifacts/front" "${bundle}/test-artifacts/memories-import" \
      "$(pinned_dependency_ref_name "${source}" reviewed-branch branch)" >/dev/null
  )
  node - "${bundle}/memories-source-provenance.json" "${revision}" <<'NODE'
const fs = require("node:fs");
const [manifestPath, revision] = process.argv.slice(2);
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
if (manifest.source_ref !== "reviewed-branch" || manifest.source_revision !== revision) {
  throw new Error("branch provenance must record the selected branch and fetched pinned SHA");
}
NODE
}

test_host_protobuf_tool_preflight() {
  local tools="${TMP_ROOT}/host-tools"
  mkdir -p "${tools}"
  printf '#!/bin/sh\nexit 0\n' >"${tools}/jq"
  chmod +x "${tools}/jq"
  if (PATH="${tools}" require_host_protobuf_tools) \
    >"${TMP_ROOT}/missing-protoc.out" 2>"${TMP_ROOT}/missing-protoc.err"; then
    fail "a missing host protoc must fail preflight"
  fi
  grep -Fq "host protoc is required" "${TMP_ROOT}/missing-protoc.err" \
    || fail "the missing protoc error must explain the host build requirement"
  printf '#!/bin/sh\nexit 0\n' >"${tools}/protoc"
  chmod +x "${tools}/protoc"
  rm "${tools}/jq"
  if (PATH="${tools}" require_host_protobuf_tools) \
    >"${TMP_ROOT}/missing-jq.out" 2>"${TMP_ROOT}/missing-jq.err"; then
    fail "a missing host jq must fail preflight"
  fi
  grep -Fq "host jq is required" "${TMP_ROOT}/missing-jq.err" \
    || fail "the missing jq error must explain the host build requirement"
  printf '#!/bin/sh\nexit 0\n' >"${tools}/jq"
  require_host_protobuf_tools
}

test_release_preflight_requires_host_protoc_before_build() {
  local tools="${TMP_ROOT}/missing-protoc-tools" build_log="${TMP_ROOT}/build-invocations.log"
  mkdir -p "${tools}"
  : >"${build_log}"
  local command
  for command in git cargo rustc node pnpm cmake pkg-config curl unzip tar; do
    cat >"${tools}/${command}" <<EOF
#!/usr/bin/env bash
printf '%s\\n' "${command} \\$*" >>"${build_log}"
EOF
    chmod +x "${tools}/${command}"
  done
  ln -s "$(command -v dirname)" "${tools}/dirname"
  if PATH="${tools}" /bin/bash "${SCRIPT_DIR}/build-release.sh" \
    --platform linux --gpu cpu --bundle deb --skip-frontend \
    >"${TMP_ROOT}/release-missing-protoc.out" 2>"${TMP_ROOT}/release-missing-protoc.err"; then
    fail "release preflight must stop when host protoc is unavailable"
  fi
  grep -Fq "host protoc is required" "${TMP_ROOT}/release-missing-protoc.err" \
    || fail "release preflight must explain the missing host protoc requirement"
  [[ ! -s "${build_log}" ]] \
    || fail "missing host protoc must be detected before source fetching or building"
}

test_pinned_public_git_ref_checkout
test_cached_local_branch_does_not_shadow_fetched_remote_provenance
test_host_protobuf_tool_preflight
test_release_preflight_requires_host_protoc_before_build

echo "build dependency feature tests passed"
