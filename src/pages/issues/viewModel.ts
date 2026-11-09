import type {
  IssueListItem,
  IssueReviewItem,
  IssueState,
  ListIssueReviewRequest,
} from "@/types/api";

export const EMPTY_REVIEW_FILTERS: ListIssueReviewRequest = {
  target: "all",
  priority: "all",
  priorities: [],
  min_confidence: null,
  detection: "all",
};

export const ACTIVE_ISSUE_STATES: IssueState[] = [
  "OPEN",
  "IN_PROGRESS",
  "BLOCKED",
  "DEFERRED",
  "AWAITING_REVIEW",
];

const REVIEW_INTEGRITY_LABELS: Record<IssueReviewItem["integrity"], string> = {
  normal: "整合性: 正常",
  mapping_missing: "整合性: mapping 欠損",
  mapping_invalid: "整合性: mapping 不正",
  issue_not_found: "整合性: Issue 不在",
  unknown_issue_state: "整合性: Issue 状態不明",
};

export function reviewIntegrityLabel(value: IssueReviewItem["integrity"]) {
  return REVIEW_INTEGRITY_LABELS[value];
}

export function reviewResolutionLabel(value: IssueReviewItem["resolution_state"]) {
  return value === "resolution_pending" ? "別の解決操作を確認中" : "操作可能";
}

export function userStateTransitionTarget(
  state: IssueListItem["state"],
): "BLOCKED" | "OPEN" | null {
  if (state === "CANCELED") return "OPEN";
  if (state === "BLOCKED") return "OPEN";
  if (["OPEN", "IN_PROGRESS", "DEFERRED"].includes(state)) return "BLOCKED";
  return null;
}

export function reviewLabel(
  value: IssueReviewItem["review_state"] | IssueListItem["review_state"],
) {
  return value === "candidate" ? "候補" : value === "unreviewed" ? "未レビュー" : "";
}

/**
 * Client-side equivalent of the review list filter (`review_matches` in
 * src-tauri/src/commands/issues/review.rs), so a filtered view can be derived
 * from the single unfiltered review query instead of fetching every page again.
 */
export function reviewMatches(
  item: Pick<IssueReviewItem, "review_state" | "priority" | "confidence" | "detection">,
  filters: ListIssueReviewRequest,
): boolean {
  const priorities = filters.priorities ?? [];
  const priorityMatches = priorities.length
    ? priorities.includes(item.priority)
    : (filters.priority ?? "all") === "all" || filters.priority === item.priority;
  return (
    ((filters.target ?? "all") === "all" || filters.target === item.review_state) &&
    priorityMatches &&
    (filters.min_confidence == null || item.confidence >= filters.min_confidence) &&
    ((filters.detection ?? "all") === "all" || filters.detection === item.detection)
  );
}

export function formatIssueDate(value: number) {
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(
    value,
  );
}

export function uniqueSourceLabels(labels: string[]) {
  return [...new Set(labels)].sort((left, right) => left.localeCompare(right));
}

export function matchesSourceLabels(
  item: Pick<IssueReviewItem, "source_labels"> | Pick<IssueListItem, "source_labels">,
  selected: string[],
  match: "any" | "all" = "any",
) {
  if (!selected.length) return true;
  const values = new Set(item.source_labels);
  return match === "all"
    ? selected.every((label) => values.has(label))
    : selected.some((label) => values.has(label));
}

export function splitSourceLabels(labels: string[], max: number) {
  const visible = labels.slice(0, Math.max(0, max));
  return { visible, hiddenCount: labels.length - visible.length };
}
