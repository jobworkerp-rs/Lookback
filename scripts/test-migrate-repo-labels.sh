#!/usr/bin/env bash
# Exercises scripts/migrate-repo-labels.sh against a fake grpcurl that keeps
# thread labels in a file, so no Memories server is required.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TARGET="${SCRIPT_DIR}/migrate-repo-labels.sh"
TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT

DATA_DIR="${TMP_ROOT}/data"
BIN="${TMP_ROOT}/bin"
CALLS="${TMP_ROOT}/calls.log"
LABELS="${TMP_ROOT}/labels.tsv"
mkdir -p "${DATA_DIR}" "${BIN}"
: >"${CALLS}"
printf '{"mode":"remote","remote_memories_url":"https://memories.example:9000"}\n' >"${DATA_DIR}/connection.json"

# "<thread_id>\t<label>" per line. Thread 3 is the summary memory thread of
# thread 1 and carries the same labels.
cat >"${LABELS}" <<'TSV'
1	repo:git@git.example.com:jobworkerp-rs/Lookback-app.git
1	path:/w/app
3	repo:git@git.example.com:jobworkerp-rs/Lookback-app.git
2	repo:https://User@GitHub.com:443/org/tool.git
4	repo:git.example.com/jobworkerp-rs/Lookback-app
5	repo:file:///home/x/repo
TSV

cat >"${BIN}/grpcurl" <<'SH'
#!/usr/bin/env bash
echo "grpcurl $*" >>"${CALLS_LOG}"
args=("$@")
method=${args[$((${#args[@]} - 1))]}
body=""
for ((i = 0; i < ${#args[@]}; i++)); do
  if [[ "${args[$i]}" == "-d" ]]; then body=${args[$((i + 1))]}; fi
done
case "${method}" in
  */FindDistinctLabels)
    cut -f2 "${LABELS_FILE}" | sort | uniq -c \
      | awk '{ printf "%s\t%s\n", $2, $1 }' \
      | jq -R -s 'split("\n") | map(select(. != "") | split("\t") | { label: .[0], threadCount: .[1] }) | { labels: . }'
    ;;
  */FindThreadListByLabels)
    label=$(jq -r '.labels[0]' <<<"${body}")
    offset=$(jq -r '.offset // 0' <<<"${body}")
    awk -F'\t' -v l="${label}" '$2 == l { print $1 }' "${LABELS_FILE}" | sort -u | tail -n +"$((offset + 1))" \
      | while read -r id; do printf '{"id":{"value":"%s"},"data":{}}\n' "${id}"; done
    ;;
  */AddLabels)
    id=$(jq -r '.thread_id.value' <<<"${body}")
    jq -r '.labels[]' <<<"${body}" | while read -r l; do printf '%s\t%s\n' "${id}" "${l}" >>"${LABELS_FILE}"; done
    echo '{"isSuccess":true}'
    ;;
  */RemoveLabels)
    id=$(jq -r '.thread_id.value' <<<"${body}")
    l=$(jq -r '.labels[0]' <<<"${body}")
    awk -F'\t' -v id="${id}" -v l="${l}" '!($1 == id && $2 == l)' "${LABELS_FILE}" >"${LABELS_FILE}.new"
    mv "${LABELS_FILE}.new" "${LABELS_FILE}"
    echo '{"isSuccess":true}'
    ;;
  *) echo "unexpected grpcurl invocation: $*" >&2; exit 1 ;;
esac
SH
chmod +x "${BIN}/grpcurl"
export PATH="${BIN}:${PATH}" CALLS_LOG="${CALLS}" LABELS_FILE="${LABELS}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

echo "== dry run lists each legacy label with its canonical form and changes nothing"
cp "${LABELS}" "${TMP_ROOT}/before.tsv"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}")
grep -q 'repo:git@git.example.com:jobworkerp-rs/Lookback-app.git -> repo:git.example.com/jobworkerp-rs/Lookback-app (2 threads)' <<<"${out}" \
  || fail "scp label not mapped: ${out}"
grep -q 'repo:https://User@GitHub.com:443/org/tool.git -> repo:github.com/org/tool (1 threads)' <<<"${out}" \
  || fail "https label not mapped: ${out}"
grep -q 'repo:git.example.com/jobworkerp-rs/Lookback-app ->' <<<"${out}" && fail "canonical label must not be migrated: ${out}"
grep -q 'repo:file:///home/x/repo' <<<"${out}" && fail "a label naming no repository must be left alone: ${out}"
cmp -s "${LABELS}" "${TMP_ROOT}/before.tsv" || fail "dry run changed labels"
grep -q 'AddLabels\|RemoveLabels' "${CALLS}" && fail "dry run must not write"
grep -q -- '-plaintext' "${CALLS}" && fail "an https Memories URL must use TLS"
grep -q 'memories.example:9000' "${CALLS}" || fail "memories address not used: $(cat "${CALLS}")"

echo "== apply relabels every thread carrying a legacy label, summary threads included"
bash "${TARGET}" --data-dir "${DATA_DIR}" --apply >"${TMP_ROOT}/apply.out"
for thread in 1 3; do
  grep -qP "^${thread}\trepo:git.example.com/jobworkerp-rs/Lookback-app$" "${LABELS}" || fail "thread ${thread} not relabeled: $(cat "${LABELS}")"
done
grep -qP '^2\trepo:github.com/org/tool$' "${LABELS}" || fail "thread 2 not relabeled"
grep -q 'repo:git@' "${LABELS}" && fail "legacy label left behind: $(cat "${LABELS}")"
grep -qP '^1\tpath:/w/app$' "${LABELS}" || fail "other labels must stay"
grep -qP '^5\trepo:file:///home/x/repo$' "${LABELS}" || fail "unmappable label must stay"
grep -q 'relabeled 3 threads' "${TMP_ROOT}/apply.out" || fail "summary missing: $(cat "${TMP_ROOT}/apply.out")"
# The canonical label is added before the legacy one is removed, so a thread
# never loses its repository between the two calls.
first_add=$(grep -n 'AddLabels' "${CALLS}" | head -1 | cut -d: -f1)
first_remove=$(grep -n 'RemoveLabels' "${CALLS}" | head -1 | cut -d: -f1)
[[ "${first_add}" -lt "${first_remove}" ]] || fail "labels must be added before removal"

echo "== a second run finds nothing left"
out=$(bash "${TARGET}" --data-dir "${DATA_DIR}")
grep -q 'no legacy repo labels' <<<"${out}" || fail "rerun should be empty: ${out}"

echo "PASS"
