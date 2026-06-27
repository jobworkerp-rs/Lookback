import type {
  PersonalityProfileContent,
  PersonalitySignalContent,
  SignalCommunicationStyle,
  SignalDecisionStyle,
  SignalPreference,
} from "@/types/api";

/**
 * A flattened, display-ready view of one signal category.
 *
 * The layer-1 signal JSON nests per-category objects with topic/confidence/
 * evidence etc. (see thread-personality-single.yaml `json_schema`). The
 * drawer renders the raw shape unreadable, so we flatten each entry into a
 * `primary` line (the salient claim, with confidence inline) plus an
 * optional `detail` (evidence / supporting text). Empty categories are
 * dropped so the UI only shows what was actually extracted.
 */
export interface SignalCategoryView {
  /** Category label, e.g. "Interests". */
  label: string;
  items: SignalItemView[];
}

export interface SignalItemView {
  primary: string;
  detail?: string;
  /** Source-thread memory ids cited as evidence; rendered as scroll links. */
  memoryIds?: string[];
  /** Profile-only: importance ("high"/"medium"/"low") shown as a badge. */
  weight?: string;
}

function confidenceSuffix(confidence?: string): string {
  const c = confidence?.trim();
  return c ? ` (${c})` : "";
}

function nonEmpty(s?: string): string | undefined {
  const t = s?.trim();
  return t ? t : undefined;
}

/** Normalize a `memory_ids` array to trimmed non-empty ids, or undefined. */
function memoryIds(ids?: string[]): string[] | undefined {
  const out = (ids ?? []).map((id) => id.trim()).filter((id) => id.length > 0);
  return out.length > 0 ? out : undefined;
}

/** Fields common to every list entry across BOTH observed shapes (schema
 *  names + the uniform category/description the LLM actually emits). */
interface ListEntry {
  category?: string;
  description?: string;
  confidence?: string;
  evidence?: string;
  memory_ids?: string[];
}

/**
 * Render one list entry. The primary line prefers the schema's salient field
 * (e.g. `topic`, `avoid`), falling back to the observed `category`. The
 * detail line prefers `description` (the observed body), falling back to
 * `evidence` (the schema's supporting text). Returns null when neither a
 * primary nor a detail is present.
 */
function listItem(row: ListEntry, schemaPrimary?: string): SignalItemView | null {
  const primary = nonEmpty(schemaPrimary) ?? nonEmpty(row.category);
  const detail = nonEmpty(row.description) ?? nonEmpty(row.evidence);
  const ids = memoryIds(row.memory_ids);
  // When only a description exists (no heading), promote it to primary so
  // the item isn't a dangling detail with no claim.
  if (!primary) {
    return detail ? { primary: detail, memoryIds: ids } : null;
  }
  return { primary: `${primary}${confidenceSuffix(row.confidence)}`, detail, memoryIds: ids };
}

function listCategory<T extends ListEntry>(
  label: string,
  rows: T[] | undefined,
  schemaPrimary: (row: T) => string | undefined,
): SignalCategoryView | null {
  const items = (rows ?? [])
    .map((row) => listItem(row, schemaPrimary(row)))
    .filter((v): v is SignalItemView => v !== null);
  return items.length > 0 ? { label, items } : null;
}

// The 6 schema-locked category labels, in presentation order. Shared by the
// signal drawer and the profile grid (and re-exported so the grid renders the
// same fixed slots the formatters emit — keeping them in one place avoids the
// silent-empty-category drift of two independently hard-coded copies).
export const PERSONA_CATEGORY_LABELS = [
  "Interests",
  "Preferences",
  "Decision style",
  "Communication style",
  "Values & beliefs",
  "Anti-preferences",
] as const;

// Per-category headline builders, shared between the signal and profile
// formatters (the two shapes reuse the same per-category interfaces, so the
// extraction rule is identical for both layers).
const preferencePrimary = (p: SignalPreference): string | undefined => {
  const pref = nonEmpty(p.preference);
  if (!pref) return undefined;
  const axis = nonEmpty(p.axis);
  return axis ? `${axis}: ${pref}` : pref;
};

const decisionStylePrimary = (ds: SignalDecisionStyle): string | undefined => {
  const summary = nonEmpty(ds.summary);
  const traits = (ds.traits ?? []).map(nonEmpty).filter((t): t is string => t !== undefined);
  return summary && traits.length > 0
    ? `${summary} · ${traits.join(" · ")}`
    : (summary ?? (traits.length > 0 ? traits.join(" · ") : undefined));
};

const communicationStylePrimary = (cs: SignalCommunicationStyle): string | undefined => {
  const head = [
    nonEmpty(cs.tone) && `tone: ${nonEmpty(cs.tone)}`,
    nonEmpty(cs.verbosity) && `verbosity: ${nonEmpty(cs.verbosity)}`,
    nonEmpty(cs.language_preference) && `language: ${nonEmpty(cs.language_preference)}`,
  ]
    .filter(Boolean)
    .join(" · ");
  return nonEmpty(head);
};

