import type { QueryClient } from "@tanstack/react-query";
import { PERSONALITY_QUERY_KEY } from "@/lib/queryKeys";
import type { GeneratedRefreshScope } from "@/types/api";

function invalidate(queryClient: QueryClient, queryKey: readonly unknown[]) {
  void queryClient.invalidateQueries({ queryKey });
}

export function refreshGeneratedCaches(
  queryClient: QueryClient,
  scopes: readonly GeneratedRefreshScope[],
): void {
  const unique = new Set(scopes);

  if (unique.has("thread_summary")) {
    invalidate(queryClient, ["threads"]);
    invalidate(queryClient, ["thread-search"]);
    invalidate(queryClient, ["distinct-labels"]);
    invalidate(queryClient, ["co-occurring-labels"]);
    invalidate(queryClient, ["summaries"]);
    invalidate(queryClient, ["count-summaries"]);
    invalidate(queryClient, ["summary-search"]);
    invalidate(queryClient, ["summary-distinct-labels"]);
    invalidate(queryClient, ["summary-co-occurring-labels"]);
    invalidate(queryClient, ["summary-hit"]);
    invalidate(queryClient, ["memories"]);
  }

  if (
    unique.has("daily_summary") ||
    unique.has("weekly_summary") ||
    unique.has("monthly_summary")
  ) {
    invalidate(queryClient, ["summaries"]);
    invalidate(queryClient, ["summary-period-keys"]);
    invalidate(queryClient, ["summary-search"]);
    invalidate(queryClient, ["count-summaries"]);
    invalidate(queryClient, ["summary-hit"]);
  }

  if (unique.has("personality")) {
    invalidate(queryClient, PERSONALITY_QUERY_KEY);
    invalidate(queryClient, ["personality-signals"]);
    invalidate(queryClient, ["memories"]);
  }

  if (unique.has("reflection")) {
    invalidate(queryClient, ["reflections"]);
    invalidate(queryClient, ["memories"]);
  }
}

export function refreshImportedThreadCaches(queryClient: QueryClient): void {
  invalidate(queryClient, ["threads"]);
  invalidate(queryClient, ["thread-search"]);
  invalidate(queryClient, ["distinct-labels"]);
  invalidate(queryClient, ["co-occurring-labels"]);
  invalidate(queryClient, PERSONALITY_QUERY_KEY);
  invalidate(queryClient, ["memories"]);
}
