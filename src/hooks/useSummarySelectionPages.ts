import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listReflectionSelection, listSummariesForSelection } from "@/api";
import type { ReflectionSelectionEntry, SelectableMemoryKind, SummaryEntry } from "@/types/api";

const PAGE_SIZE = 50;

export type SelectionEntry = SummaryEntry | ReflectionSelectionEntry;

function pageRequest(kind: SelectableMemoryKind, offset: number, cursor: string | undefined) {
  if (kind === "reflection") {
    return listReflectionSelection(
      cursor === undefined
        ? { limit: PAGE_SIZE }
        : { limit: PAGE_SIZE, cursor_after_memory_id: cursor },
    );
  }
  return listSummariesForSelection({ kind, limit: PAGE_SIZE, offset });
}

/** Owns a tab's page continuation and rejects responses from a superseded tab. */
export function useSummarySelectionPages() {
  const [kind, setKind] = useState<SelectableMemoryKind>("per-thread");
  const activeKind = useRef(kind);
  const [entries, setEntries] = useState<SelectionEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(false);
  const [nextOffset, setNextOffset] = useState(0);
  const [nextCursor, setNextCursor] = useState<string | undefined>();
  const [error, setError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const requestKey = useMemo(() => ({ kind, reloadKey }), [kind, reloadKey]);
  const sessionGeneration = useRef(0);

  const updateContinuation = useCallback((response: Awaited<ReturnType<typeof pageRequest>>) => {
    if ("next_cursor_after_memory_id" in response) {
      setHasMore(response.next_cursor_after_memory_id !== null);
      setNextCursor(response.next_cursor_after_memory_id ?? undefined);
      return;
    }
    setHasMore(response.next_offset !== null);
    setNextOffset(response.next_offset ?? 0);
  }, []);
  const isCurrentGeneration = useCallback(
    (generation: number) => generation === sessionGeneration.current,
    [],
  );

  useEffect(() => {
    let cancelled = false;
    const requestGeneration = sessionGeneration.current;
    setLoading(true);
    setError(null);
    setEntries([]);
    setHasMore(false);
    setNextOffset(0);
    setNextCursor(undefined);
    setLoadingMore(false);
    void pageRequest(requestKey.kind, 0, undefined)
      .then((response) => {
        if (cancelled || !isCurrentGeneration(requestGeneration)) return;
        setEntries(response.entries);
        updateContinuation(response);
      })
      .catch((reason) => {
        if (!cancelled && isCurrentGeneration(requestGeneration)) {
          setError(reason instanceof Error ? reason.message : String(reason));
        }
      })
      .finally(() => {
        if (!cancelled && isCurrentGeneration(requestGeneration)) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [isCurrentGeneration, requestKey, updateContinuation]);

  const selectKind = useCallback((nextKind: SelectableMemoryKind) => {
    if (nextKind === activeKind.current) return;
    sessionGeneration.current += 1;
    activeKind.current = nextKind;
    setKind(nextKind);
  }, []);

  const retry = useCallback(() => {
    sessionGeneration.current += 1;
    setReloadKey((current) => current + 1);
  }, []);
  const isCurrentKind = useCallback(
    (candidate: SelectableMemoryKind) => candidate === activeKind.current,
    [],
  );

  const loadMore = useCallback(() => {
    if (loadingMore || !hasMore) return;
    const requestedKind = kind;
    const requestedOffset = nextOffset;
    const requestedCursor = nextCursor;
    const requestGeneration = sessionGeneration.current;
    setLoadingMore(true);
    void pageRequest(requestedKind, requestedOffset, requestedCursor)
      .then((response) => {
        if (!isCurrentGeneration(requestGeneration)) return;
        setEntries((current) => [...current, ...response.entries]);
        updateContinuation(response);
      })
      .catch((reason) => {
        if (isCurrentGeneration(requestGeneration)) {
          setError(reason instanceof Error ? reason.message : String(reason));
        }
      })
      .finally(() => {
        if (isCurrentGeneration(requestGeneration)) setLoadingMore(false);
      });
  }, [hasMore, isCurrentGeneration, kind, loadingMore, nextCursor, nextOffset, updateContinuation]);

  return {
    entries,
    error,
    hasMore,
    isCurrentKind,
    kind,
    loadMore,
    loading,
    loadingMore,
    retry,
    selectKind,
    setError,
  };
}
