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

/** No-prefix labels are surfaced first as a single category section so
 *  curated labels (`summary`, `coding_agent`, ...) don't get buried under
 *  prefix-dumped lists. Remaining prefixes follow by descending label
 *  count — a `dir` group with many distinct paths is a more useful
 *  filter axis than a one-off prefix. */
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
    list.sort((a, b) => b.thread_count - a.thread_count || a.label.localeCompare(b.label));
  }
  const category = buckets.get(CATEGORY_GROUP_KEY);
  const rest = [...buckets]
    .filter(([k]) => k !== CATEGORY_GROUP_KEY)
    .map(([prefix, labels]) => ({ prefix, labels }))
    .sort((a, b) => b.labels.length - a.labels.length || a.prefix.localeCompare(b.prefix));
  return category ? [{ prefix: CATEGORY_GROUP_KEY, labels: category }, ...rest] : rest;
}
