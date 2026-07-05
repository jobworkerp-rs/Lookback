import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { findMemoriesByThreadId, findThread } from "@/api";
import { visibleConversationMemories } from "@/lib/codexMemoryVisibility";
import { sortLabelsByPrefixPriority } from "@/lib/labelPrefix";
import { parseThreadDescription } from "@/lib/threadDescription";
import {
  flattenMemories,
  initialOffset,
  nextOffset,
  PAGE_SIZE,
  prependScrollTop,
  prevOffset,
} from "@/lib/threadPaging";
import type { ThreadSummary } from "@/types/api";
import {
  MarkdownBody,
  MarkdownMessage,
  MESSAGE_ROLE_LABEL,
  MessageRole,
  memoryDomId,
} from "./MarkdownMessage";
import { Modal } from "./Modal";

const THREAD_ROLE_FILTERS = [
  MessageRole.User,
  MessageRole.Assistant,
  MessageRole.System,
  MessageRole.Tool,
  MessageRole.Meta,
  MessageRole.Reflection,
  MessageRole.Unspecified,
] as const;

const DEFAULT_VISIBLE_ROLES = new Set<number>([MessageRole.User, MessageRole.Assistant]);

/** Search-hit context: scroll target plus the paging seeds derived from it. */
export interface ThreadHighlight {
  /** Memory to scroll to / highlight. */
  memoryId: string;
  /** Hit's thread-internal position; seeds the initial paging offset. */
  position?: number;
  /** Thread's total memory count; caps downward paging. */
  threadTotal?: number;
}

/**
 * State shape for "open ThreadDetail with an optional scroll target".
 * Shared by every caller that pops the modal from a click — search
 * results, personality signal/profile chips, RAG chat source pills.
 */
export interface OpenThreadState {
  thread: ThreadSummary;
  highlight?: ThreadHighlight;
}

