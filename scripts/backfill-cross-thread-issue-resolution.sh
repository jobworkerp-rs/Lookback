#!/usr/bin/env bash
# Run the cross-thread Issue resolution over the review backlog by hand.
#
# Automatic runs only look at what changed since the last clean run and only
# once `cross_thread_resolution_mode` is APPLY. This script starts the manual
# worker over every candidate (or those changed since --since) so the backlog
# can be judged before the automatic runs are switched on. It is a dry run
# unless --apply is given: the workflow judges every candidate and reports
# what it would write. The manual worker never moves the automatic runs'
# position.
#
# --limit N judges only the first N candidates in range and --candidate-ids
# FILE (one ID per line) only the listed ones; try a change on a few first.
#
# The full workflow output is saved next to the app data as
# backfill-cross-thread-issue-resolution-<time>.json for evaluation.
#
# Requirements: the app (or its sidecars) must be running, plus grpcurl, jq,
# ss, and jobworkerp-client on PATH. The run waits for its result.
#
# Usage:
#   scripts/backfill-cross-thread-issue-resolution.sh [--data-dir DIR] [--apply]
#       [--since YYYY-MM-DD|EPOCH_MS] [--user-id N] [--search-limit N]
#       [--score-min X] [--limit N] [--candidate-ids FILE]
#       [--tracker-port PORT] [--jobworkerp-address URL]
#       [--memories-url URL] [--timeout-sec SEC]
set -euo pipefail

DATA_DIR="${LOOKBACK_DATA_DIR:-${HOME}/.local/share/lookback}"
APPLY=0
SINCE=""
USER_ID=1
SEARCH_LIMIT=""
SCORE_MIN=""
LIMIT=0
CANDIDATE_IDS_FILE=""
TRACKER_PORT=""
JOBWORKERP_ADDRESS=""
MEMORIES_URL=""
TIMEOUT_SEC=86400
WORKER_NAME="lookback-cross-thread-issue-resolution-manual"

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data-dir) DATA_DIR=$2; shift 2 ;;
    --apply) APPLY=1; shift ;;
    --since) SINCE=$2; shift 2 ;;
    --user-id) USER_ID=$2; shift 2 ;;
    --search-limit) SEARCH_LIMIT=$2; shift 2 ;;
    --score-min) SCORE_MIN=$2; shift 2 ;;
    --limit) LIMIT=$2; shift 2 ;;
    --candidate-ids) CANDIDATE_IDS_FILE=$2; shift 2 ;;
    --tracker-port) TRACKER_PORT=$2; shift 2 ;;
    --jobworkerp-address) JOBWORKERP_ADDRESS=$2; shift 2 ;;
    --memories-url) MEMORIES_URL=$2; shift 2 ;;
    --timeout-sec) TIMEOUT_SEC=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# shellcheck source=lib/lookback-sidecar.sh source-path=SCRIPTDIR
source "$(dirname "${BASH_SOURCE[0]}")/lib/lookback-sidecar.sh"
require_tools grpcurl jq ss jobworkerp-client
lookback_sidecar_init

DRY_RUN=true
if [[ "${APPLY}" -eq 1 ]]; then
  DRY_RUN=false
  require_resolution_apply "applying"
fi

SINCE_MS=0
if [[ -n "${SINCE}" ]]; then
  if [[ "${SINCE}" =~ ^[0-9]+$ ]]; then
    SINCE_MS=${SINCE}
  else
    since_sec=$(date -d "${SINCE}" +%s) || die "--since must be a date such as 2026-10-01 or epoch ms"
    SINCE_MS=$((since_sec * 1000))
  fi
fi

CANDIDATE_IDS='[]'
if [[ -n "${CANDIDATE_IDS_FILE}" ]]; then
  CANDIDATE_IDS=$(grep -v '^[[:space:]]*$' "${CANDIDATE_IDS_FILE}" | jq -R . | jq -cs .)
fi

input=$(jq -cn --argjson common "$(common_worker_input)" --argjson user "${USER_ID}" \
  --argjson since "${SINCE_MS}" --argjson dry_run "${DRY_RUN}" \
  --arg policy "${POLICY_JSON}" --arg limit "${SEARCH_LIMIT}" --arg score "${SCORE_MIN}" \
  --argjson max "${LIMIT}" --argjson ids "${CANDIDATE_IDS}" '
  $common + { mode: "manual", user_id: $user, since_ms: $since, dry_run: $dry_run, policy_json: $policy,
              max_candidates: $max, candidate_ids: $ids }
  + (if $limit == "" then {} else { search_limit: ($limit | tonumber) } end)
  + (if $score == "" then {} else { search_score_min: ($score | tonumber) } end)')

echo "# cross-thread resolution (dry_run=${DRY_RUN}) resolution since ${SINCE_MS} (jobworkerp ${JOBWORKERP_ADDRESS}, tracker ${TRACKER_ADDR}, llm ${LLM_WORKER})" >&2
client_output=$(run_workflow_job "${WORKER_NAME}" "${input}") || true
output=$(workflow_output_json "${client_output}")
if [[ -z "${output}" ]] || ! jq -e . <<<"${output}" >/dev/null 2>&1; then
  printf '%s\n' "${client_output}" | sed 's/^/    /'
  die "the workflow returned no parsable output"
fi
saved="${DATA_DIR}/backfill-cross-thread-issue-resolution-$(date -u +%Y%m%dT%H%M%SZ).json"
printf '%s\n' "${output}" >"${saved}"

echo "# results (full output: ${saved})"
jq -r '
  "  completed: \(.completed)",
  "  candidates: \(.candidate_count) in range: \(.scoped_count) without a matching thread: \(.unmatched_count)",
  "  dry_run: \(.dry_run_count) applied: \(.applied_count) kept: \(.kept_count) contradicted: \(.contradicted_count)",
  "  protected: \(.protected_count) skipped: \(.skipped_count) conflicted: \(.conflicted_count) failed: \(.failed_count)"' <<<"${output}"
print_list() {
  local title=$1 filter=$2 lines
  lines=$(jq -r "${filter}" <<<"${output}")
  [[ -n "${lines}" ]] || return 0
  echo "# ${title}"
  printf '%s\n' "${lines}" | sed 's/^/  /'
}
print_list "judged (candidate outcome search hits=thread:score verdicts=thread:kind/decision/confidence title)" \
  '.evaluations[]? | "\(.candidate_id) \(.outcome) \(.search_mode) hits=\(.hits | map("\(.thread_id):\(.score * 1000 | round / 1000)") | join(",")) verdicts=\(.verdicts | map("\(.thread_id):\(.kind)/\(.decision // "-")/\(.confidence // "-")") | join(",")) \(.title | .[0:60])"'
print_list "would write (candidate decision threads memories)" \
  '.dry_run_candidates[]? | "\(.candidate_id) \(.decision) threads=\(.evidence_thread_ids | join(",")) memories=\(.evidence_memory_ids | join(","))"'
print_list "written (candidate decision operation)" \
  '.applied_candidates[]? | "\(.candidate_id) \(.decision) \(.operation_id)"'
print_list "contradicting judgements (candidate decisions threads)" \
  '.contradicted_candidates[]? | "\(.candidate_id) \(.decisions | join(",")) threads=\(.thread_ids | join(","))"'
print_list "failed (candidate source reason)" \
  '.failed_candidates[]? | "\(.candidate_id) \(.source) \(.reason | tostring | .[0:200])"'
