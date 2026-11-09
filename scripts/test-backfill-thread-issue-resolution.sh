#!/usr/bin/env bash
# Exercises scripts/backfill-thread-issue-resolution.sh against fake
# grpcurl / jobworkerp-client / ss binaries so no sidecar is required.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TARGET="${SCRIPT_DIR}/backfill-thread-issue-resolution.sh"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

DATA_DIR="${TMP_ROOT}/data"
BIN="${TMP_ROOT}/bin"
CALLS="${TMP_ROOT}/calls.log"
mkdir -p "${DATA_DIR}/issue-auto-policy" "${BIN}"
: >"${CALLS}"

cat >"${DATA_DIR}/issue-auto-policy/current.json" <<'JSON'
{"version":2,"changed_at_ms":1,"policy":{"extraction_enabled":true,"resolution_mode":"APPLY","resolution_min_confidence":0.9}}
JSON
printf '{"mode":"external"}\n' >"${DATA_DIR}/llm-settings.json"
printf '{"output_language":"en"}\n' >"${DATA_DIR}/app-settings.json"
printf '{"mode":"remote","remote_jobworkerp_url":"http://localhost:9000","remote_memories_url":"https://memories.example:9000"}\n' \
  >"${DATA_DIR}/connection.json"

# Fake listening sockets: one gateway port and one manager port.
cat >"${BIN}/ss" <<'SH'
#!/usr/bin/env bash
cat <<'OUT'
LISTEN 0 128 127.0.0.1:41143 0.0.0.0:* users:(("issue-tracker-s",pid=1,fd=19))
LISTEN 0 128 127.0.0.1:40349 0.0.0.0:* users:(("issue-tracker-s",pid=1,fd=21))
LISTEN 0 128 0.0.0.0:8001 0.0.0.0:* users:(("all-in-one",pid=2,fd=28))
LISTEN 0 128 127.0.0.1:40551 0.0.0.0:* users:(("all-in-one",pid=2,fd=27))
OUT
SH

