import {
  type SetStateAction,
  useCallback,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
} from "react";
import {
  listReflectionSelection,
  listSummariesForSelection,
  listSummaryPathCandidates,
} from "@/api";
import type {
  ReflectionSelectionEntry,
  SelectableMemoryKind,
  SummaryEntry,
  SummaryPathCandidate,
} from "@/types/api";

const PAGE_SIZE = 50;

export type SelectionEntry = SummaryEntry | ReflectionSelectionEntry;

type PeriodScope = "overall" | "path";

type PeriodFilters = {
  periodScope: PeriodScope;
  includeOverall: boolean;
  pathBucketDigests: string[];
  includeLegacy: boolean;
};

type PeriodFilterAction =
  | { field: "periodScope"; next: SetStateAction<PeriodScope> }
  | { field: "includeOverall"; next: SetStateAction<boolean> }
  | { field: "pathBucketDigests"; next: SetStateAction<string[]> }
  | { field: "includeLegacy"; next: SetStateAction<boolean> };

const INITIAL_PERIOD_FILTERS: PeriodFilters = {
  periodScope: "overall",
  includeOverall: true,
  pathBucketDigests: [],
  includeLegacy: false,
};

function periodFiltersReducer(state: PeriodFilters, action: PeriodFilterAction): PeriodFilters {
  switch (action.field) {
    case "periodScope": {
      const next = typeof action.next === "function" ? action.next(state.periodScope) : action.next;
      return { ...state, periodScope: next };
    }
    case "includeOverall": {
      const next =
        typeof action.next === "function" ? action.next(state.includeOverall) : action.next;
      return { ...state, includeOverall: next };
    }
    case "pathBucketDigests": {
      const next =
        typeof action.next === "function" ? action.next(state.pathBucketDigests) : action.next;
      return { ...state, pathBucketDigests: next };
    }
    case "includeLegacy": {
      const next =
        typeof action.next === "function" ? action.next(state.includeLegacy) : action.next;
      return { ...state, includeLegacy: next };
    }
  }
}

function pageRequest(
  kind: SelectableMemoryKind,
  offset: number,
  cursor: string | undefined,
  periodScope: PeriodScope,
  includeOverall: boolean,
  pathBucketDigests: string[],
  includeLegacy: boolean,
) {
  if (kind === "reflection") {
    return listReflectionSelection(
      cursor === undefined
        ? { limit: PAGE_SIZE }
        : { limit: PAGE_SIZE, cursor_after_memory_id: cursor },
    );
  }
  return listSummariesForSelection({
    kind,
    limit: PAGE_SIZE,
    offset,
    ...(kind === "thread-group"
      ? {}
      : {
          rollup_scopes:
            periodScope === "path" && includeOverall ? ["overall", "path"] : [periodScope],
          path_bucket_digests: pathBucketDigests,
          include_shadowed_legacy: includeLegacy,
        }),
  });
}

/** Owns a tab's page continuation and rejects responses from a superseded tab. */
export function useSummarySelectionPages() {
  const [kind, setKind] = useState<SelectableMemoryKind>("thread-group");
  const activeKind = useRef(kind);
  const [entries, setEntries] = useState<SelectionEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(false);
  const [nextOffset, setNextOffset] = useState(0);
  const [nextCursor, setNextCursor] = useState<string | undefined>();
  const [error, setError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [periodFilters, dispatchPeriodFilter] = useReducer(
    periodFiltersReducer,
    INITIAL_PERIOD_FILTERS,
  );
  const [pathCandidates, setPathCandidates] = useState<SummaryPathCandidate[]>([]);
  const { periodScope, includeOverall, pathBucketDigests, includeLegacy } = periodFilters;
  const requestKey = useMemo(
    () => ({ kind, reloadKey, periodScope, includeOverall, pathBucketDigests, includeLegacy }),
    [kind, reloadKey, periodScope, includeOverall, pathBucketDigests, includeLegacy],
  );
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
    void pageRequest(
      requestKey.kind,
      0,
      undefined,
      requestKey.periodScope,
      requestKey.includeOverall,
      requestKey.pathBucketDigests,
      requestKey.includeLegacy,
    )
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

  useEffect(() => {
    if (kind === "thread-group" || kind === "reflection") {
      setPathCandidates([]);
      return;
    }
    let cancelled = false;
    void listSummaryPathCandidates({ kind })
      .then((candidates) => {
        if (!cancelled) setPathCandidates(candidates);
      })
      .catch((reason) => {
        if (!cancelled) setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => {
      cancelled = true;
    };
  }, [kind]);

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
  // A continuation is scoped by every period filter, not only by its tab.
  // Invalidate an in-flight load-more response before changing any of those
  // fields so it cannot append rows from a previous path/legacy scope.
  const invalidateContinuation = useCallback(() => {
    sessionGeneration.current += 1;
  }, []);
  const updatePeriodFilter = useCallback(
    (action: PeriodFilterAction) => {
      invalidateContinuation();
      dispatchPeriodFilter(action);
    },
    [invalidateContinuation],
  );
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
    void pageRequest(
      requestedKind,
      requestedOffset,
      requestedCursor,
      periodScope,
      includeOverall,
      pathBucketDigests,
      includeLegacy,
    )
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
  }, [
    hasMore,
    includeOverall,
    includeLegacy,
    isCurrentGeneration,
    kind,
    loadingMore,
    nextCursor,
    nextOffset,
    pathBucketDigests,
    periodScope,
    updateContinuation,
  ]);

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
    periodScope,
    setPeriodScope: (next: SetStateAction<PeriodScope>) =>
      updatePeriodFilter({ field: "periodScope", next }),
    includeOverall,
    setIncludeOverall: (next: SetStateAction<boolean>) =>
      updatePeriodFilter({ field: "includeOverall", next }),
    pathBucketDigests,
    setPathBucketDigests: (next: SetStateAction<string[]>) =>
      updatePeriodFilter({ field: "pathBucketDigests", next }),
    pathCandidates,
    includeLegacy,
    setIncludeLegacy: (next: SetStateAction<boolean>) =>
      updatePeriodFilter({ field: "includeLegacy", next }),
    setError,
  };
}
