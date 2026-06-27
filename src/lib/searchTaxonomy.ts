// Frontend label mapping for proto enum integers. See the proto files
// under `proto/llm_memory/data/reflection.proto` for the authoritative
// declarations. Keeping a separate lookup here (rather than importing
// generated TS types) avoids pulling in a protobuf runtime for a
// handful of constants. The display labels live in the i18n dictionaries
// under `taxonomy.*`; this module just maps an enum integer to its key and
// resolves it via the caller's `t` (so it stays React-free).

import type { TFunction } from "i18next";

/** Highest declared enum value per table, used to detect out-of-range ints. */
const TASK_CATEGORY_MAX = 5;
const OUTCOME_MAX = 5;
const REFLECTION_ASPECT_MAX = 3;

/// New proto enum values surface as `?(N)` until the dictionary is updated —
/// visible enough to prompt a fix without crashing the UI.
function resolve(t: TFunction, ns: string, value: number, max: number): string {
  if (value < 0 || value > max) return t("taxonomy.unknown", { value });
  return t(`taxonomy.${ns}.${value}`);
}

export function taskCategoryLabel(t: TFunction, value: number): string {
  return resolve(t, "taskCategory", value, TASK_CATEGORY_MAX);
}

export function outcomeLabel(t: TFunction, value: number): string {
  return resolve(t, "outcome", value, OUTCOME_MAX);
}

export function reflectionAspectLabel(t: TFunction, value: number): string {
  return resolve(t, "reflectionAspect", value, REFLECTION_ASPECT_MAX);
}

/** Outcome enum integers that are user-selectable filters (drops the proto3
 *  zero `Unspecified`, which is never displayed as a filter). */
export const OUTCOME_FILTER_VALUES: readonly number[] = [1, 2, 3, 4, 5];