export function ThreadDetail({
  thread,
  highlight,
  onClose,
  onDelete,
}: {
  thread: ThreadSummary;
  /** Set when opened from a search hit; absent for plain browsing. */
  highlight?: ThreadHighlight;
  onClose: () => void;
  /** When provided, the header shows a 削除 button that calls this. Absent
   *  (e.g. opened read-only from a reflection link) → no delete affordance. */
  onDelete?: () => void;
}) {
  const { t } = useTranslation();
  const initial = initialOffset(highlight?.position);
  const query = useInfiniteQuery({
    // `initial` is part of the key: opening the same thread from a different
    // hit changes the page structure, so each entry point caches separately.
    queryKey: ["memories", thread.id, initial],
    queryFn: ({ pageParam }) =>
      findMemoriesByThreadId({ thread_id: thread.id, limit: PAGE_SIZE, offset: pageParam }),
    initialPageParam: initial,
    getNextPageParam: (lastPage, _all, lastParam) =>
      nextOffset(lastPage.length, lastParam, highlight?.threadTotal),
    getPreviousPageParam: (_first, _all, firstParam) => prevOffset(firstParam),
  });
  const memories = useMemo(() => flattenMemories(query.data?.pages ?? []), [query.data?.pages]);

  // A cross-tab jump (RAG source / personality / summary ref / reflection /
  // search-cache miss) opens the modal with a synthesized ThreadSummary that
  // has no channel and no labels. Fetch the real row to hydrate the header;
  // skip it entirely when the prop already carries them (the browse path).
  const needsHydration = thread.channel == null && thread.labels.length === 0;
  const threadRow = useQuery({
    queryKey: ["thread-row", thread.id],
    queryFn: () => findThread(thread.id),
    enabled: needsHydration,
  });
  const headerThread = threadRow.data ?? thread;
  const sortedLabels = useMemo(
    () => sortLabelsByPrefixPriority(headerThread.labels),
    [headerThread.labels],
  );

  const [summaryOpen, setSummaryOpen] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const topSentinel = useRef<HTMLDivElement>(null);
  const bottomSentinel = useRef<HTMLDivElement>(null);
  // Gate auto-paging until the initial scroll-to-hit settles, so sentinels
  // near the centered hit don't immediately trigger up/down fetches.
  const readyRef = useRef(false);
  // Distance from the bottom captured just before an upward fetch, used to
  // restore scrollTop after the prepend so the view doesn't jump.
  const pendingPrependRef = useRef<number | null>(null);

  // Scroll to the hit once its row is in the DOM, then enable the observer.
  // Browse opens (no hit) enable it immediately.
  const highlightMemoryId = highlight?.memoryId;
  const visibleMemories = useMemo(
    () =>
      visibleConversationMemories(memories, {
        alwaysIncludeIds: highlightMemoryId ? [highlightMemoryId] : [],
      }),
    [memories, highlightMemoryId],
  );
  useEffect(() => {
    if (!query.isSuccess) return;
    if (!highlightMemoryId) {
      readyRef.current = true;
      return;
    }
    document
      .getElementById(memoryDomId(highlightMemoryId))
      ?.scrollIntoView({ behavior: "auto", block: "center" });
    const raf = requestAnimationFrame(() => {
      readyRef.current = true;
    });
    return () => cancelAnimationFrame(raf);
  }, [query.isSuccess, highlightMemoryId]);

  // Restore scroll position after an upward prepend. useLayoutEffect runs
  // before paint so the correction isn't visible as a jump. `firstParam` is a
  // change-trigger only (a new first page param means rows were prepended), not
  // read inside — hence the lint suppression.
  const firstParam = query.data?.pageParams?.[0] as number | undefined;
  // biome-ignore lint/correctness/useExhaustiveDependencies: firstParam triggers the prepend correction
  useLayoutEffect(() => {
    const root = scrollRef.current;
    const saved = pendingPrependRef.current;
    if (root && saved != null) {
      root.scrollTop = prependScrollTop(root.scrollHeight, saved);
      pendingPrependRef.current = null;
    }
  }, [firstParam]);

  const { fetchNextPage, fetchPreviousPage } = query;
  const { hasNextPage, hasPreviousPage, isFetchingNextPage, isFetchingPreviousPage } = query;
  useEffect(() => {
    const root = scrollRef.current;
    if (!root) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (!readyRef.current) return;
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          if (entry.target === bottomSentinel.current && hasNextPage && !isFetchingNextPage) {
            fetchNextPage();
          }
          if (entry.target === topSentinel.current && hasPreviousPage && !isFetchingPreviousPage) {
            pendingPrependRef.current = root.scrollHeight - root.scrollTop;
            fetchPreviousPage();
          }
        }
      },
      { root, rootMargin: "200px 0px" },
    );
    if (topSentinel.current) observer.observe(topSentinel.current);
    if (bottomSentinel.current) observer.observe(bottomSentinel.current);
    return () => observer.disconnect();
  }, [
    fetchNextPage,
    fetchPreviousPage,
    hasNextPage,
    hasPreviousPage,
    isFetchingNextPage,
    isFetchingPreviousPage,
  ]);

  // Summary workflows pack `【title】 markdown` into description; show only
  // the short title in the header and fold the (long) markdown body away so
  // it doesn't bury the message list under a wall of text.
  const { title, body } = parseThreadDescription(thread.description, `Thread #${thread.id}`);
  const [enabledRoles, setEnabledRoles] = useState<Set<number>>(
    () => new Set(DEFAULT_VISIBLE_ROLES),
  );
  const filteredMemories = useMemo(
    () =>
      visibleMemories.filter(
        (mem) =>
          enabledRoles.has(mem.role) || (highlightMemoryId != null && mem.id === highlightMemoryId),
      ),
    [enabledRoles, visibleMemories, highlightMemoryId],
  );
  const toggleRole = (role: number) => {
    setEnabledRoles((current) => {
      const next = new Set(current);
      if (next.has(role)) {
        next.delete(role);
      } else {
        next.add(role);
      }
      return next;
    });
  };

  return (
    <Modal onClose={onClose} wide ariaLabel={title}>
      <div className="modal-head">
        <div className="thread-detail-meta-row">
          <span>
            {headerThread.channel ?? t("threadDetail.noChannel")} ·{" "}
            {sortedLabels.join(", ") || t("threadDetail.noLabels")}
          </span>
          <fieldset className="thread-role-filter">
            <legend className="visually-hidden">{t("threadDetail.roleFilter")}</legend>
            {THREAD_ROLE_FILTERS.map((role) => (
              <button
                key={role}
                type="button"
                className="thread-role-toggle"
                aria-pressed={enabledRoles.has(role)}
                onClick={() => toggleRole(role)}
              >
                {MESSAGE_ROLE_LABEL[role]}
              </button>
            ))}
          </fieldset>
          {onDelete && (
            <button type="button" className="btn danger" onClick={onDelete}>
              {t("threadDetail.deleteThread")}
            </button>
          )}
        </div>
        <div className="modal-title modal-title-clamp">{title}</div>
        {body && (
          // Render the markdown body only once expanded — a collapsed
          // `<details>` still mounts its children, so eager rendering would
          // parse a long summary on every modal open for nothing.
          <details
            className="thread-summary-fold"
            onToggle={(e) => setSummaryOpen(e.currentTarget.open)}
          >
            <summary>{t("threadDetail.showSummary")}</summary>
            {summaryOpen && (
              <div className="thread-summary-body message-body">
                <MarkdownBody>{body}</MarkdownBody>
              </div>
            )}
          </details>
        )}
      </div>
      <div className="modal-body" ref={scrollRef}>
        <div ref={topSentinel} style={{ overflowAnchor: "none" }} />
        {query.isFetchingPreviousPage && (
          <div className="empty-desc">{t("threadDetail.loadingPrev")}</div>
        )}
        {query.isLoading && <div className="empty-desc">{t("common.loading")}</div>}
        {query.error && (
          <div style={{ color: "var(--danger)", fontSize: 12 }}>
            {(query.error as Error).message}
          </div>
        )}
        {filteredMemories.map((mem) => (
          <MarkdownMessage
            key={mem.id}
            memoryId={mem.id}
            highlight={mem.id === highlightMemoryId}
            role={mem.role}
            createdAtMs={mem.created_at_ms}
            content={mem.content}
            metadata={mem.metadata}
          />
        ))}
        {!query.isLoading && visibleMemories.length === 0 && (
          <div className="empty-desc">{t("threadDetail.noMessages")}</div>
        )}
        {!query.isLoading && visibleMemories.length > 0 && filteredMemories.length === 0 && (
          <div className="empty-desc">{t("threadDetail.noRoleMatch")}</div>
        )}
        {query.isFetchingNextPage && (
          <div className="empty-desc">{t("threadDetail.loadingNext")}</div>
        )}
        <div ref={bottomSentinel} style={{ overflowAnchor: "none" }} />
      </div>
      <div className="modal-foot">
        <button type="button" className="btn" onClick={onClose}>
          {t("threadDetail.close")}
        </button>
      </div>
    </Modal>
  );
}
