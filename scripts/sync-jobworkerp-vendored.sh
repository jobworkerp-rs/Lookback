#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
# shellcheck source=SCRIPTDIR/lib/build-deps.sh
source "${ROOT_DIR}/scripts/lib/build-deps.sh"

# Fetch vendored jobworkerp files from the same configurable ref as the
# release sidecar. The default is upstream main, so both continuously follow
# jobworkerp fixes. CI checks out this repository alone, so anything read
# from jobworkerp (bundled schemas, test fixtures) is vendored here.
JOBWORKERP_REF="$(repo_branch jobworkerp)"
readonly JOBWORKERP_REF
readonly BASE_URL="https://raw.githubusercontent.com/jobworkerp-rs/jobworkerp-rs/${JOBWORKERP_REF}"
readonly PROTO_DIR="runner/protobuf/jobworkerp/runner"
readonly PROTO_FIXTURES="src-tauri/tests/fixtures/jobworkerp-proto"
# upstream path -> destination directory (relative to the repository root)
readonly ENTRIES=(
  "runner/schema/workflow.yaml workers/schema/jobworkerp"
  "runner/schema/workflow_minimal_fix.json workers/schema/jobworkerp"
  "${PROTO_DIR}/sandbox_args.proto ${PROTO_FIXTURES}"
  "${PROTO_DIR}/sandbox_common.proto ${PROTO_FIXTURES}"
  "${PROTO_DIR}/sandbox_result.proto ${PROTO_FIXTURES}"
  "${PROTO_DIR}/sandbox_settings.proto ${PROTO_FIXTURES}"
  "${PROTO_DIR}/workflow_result.proto ${PROTO_FIXTURES}"
  "${PROTO_DIR}/workflow_run_args.proto ${PROTO_FIXTURES}"
  "${PROTO_DIR}/workflow_runner_settings.proto ${PROTO_FIXTURES}"
)

mode="sync"
if [[ "${1:-}" == "--check" ]]; then
  mode="check"
elif [[ $# -ne 0 ]]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf "${tmp_dir}"' EXIT

for entry in "${ENTRIES[@]}"; do
  read -r upstream dest_dir <<<"${entry}"
  file=$(basename "${upstream}")
  source_file="${tmp_dir}/${file}"
  destination="${ROOT_DIR}/${dest_dir}/${file}"
  curl -fsSL "${BASE_URL}/${upstream}" -o "${source_file}"

  if [[ "${mode}" == "check" ]]; then
    cmp -s "${source_file}" "${destination}" || {
      echo "vendored jobworkerp file is stale: ${dest_dir}/${file}; run scripts/sync-jobworkerp-vendored.sh" >&2
      exit 1
    }
  else
    mkdir -p "${ROOT_DIR}/${dest_dir}"
    install -m 0644 "${source_file}" "${destination}"
  fi
done

echo "vendored jobworkerp files at ${JOBWORKERP_REF} are ${mode}ed"
