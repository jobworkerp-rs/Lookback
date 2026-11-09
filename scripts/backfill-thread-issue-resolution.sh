#!/usr/bin/env bash
# Re-evaluate existing Issue candidates against their source threads.
#
# The automatic same-thread resolution only runs when a thread summary is
# saved with messages newer than its checkpoint, so candidates created while
# the resolution workflow was broken are never revisited on their own. This
# script lists every source thread that still has a review candidate and
# enqueues the manual re-evaluation worker for each one, sequentially, with the
# same inputs the app's "re-evaluate Issue resolution" button sends.
#
# A saved summary without a message checkpoint cannot be re-evaluated. Old
# summaries whose thread has no newer message are skipped by a normal
# summary run, so --refresh-summaries force-resummarizes exactly the listed
# threads through memories-summarize-batch (with Issue extraction turned
# off: the point is to write the checkpoint, not to mint candidates). Rerun
# with --thread-ids on the same list afterwards.
#
# Requirements: the app (or its sidecars) must be running, plus grpcurl, jq,
# ss, and jobworkerp-client on PATH. Each job waits for its result, so a run
# over hundreds of threads takes a while; use --limit to work in slices.
#
# Usage:
#   scripts/backfill-thread-issue-resolution.sh [--data-dir DIR] [--dry-run]
#       [--limit N] [--thread-ids FILE] [--newest-first] [--candidates-since DATE]
#       [--include-done] [--progress-file FILE] [--tracker-port PORT]
#       [--jobworkerp-address URL] [--memories-url URL] [--timeout-sec SEC]
#   scripts/backfill-thread-issue-resolution.sh --refresh-summaries FILE
#       [--user-id N] [--dry-run] [common options]
#
# Progress: every thread result is appended to <data-dir>/backfill-thread-
# issue-resolution.progress ("<thread_id> <status> <iso time>"). Threads whose
# last recorded status is COMPLETE or UNCONFIRMED are skipped on later runs,
# so repeating `--limit N` walks through the backlog; FAILED threads are
# retried. --include-done disables the skip.
#
# Ordering: source threads come out in ascending thread-ID order. With
# --newest-first they are ordered by the creation time of their newest review
# candidate instead, and --candidates-since YYYY-MM-DD keeps only threads that
# have at least one candidate created on or after that local date, so the most
# recently filed candidates can be re-evaluated before older ones.
set -euo pipefail

DATA_DIR="${LOOKBACK_DATA_DIR:-${HOME}/.local/share/lookback}"
DRY_RUN=0
LIMIT=0
THREAD_IDS_FILE=""
TRACKER_PORT=""
JOBWORKERP_ADDRESS=""
MEMORIES_URL=""
# One forced summary batch over dozens of threads runs each LLM call in
# sequence, so the default wait is generous for both modes.
TIMEOUT_SEC=21600
REFRESH_FILE=""
USER_ID=1
NEWEST_FIRST=0
CANDIDATES_SINCE=""
INCLUDE_DONE=0
PROGRESS_FILE=""
WORKER_NAME="lookback-thread-issue-resolution-manual"
SUMMARY_BATCH_WORKER="memories-summarize-batch"

usage() {
  # The header comment above `set -euo pipefail` is the help text.
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data-dir) DATA_DIR=$2; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --limit) LIMIT=$2; shift 2 ;;
    --thread-ids) THREAD_IDS_FILE=$2; shift 2 ;;
    --tracker-port) TRACKER_PORT=$2; shift 2 ;;
    --jobworkerp-address) JOBWORKERP_ADDRESS=$2; shift 2 ;;
    --memories-url) MEMORIES_URL=$2; shift 2 ;;
    --timeout-sec) TIMEOUT_SEC=$2; shift 2 ;;
    --refresh-summaries) REFRESH_FILE=$2; shift 2 ;;
    --newest-first) NEWEST_FIRST=1; shift ;;
    --candidates-since) CANDIDATES_SINCE=$2; shift 2 ;;
    --include-done) INCLUDE_DONE=1; shift ;;
    --progress-file) PROGRESS_FILE=$2; shift 2 ;;
    --user-id) USER_ID=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# shellcheck source=lib/lookback-sidecar.sh source-path=SCRIPTDIR
source "$(dirname "${BASH_SOURCE[0]}")/lib/lookback-sidecar.sh"
require_tools grpcurl jq ss jobworkerp-client
PROGRESS_FILE=${PROGRESS_FILE:-${DATA_DIR}/backfill-thread-issue-resolution.progress}

# Threads whose newest progress entry is a settled status. FAILED is left out
# on purpose so a fixed summary or workflow gets another attempt.
done_threads() {
  [[ -f "${PROGRESS_FILE}" ]] || return 0
  awk '{ last[$1] = $2 } END { for (t in last) if (last[t] == "COMPLETE" || last[t] == "UNCONFIRMED") print t }' "${PROGRESS_FILE}" | sort -n
}

# --- settings the app would forward -------------------------------------------
lookback_sidecar_init
require_resolution_apply "backfilling"

