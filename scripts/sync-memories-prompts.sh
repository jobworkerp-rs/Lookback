#!/usr/bin/env bash
# Sync multilingual generation prompts from the sibling memories repo into
# agent-app's lang-workers tree.
#
# ONLY prompts (`prompts/<role>.<lang>.txt`) are machine-copied — they are
# LLM-backend-agnostic, so a verbatim copy is safe. The single/batch YAML are
# NOT copied: agent-app keeps a local-merged variant (LLM call stays on
# `workerName: memories-llm`, plus issue-tracking steps), so those are
# hand-maintained. Use `diff-memories-singles.sh` to review YAML drift.
#
# Features in HAND_MERGED_FEATURES (scripts/lib/hand-merged-features.sh)
# carry agent-app prompt additions (the `has_unfinished_issue_candidates`
# schema field), so a verbatim copy would drop them; they are skipped here and
# reviewed with diff-memories-singles.sh.
#
# Usage: scripts/sync-memories-prompts.sh [MEMORIES_REPO_ROOT]
#   MEMORIES_REPO_ROOT defaults to ../memories (sibling of agent-app).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_APP_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MEMORIES_ROOT="${1:-$(cd "${AGENT_APP_ROOT}/../memories" && pwd)}"

SRC_BASE="${MEMORIES_ROOT}/agent-chat-import/workers"
DST_BASE="${AGENT_APP_ROOT}/workers/lang-workers/workers"

if [[ ! -d "${SRC_BASE}" ]]; then
  echo "error: memories workers dir not found: ${SRC_BASE}" >&2
  echo "       pass the memories repo root as the first argument." >&2
  exit 1
fi

# Generation features whose prompts live under workers/<feature>/prompts/.
FEATURES=(
  thread-summary
  thread-group-summary
  thread-reflection
  personality
  daily-work-summary
  weekly-work-summary
  monthly-work-summary
)

# shellcheck source=lib/hand-merged-features.sh
source "${SCRIPT_DIR}/lib/hand-merged-features.sh"

is_hand_merged() {
  local feature="$1" merged
  for merged in "${HAND_MERGED_FEATURES[@]}"; do
    [[ "${merged}" == "${feature}" ]] && return 0
  done
  return 1
}

for feature in "${FEATURES[@]}"; do
  if is_hand_merged "${feature}"; then
    echo "skip ${feature}/prompts (hand-merged; review with diff-memories-singles.sh)"
    continue
  fi
  src="${SRC_BASE}/${feature}/prompts"
  dst="${DST_BASE}/${feature}/prompts"
  if [[ ! -d "${src}" ]]; then
    echo "warn: no prompts dir for ${feature} (${src}); skipping" >&2
    continue
  fi
  mkdir -p "${dst}"
  # --delete so a renamed/removed prompt on the memories side is reflected.
  rsync -a --delete --include='*.txt' --exclude='*' "${src}/" "${dst}/"
  echo "synced ${feature}/prompts -> ${dst}"
done

echo "done. Remember: single/batch YAML are hand-merged (see diff-memories-singles.sh)."
