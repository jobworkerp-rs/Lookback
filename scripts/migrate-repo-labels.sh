#!/usr/bin/env bash
# Rewrite legacy `repo:` thread labels to the canonical repository identity.
#
# Before 2026-09-22 the importer stored git remotes as given (e.g.
# `repo:git@host:owner/name.git`); it now stores `repo:host/owner/name`.
# The cross-thread Issue resolution searches threads by exact label, so a
# thread still carrying the legacy form is never found by it.
#
# Every thread carrying a legacy label is relabeled, which includes the
# Memories threads that hold thread summaries: a summary is found by the
# labels of its thread, so relabeling only one side would make the next
# summary run write a new summary and drop the old one with its metadata.
# Stop summary generation (periodic runs, imports) before --apply.
# Issue Tracker label snapshots are left as they are; the workflows
# normalize them when reading.
#
# Requirements: grpcurl and jq on PATH and a reachable Memories server.
#
# Usage:
#   scripts/migrate-repo-labels.sh [--data-dir DIR] [--memories-url URL]
#       [--user-id N] [--apply]
set -euo pipefail

DATA_DIR="${LOOKBACK_DATA_DIR:-${HOME}/.local/share/lookback}"
MEMORIES_URL=""
USER_ID=1
APPLY=0
PAGE_SIZE=200

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data-dir) DATA_DIR=$2; shift 2 ;;
    --memories-url) MEMORIES_URL=$2; shift 2 ;;
    --user-id) USER_ID=$2; shift 2 ;;
    --apply) APPLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# shellcheck source=lib/lookback-sidecar.sh source-path=SCRIPTDIR
source "$(dirname "${BASH_SOURCE[0]}")/lib/lookback-sidecar.sh"
require_tools grpcurl jq
resolve_memories_endpoint

# Same normalization as the Issue resolution workflows (validateProjectContext
# in thread-group-issue-resolution-batch.yaml); a label naming no host or no
# path maps to nothing and is left alone.
# shellcheck disable=SC2016 # jq variables, not shell ones
CANONICAL_JQ='
def repo_identity:
      sub("^repo:"; "") as $raw
      | (if ($raw | test("^[A-Za-z][A-Za-z0-9+.-]*://"))
         then ($raw | index("://")) as $scheme_end
              | ($raw[0:$scheme_end] | ascii_downcase) as $scheme
              | $raw[($scheme_end + 3):] as $rest
              | (($rest | index("/")) // ($rest | length)) as $slash
              | ($rest[0:$slash] | split("@") | last // "" | split(":")) as $host_port
              | { host: (if (($host_port | length) == 2)
                             and (({ http: "80", https: "443", ssh: "22", git: "9418" }[$scheme]) != $host_port[1])
                         then $host_port[0] + ":" + $host_port[1]
                         else $host_port[0] end),
                  path: $rest[$slash:] }
         elif ($raw | test("^[^/]*:")) and (($raw | test("^[^/]*:[0-9]+(/|$)")) | not)
         then ($raw | index(":")) as $colon
              | { host: ($raw[0:$colon] | split("@") | last), path: $raw[($colon + 1):] }
         else (($raw | index("/")) // ($raw | length)) as $slash
              | { host: $raw[0:$slash], path: $raw[$slash:] }
         end)
      | ((.host // "") | ascii_downcase | rtrimstr(".")) + "/"
        + (.path | sub("^/+"; "") | sub("/+$"; "") | sub("\\.[Gg][Ii][Tt]$"; ""));
# end of repo_identity
.[] | (.threadCount // .thread_count // 0 | tostring) as $count
| .label | select(startswith("repo:"))
| (repo_identity) as $identity
| select($identity | test("^[^/]+/.+"))
| select(("repo:" + $identity) != .)
| [., "repo:" + $identity, $count] | @tsv'

labels=$(memories_grpc llm_memory.service.ThreadService/FindDistinctLabels \
  "$(jq -cn --argjson user "${USER_ID}" '{ user_id: $user, limit: 100000 }')")
mapping=$(jq -r ".labels // [] | ${CANONICAL_JQ}" <<<"${labels}")

if [[ -z "${mapping}" ]]; then
  echo "no legacy repo labels"
  exit 0
fi

while IFS=$'\t' read -r old new count; do
  echo "${old} -> ${new} (${count} threads)"
done <<<"${mapping}"

if [[ "${APPLY}" -eq 0 ]]; then
  echo "# dry run; rerun with --apply after stopping summary generation"
  exit 0
fi

relabeled=0
while IFS=$'\t' read -r old new _count; do
  # Relabeled threads drop out of the result, so always read the first page.
  while :; do
    ids=$(memories_grpc llm_memory.service.ThreadService/FindThreadListByLabels \
      "$(jq -cn --arg label "${old}" --argjson user "${USER_ID}" --argjson limit "${PAGE_SIZE}" \
        '{ labels: [$label], match_mode: "LABEL_ALL", user_id: $user, limit: $limit, offset: 0 }')" \
      | jq -r '.id.value // .id // empty')
    [[ -n "${ids}" ]] || break
    for id in ${ids}; do
      # Add first: a failure in between leaves the thread with both labels,
      # never with none.
      memories_grpc llm_memory.service.ThreadService/AddLabels \
        "$(jq -cn --arg id "${id}" --arg label "${new}" '{ thread_id: { value: $id }, labels: [$label] }')" >/dev/null
      memories_grpc llm_memory.service.ThreadService/RemoveLabels \
        "$(jq -cn --arg id "${id}" --arg label "${old}" '{ thread_id: { value: $id }, labels: [$label] }')" >/dev/null
      relabeled=$((relabeled + 1))
    done
  done
done <<<"${mapping}"
echo "relabeled ${relabeled} threads"
