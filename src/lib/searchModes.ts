import type { TFunction } from "i18next";
import { searchMemoriesHybrid, searchMemoriesKeyword, searchMemoriesSemantic } from "@/api";
import type { SearchMode, SearchThreadsRequest, ThreadHit } from "@/types/api";

// Per-mode search behavior in one place so the RPC call and empty-state hint
// don't drift apart as modes are added. `hintKey` is a translation key resolved
// by the caller's `t()`; this module stays React-free.
export const SEARCH_MODES: Record<
  SearchMode,
  {
    fn: (req: SearchThreadsRequest) => Promise<ThreadHit[]>;
    hintKey: string;
  }
> = {
  keyword: { fn: searchMemoriesKeyword, hintKey: "search.hint.keyword" },
  semantic: { fn: searchMemoriesSemantic, hintKey: "search.hint.semantic" },
  hybrid: { fn: searchMemoriesHybrid, hintKey: "search.hint.hybrid" },
};

/** Modes that embed the query, so they're unavailable while the local vector
 *  store is degraded. Keyword (BM25/FTS) is dimension-independent and stays on.
 *  Single source of truth for the search-panel disable logic. */
export function isEmbeddingSearchMode(mode: SearchMode): boolean {
  return mode === "semantic" || mode === "hybrid";
}

const SHORT_QUERY_ERROR_RE = /\b(short|too few|min(?:imum)?|token|empty|invalid argument)\b/i;
const EMBEDDING_WORKER_ERROR_RE =
  /\b(embedding|multimodalembeddingrunner|runner|worker|model|metal)\b/i;

export type SearchErrorHint =
  | { kind: "shortQuery" }
  | { kind: "embeddingWorker"; mode: "Semantic" | "Hybrid" }
  | null;

/**
 * Classify a search error into a translation-ready hint descriptor (or null when
 * no hint applies). The caller maps `kind` to `search.errorHint.*` via `t()`,
 * keeping this module free of UI strings.
 */
export function searchErrorHint(mode: SearchMode, message: string): SearchErrorHint {
  if (mode === "keyword") return null;
  if (SHORT_QUERY_ERROR_RE.test(message)) return { kind: "shortQuery" };
  if (EMBEDDING_WORKER_ERROR_RE.test(message)) {
    return { kind: "embeddingWorker", mode: mode === "semantic" ? "Semantic" : "Hybrid" };
  }
  return null;
}

/** Resolve a search error into a localized hint string (or null). Shared by the
 *  Threads and Summaries search panels so the mapping lives in one place. */
export function resolveSearchErrorHint(
  t: TFunction,
  mode: SearchMode,
  message: string,
): string | null {
  const hint = searchErrorHint(mode, message);
  if (!hint) return null;
  if (hint.kind === "shortQuery") return t("search.errorHint.shortQuery");
  return t("search.errorHint.embeddingWorker", { mode: hint.mode });
}
