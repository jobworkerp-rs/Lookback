#!/usr/bin/env bash
# Review drift between memories' generation single/batch YAML and agent-app's
# local-merged copies. This does NOT overwrite anything — agent-app keeps a
# hand-merged variant that must preserve two things memories does not have:
#   1. the LLM call on `workerName: memories-llm` (NOT `runnerName: LLM` + ollama)
#   2. the batch progress-report steps (`reportProgress` / progress_processed)
# so the YAML can only be merged by hand. Use this to spot upstream changes.
#
# Usage: scripts/diff-memories-singles.sh [MEMORIES_REPO_ROOT]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_APP_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MEMORIES_ROOT="${1:-$(cd "${AGENT_APP_ROOT}/../memories" && pwd)}"

MEM_WORKERS="${MEMORIES_ROOT}/agent-chat-import/workers"
MEM_WORKFLOWS="${MEMORIES_ROOT}/agent-chat-import/workflows"
APP_LANG="${AGENT_APP_ROOT}/workers/lang-workers/workers"
APP_WF="${AGENT_APP_ROOT}/workers/workflows"

# feature  single-rel-path                       batch-rel-path
SINGLES=(
  "thread-summary|thread-summary/thread-summary-single.yaml|thread-summary/thread-summary-batch.yaml"
  "thread-reflection|thread-reflection/thread-reflection-single.yaml|thread-reflection/thread-reflection-batch.yaml"
  "personality|personality/thread-personality-single.yaml|personality/thread-personality-batch.yaml"
  "personality-merge|personality/user-personality-merge.yaml|"
  "daily-work-summary|daily-work-summary/daily-work-summary-single.yaml|daily-work-summary/daily-work-summary-batch.yaml"
  "weekly-work-summary|weekly-work-summary/weekly-work-summary-single.yaml|weekly-work-summary/weekly-work-summary-batch.yaml"
  "monthly-work-summary|monthly-work-summary/monthly-work-summary-single.yaml|monthly-work-summary/monthly-work-summary-batch.yaml"
)

for row in "${SINGLES[@]}"; do
  IFS='|' read -r feature single batch <<<"${row}"
  echo "================================================================"
  echo "## ${feature} (single)"
  echo "   memories : ${MEM_WORKERS}/${single}"
  echo "   agent-app: ${APP_LANG}/${single}"
  if [[ -f "${MEM_WORKERS}/${single}" && -f "${APP_LANG}/${single}" ]]; then
    diff "${MEM_WORKERS}/${single}" "${APP_LANG}/${single}" || true
  else
    echo "   (one side missing — skipping)"
  fi
  if [[ -n "${batch}" ]]; then
    echo "## ${feature} (batch)"
    echo "   memories : ${MEM_WORKFLOWS}/${batch}"
    echo "   agent-app: ${APP_WF}/${batch}"
    if [[ -f "${MEM_WORKFLOWS}/${batch}" && -f "${APP_WF}/${batch}" ]]; then
      diff "${MEM_WORKFLOWS}/${batch}" "${APP_WF}/${batch}" || true
    else
      echo "   (one side missing — skipping)"
    fi
  fi
done

echo "================================================================"
echo "Review intent: a memories-side change to a non-LLM step (GRPC method,"
echo "schema, validate) should be ported by hand; LLM-call and progress lines"
echo "are agent-app-specific and SHOULD differ."