export function formatSignalContent(content: PersonalitySignalContent): SignalCategoryView[] {
  const views: (SignalCategoryView | null)[] = [
    listCategory("Interests", content.interests, (i) => i.topic),
    listCategory("Preferences", content.preferences, preferencePrimary),
    objectCategory("Decision style", content.decision_style, decisionStylePrimary),
    objectCategory("Communication style", content.communication_style, communicationStylePrimary),
    listCategory("Values & beliefs", content.values_and_beliefs, (b) => b.belief),
    listCategory("Anti-preferences", content.anti_preferences, (a) => a.avoid),
    reasonCategory(content.reason),
  ];
  return views.filter((v): v is SignalCategoryView => v !== null);
}

function reasonCategory(reason: string | undefined): SignalCategoryView | null {
  const text = nonEmpty(reason);
  return text ? { label: "Reason", items: [{ primary: text }] } : null;
}

/**
 * Render one of the single-object categories (decision_style /
 * communication_style). `schemaPrimary` builds the headline from the schema
 * fields; we fall back to the observed `description` for either the primary
 * (when no schema fields) or the detail (when both exist). `notes` is folded
 * into the detail for communication_style.
 */
function objectCategory<T extends { description?: string; notes?: string; memory_ids?: string[] }>(
  label: string,
  obj: T | undefined,
  schemaPrimary: (obj: T) => string | undefined,
): SignalCategoryView | null {
  if (!obj) return null;
  const primary = nonEmpty(schemaPrimary(obj));
  const description = nonEmpty(obj.description);
  const notes = nonEmpty(obj.notes);
  const detail = description ?? notes;
  const ids = memoryIds(obj.memory_ids);
  if (!primary) {
    return detail ? { label, items: [{ primary: detail, memoryIds: ids }] } : null;
  }
  return { label, items: [{ primary, detail, memoryIds: ids }] };
}

/**
 * Profile counterpart of `listCategory`: reuses `listItem` to build the
 * primary/detail/memoryIds, then attaches the profile-only `weight`.
 * `Array.isArray` guard makes the formatter robust against malformed stored
 * JSON — a non-array field yields an empty category rather than throwing (the
 * original blank-screen bug).
 */
function profileListCategory<
  T extends ListEntry & { weight?: string; supporting_source_thread_ids?: string[] },
>(
  label: string,
  rows: T[] | undefined,
  schemaPrimary: (row: T) => string | undefined,
): SignalCategoryView | null {
  const safe = Array.isArray(rows) ? rows : [];
  const items = safe
    .map((row): SignalItemView | null => {
      const base = listItem(row, schemaPrimary(row));
      if (!base) return null;
      return {
        ...base,
        weight: nonEmpty(row.weight),
      };
    })
    .filter((v): v is SignalItemView => v !== null);
  return items.length > 0 ? { label, items } : null;
}

/**
 * Profile counterpart of `objectCategory` (decision_style / communication_
 * style). These single objects carry no `weight`.
 */
function profileObjectCategory<
  T extends {
    description?: string;
    notes?: string;
    memory_ids?: string[];
    supporting_source_thread_ids?: string[];
  },
>(
  label: string,
  obj: T | undefined,
  schemaPrimary: (obj: T) => string | undefined,
): SignalCategoryView | null {
  return objectCategory(label, obj, schemaPrimary);
}

// Profiles reuse the layer-1 per-category interfaces; the only deltas are
// `weight` (a badge) and `supporting_source_thread_ids` (the chip nav target).
// Profiles have no `reason` category. Labels match PERSONA_CATEGORY_LABELS (a
// drift test guards the correspondence).
export function formatProfileContent(content: PersonalityProfileContent): SignalCategoryView[] {
  const views: (SignalCategoryView | null)[] = [
    profileListCategory("Interests", content.interests, (i) => i.topic),
    profileListCategory("Preferences", content.preferences, preferencePrimary),
    profileObjectCategory("Decision style", content.decision_style, decisionStylePrimary),
    profileObjectCategory(
      "Communication style",
      content.communication_style,
      communicationStylePrimary,
    ),
    profileListCategory("Values & beliefs", content.values_and_beliefs, (b) => b.belief),
    profileListCategory("Anti-preferences", content.anti_preferences, (a) => a.avoid),
  ];
  return views.filter((v): v is SignalCategoryView => v !== null);
}

/**
 * Fallback for signals that `formatSignalContent` can't surface (e.g. a
 * thin `no_signal:false` payload with empty categories): pretty-print the
 * raw content with the boolean flag stripped, so the row always shows
 * *something* rather than "no displayable content". Returns null only when
 * there is genuinely nothing left after dropping `no_signal`. Falls back to
 * the raw string when the content isn't valid JSON.
 */
export function rawSignalFallback(contentJson: string): string | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(contentJson);
  } catch {
    return nonEmpty(contentJson) ?? null;
  }
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    const { no_signal: _drop, ...rest } = parsed as Record<string, unknown>;
    if (Object.keys(rest).length === 0) return null;
    return JSON.stringify(rest, null, 2);
  }
  return nonEmpty(JSON.stringify(parsed)) ?? null;
}
