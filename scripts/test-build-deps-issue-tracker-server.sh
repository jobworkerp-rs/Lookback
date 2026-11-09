#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

# shellcheck source=lib/build-common.sh
source "${SCRIPT_DIR}/lib/build-common.sh"
# shellcheck source=lib/build-deps.sh
source "${SCRIPT_DIR}/lib/build-deps.sh"

# stage_binaries normally fetches protoc. This unit test isolates the Issue
# Tracker binary staging contract and does not exercise the fetcher.
fetch_protoc_bin() { :; }

test_stages_issue_tracker_server_binary() {
  local source="${TMP_DIR}/work/issue-tracker-server/target/release/issue-tracker-server"
  BIN_DIR="${TMP_DIR}/bin"
  WORKDIR="${TMP_DIR}/work"
  TRIPLE=test-triple
  GPU=cpu
  ONLY_REPOS=issue-tracker-server
  SKIP_FRONTEND=1
  mkdir -p "$(dirname "${source}")"
  printf 'issue-tracker-server\n' >"${source}"

  stage_binaries

  cmp "${source}" "${BIN_DIR}/issue-tracker-server-${TRIPLE}"
}

test_builds_server_from_front_crate() {
  local cargo_dir="${TMP_DIR}/bin" cargo_log="${TMP_DIR}/cargo.log"
  WORKDIR="${TMP_DIR}/build-work"
  mkdir -p "${cargo_dir}" "${WORKDIR}/issue-tracker-server"
  : >"${WORKDIR}/issue-tracker-server/Cargo.toml"
  cat >"${cargo_dir}/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${CARGO_LOG}"
EOF
  chmod +x "${cargo_dir}/cargo"

  CARGO_LOG="${cargo_log}" PATH="${cargo_dir}:${PATH}" build_issue_tracker_server

  grep -Fxq -- 'build --release -p issue-tracker-front --bin issue-tracker-server' "${cargo_log}"
}

test_stages_issue_tracker_server_binary
test_builds_server_from_front_crate

echo "issue tracker server staging tests passed"
