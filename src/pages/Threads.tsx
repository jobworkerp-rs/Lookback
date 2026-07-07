import { useQuery } from "@tanstack/react-query";
import { type KeyboardEvent, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { deleteThread, findCoOccurringLabels, findDistinctLabels, listThreads } from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { DateInput } from "@/components/DateInput";
import { LabelFilter } from "@/components/LabelFilter";
import { ThreadDetail, type ThreadHighlight } from "@/components/ThreadDetail";
import { Toolbar } from "@/components/Toolbar";
import { useDebouncedValue } from "@/hooks/useDebouncedValue";
import { useDeleteAction } from "@/hooks/useDeleteAction";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import { isVectorDegraded, type SidecarStatus } from "@/hooks/useSidecarStatus";
import { useTimezone } from "@/hooks/useTimezone";
import { localDateToEpochMs } from "@/lib/dateInput";
import { sortLabelsByPrefixPriority } from "@/lib/labelPrefix";
import { formatDateTime } from "@/lib/localeFormat";
import { PERSONALITY_QUERY_KEY } from "@/lib/queryKeys";
import { isEmbeddingSearchMode, resolveSearchErrorHint, SEARCH_MODES } from "@/lib/searchModes";
import { parseThreadDescription } from "@/lib/threadDescription";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type { ConnectionMode, LabelMatch, SearchMode, ThreadHit, ThreadSummary } from "@/types/api";

export interface ThreadsPageProps {
  onOpenImport: () => void;
  sidecar: SidecarStatus;
  connectionMode?: ConnectionMode | null;
}

type Mode = "browse" | SearchMode;

interface Selection {
  thread: ThreadSummary;
  // Set when opened from a search hit, so ThreadDetail can scroll to it.
  highlight?: ThreadHighlight;
}

export function Threads({ onOpenImport, sidecar, connectionMode = "local" }: ThreadsPageProps) {
  const { t } = useTranslation();
  // Date filters are anchored to midnight in the display timezone so the search
  // boundary matches the timestamps the list renders (both go through the same
  // zone); without this a thread shown as "4/30" could leak into a "5/1 onward"
  // search when the OS zone differs from the configured one.
  const timezone = useTimezone();
  // Semantic / hybrid embed the query, so they're unavailable while the local
  // vector store is degraded. Keyword (FTS) and browse stay usable.
  const vectorDisabled = isVectorDegraded(sidecar, connectionMode) != null;
  const [selected, setSelected] = useState<Selection | null>(null);
  const [filter, setFilter] = useState("");
  const [mode, setMode] = useState<Mode>("browse");
  // If the vector store degrades while an embedding mode is active, fall back
  // to keyword so the search panel doesn't sit on a disabled mode. Only the
  // degrade transition should trigger this — depending on `mode` too would
  // loop on the reset it performs.
  // biome-ignore lint/correctness/useExhaustiveDependencies: intentional; see comment
  useEffect(() => {
    if (vectorDisabled && mode !== "browse" && isEmbeddingSearchMode(mode)) {
      setMode("keyword");
    }
  }, [vectorDisabled]);
  const [query, setQuery] = useState("");
  const debouncedQuery = useDebouncedValue(query, 300);
  const [createdAfter, setCreatedAfter] = useState("");
  const [createdBefore, setCreatedBefore] = useState("");
  // Shared across browse/search so switching modes keeps the user's narrowing.
  const [selectedLabels, setSelectedLabels] = useState<string[]>([]);
  const [labelMatch, setLabelMatch] = useState<LabelMatch>("all");
  const toggleLabel = (label: string) => {
    setSelectedLabels((cur) =>
      cur.includes(label) ? cur.filter((l) => l !== label) : [...cur, label],
    );
  };
  const toggleManyLabels = (labels: string[], turnOn: boolean) => {
    setSelectedLabels((cur) => {
      const set = new Set(cur);
      if (turnOn) for (const l of labels) set.add(l);
      else for (const l of labels) set.delete(l);
      return Array.from(set);
    });
  };
  // Thread deletion (initiated from the detail modal) removes the thread and
  // its attached conversation Memories. Derived per-thread summaries/reflections
  // live under separate owner threads (SUMMARY_USER_ID / reflection_user_id) and
  // are NOT cascade-deleted, so their caches are intentionally left alone.
  // Label aggregates (distinct + co-occurring) ARE invalidated — removing the
  // last thread carrying a label should drop it from the filter bar.
  const del = useDeleteAction(deleteThread, [
    ["threads"],
    ["thread-search"],
    ["distinct-labels"],
    ["co-occurring-labels"],
    PERSONALITY_QUERY_KEY,
  ]);

  // memories defaults label RPCs to a small page (distinct=100, co-occurring=20)
  // ordered by label ASC; alphabetically late prefixes (`dir:*`, `path:*`)
  // would silently truncate without an explicit override. The bar is
  // bounded by the user's distinct label cardinality, not paged.
  const LABEL_FETCH_LIMIT = 10_000;
  // Refetched only when an import lands (App.tsx invalidates this key); a
  // tab re-entry must not pay for another DB scan against memories.
  const labelAggregate = useQuery({
    queryKey: ["distinct-labels", 1],
    queryFn: () => findDistinctLabels({ user_id: 1, limit: LABEL_FETCH_LIMIT }),
    staleTime: Number.POSITIVE_INFINITY,
  });

  // Sort so toggling labels in different orders doesn't create distinct
  // cache entries for the same semantic selection. Passed as an array into
  // query keys so labels containing `|` (or any other delimiter) can't
  // collide with another selection.
  const sortedLabels = useMemo(() => [...selectedLabels].sort(), [selectedLabels]);
  const useCoOccurring = labelMatch === "all" && selectedLabels.length > 0;
  const coOccurringQuery = useQuery({
    queryKey: ["co-occurring-labels", sortedLabels],
    queryFn: () =>
      findCoOccurringLabels({ user_id: 1, labels: sortedLabels, limit: LABEL_FETCH_LIMIT }),
    enabled: useCoOccurring,
    staleTime: Number.POSITIVE_INFINITY,
  });
  // label_match is omitted when <2 labels are selected; collapse the key
  // the same way so toggling AND/OR with 0–1 labels doesn't refetch.
  const effectiveMatch = selectedLabels.length >= 2 ? labelMatch : undefined;
  const labelArgs = {
    labels_any: selectedLabels.length > 0 ? sortedLabels : undefined,
    label_match: effectiveMatch,
  };

  const threads = useQuery({
    queryKey: ["threads", sortedLabels, effectiveMatch ?? "any"],
    queryFn: () => listThreads({ user_id: 1, limit: 200, ...labelArgs }),
    enabled: mode === "browse",
  });

  // Empty query disables the search query so we don't fire an
  // RPC with empty text the moment the user picks a mode.
  const searchEnabled = mode !== "browse" && debouncedQuery.trim().length > 0;
  const search = useQuery({
    queryKey: [
      "thread-search",
      mode,
      debouncedQuery,
      createdAfter,
      createdBefore,
      sortedLabels,
      effectiveMatch ?? "any",
      timezone,
    ],
    enabled: searchEnabled,
    queryFn: () => {
      // `enabled: searchEnabled` guarantees mode is a SearchMode here.
      const m = mode as SearchMode;
      return SEARCH_MODES[m].fn({
        query_text: debouncedQuery.trim(),
        mode: m,
        user_id: 1,
        created_after_ms: localDateToEpochMs(createdAfter, timezone),
        created_before_ms: localDateToEpochMs(createdBefore, timezone),
        ...labelArgs,
        limit: 50,
      });
    },
  });

  const filtered = useMemo(() => {
    if (!threads.data) return [];
    const q = filter.trim().toLowerCase();
    if (!q) return threads.data;
    return threads.data.filter(
      (t) =>
        (t.description?.toLowerCase().includes(q) ?? false) ||
        t.labels.some((l) => l.toLowerCase().includes(q)) ||
        (t.channel?.toLowerCase().includes(q) ?? false),
    );
  }, [threads.data, filter]);

  const isSearching = mode !== "browse";
  const subtitle = isSearching
    ? searchEnabled
      ? t("threads.subtitleHits", { count: search.data?.length ?? 0 })
      : t("search.enterQuery")
    : t("threads.subtitleImported", { count: threads.data?.length ?? 0 });

  // After a successful delete also close the detail modal the delete was
  // launched from (the thread no longer exists). A failure leaves both open.
  async function confirmDelete() {
    if (await del.confirm()) setSelected(null);
  }

  return (
    <>
      <Toolbar
        title={t("threads.title")}
        subtitle={subtitle}
        actions={
          <>
            <button
              type="button"
              className="btn"
              onClick={() => (isSearching ? search.refetch() : threads.refetch())}
              disabled={isSearching ? search.isFetching : threads.isFetching}
            >
              {t("common.reload")}
            </button>
            <button type="button" className="btn primary" onClick={onOpenImport}>
              {t("common.import")}
            </button>
          </>
        }
      />
      <div className="content">
        <div className="search-bar">
          <div className="segment">
            <button
              type="button"
              className={`segment-btn ${mode === "browse" ? "active" : ""}`}
              onClick={() => setMode("browse")}
            >
              {t("search.mode.browse")}
            </button>
            <button
              type="button"
              className={`segment-btn ${mode === "keyword" ? "active" : ""}`}
              onClick={() => setMode("keyword")}
              title={t("search.modeTitle.keyword")}
            >
              {t("search.mode.keyword")}
            </button>
            <button
              type="button"
              className={`segment-btn ${mode === "semantic" ? "active" : ""}`}
              onClick={() => setMode("semantic")}
              disabled={vectorDisabled}
              title={vectorDisabled ? t("search.modeDisabled") : t("search.modeTitle.semantic")}
            >
              {t("search.mode.semantic")}
            </button>
            <button
              type="button"
              className={`segment-btn ${mode === "hybrid" ? "active" : ""}`}
              onClick={() => setMode("hybrid")}
              disabled={vectorDisabled}
              title={vectorDisabled ? t("search.modeDisabled") : t("search.modeTitle.hybrid")}
            >
              {t("search.mode.hybrid")}
            </button>
          </div>
          {isSearching ? (
            <>
              <input
                type="text"
                className="text-input"
                placeholder={t("threads.searchPlaceholder")}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                style={{ flex: 1, minWidth: 240 }}
              />
              <DateInput
                value={createdAfter}
                onChange={setCreatedAfter}
                title={t("threads.createdAfterTitle")}
              />
              <DateInput
                value={createdBefore}
                onChange={setCreatedBefore}
                title={t("threads.createdBeforeTitle")}
              />
            </>
          ) : (
            <input
              type="text"
              className="text-input"
              placeholder={t("threads.filterPlaceholder")}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              style={{ flex: 1 }}
            />
          )}
        </div>

        <LabelFilter
          labels={labelAggregate.data ?? []}
          coOccurringLabels={useCoOccurring ? coOccurringQuery.data : undefined}
          selected={selectedLabels}
          match={labelMatch}
          onToggle={toggleLabel}
          onToggleMany={toggleManyLabels}
          onSetMatch={setLabelMatch}
          isLoading={labelAggregate.isLoading}
        />

        {isSearching ? (
          <SearchResults
            hits={search.data}
            isLoading={searchEnabled && search.isLoading}
            error={search.error as Error | null}
            mode={mode as SearchMode}
            enabled={searchEnabled}
            onSelectHit={(hit) => setSelected(resolveHitSelection(hit, threads.data))}
          />
        ) : (
          <BrowseList
            data={filtered}
            isLoading={threads.isLoading}
            error={threads.error as Error | null}
            onSelect={(thread) => setSelected({ thread })}
            onOpenImport={onOpenImport}
            selectedLabels={selectedLabels}
            onToggleLabel={toggleLabel}
          />
        )}
      </div>

      {selected && (
        <ThreadDetail
          thread={selected.thread}
          highlight={selected.highlight}
          onClose={() => setSelected(null)}
          onDelete={() => del.request(selected.thread.id)}
        />
      )}

      {del.pendingId != null && (
        <ConfirmDialog
          title={t("threads.delete.title")}
          message={t("threads.delete.message")}
          confirmLabel={t("threads.delete.confirm")}
          busy={del.busy}
          error={del.error}
          onConfirm={() => void confirmDelete()}
          onCancel={del.cancel}
        />
      )}
    </>
  );
}

function BrowseList({
  data,
  isLoading,
  error,
  onSelect,
  onOpenImport,
  selectedLabels,
  onToggleLabel,
}: {
  data: ThreadSummary[];
  isLoading: boolean;
  error: Error | null;
  onSelect: (thread: ThreadSummary) => void;
  onOpenImport: () => void;
  selectedLabels: string[];
  onToggleLabel: (label: string) => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const selectedSet = useMemo(() => new Set(selectedLabels), [selectedLabels]);
  // Sort each thread's chips once per data load, not on every re-render (a
  // label toggle re-renders the whole list but leaves `data` — and thus every
  // label order — unchanged).
  const sortedLabelsById = useMemo(
    () => new Map(data.map((t) => [t.id, sortLabelsByPrefixPriority(t.labels)])),
    [data],
  );
  return (
    <>
      {isLoading && <div className="empty-desc">{t("common.loading")}</div>}
      {error && <div style={{ color: "var(--danger)", fontSize: 12 }}>{error.message}</div>}

      <div className="thread-list">
        {data.map((thread) => (
          <button
            type="button"
            key={thread.id}
            className="thread-card"
            onClick={() => onSelect(thread)}
            style={{ textAlign: "left" }}
          >
            <div className="thread-card-head">
              <span className={`thread-source ${thread.channel === "codex" ? "codex" : ""}`}>
                {thread.channel ?? "?"}
              </span>
              <span>{formatDateTime(thread.updated_at_ms, locale, timezone)}</span>
            </div>
            <div className="thread-title">
              {
                parseThreadDescription(
                  thread.description,
                  t("threads.fallbackTitle", {
                    id: thread.id,
                  }),
                ).title
              }
            </div>
            <div className="thread-card-foot">
              {(sortedLabelsById.get(thread.id) ?? thread.labels).map((l) => (
                <ClickableLabelPill
                  key={l}
                  label={l}
                  active={selectedSet.has(l)}
                  onToggle={onToggleLabel}
                />
              ))}
            </div>
          </button>
        ))}
      </div>

      {!isLoading && !error && data.length === 0 && (
        <div className="empty-state">
          <div className="empty-title">{t("threads.empty.title")}</div>
          <div className="empty-desc">
            <button type="button" className="btn primary" onClick={onOpenImport}>
              {t("common.import")}
            </button>
            {t("threads.empty.descSuffix")}
          </div>
        </div>
      )}
    </>
  );
}

function SearchResults({
  hits,
  isLoading,
  error,
  mode,
  enabled,
  onSelectHit,
}: {
  hits: ThreadHit[] | undefined;
  isLoading: boolean;
  error: Error | null;
  mode: SearchMode;
  enabled: boolean;
  onSelectHit: (hit: ThreadHit) => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  if (!enabled) {
    return (
      <div className="empty-state">
        <div className="empty-title">{t("search.enterQuery")}</div>
        <div className="empty-desc">{t(SEARCH_MODES[mode].hintKey)}</div>
      </div>
    );
  }
  if (isLoading) return <div className="empty-desc">{t("search.searching")}</div>;
  if (error) {
    const hint = resolveSearchErrorHint(t, mode, error.message);
    return (
      <div style={{ color: "var(--danger)", fontSize: 12 }}>
        {error.message}
        {hint && <div style={{ marginTop: 4, color: "var(--label-tertiary)" }}>{hint}</div>}
      </div>
    );
  }
  if (!hits || hits.length === 0) {
    return (
      <div className="empty-state">
        <div className="empty-title">{t("threads.search.noHitsTitle")}</div>
        <div className="empty-desc">{t("threads.search.noHitsDesc")}</div>
      </div>
    );
  }
  return (
    <div>
      {hits.map((hit) => (
        <button
          type="button"
          key={hit.thread_id}
          className="thread-hit-card"
          onClick={() => onSelectHit(hit)}
        >
          <div className="thread-hit-head">
            <span>
              {
                parseThreadDescription(
                  hit.thread_description,
                  t("threads.fallbackTitle", {
                    id: hit.thread_id,
                  }),
                ).title
              }
              {hit.top_position != null && hit.top_thread_total != null
                ? ` · ${t("threads.search.messageLabel", {
                    position: hit.top_position,
                    total: hit.top_thread_total,
                  })}`
                : ""}
            </span>
            <span>
              {t("threads.search.score", {
                count: hit.hit_count,
                score: hit.top_score.toFixed(3),
              })}{" "}
              · {formatDateTime(hit.top_created_at_ms, locale, timezone)}
            </span>
          </div>
          <div className="thread-hit-snippet">{hit.top_snippet}</div>
        </button>
      ))}
    </div>
  );
}

// span+role: a nested <button> inside the .thread-card button is invalid HTML.
function ClickableLabelPill({
  label,
  active,
  onToggle,
}: {
  label: string;
  active: boolean;
  onToggle: (label: string) => void;
}) {
  const handle = (e: { stopPropagation: () => void }) => {
    e.stopPropagation();
    onToggle(label);
  };
  return (
    // biome-ignore lint/a11y/useSemanticElements: nested <button> inside the .thread-card button is invalid HTML; span+role keeps focus/aria honest
    <span
      role="button"
      tabIndex={0}
      className={`label-pill${active ? " active" : ""}`}
      aria-pressed={active}
      onClick={handle}
      onKeyDown={(e: KeyboardEvent<HTMLSpanElement>) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          handle(e);
        }
      }}
    >
      {label}
    </span>
  );
}

/**
 * Open a thread detail modal from a search hit. We pick the matching
 * `ThreadSummary` out of the browse-mode cache when available so the
 * modal head can show description/labels; falls back to a synthetic
 * one when the user has never run a browse fetch in this session.
 */
function resolveHitSelection(hit: ThreadHit, cached: ThreadSummary[] | undefined): Selection {
  const highlight: ThreadHighlight = {
    memoryId: hit.top_memory_id,
    position: hit.top_position ?? undefined,
    threadTotal: hit.top_thread_total ?? undefined,
  };
  const match = cached?.find((t) => t.id === hit.thread_id);
  if (match) {
    return { thread: match, highlight };
  }
  return {
    thread: synthesizeThreadSummary({
      id: hit.thread_id,
      description: hit.thread_description,
      createdAtMs: hit.top_created_at_ms,
      updatedAtMs: hit.top_created_at_ms,
    }),
    highlight,
  };
}