# Fake grpcurl: reflection list per port, contract info, and two candidate pages.
cat >"${BIN}/grpcurl" <<'SH'
#!/usr/bin/env bash
echo "grpcurl $*" >>"${CALLS_LOG}"
args=("$@")
last=${args[$((${#args[@]} - 1))]}
case "${last}" in
  list)
    addr=${args[$((${#args[@]} - 2))]}
    case "${addr}" in
      *:40349) printf 'grpc.reflection.v1.ServerReflection\nissue_tracker.rpc.v1.ManagerConsumerService\n' ;;
      *:40551) printf 'grpc.reflection.v1.ServerReflection\njobworkerp.service.JobService\njobworkerp.service.WorkerService\n' ;;
      *:8001) echo "Failed to list services: server does not support the reflection API" >&2; exit 1 ;;
      *) printf 'grpc.reflection.v1.ServerReflection\nissue_tracker.rpc.v1.IssueGateway\n' ;;
    esac
    ;;
  issue_tracker.rpc.v1.ManagerInfoService/GetContractInfo)
    printf '{"success":{"contractFingerprint":"sha256:c","schemaFingerprint":"schema-v1","storeIdentities":[{"role":"STORE_ROLE_MANAGER","dataRootId":"sha256:root"}]}}\n'
    ;;
  issue_tracker.rpc.v1.ManagerConsumerService/ListReviewCandidates)
    body=""
    for ((i = 0; i < ${#args[@]}; i++)); do
      if [[ "${args[$i]}" == "-d" ]]; then body=${args[$((i + 1))]}; fi
    done
    # createdAt: c1 2026-01-01, c2 2026-03-01 (newest), c3 2026-02-01 (shared by 30 and 10).
    if [[ "${body}" == *'"page_token"'* ]]; then
      printf '{"success":{"rows":[{"candidate":{"candidateId":"c3","createdAt":{"unixEpochMillis":"1769904000000"},"sourceEvidence":[{"threadId":"30"},{"threadId":"10"}]}}],"page":{}}}\n'
    else
      printf '{"success":{"rows":[{"candidate":{"candidateId":"c1","createdAt":{"unixEpochMillis":"1767225600000"},"sourceEvidence":[{"threadId":"10"}]}},{"candidate":{"candidateId":"c2","createdAt":{"unixEpochMillis":"1772323200000"},"sourceEvidence":[{"threadId":"20"}]}}],"page":{"nextPageToken":"t2"}}}\n'
    fi
    ;;
  *)
    echo "unexpected grpcurl invocation: $*" >&2
    exit 1
    ;;
esac
SH

# Fake client: thread 10 resolves, every other thread lacks a usable summary.
cat >"${BIN}/jobworkerp-client" <<'SH'
#!/usr/bin/env bash
echo "jobworkerp-client $*" >>"${CALLS_LOG}"
if [[ "$*" == *"-w memories-summarize-batch"* ]]; then
  printf 'Job enqueued successfully - Result ID: 2\nWorker: memories-summarize-batch | Status: SUCCESS\n'
  printf 'id: 01b1\n'
  printf 'output: {"completed":false,"generated_threads":["77"],"skipped_threads":[],"failed_threads":[{"thread_id":"88","error":"llm failed"}],"last_error":"llm failed"}\n'
  printf 'position: /ROOT/do/9\n'
  exit 0
fi
printf 'Job enqueued successfully - Result ID: 1\nWorker: lookback-thread-issue-resolution-manual | Status: SUCCESS\n'
printf 'id: 01a1\n'
if grep -q 'thread_id\\":\\"10\\"' <<<"$*"; then
  printf 'output: {"status":"COMPLETE","complete":true,"failed":false,"failure_reason":"","child_outcome":{"applied_count":1}}\n'
else
  printf 'output: {"status":"FAILED","complete":false,"failed":true,"failure_reason":"saved_summary_missing_or_incomplete","child_outcome":null}\n'
fi
printf 'position: /ROOT/do/9\n'
SH
chmod +x "${BIN}/ss" "${BIN}/grpcurl" "${BIN}/jobworkerp-client"

export PATH="${BIN}:${PATH}" CALLS_LOG="${CALLS}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

echo "== dry run discovers, deduplicates, and sorts source threads"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run)
printf '%s\n' "${out}" | grep -qx '10' || fail "thread 10 missing: ${out}"
printf '%s\n' "${out}" | grep -qx '20' || fail "thread 20 missing: ${out}"
printf '%s\n' "${out}" | grep -qx '30' || fail "thread 30 missing: ${out}"
[[ $(printf '%s\n' "${out}" | grep -c '^[0-9]*$') -eq 3 ]] || fail "expected exactly 3 threads: ${out}"
grep -q 'jobworkerp-client' "${CALLS}" && fail "dry run must not enqueue"
grep -q '127.0.0.1:40349 issue_tracker.rpc.v1.ManagerConsumerService/ListReviewCandidates' "${CALLS}" \
  || fail "manager port was not auto-discovered"
grep -q '"page_token":"t2"' "${CALLS}" || fail "second page was not requested"

echo "== newest-first orders threads by their latest candidate"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run --newest-first 2>/dev/null | grep '^[0-9]*$' | tr '\n' ' ')
[[ "${out}" == "20 30 10 " ]] || fail "expected 20 30 10, got: ${out}"

echo "== candidates-since keeps only threads with a candidate created on or after the date"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run --candidates-since 2026-02-15 2>/dev/null | grep '^[0-9]*$' | tr '\n' ' ')
[[ "${out}" == "20 " ]] || fail "expected only 20, got: ${out}"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run --candidates-since 2026-02-01 --newest-first 2>/dev/null | grep '^[0-9]*$' | tr '\n' ' ')
[[ "${out}" == "20 30 10 " ]] || fail "expected 20 30 10 since Feb 1, got: ${out}"

echo "== real run enqueues one manual job per thread with the app's inputs"
: >"${CALLS}"
bash "${TARGET}" --data-dir "${DATA_DIR}" --limit 2 >"${TMP_ROOT}/run.out"
[[ $(grep -c '^jobworkerp-client' "${CALLS}") -eq 2 ]] || fail "expected 2 enqueues: $(cat "${CALLS}")"
first=$(grep -m1 '^jobworkerp-client' "${CALLS}")
[[ "${first}" == *"-a http://127.0.0.1:40551 "* ]] || fail "jobworkerp sidecar port not auto-discovered: ${first}"
[[ "${first}" == *"job enqueue -w lookback-thread-issue-resolution-manual"* ]] || fail "wrong worker: ${first}"
# The WORKFLOW runner exposes run and create, so the method must be explicit.
[[ "${first}" == *" --using run "* ]] || fail "--using run missing: ${first}"
[[ "${first}" == *'\"thread_id\":\"10\"'* ]] || fail "thread id missing: ${first}"
[[ "${first}" == *'\"memories_grpc_host\":\"memories.example\"'* ]] || fail "memories host missing: ${first}"
[[ "${first}" == *'\"memories_grpc_port\":9000'* ]] || fail "memories port missing: ${first}"
[[ "${first}" == *'\"memories_grpc_tls\":true'* ]] || fail "memories tls missing: ${first}"
[[ "${first}" == *'\"issue_tracker_grpc_port\":40349'* ]] || fail "tracker port missing: ${first}"
[[ "${first}" == *'\"llm_worker_name\":\"memories-llm-external\"'* ]] || fail "llm worker missing: ${first}"
[[ "${first}" == *'\"output_language\":\"en\"'* ]] || fail "output language missing: ${first}"
[[ "${first}" == *'resolution_mode'* ]] || fail "policy json missing: ${first}"
grep -q 'thread 10 .*COMPLETE' "${TMP_ROOT}/run.out" || fail "per-thread status missing: $(cat "${TMP_ROOT}/run.out")"
grep -q 'thread 20 .*FAILED.*saved_summary_missing_or_incomplete' "${TMP_ROOT}/run.out" \
  || fail "per-thread failure reason missing: $(cat "${TMP_ROOT}/run.out")"
grep -q 'COMPLETE: 1' "${TMP_ROOT}/run.out" || fail "tally missing: $(cat "${TMP_ROOT}/run.out")"
grep -q 'FAILED: 1' "${TMP_ROOT}/run.out" || fail "failed tally missing: $(cat "${TMP_ROOT}/run.out")"
grep -A2 'summary refresh' "${TMP_ROOT}/run.out" | grep -qx '20' \
  || fail "threads needing a summary refresh not listed: $(cat "${TMP_ROOT}/run.out")"

echo "== a second run skips COMPLETE threads, retries FAILED ones, and reports the skip"
# The run above resolved thread 10 (COMPLETE) and thread 20 failed.
PROGRESS="${DATA_DIR}/backfill-thread-issue-resolution.progress"
[[ -f "${PROGRESS}" ]] || fail "progress file not written: ${PROGRESS}"
grep -q '^10 COMPLETE ' "${PROGRESS}" || fail "COMPLETE result not recorded: $(cat "${PROGRESS}")"
grep -q '^20 FAILED ' "${PROGRESS}" || fail "FAILED result not recorded: $(cat "${PROGRESS}")"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run 2>"${TMP_ROOT}/dry.err" | grep '^[0-9]*$' | tr '\n' ' ')
[[ "${out}" == "20 30 " ]] || fail "second dry run must skip thread 10: ${out}"
grep -q 'already COMPLETE' "${TMP_ROOT}/dry.err" || fail "skip count not reported: $(cat "${TMP_ROOT}/dry.err")"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run --include-done 2>/dev/null | grep '^[0-9]*$' | tr '\n' ' ')
[[ "${out}" == "10 20 30 " ]] || fail "--include-done must restore thread 10: ${out}"
: >"${CALLS}"
bash "${TARGET}" --data-dir "${DATA_DIR}" --limit 1 >/dev/null
grep -q 'thread_id\\":\\"20\\"' "${CALLS}" || fail "limit after skipping must start at thread 20: $(cat "${CALLS}")"
rm -f "${PROGRESS}"

