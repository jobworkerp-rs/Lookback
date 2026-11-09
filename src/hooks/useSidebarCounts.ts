import { useQuery } from "@tanstack/react-query";
import { countActiveThreadGroups, countThreadGroupSummaries, getPersonality } from "@/api";
import { PERSONALITY_QUERY_KEY } from "@/lib/queryKeys";

/**
 * Generated-refresh and import events invalidate these counts as soon as the
 * underlying data changes, so the timer is only a backstop for changes made
 * outside this app. Each refetch scans server-side data, which is why it must
 * not run on a short interval on every screen.
 */
export const SIDEBAR_COUNT_SAFETY_REFETCH_MS = 5 * 60_000;

export interface SidebarCounts {
  threadCount: number | null;
  threadCountTruncated: boolean;
  threadGroupCount: number | null;
  summaryCount: number | null;
}

export function useSidebarCounts(enabled: boolean): SidebarCounts {
  // The thread count piggybacks on the Personality query: `get_personality`
  // already returns `thread_count`, and sharing the key dedupes with the
  // Personality tab.
  const personality = useQuery({
    queryKey: PERSONALITY_QUERY_KEY,
    queryFn: () => getPersonality({ user_id: 1 }),
    enabled,
    refetchInterval: SIDEBAR_COUNT_SAFETY_REFETCH_MS,
  });
  const summaryCount = useQuery({
    queryKey: ["count-thread-group-summaries", 1],
    queryFn: () => countThreadGroupSummaries(1),
    enabled,
    refetchInterval: SIDEBAR_COUNT_SAFETY_REFETCH_MS,
  });
  const threadGroupCount = useQuery({
    queryKey: ["count-thread-groups", 1],
    queryFn: () => countActiveThreadGroups(1),
    enabled,
    refetchInterval: SIDEBAR_COUNT_SAFETY_REFETCH_MS,
    retry: false,
  });
  return {
    threadCount: personality.data?.thread_count ?? null,
    threadCountTruncated: personality.data?.thread_count_truncated ?? false,
    threadGroupCount: threadGroupCount.data ?? null,
    summaryCount: summaryCount.data ?? null,
  };
}
