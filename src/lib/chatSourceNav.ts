import { findMemoryPosition } from "@/api";
import type { ThreadHighlight } from "@/components/ThreadDetail";

/**
 * Resolve a memory's position within its thread so `ThreadDetail` can scroll
 * to it, reusing the same highlight shape the Threads-search hit path uses.
 *
 * Best-effort: a failed/absent lookup falls back to a memoryId-only
 * highlight that still opens the thread (paging will start from the top
 * instead of the hit's neighborhood, but the row is still findable).
 *
 * Shared by `pages/Personality.tsx` (signal / profile chips) and
 * `pages/Chat.tsx` (RAG source pills) so both jump paths produce
 * identical Modal behaviour.
 */
export async function resolveThreadHighlight(
  threadId: string,
  memoryId: string,
): Promise<ThreadHighlight> {
  try {
    const pos = await findMemoryPosition({ thread_id: threadId, memory_id: memoryId });
    if (pos) return { memoryId, position: pos.position, threadTotal: pos.thread_total };
  } catch {
    // Fall through with the memoryId-only highlight.
  }
  return { memoryId };
}
