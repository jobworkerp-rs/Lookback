import { useCallback, useEffect, useRef, useState } from "react";
import { listIssueDirectHistoryPage } from "@/api";
import type {
  IssueHistoryEntry,
  IssueHistoryMode,
  IssueHistoryPage,
  IssueHistoryPageOutcome,
} from "@/types/api";

interface UseIssueHistoryPagerOptions {
  issueId: string | null;
  mode: IssueHistoryMode;
  enabled: boolean;
}

export interface IssueHistoryPager {
  entries: IssueHistoryEntry[];
  loading: boolean;
  loadingMore: boolean;
  error: Error | null;
  resetNotice: boolean;
  hasNextPage: boolean;
  loadMore: () => void;
  reload: () => void;
}

const RESET_ERROR_MESSAGE = "操作履歴のスナップショットが更新されました。再読み込みしてください。";

// React Strict Mode mounts effects twice in development. Share concurrent
// requests by cursor so that the effect probe does not issue a duplicate RPC.
const inFlightRequests = new Map<string, Promise<IssueHistoryPageOutcome>>();

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function pageEntries(page: IssueHistoryPage): IssueHistoryEntry[] {
  return page.entries;
}

function requestPage(
  issueId: string,
  mode: IssueHistoryMode,
  pageToken: string | null,
  forceNew: boolean,
): Promise<IssueHistoryPageOutcome> {
  const key = JSON.stringify([issueId, mode, pageToken]);
  const existing = inFlightRequests.get(key);
  if (existing && !forceNew) return existing;

  let request: Promise<IssueHistoryPageOutcome>;
  request = listIssueDirectHistoryPage({
    issue_id: issueId,
    mode,
    page_token: pageToken,
  }).finally(() => {
    if (inFlightRequests.get(key) === request) inFlightRequests.delete(key);
  });
  inFlightRequests.set(key, request);
  return request;
}

export function useIssueHistoryPager({
  issueId,
  mode,
  enabled,
}: UseIssueHistoryPagerOptions): IssueHistoryPager {
  const [pages, setPages] = useState<IssueHistoryPage[]>([]);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [resetNotice, setResetNotice] = useState(false);
  const [nextPageToken, setNextPageToken] = useState<string | null>(null);
  const sessionRef = useRef(0);
  const nextPageTokenRef = useRef<string | null>(null);
  const resetUsedRef = useRef(false);
  const inFlightRef = useRef(false);

  const setContinuation = useCallback((token: string | null) => {
    setNextPageToken(token);
    nextPageTokenRef.current = token;
  }, []);

  const clearVisibleState = useCallback(() => {
    setPages([]);
    setContinuation(null);
    setError(null);
    setResetNotice(false);
  }, [setContinuation]);

  const fetchPage = useCallback(
    async (
      pageToken: string | null,
      session: number,
      replace: boolean,
      forceNew = false,
    ): Promise<void> => {
      if (!issueId) return;
      if (replace) setLoading(true);
      else setLoadingMore(true);
      inFlightRef.current = true;
      try {
        const outcome = await requestPage(issueId, mode, pageToken, forceNew);
        if (sessionRef.current !== session) return;
        if (outcome.kind === "reset_required") {
          if (pageToken && !resetUsedRef.current) {
            resetUsedRef.current = true;
            setResetNotice(true);
            setPages([]);
            setContinuation(null);
            await fetchPage(null, session, true);
          } else {
            setResetNotice(false);
            setContinuation(null);
            setError(new Error(RESET_ERROR_MESSAGE));
          }
          return;
        }
        const page = outcome.page;
        setPages((current) => (replace ? [page] : [...current, page]));
        const token = page.next_page_token || null;
        setContinuation(token);
        setError(null);
      } catch (requestError) {
        if (sessionRef.current === session) setError(asError(requestError));
      } finally {
        if (sessionRef.current === session) {
          inFlightRef.current = false;
          setLoading(false);
          setLoadingMore(false);
        }
      }
    },
    [issueId, mode, setContinuation],
  );

  useEffect(() => {
    const session = ++sessionRef.current;
    inFlightRef.current = false;
    resetUsedRef.current = false;
    clearVisibleState();
    if (enabled && issueId) void fetchPage(null, session, true);
    return () => {
      if (sessionRef.current === session) {
        sessionRef.current += 1;
        inFlightRef.current = false;
      }
    };
  }, [clearVisibleState, enabled, fetchPage, issueId]);

  const loadMore = useCallback(() => {
    const token = nextPageTokenRef.current;
    if (!enabled || !issueId || !token || inFlightRef.current) return;
    void fetchPage(token, sessionRef.current, false);
  }, [enabled, fetchPage, issueId]);

  const reload = useCallback(() => {
    if (!enabled || !issueId) return;
    const session = ++sessionRef.current;
    inFlightRef.current = false;
    resetUsedRef.current = false;
    clearVisibleState();
    void fetchPage(null, session, true, true);
  }, [clearVisibleState, enabled, fetchPage, issueId]);

  return {
    entries: pages.flatMap(pageEntries),
    loading,
    loadingMore,
    error,
    resetNotice,
    hasNextPage: Boolean(nextPageToken),
    loadMore,
    reload,
  };
}