echo "== explicit jobworkerp address overrides discovery"
: >"${CALLS}"
printf '99\n' >"${TMP_ROOT}/explicit.txt"
bash "${TARGET}" --data-dir "${DATA_DIR}" --thread-ids "${TMP_ROOT}/explicit.txt" --jobworkerp-address http://localhost:9000 >/dev/null
grep -q '^jobworkerp-client -a http://localhost:9000 ' "${CALLS}" || fail "explicit address must be used as given: $(cat "${CALLS}")"

echo "== explicit thread list skips discovery"
: >"${CALLS}"
printf '77\n\n88\n' >"${TMP_ROOT}/threads.txt"
bash "${TARGET}" --data-dir "${DATA_DIR}" --thread-ids "${TMP_ROOT}/threads.txt" >/dev/null
grep -q 'ListReviewCandidates' "${CALLS}" && fail "explicit list must not page candidates"
[[ $(grep -c '^jobworkerp-client' "${CALLS}") -eq 2 ]] || fail "expected 2 enqueues from file"
grep -q '\\"thread_id\\":\\"88\\"' "${CALLS}" || fail "thread 88 not enqueued"

echo "== refresh mode force-resummarizes exactly the listed threads without extraction"
: >"${CALLS}"
printf '77\n88\n' >"${TMP_ROOT}/refresh.txt"
bash "${TARGET}" --data-dir "${DATA_DIR}" --refresh-summaries "${TMP_ROOT}/refresh.txt" --user-id 3 >"${TMP_ROOT}/refresh.out"
[[ $(grep -c '^jobworkerp-client' "${CALLS}") -eq 1 ]] || fail "refresh must enqueue exactly one batch: $(cat "${CALLS}")"
grep -q 'ListReviewCandidates' "${CALLS}" && fail "refresh must not page candidates"
grep -q 'lookback-thread-issue-resolution-manual' "${CALLS}" && fail "refresh must not enqueue resolution"
batch=$(grep '^jobworkerp-client' "${CALLS}")
[[ "${batch}" == *"job enqueue -w memories-summarize-batch --using run"* ]] || fail "wrong batch worker: ${batch}"
[[ "${batch}" == *'\"thread_ids\":[\"77\",\"88\"]'* ]] || fail "thread_ids missing: ${batch}"
[[ "${batch}" == *'\"force_resummarize\":true'* ]] || fail "force missing: ${batch}"
[[ "${batch}" == *'\"user_id\":3'* ]] || fail "user_id missing: ${batch}"
[[ "${batch}" == *'extraction_enabled\\\":false'* ]] || fail "extraction must be disabled in the forwarded policy: ${batch}"
[[ "${batch}" == *'resolution_mode\\\":\\\"APPLY'* ]] || fail "resolution_mode must be preserved: ${batch}"
[[ "${batch}" == *'\"llm_worker_name\":\"memories-llm-external\"'* ]] || fail "llm worker missing: ${batch}"
[[ "${batch}" == *'\"issue_tracker_grpc_port\":40349'* ]] || fail "tracker port missing: ${batch}"
grep -q 'generated: 1' "${TMP_ROOT}/refresh.out" || fail "generated count missing: $(cat "${TMP_ROOT}/refresh.out")"
grep -q 'skipped: 0' "${TMP_ROOT}/refresh.out" || fail "skipped count missing: $(cat "${TMP_ROOT}/refresh.out")"
grep -q 'failed: 1' "${TMP_ROOT}/refresh.out" || fail "failed count missing: $(cat "${TMP_ROOT}/refresh.out")"
grep -A1 'failed threads' "${TMP_ROOT}/refresh.out" | grep -q '88' || fail "failed thread not listed: $(cat "${TMP_ROOT}/refresh.out")"

echo "== refresh dry run prints the batch input without enqueueing"
: >"${CALLS}"
bash "${TARGET}" --data-dir "${DATA_DIR}" --refresh-summaries "${TMP_ROOT}/refresh.txt" --dry-run >"${TMP_ROOT}/refresh-dry.out"
grep -q 'jobworkerp-client' "${CALLS}" && fail "refresh dry run must not enqueue"
grep -q '"force_resummarize":true' "${TMP_ROOT}/refresh-dry.out" || fail "dry run must print the batch input: $(cat "${TMP_ROOT}/refresh-dry.out")"

echo "== refuses to run when resolution_mode is not APPLY"
printf '{"version":3,"changed_at_ms":1,"policy":{"resolution_mode":"DISABLED"}}\n' \
  >"${DATA_DIR}/issue-auto-policy/current.json"
if bash "${TARGET}" --data-dir "${DATA_DIR}" --dry-run >/dev/null 2>"${TMP_ROOT}/err"; then
  fail "DISABLED policy must abort"
fi
grep -q 'resolution_mode' "${TMP_ROOT}/err" || fail "abort reason not explained: $(cat "${TMP_ROOT}/err")"

echo "PASS"
