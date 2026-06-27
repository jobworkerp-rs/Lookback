import { createContext, useContext } from "react";
import { MarkdownBody } from "@/components/MarkdownMessage";
import { MemoryRefs } from "@/components/MemoryRefs";
import type { SummaryContent, SummaryValue } from "@/types/api";

/** Keys that, when present, are promoted to the card title rather than shown
 *  as a labelled section. Covers the English schema key and its JA aliases. */
const TITLE_KEYS = ["title", "タイトル", "件名"];

function isTitleKey(key: string): boolean {
  return TITLE_KEYS.includes(key);
}

/** Reference-array keys defined in the summary workflow YAML schemas
 *  (daily/weekly/monthly: `purpose_groups[].source_memory_ids`,
 *  `trends[].source_memory_ids`; `source_thread_ids` is used on the
 *  thread-side at the workflow root). When an array under these keys is a
 *  flat list of stringified i64 ids, render it as clickable chips that
 *  open the cited thread / period card instead of a generic bullet list. */
const SOURCE_MEMORY_IDS_KEY = "source_memory_ids";
const SOURCE_THREAD_IDS_KEY = "source_thread_ids";
type ReferenceArrayKey = typeof SOURCE_MEMORY_IDS_KEY | typeof SOURCE_THREAD_IDS_KEY;

/** Navigation injected by the Summaries page so reference chips can open
 *  the cited thread / period. Both fields are optional: an absent handler
 *  degrades the chip to a static span rather than rendering a broken link. */
export interface SummaryRefHandlers {
  onOpenThread?: (threadId: string) => void;
  onOpenMemoryRef?: (memoryId: string) => void;
}

const SummaryRefHandlersContext = createContext<SummaryRefHandlers | null>(null);

/** Provider scope for the reference-chip handlers. Wrap the Summaries page
 *  subtree so `SummaryBody`'s nested `SummaryRefChips` and the per-thread
 *  card heading can pick them up without prop drilling through every list
 *  / card wrapper. */
export function SummaryRefHandlersProvider({
  value,
  children,
}: {
  value: SummaryRefHandlers;
  children: React.ReactNode;
}) {
  return (
    <SummaryRefHandlersContext.Provider value={value}>
      {children}
    </SummaryRefHandlersContext.Provider>
  );
}

export function useSummaryRefHandlers(): SummaryRefHandlers {
  return useContext(SummaryRefHandlersContext) ?? {};
}

/** `"title":"value"` matchers, precompiled once (keys are static) so the
 *  snippet scrape in `extractSummaryTitle` doesn't rebuild them per call. */
const TITLE_SCRAPE_PATTERNS: ReadonlyArray<RegExp> = TITLE_KEYS.map(
  (key) => new RegExp(`"${key}"\\s*:\\s*"((?:[^"\\\\]|\\\\.)*)"`),
);

/** Render a parsed summary object generically: each key becomes a labelled
 *  section, regardless of whether the keys are the English schema fields or
 *  the per-category Japanese labels the LLM emits. Falls back to the raw body
 *  only when the content is genuinely not a JSON object. */
export function SummaryBody({
  parsed,
  raw,
}: {
  parsed: Record<string, SummaryValue> | null;
  raw: string;
}) {
  if (parsed == null) {
    return (
      <div className="sum-section-body message-body">
        <MarkdownBody>{raw}</MarkdownBody>
      </div>
    );
  }

  const entries = Object.entries(parsed).filter(([, v]) => !isEmptyValue(v));
  if (entries.length === 0) {
    return (
      <div className="sum-section-body message-body">
        <MarkdownBody>{raw}</MarkdownBody>
      </div>
    );
  }

  const titleEntry = entries.find(([k]) => isTitleKey(k));
  return (
    <>
      {titleEntry && <div className="sum-title">{stringifyScalar(titleEntry[1])}</div>}
      {entries
        .filter(([k]) => !isTitleKey(k))
        .map(([key, value]) => (
          <div key={key} className="sum-section">
            <div className="sum-section-label">{key}</div>
            <SummaryValueView value={value} parentKey={key} />
          </div>
        ))}
    </>
  );
}

/** Render a single summary field value by its runtime shape: list → bullets,
 *  nested object → indented label/value rows, scalar → text.
 *
 *  `parentKey` carries the directly-enclosing object key (or — for elements
 *  of an array — the array's own enclosing key, propagated so nested
 *  `purpose_groups[].source_memory_ids` is still recognised). */