# --- summary refresh mode ------------------------------------------------------
refresh_summaries() {
  local threads thread_json policy input client_output output failed_count
  threads=$(grep -E '^[0-9]+$' "${REFRESH_FILE}" || true)
  [[ -n "${threads}" ]] || die "no thread IDs in ${REFRESH_FILE}"
  thread_json=$(printf '%s\n' "${threads}" | jq -R . | jq -cs .)
  # Checkpoints are written by the summary itself; extraction would only
  # re-mint candidates for threads whose candidates already exist.
  policy=$(jq -c '. + { extraction_enabled: false }' <<<"${POLICY_JSON}")
  input=$(jq -cn --argjson common "$(common_worker_input)" --argjson user_id "${USER_ID}" \
    --argjson thread_ids "${thread_json}" --arg policy "${policy}" '
    $common + { user_id: $user_id, thread_ids: $thread_ids, force_resummarize: true, policy_json: $policy }')
  echo "# force re-summarizing $(printf '%s\n' "${threads}" | grep -c .) threads via ${SUMMARY_BATCH_WORKER} (jobworkerp ${JOBWORKERP_ADDRESS}, llm ${LLM_WORKER})" >&2
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    printf '%s\n' "${input}"
    return 0
  fi
  client_output=$(run_workflow_job "${SUMMARY_BATCH_WORKER}" "${input}") \
    || echo "  summary batch enqueue failed" >&2
  output=$(workflow_output_json "${client_output}")
  if [[ -z "${output}" ]] || ! jq -e . <<<"${output}" >/dev/null 2>&1; then
    printf '%s\n' "${client_output}" | sed 's/^/    /'
    die "summary batch returned no parsable output"
  fi
  echo "# summary refresh results"
  echo "  generated: $(jq -r '.generated_threads // [] | length' <<<"${output}")"
  echo "  skipped: $(jq -r '.skipped_threads // [] | length' <<<"${output}") (a skipped thread means force did not apply; check its summary metadata)"
  failed_count=$(jq -r '.failed_threads // [] | length' <<<"${output}")
  echo "  failed: ${failed_count}"
  if [[ "${failed_count}" -gt 0 ]]; then
    echo "# failed threads (thread_id error):"
    jq -r '.failed_threads[] | if type == "object" then "\(.thread_id // "?") \(.error // .message // "")" else tostring end' <<<"${output}"
  fi
  if [[ $(jq -r '.last_error // "" | length' <<<"${output}") -gt 0 ]]; then
    echo "# last_error: $(jq -r '.last_error' <<<"${output}" | cut -c1-400)"
  fi
}

if [[ -n "${REFRESH_FILE}" ]]; then
  refresh_summaries
  exit 0
fi

# --- source threads -------------------------------------------------------------
list_source_threads() {
  local contract schema fingerprint data_root token body page rows_threads
  contract=$(grpcurl -plaintext -d '{}' "${TRACKER_ADDR}" issue_tracker.rpc.v1.ManagerInfoService/GetContractInfo)
  schema=$(jq -r '.success.schemaFingerprint // .success.schema_fingerprint // empty' <<<"${contract}")
  fingerprint=$(jq -r '.success.contractFingerprint // .success.contract_fingerprint // empty' <<<"${contract}")
  data_root=$(jq -r '[.success.storeIdentities // .success.store_identities // [] | .[]
      | select((.role // "") | tostring | test("MANAGER"))][0]
      | .dataRootId // .data_root_id // empty' <<<"${contract}")
  [[ -n "${schema}" && -n "${fingerprint}" && -n "${data_root}" ]] \
    || die "GetContractInfo did not return a manager store identity: ${contract}"

  local since_ms=0
  if [[ -n "${CANDIDATES_SINCE}" ]]; then
    since_ms=$(( $(date -d "${CANDIDATES_SINCE}" +%s) * 1000 )) \
      || die "--candidates-since must be a date such as 2026-10-01"
  fi
  token=""
  rows_threads=""
  while :; do
    body=$(jq -cn --arg schema "${schema}" --arg fp "${fingerprint}" --arg root "${data_root}" --arg token "${token}" '
      { context: { correlation_id: "backfill-thread-issue-resolution",
                   schema_fingerprint: $schema, data_root_id: $root, contract_fingerprint: $fp },
        page: ({ page_size: 100 } + (if $token == "" then {} else { page_token: $token } end)),
        filter: {} }')
    page=$(grpcurl -plaintext -d "${body}" "${TRACKER_ADDR}" issue_tracker.rpc.v1.ManagerConsumerService/ListReviewCandidates)
    jq -e '.success' <<<"${page}" >/dev/null || die "ListReviewCandidates failed: ${page}"
    # One "<thread_id> <candidate_created_at_ms>" line per source reference.
    rows_threads+=$(jq -r '.success.rows // [] | .[]
        | ((.candidate.createdAt.unixEpochMillis // .candidate.created_at.unix_epoch_millis // 0) | tonumber) as $created
        | .candidate.sourceEvidence // .candidate.source_evidence // [] | .[]
        | "\(.threadId // .thread_id // empty | tostring) \($created)"' <<<"${page}")$'\n'
    token=$(jq -r '.success.page.nextPageToken // .success.page.next_page_token // empty' <<<"${page}")
    [[ -n "${token}" ]] || break
  done
  # Reduce to one line per thread carrying its newest candidate time, then
  # apply the date filter and the requested order.
  printf '%s' "${rows_threads}" | grep -E '^[0-9]+ [0-9]+$' \
    | sort -k1,1n -k2,2nr | sort -s -u -k1,1n \
    | awk -v since="${since_ms}" '$2 >= since { print }' \
    | if [[ "${NEWEST_FIRST}" -eq 1 ]]; then sort -k2,2nr -k1,1nr; else sort -k1,1n; fi \
    | awk '{ print $1 }'
}

if [[ -n "${THREAD_IDS_FILE}" ]]; then
  THREADS=$(grep -E '^[0-9]+$' "${THREAD_IDS_FILE}" || true)
else
  THREADS=$(list_source_threads)
fi
if [[ "${INCLUDE_DONE}" -eq 0 ]]; then
  DONE=$(done_threads)
  if [[ -n "${DONE}" ]]; then
    BEFORE=$(printf '%s\n' "${THREADS}" | grep -c . || true)
    THREADS=$(printf '%s\n' "${THREADS}" | grep -vxF -f <(printf '%s\n' "${DONE}") || true)
    AFTER=$(printf '%s\n' "${THREADS}" | grep -c . || true)
    echo "# skipped $((BEFORE - AFTER)) threads already COMPLETE/UNCONFIRMED in ${PROGRESS_FILE} (--include-done to redo)" >&2
  fi
fi
if [[ "${LIMIT}" -gt 0 ]]; then
  THREADS=$(printf '%s\n' "${THREADS}" | head -n "${LIMIT}")
fi
THREAD_COUNT=$(printf '%s\n' "${THREADS}" | grep -c . || true)

echo "# source threads: ${THREAD_COUNT} (tracker ${TRACKER_ADDR}, jobworkerp ${JOBWORKERP_ADDRESS}, llm ${LLM_WORKER})" >&2
if [[ "${DRY_RUN}" -eq 1 ]]; then
  printf '%s\n' "${THREADS}" | grep . || true
  exit 0
fi

# --- enqueue -------------------------------------------------------------------
index=0
declare -A STATUS_COUNTS=()
NEEDS_SUMMARY_REFRESH=()
ENQUEUE_FAILED=()

# The manual workflow prints its result as `output: {json}`; keep one line per
# thread so a long run stays readable and summarize at the end.
report_result() {
  local thread_id=$1 client_output=$2 output status reason
  output=$(workflow_output_json "${client_output}")
  if [[ -z "${output}" ]] || ! status=$(jq -er '.status // empty' <<<"${output}" 2>/dev/null); then
    ENQUEUE_FAILED+=("${thread_id}")
    STATUS_COUNTS[NO_RESULT]=$(( ${STATUS_COUNTS[NO_RESULT]:-0} + 1 ))
    echo "  thread ${thread_id} -> NO_RESULT"
    printf '%s\n' "${client_output}" | sed 's/^/    /'
    return
  fi
  reason=$(jq -r '.failure_reason // ""' <<<"${output}")
  STATUS_COUNTS[${status}]=$(( ${STATUS_COUNTS[${status}]:-0} + 1 ))
  printf '%s %s %s\n' "${thread_id}" "${status}" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"${PROGRESS_FILE}"
  if [[ "${reason}" == "saved_summary_missing_or_incomplete" ]]; then
    NEEDS_SUMMARY_REFRESH+=("${thread_id}")
  fi
  echo "  thread ${thread_id} -> ${status}${reason:+ (${reason})}"
}

COMMON_INPUT=$(common_worker_input)
for thread_id in ${THREADS}; do
  index=$((index + 1))
  input=$(jq -cn --argjson common "${COMMON_INPUT}" --arg thread_id "${thread_id}" --arg policy "${POLICY_JSON}" '
    $common + { thread_id: $thread_id, issue_tracker_grpc_tls: false, policy_json: $policy }')
  echo "[${index}/${THREAD_COUNT}] thread ${thread_id}"
  client_output=$(run_workflow_job "${WORKER_NAME}" "${input}") \
    || echo "  enqueue failed for thread ${thread_id}" >&2
  report_result "${thread_id}" "${client_output}"
done

echo "# results by status:"
for status in "${!STATUS_COUNTS[@]}"; do
  echo "  ${status}: ${STATUS_COUNTS[${status}]}"
done
if [[ ${#NEEDS_SUMMARY_REFRESH[@]} -gt 0 ]]; then
  echo "# threads whose saved summary lacks a message checkpoint; refresh their thread summary first, then rerun with --thread-ids (summary refresh needed):"
  printf '%s\n' "${NEEDS_SUMMARY_REFRESH[@]}"
fi
if [[ ${#ENQUEUE_FAILED[@]} -gt 0 ]]; then
  echo "# threads without a result (enqueue failed or no output line):"
  printf '%s\n' "${ENQUEUE_FAILED[@]}"
fi
