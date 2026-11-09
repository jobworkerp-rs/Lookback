#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_APP="$(cd "${SCRIPT_DIR}/.." && pwd)"
SCRIPT="${SCRIPT_DIR}/test-issue-tracker-grpc-e2e.sh"

TMP_ROOT="$(mktemp -d)"
cleanup() {
  rm -rf "${TMP_ROOT}"
}
trap cleanup EXIT

fake_bin="${TMP_ROOT}/bin"
mkdir -p "${fake_bin}"
cat >"${fake_bin}/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$PWD" >"${LOOKBACK_TEST_CARGO_PWD:?}"
printf '%s\n' "$*" >>"${LOOKBACK_TEST_CARGO_ARGS:?}"
if [[ "$*" == *"-- --list" ]]; then
  printf '%s\n' \
    'phase5_6_primary_and_gateway_endpoints_enforce_the_v3_boundary: test' \
    'phase5_6_apply_extraction_and_gateway_claim_lifecycle_are_idempotent: test' \
    'rejected_candidate_is_durably_confirmed_by_the_client: test' \
    'phase5_6_dynamic_grpc_runner_records_issue_extract_failures_through_reflection: test' \
    'mcp_exposes_lookback_issue_claim_lifecycle_through_gateway_workers: test' \
    'issue_tracker_toggle_controls_child_status_and_endpoint_environment: test'
fi
EOF
chmod +x "${fake_bin}/cargo"

marker_pwd="${TMP_ROOT}/cargo-pwd"
marker_args="${TMP_ROOT}/cargo-args"
outside_dir="${TMP_ROOT}/outside"
mkdir -p "${outside_dir}"

(
  cd "${outside_dir}"
  PATH="${fake_bin}:${PATH}" \
    LOOKBACK_TEST_CARGO_PWD="${marker_pwd}" \
    LOOKBACK_TEST_CARGO_ARGS="${marker_args}" \
    LOOKBACK_ISSUE_TRACKER_SERVER_BIN="${TMP_ROOT}/issue-tracker-server" \
    LOOKBACK_SIDECAR_BIN_DIR="${TMP_ROOT}/sidecars" \
    bash "${SCRIPT}"
)

[[ "$(<"${marker_pwd}")" == "${AGENT_APP}" ]] || {
  echo "expected cargo to run from the agent-app root" >&2
  exit 1
}
grep -Fxq -- 'test -p lookback-tauri --test issue_tracker_grpc_e2e -- --ignored --nocapture' "${marker_args}" \
  || { echo "expected Issue Tracker gRPC E2E cargo invocation" >&2; exit 1; }
grep -Fxq -- 'test -p lookback-tauri --test issue_tracker_grpc_e2e -- --list' "${marker_args}" \
  || { echo "expected Issue Tracker acceptance-test registration check" >&2; exit 1; }
grep -Fxq -- 'test -p lookback-tauri --test sidecar_lifecycle -- --list' "${marker_args}" \
  || { echo "expected sidecar lifecycle acceptance-test registration check" >&2; exit 1; }
grep -Fxq -- 'test -p lookback-tauri --test sidecar_lifecycle issue_tracker_toggle_controls_child_status_and_endpoint_environment -- --ignored --nocapture' "${marker_args}" \
  || { echo "expected Issue Tracker toggle lifecycle cargo invocation" >&2; exit 1; }
grep -Fxq -- 'test -p lookback-tauri --test issue_tracker_mcp_e2e -- --ignored --nocapture' "${marker_args}" \
  || { echo "expected Issue Tracker MCP E2E cargo invocation" >&2; exit 1; }
grep -Fxq -- 'test -p lookback-tauri --test issue_tracker_mcp_e2e -- --list' "${marker_args}" \
  || { echo "expected MCP acceptance-test registration check" >&2; exit 1; }

echo "Issue Tracker gRPC E2E script tests passed"
