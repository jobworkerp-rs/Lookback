import type { SummaryKind } from "@/types/api";

/** Translation keys for each summary granularity, shared by the Summaries tab
 *  segments and the generate dialog so they can't drift apart. Resolve via the
 *  caller's `t()` — this module stays React-free. */
export const KIND_LABEL_KEYS: Record<SummaryKind, string> = {
  "per-thread": "summaryKind.per-thread",
  daily: "summaryKind.daily",
  weekly: "summaryKind.weekly",
  monthly: "summaryKind.monthly",
};

/** Dependency order, finest → coarsest. A coarser layer reads the one below
 *  it (daily reads per-thread summaries, weekly reads daily, monthly reads
 *  weekly), so the staged-generate checkboxes enforce: turning a layer ON
 *  turns every finer layer ON, and turning one OFF turns every coarser one OFF. */
export const KIND_ORDER: readonly SummaryKind[] = ["per-thread", "daily", "weekly", "monthly"];

export type KindSelection = Record<SummaryKind, boolean>;

/** Apply the dependency rule after toggling `changed` to `next`:
 *  ON  → also enable every finer layer (lower index).
 *  OFF → also disable every coarser layer (higher index). */
export function applyDependency(
  current: KindSelection,
  changed: SummaryKind,
  next: boolean,
): KindSelection {
  const idx = KIND_ORDER.indexOf(changed);
  const result: KindSelection = { ...current, [changed]: next };
  if (next) {
    for (let i = 0; i < idx; i++) result[KIND_ORDER[i] as SummaryKind] = true;
  } else {
    for (let i = idx + 1; i < KIND_ORDER.length; i++) {
      result[KIND_ORDER[i] as SummaryKind] = false;
    }
  }
  return result;
}

/** The coarsest selected granularity (drives which range picker is shown and
 *  which fallback period applies), or null when nothing is selected. */
export function topKind(selection: KindSelection): SummaryKind | null {
  for (let i = KIND_ORDER.length - 1; i >= 0; i--) {
    const k = KIND_ORDER[i] as SummaryKind;
    if (selection[k]) return k;
  }
  return null;
}
