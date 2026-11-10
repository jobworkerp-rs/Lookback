#!/usr/bin/env bash
# Release builds ship the Issue Tracker server, enable its Cargo feature and
# run its E2E gate only with --experimental-issues.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

dry_run_plan() {
  local plan=$1
  shift
  DRY_RUN=1 bash "${SCRIPT_DIR}/build-release.sh" --profile mac \
    --only none --no-submodule-update --workdir "${TMP_DIR}/deps" "$@" >"${plan}" 2>&1
}

gate_line() {
  grep -F "bash ${SCRIPT_DIR}/test-issue-tracker-grpc-e2e.sh" "$1" || true
}

test_e2e_gate_runs_only_when_the_issue_tracker_is_bundled() {
  local plan="${TMP_DIR}/default.log" gate expected
  local bin_dir="${ROOT_DIR}/src-tauri/bin"
  dry_run_plan "${plan}"
  [[ -z "$(gate_line "${plan}")" ]] || fail "the default release must not run the Issue Tracker E2E gate"

  plan="${TMP_DIR}/experimental.log"
  dry_run_plan "${plan}" --experimental-issues
  # The gate fails without both variables, so the dry run must show the exact
  # command a release runs.
  gate=$(gate_line "${plan}")
  [[ -n "${gate}" ]] || fail "--experimental-issues must run the Issue Tracker E2E gate"
  for expected in \
    "LOOKBACK_ISSUE_TRACKER_SERVER_BIN=${bin_dir}/issue-tracker-server-" \
    "LOOKBACK_SIDECAR_BIN_DIR=${bin_dir} "; do
    [[ "${gate}" == *"${expected}"* ]] || fail "the Issue Tracker E2E gate must pass ${expected}"
  done
}

# Load build-release.sh without running the pipeline and call one step of it.
# Sourcing parses the positional parameters as build-release options.
with_build_release() {
  local experimental=$1 step=$2
  (
    set --
    # shellcheck source=build-release.sh
    source "${SCRIPT_DIR}/build-release.sh"
    EXPERIMENTAL_ISSUES=${experimental}
    "${step}"
  )
}

finalizer_args() {
  local experimental=$1 args_log=$2
  # shellcheck disable=SC2329 # invoked indirectly through with_build_release
  call_finalizer() {
    run() { printf '%s\n' "$@" >"${args_log}"; }
    PLATFORM=mac BUNDLE=dmg,app TRIPLE=aarch64-apple-darwin finalize_macos_release_bundle
  }
  with_build_release "${experimental}" call_finalizer
}

test_macos_finalizer_expects_the_issue_tracker_only_when_bundled() {
  local args_log="${TMP_DIR}/finalizer-default.args"
  finalizer_args 0 "${args_log}"
  ! grep -Fxq -- '--with-issue-tracker' "${args_log}" \
    || fail "the default macOS release must not require the Issue Tracker server"

  args_log="${TMP_DIR}/finalizer-experimental.args"
  finalizer_args 1 "${args_log}"
  grep -Fxq -- '--with-issue-tracker' "${args_log}" \
    || fail "an --experimental-issues macOS release must sign and verify the Issue Tracker server"
}

# Run the real command assembly against a pnpm stub, one argument per line, so
# the JSON handed to --config is checked as Tauri receives it.
tauri_args() {
  local experimental=$1 args_log=$2 tools="${TMP_DIR}/tools"
  mkdir -p "${tools}"
  # The stub expands its own arguments when it runs, not here.
  # shellcheck disable=SC2016
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$@" >"${TAURI_ARGS_LOG}"\n' >"${tools}/pnpm"
  chmod +x "${tools}/pnpm"
  # shellcheck disable=SC2329 # invoked indirectly through with_build_release
  call_tauri() {
    PLATFORM=linux
    tauri_bundles=deb
    set_issue_tracker_tauri_args
    PATH="${tools}:${PATH}" TAURI_ARGS_LOG="${args_log}" run_tauri_build ""
  }
  with_build_release "${experimental}" call_tauri
}

test_tauri_bundles_the_issue_tracker_only_with_the_feature() {
  local args_log="${TMP_DIR}/tauri-default.args"
  tauri_args 0 "${args_log}"
  ! grep -Fxq -- '--features' "${args_log}" || fail "the default release must not enable experimental-issues"
  node - "${args_log}" "${ROOT_DIR}/src-tauri/tauri.conf.json" <<'EOF' || fail "the default release must bundle every sidecar except the Issue Tracker server"
const fs = require("node:fs");
const [argsLog, confPath] = process.argv.slice(2);
const args = fs.readFileSync(argsLog, "utf8").trimEnd().split("\n");
const at = args.indexOf("--config");
if (at < 0) throw new Error(`no --config in ${args.join(" ")}`);
const bundled = JSON.parse(args[at + 1]).bundle.externalBin;
const configured = require(confPath).bundle.externalBin;
const server = "bin/issue-tracker-server";
if (!configured.includes(server)) throw new Error("tauri.conf.json no longer lists the Issue Tracker server");
if (bundled.includes(server)) throw new Error("the Issue Tracker server is still bundled");
if (bundled.length !== configured.length - 1 || !bundled.every((bin) => configured.includes(bin))) {
  throw new Error(`expected every other sidecar of ${JSON.stringify(configured)}, got ${JSON.stringify(bundled)}`);
}
EOF

  args_log="${TMP_DIR}/tauri-experimental.args"
  tauri_args 1 "${args_log}"
  if ! grep -Fxq -- '--features' "${args_log}" || ! grep -Fxq 'experimental-issues' "${args_log}"; then
    fail "--experimental-issues must pass --features experimental-issues to Tauri"
  fi
  ! grep -Fxq -- '--config' "${args_log}" || fail "--experimental-issues must bundle the full externalBin list"
}

test_e2e_gate_runs_only_when_the_issue_tracker_is_bundled
test_macos_finalizer_expects_the_issue_tracker_only_when_bundled
test_tauri_bundles_the_issue_tracker_only_with_the_feature

echo "build-release Issue Tracker opt-in tests passed"
