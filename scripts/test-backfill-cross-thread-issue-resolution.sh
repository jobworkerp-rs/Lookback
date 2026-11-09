#!/usr/bin/env bash
# Exercises scripts/backfill-cross-thread-issue-resolution.sh against fake
# ss / grpcurl / jobworkerp-client binaries so no sidecar is required.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TARGET="${SCRIPT_DIR}/backfill-cross-thread-issue-resolution.sh"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

DATA_DIR="${TMP_ROOT}/data"
BIN="${TMP_ROOT}/bin"
CALLS="${TMP_ROOT}/calls.log"
mkdir -p "${DATA_DIR}/issue-auto-policy" "${BIN}"
: >"${CALLS}"

write_policy() {
  printf '{"version":2,"changed_at_ms":1,"policy":{"resolution_mode":"%s","cross_thread_resolution_mode":"DISABLED"}}\n' "$1" \
    >"${DATA_DIR}/issue-auto-policy/current.json"
}
write_policy APPLY
printf '{"mode":"local"}\n' >"${DATA_DIR}/llm-settings.json"
printf '{"mode":"remote","remote_memories_url":"https://memories.example:9000"}\n' >"${DATA_DIR}/connection.json"

cat >"${BIN}/ss" <<'SH'
#!/usr/bin/env bash
cat <<'OUT'
LISTEN 0 128 127.0.0.1:40349 0.0.0.0:* users:(("issue-tracker-s",pid=1,fd=21))
LISTEN 0 128 127.0.0.1:40551 0.0.0.0:* users:(("all-in-one",pid=2,fd=27))
OUT
SH
cat >"${BIN}/grpcurl" <<'SH'
#!/usr/bin/env bash
case "$*" in
  *40349\ list) printf 'issue_tracker.rpc.v1.ManagerConsumerService\n' ;;
  *40551\ list) printf 'jobworkerp.service.JobService\n' ;;
  *) exit 1 ;;
esac
SH
cat >"${BIN}/jobworkerp-client" <<'SH'
#!/usr/bin/env bash
echo "jobworkerp-client $*" >>"${CALLS_LOG}"
printf 'Job enqueued successfully - Result ID: 1\n'
printf 'output: {"completed":true,"mode":"manual","dry_run":true,"since_ms":0,"candidate_count":12,"scoped_count":5,"unmatched_count":2,"processed_count":3,"dry_run_count":1,"applied_count":0,"kept_count":1,"contradicted_count":1,"protected_count":0,"skipped_count":0,"conflicted_count":0,"failed_count":0,"dry_run_candidates":[{"candidate_id":"c1","decision":"COMPLETE","evidence_thread_ids":["t2"],"evidence_memory_ids":["m9"]}],"contradicted_candidates":[{"candidate_id":"c2","decisions":["CANCEL","COMPLETE"],"thread_ids":["t3","t4"]}],"failed_candidates":[],"skipped_candidates":[],"applied_candidates":[],"evaluations":[{"candidate_id":"c3","title":"Fix parser","outcome":"keep","search_mode":"text","hits":[{"thread_id":"t5","score":0.12345}],"verdicts":[{"thread_id":"t5","kind":"keep","confidence":0.95}]}]}\n'
SH
chmod +x "${BIN}/ss" "${BIN}/grpcurl" "${BIN}/jobworkerp-client"
export PATH="${BIN}:${PATH}" CALLS_LOG="${CALLS}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

echo "== default run is a dry run over every candidate on the manual worker"
bash "${TARGET}" --data-dir "${DATA_DIR}" >"${TMP_ROOT}/dry.out"
call=$(grep '^jobworkerp-client' "${CALLS}")
[[ "${call}" == *"-a http://127.0.0.1:40551 "* ]] || fail "jobworkerp port not discovered: ${call}"
[[ "${call}" == *"job enqueue -w lookback-cross-thread-issue-resolution-manual --using run"* ]] || fail "wrong worker: ${call}"
[[ "${call}" == *'\"mode\":\"manual\"'* ]] || fail "manual mode missing: ${call}"
[[ "${call}" == *'\"dry_run\":true'* ]] || fail "dry run missing: ${call}"
[[ "${call}" == *'\"since_ms\":0'* ]] || fail "since missing: ${call}"
[[ "${call}" == *'\"user_id\":1'* ]] || fail "user missing: ${call}"
[[ "${call}" == *'\"issue_tracker_grpc_port\":40349'* ]] || fail "tracker port missing: ${call}"
[[ "${call}" == *'\"memories_grpc_tls\":true'* ]] || fail "memories tls missing: ${call}"
grep -q 'dry_run: 1' "${TMP_ROOT}/dry.out" || fail "counts missing: $(cat "${TMP_ROOT}/dry.out")"
grep -q 'contradicted: 1' "${TMP_ROOT}/dry.out" || fail "contradicted count missing"
grep -q 'c1 COMPLETE threads=t2 memories=m9' "${TMP_ROOT}/dry.out" || fail "dry-run decision missing: $(cat "${TMP_ROOT}/dry.out")"
grep -q 'c2 CANCEL,COMPLETE threads=t3,t4' "${TMP_ROOT}/dry.out" || fail "contradiction missing"
grep -q 'c3 keep text hits=t5:0.123 verdicts=t5:keep/-/0.95 Fix parser' "${TMP_ROOT}/dry.out" \
  || fail "per-candidate evaluation missing: $(cat "${TMP_ROOT}/dry.out")"
ls "${DATA_DIR}"/backfill-cross-thread-issue-resolution-*.json >/dev/null 2>&1 || fail "full output not saved"

echo "== apply and since are forwarded"
: >"${CALLS}"
bash "${TARGET}" --data-dir "${DATA_DIR}" --apply --since 2026-10-01 --user-id 3 --search-limit 7 >/dev/null
call=$(grep '^jobworkerp-client' "${CALLS}")
[[ "${call}" == *'\"dry_run\":false'* ]] || fail "apply must not be a dry run: ${call}"
expected=$(( $(date -d 2026-10-01 +%s) * 1000 ))
[[ "${call}" == *"\\\"since_ms\\\":${expected}"* ]] || fail "since date not converted: ${call}"
[[ "${call}" == *'\"user_id\":3'* ]] || fail "user id not forwarded: ${call}"
[[ "${call}" == *'\"search_limit\":7'* ]] || fail "search limit not forwarded: ${call}"

echo "== limit and candidate IDs narrow the run"
: >"${CALLS}"
printf 'c1\n\nc2\n' >"${TMP_ROOT}/ids.txt"
bash "${TARGET}" --data-dir "${DATA_DIR}" --limit 5 --candidate-ids "${TMP_ROOT}/ids.txt" >/dev/null
call=$(grep '^jobworkerp-client' "${CALLS}")
[[ "${call}" == *'\"max_candidates\":5'* ]] || fail "limit not forwarded: ${call}"
[[ "${call}" == *'\"candidate_ids\":[\"c1\",\"c2\"]'* ]] || fail "candidate ids not forwarded: ${call}"

echo "== applying needs resolution_mode APPLY; a dry run does not"
write_policy DISABLED
: >"${CALLS}"
if bash "${TARGET}" --data-dir "${DATA_DIR}" --apply >/dev/null 2>"${TMP_ROOT}/err"; then
  fail "apply under DISABLED must abort"
fi
grep -q 'resolution_mode' "${TMP_ROOT}/err" || fail "abort reason missing"
bash "${TARGET}" --data-dir "${DATA_DIR}" >/dev/null || fail "dry run must work while disabled"

echo "PASS"
