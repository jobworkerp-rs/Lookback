import type { MemoryRow } from "@/types/api";

// Hit ± 50 rows on first load. The thread fetch was previously a single
// limit:200 call without issues, so a 100-row page is comfortably light.
export const PAGE_SIZE = 100;

/**
 * Offset of the first page. Centers the window on the search hit so it lands
 * mid-viewport; falls back to the thread start for plain browsing (no hit).
 * `find_memories_by_thread_id` orders by `tm.position ASC`, so offset == the
 * thread-internal position carried by `ThreadHit.top_position`.
 */
export function initialOffset(highlightPosition: number | null | undefined): number {
  if (highlightPosition == null) return 0;
  return Math.max(0, highlightPosition - Math.floor(PAGE_SIZE / 2));
}

/** Next (downward) page offset, or undefined at the end of the thread. */
export function nextOffset(lastLen: number, lastParam: number, total?: number): number | undefined {
  if (lastLen < PAGE_SIZE) return undefined;
  const next = lastParam + PAGE_SIZE;
  if (total != null && next >= total) return undefined;
  return next;
}

/** Previous (upward) page offset, or undefined once the top is reached. */
export function prevOffset(firstParam: number): number | undefined {
  if (firstParam <= 0) return undefined;
  return Math.max(0, firstParam - PAGE_SIZE);
}

/**
 * Flatten infinite-query pages into one ordered list. Pages are already in
 * position order (previous pages prepended, next pages appended by the query),
 * so this just concatenates; the id Set guards against any overlap.
 */
export function flattenMemories(pages: MemoryRow[][]): MemoryRow[] {
  const seen = new Set<string>();
  const out: MemoryRow[] = [];
  for (const page of pages) {
    for (const m of page) {
      if (seen.has(m.id)) continue;
      seen.add(m.id);
      out.push(m);
    }
  }
  return out;
}

/**
 * scrollTop that keeps the viewport anchored after rows are prepended.
 * `savedBottomDistance` is the pre-prepend `scrollHeight - scrollTop`; keeping
 * that distance constant means the previously-visible rows don't jump.
 */
export function prependScrollTop(newScrollHeight: number, savedBottomDistance: number): number {
  return newScrollHeight - savedBottomDistance;
}
