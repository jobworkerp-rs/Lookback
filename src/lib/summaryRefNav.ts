import { resolveSummaryMemoryRef } from "@/api";
import type { OpenThreadState } from "@/components/ThreadDetail";
import { resolveThreadHighlight } from "@/lib/chatSourceNav";
import { classifyPeriodKey } from "@/lib/summaryPeriod";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type { SummariesFocus } from "@/pages/Summaries";

/**
 * Navigation intent computed from a `source_memory_ids` chip click. The
 * caller (Summaries page) dispatches on `kind`:
 *
 * - `open-thread`: the chip pointed at a per-thread summary, so open the
 *   underlying conversation thread in the existing `ThreadDetail` modal.
 *   `highlight` is the same shape the search-hit / personality paths use.
 * - `open-summaries-focus`: the chip pointed at a period (daily / weekly /
 *   monthly) summary, so jump within the Summaries tab to the cited
 *   calendar card. Mirrors the `Chat` tab's `period_summary` deep-link.
 */
export type SummaryRefNavTarget =
  | { kind: "open-thread"; open: OpenThreadState }
  | { kind: "open-summaries-focus"; focus: SummariesFocus };

/**
 * Resolve a `source_memory_ids` chip into a navigation intent. The cited
 * memory's `external_id` is parsed on the Rust side; here we map it onto a
 * UI action. Returns `null` when:
 *
 * - the memory no longer exists (deleted), or
 * - the external_id doesn't match a known summary prefix (legacy / wrong
 *   namespace), or
 * - a period summary's `period_key` can't be classified into a calendar
 *   tuple.
 *
 * In all null cases the caller renders a disabled chip with a tooltip.
 */
export async function resolveSummaryRefNavigation(
  memoryId: string,
): Promise<SummaryRefNavTarget | null> {
  const ref = await resolveSummaryMemoryRef(memoryId);
  if (!ref) return null;

  if (ref.kind === "per-thread") {
    // `summary:<thread_id>` carries the conversation thread directly; open
    // the modal and let `resolveThreadHighlight` seed the scroll position
    // (best-effort — same helper Lookback's signal / chat paths use).
    const threadId = ref.thread_id;
    if (!threadId) return null;
    const thread = synthesizeThreadSummary({ id: threadId });
    const highlight = await resolveThreadHighlight(threadId, ref.memory_id);
    return { kind: "open-thread", open: { thread, highlight } };
  }

  if (ref.kind && ref.period_key) {
    // Reuse `classifyPeriodKey` (Chat's period-summary deep-link helper) so
    // the calendar token parsing rules stay shared. The classifier's `kind`
    // is the source of truth — if it disagrees with the server's parsed
    // `ref.kind` the token shape is the better signal of where to navigate.
    const cls = classifyPeriodKey(ref.period_key);
    if (!cls) return null;
    return {
      kind: "open-summaries-focus",
      focus: { kind: cls.kind, month: cls.month, periodKey: ref.period_key },
    };
  }

  return null;
}
