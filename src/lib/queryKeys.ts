export const PERSONALITY_QUERY_KEY = ["personality", 1] as const;

/** Prefix shared by the per-group and batched summary reads, so one
 * invalidation refreshes both. */
export const THREAD_GROUP_SUMMARY_PREFIX = ["thread-group-summary"] as const;

/** Per-group summary entry; the batch read seeds exactly this key, so the
 * detail pane must read through it too. */
export function threadGroupSummaryKey(
  groupId: string | null | undefined,
  ownerUserId: number | undefined,
) {
  return [...THREAD_GROUP_SUMMARY_PREFIX, groupId, ownerUserId] as const;
}

export function threadGroupSummaryBatchKey(groups: readonly { id: string; user_id: number }[]) {
  return [
    ...THREAD_GROUP_SUMMARY_PREFIX,
    "batch",
    groups.map((group) => [group.id, group.user_id] as const),
  ] as const;
}
