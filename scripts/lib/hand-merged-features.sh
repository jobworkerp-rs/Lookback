# shellcheck shell=bash
# Generation features whose agent-app prompts carry local additions (the
# `has_unfinished_issue_candidates` schema field) on top of memories' text.
# sync-memories-prompts.sh skips them; diff-memories-singles.sh diffs their
# prompts for review. Sourced by both so the list cannot drift.
HAND_MERGED_FEATURES=(
  thread-summary
  thread-group-summary
)