function SummaryValueView({ value, parentKey }: { value: SummaryValue; parentKey?: string }) {
  if (Array.isArray(value)) {
    const items = value.filter((v) => !isEmptyValue(v));
    if (items.length === 0) return null;
    const refIds = extractReferenceIds(parentKey, items);
    if (refIds) {
      return <SummaryRefChips kind={refIds.kind} ids={refIds.ids} />;
    }
    return (
      <ul className="sum-section-body" style={{ margin: 0, paddingLeft: 18 }}>
        {items.map((item, i) => (
          // Summary array items are content, not a stable identity set.
          // biome-ignore lint/suspicious/noArrayIndexKey: order-stable display list
          <li key={i}>
            {typeof item === "object" && item != null ? (
              <SummaryValueView value={item} parentKey={parentKey} />
            ) : (
              stringifyScalar(item)
            )}
          </li>
        ))}
      </ul>
    );
  }
  if (value != null && typeof value === "object") {
    const nested = Object.entries(value).filter(([, v]) => !isEmptyValue(v));
    return (
      <div style={{ paddingLeft: 8 }}>
        {nested.map(([k, v]) => (
          <div key={k} className="sum-section" style={{ marginTop: 4 }}>
            <div className="sum-section-label">{k}</div>
            <SummaryValueView value={v} parentKey={k} />
          </div>
        ))}
      </div>
    );
  }
  // The `summary` field (and JA category variants) is markdown with
  // `## 目的` style headings; render it instead of dumping raw markdown.
  // Short enum-ish scalars (category/status) render harmlessly as plain text.
  return (
    <div className="sum-section-body message-body">
      <MarkdownBody>{stringifyScalar(value)}</MarkdownBody>
    </div>
  );
}

/** Return `{ kind, ids }` when the items form a reference array (chip path),
 *  else null (generic bullet path). Filters to non-empty string items so the
 *  caller gets a fully-typed `string[]` without an unsafe cast. */
function extractReferenceIds(
  parentKey: string | undefined,
  items: SummaryValue[],
): { kind: ReferenceArrayKey; ids: string[] } | null {
  if (parentKey !== SOURCE_MEMORY_IDS_KEY && parentKey !== SOURCE_THREAD_IDS_KEY) return null;
  const ids: string[] = [];
  for (const v of items) {
    // Schema guarantees `array<string>`; bail to the generic path if the
    // LLM returned a non-stringified element (rare but observed when
    // prompts drift).
    if (typeof v !== "string" || v.length === 0) return null;
    ids.push(v);
  }
  return ids.length > 0 ? { kind: parentKey, ids } : null;
}

function SummaryRefChips({ kind, ids }: { kind: ReferenceArrayKey; ids: string[] }) {
  const handlers = useContext(SummaryRefHandlersContext);
  const isThread = kind === SOURCE_THREAD_IDS_KEY;
  return (
    <MemoryRefs
      ids={ids}
      onOpen={isThread ? handlers?.onOpenThread : handlers?.onOpenMemoryRef}
      titlePrefix={isThread ? "thread" : "memory"}
    />
  );
}

function stringifyScalar(value: SummaryValue): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  return String(value);
}

function isEmptyValue(value: SummaryValue): boolean {
  if (value == null) return true;
  if (typeof value === "string") return value.trim() === "";
  if (Array.isArray(value)) return value.every(isEmptyValue);
  return false;
}

/**
 * Best-effort summary title for a collapsed header. Prefers a parsed title
 * key; otherwise scrapes `"title":"..."` from the raw body even when it's a
 * truncated search snippet (the JSON won't parse, but the title field is
 * near the front and usually survives the cut). Returns null when nothing
 * usable is found so the caller can fall back to the thread name.
 */
export function extractSummaryTitle({ parsed, raw }: SummaryContent): string | null {
  if (parsed) {
    for (const [k, v] of Object.entries(parsed)) {
      if (isTitleKey(k) && typeof v === "string" && v.trim() !== "") return v;
    }
  }
  for (const pattern of TITLE_SCRAPE_PATTERNS) {
    const m = pattern.exec(raw);
    if (m?.[1]) {
      // The capture is still JSON-escaped (\", \n, \uXXXX); decode it as a
      // JSON string. The pattern keeps the value's quotes balanced, so this
      // can't parse beyond the title.
      try {
        return JSON.parse(`"${m[1]}"`);
      } catch {
        return m[1];
      }
    }
  }
  return null;
}
