#!/usr/bin/env bash
# Runs the release acceptance gate for Lookback's private Issue Tracker gRPC server.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_APP="$(cd "${SCRIPT_DIR}/.." && pwd)"

required=(
  LOOKBACK_ISSUE_TRACKER_SERVER_BIN
  LOOKBACK_SIDECAR_BIN_DIR
)

for name in "${required[@]}"; do
  if [[ -z "${!name:-}" ]]; then
    echo "${name} is required for the Issue Tracker gRPC E2E gate" >&2
    exit 2
  fi
done

cd "${AGENT_APP}"
test_list="$(cargo test -p lookback-tauri --test issue_tracker_grpc_e2e -- --list)"
for test_name in \
  phase5_6_primary_and_gateway_endpoints_enforce_the_v3_boundary \
  phase5_6_apply_extraction_and_gateway_claim_lifecycle_are_idempotent \
  rejected_candidate_is_durably_confirmed_by_the_client \
  phase5_6_dynamic_grpc_runner_records_issue_extract_failures_through_reflection
do
  if ! grep -Fq "${test_name}: test" <<<"${test_list}"; then
    echo "required Phase 5/6 Issue Tracker acceptance test is not registered: ${test_name}" >&2
    exit 1
  fi
done

sidecar_test_name=issue_tracker_toggle_controls_child_status_and_endpoint_environment
sidecar_test_list="$(cargo test -p lookback-tauri --test sidecar_lifecycle -- --list)"
if ! grep -Fq "${sidecar_test_name}: test" <<<"${sidecar_test_list}"; then
  echo "required sidecar lifecycle acceptance test is not registered: ${sidecar_test_name}" >&2
  exit 1
fi

cargo test -p lookback-tauri --test issue_tracker_grpc_e2e -- --ignored --nocapture
cargo test -p lookback-tauri issue_tracker_sidecar_maintenance_recovers_after_server_restart --lib -- --ignored --nocapture
cargo test -p lookback-tauri --test sidecar_lifecycle "${sidecar_test_name}" -- --ignored --nocapture
cargo test -p lookback-tauri --test issue_tracker_mcp_e2e -- --list | grep -Fq 'mcp_exposes_lookback_issue_claim_lifecycle_through_gateway_workers: test'
cargo test -p lookback-tauri --test issue_tracker_mcp_e2e -- --ignored --nocapture
