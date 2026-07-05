import type { LabelWithCount } from "@/types/api";

/** Internal bucket key for prefix-less labels. Language-independent so it never
 *  collides with a real label prefix or leaks as a hardcoded UI string — the
 *  display name is resolved via `t("labelPrefix.category")` at render time. */
export const CATEGORY_GROUP_KEY = "__category__";

export interface LabelGroup {
  prefix: string;
  labels: LabelWithCount[];
}

export function splitLabelPrefix(label: string): { prefix: string | null; value: string } {
  const idx = label.indexOf(":");
  if (idx < 0) return { prefix: null, value: label };
  return { prefix: label.slice(0, idx), value: label.slice(idx + 1) };
}

/** The single source of truth for the *prefix* order, shared by the filter
 *  bar sections (`groupByPrefix`) and a thread's own chips
 *  (`sortLabelsByPrefixPriority`): curated category labels first, then the
 *  location/tooling axes agent → provider → vault → branch → dir → path. Any
 *  prefix not listed here sorts after these, by prefix name ascending.
 *  `CATEGORY_GROUP_KEY` stands in for prefix-less labels (see
 *  `splitLabelPrefix`). Only the prefix order is shared — each consumer orders
 *  labels *within* a prefix its own way (see each function's doc). */
export const LABEL_PREFIX_PRIORITY = [
  CATEGORY_GROUP_KEY,
  "agent",
  "provider",
  "vault",
  "branch",
  "dir",
  "path",
] as const;

/** Rank a prefix by `LABEL_PREFIX_PRIORITY`; unknown prefixes share the same
 *  (largest) rank so they land after every known prefix, then break ties on
 *  the prefix name at the call site. */
function prefixRank(prefix: string | null): number {
  const key = prefix ?? CATEGORY_GROUP_KEY;
  const idx = (LABEL_PREFIX_PRIORITY as readonly string[]).indexOf(key);
  return idx < 0 ? LABEL_PREFIX_PRIORITY.length : idx;
}

/** Bucket the aggregate distinct-label list into per-prefix sections for the
 *  filter bar. Sections follow `LABEL_PREFIX_PRIORITY`; within a section labels
 *  are sorted by name, so the display stays stable regardless of usage. */
export function groupByPrefix(labels: LabelWithCount[]): LabelGroup[] {
  const buckets = new Map<string, LabelWithCount[]>();
  for (const l of labels) {
    const { prefix } = splitLabelPrefix(l.label);
    const key = prefix ?? CATEGORY_GROUP_KEY;
    const list = buckets.get(key);
    if (list) list.push(l);
    else buckets.set(key, [l]);
  }
  for (const list of buckets.values()) {
    list.sort((a, b) => a.label.localeCompare(b.label));
  }
  return [...buckets]
    .map(([prefix, labels]) => ({ prefix, labels }))
    .sort(
      (a, b) => prefixRank(a.prefix) - prefixRank(b.prefix) || a.prefix.localeCompare(b.prefix),
    );
}

/**
 * Order a single thread's chips by `LABEL_PREFIX_PRIORITY`, returning a flat
 * `string[]` (unlike `groupByPrefix`, which returns per-prefix sections from
 * the aggregate list). Within one prefix, input order is preserved via the
 * stable sort — no name sort here, so same-prefix chips keep their given
 * order.
 */
export function sortLabelsByPrefixPriority(labels: string[]): string[] {
  // The localeCompare only ever splits distinct unknown prefixes; known
  // prefixes never share a rank, and same-rank category labels keep input
  // order (stable sort), so no explicit index tiebreak is needed.
  return [...labels].sort((a, b) => {
    const pa = splitLabelPrefix(a).prefix;
    const pb = splitLabelPrefix(b).prefix;
    return prefixRank(pa) - prefixRank(pb) || (pa ?? "").localeCompare(pb ?? "");
  });
}
