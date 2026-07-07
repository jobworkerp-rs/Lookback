import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  deleteSummary,
  findMemoriesByThreadId,
  listSummaries,
  listSummaryPeriodKeys,
  parseSummaryContent,
} from "@/api";
import { AnalysisProgress } from "@/components/AnalysisProgress";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { DateInput } from "@/components/DateInput";
import {
  extractSummaryTitle,
  SummaryBody,
  SummaryRefHandlersProvider,
  useSummaryRefHandlers,
} from "@/components/SummaryBody";
import { SummaryCalendar } from "@/components/SummaryCalendar";
import { SummaryGenerateDialog } from "@/components/SummaryGenerateDialog";
import { type OpenThreadState, ThreadDetail } from "@/components/ThreadDetail";
import { Toolbar } from "@/components/Toolbar";
import { useDebouncedValue } from "@/hooks/useDebouncedValue";
import { useDeleteAction } from "@/hooks/useDeleteAction";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import { isVectorDegraded, type SidecarStatus } from "@/hooks/useSidecarStatus";
import type { StepStreamProgressHandle } from "@/hooks/useStepStreamProgress";
import { useTimezone } from "@/hooks/useTimezone";
import { dayRangeToEpochMs, localDateToEpochMs } from "@/lib/dateInput";
import { formatDateTime } from "@/lib/localeFormat";
import { isEmbeddingSearchMode, resolveSearchErrorHint, SEARCH_MODES } from "@/lib/searchModes";
import { KIND_LABEL_KEYS } from "@/lib/summaryKind";
import { dayKey, monthKeyToYearMonth, yearMonthToKey } from "@/lib/summaryPeriod";
import { resolveSummaryRefNavigation } from "@/lib/summaryRefNav";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type { ConnectionMode, SearchMode, SummaryEntry, SummaryKind, ThreadHit } from "@/types/api";

// Synthetic owner of all summaries (Rust SUMMARY_USER_ID); summary search
// scopes to this owner so it never returns raw conversation memories.
const SUMMARY_USER_ID = 100_000;

type View = "list" | "search" | "calendar";
type CalendarKind = Exclude<SummaryKind, "per-thread">;

/** Summary-thread label that scopes full-text search to a single kind.
 *  All four kinds share `user_id=100000`, so without a label filter a search
 *  would mix per-thread and period summaries; each kind's thread carries a
 *  distinct label (`summary` / `daily_summary` / ...) that keeps them apart. */
const KIND_SEARCH_LABEL: Record<SummaryKind, string> = {
  "per-thread": "summary",
  daily: "daily_summary",
  weekly: "weekly_summary",
  monthly: "monthly_summary",
};

/** Deep-link hint a caller (currently: the RAG chat tab's
 *  `period_summary` source pill) can pass so Summaries opens straight to
 *  the cited period card instead of the default per-thread / current-month
 *  view. `view` is fixed to `calendar` because that's the only view that
 *  honours `selectedKey`. */
export interface SummariesFocus {
  kind: CalendarKind;
  month: string;
  periodKey: string;
}

export function Summaries({
  summaryProgress,
  sidecar,
  connectionMode = "local",
  focus,
  onFocusConsumed,
}: {
  summaryProgress: StepStreamProgressHandle;
  sidecar: SidecarStatus;
  connectionMode?: ConnectionMode | null;
  /** One-shot focus seed: applied once on mount or whenever a new value
   *  arrives, then `onFocusConsumed` clears it so navigating around the
   *  tab freely doesn't keep snapping back. */
  focus?: SummariesFocus | null;
  onFocusConsumed?: () => void;
}) {
  const { t } = useTranslation();
  // Date filters anchored to midnight in the display timezone so the range
  // boundary matches the rendered timestamps (see Threads for the rationale).
  const timezone = useTimezone();
  const [kind, setKind] = useState<SummaryKind>(focus?.kind ?? "per-thread");
  const [view, setView] = useState<View>(focus ? "calendar" : "list");
  const [updatedAfter, setUpdatedAfter] = useState("");
  const [updatedBefore, setUpdatedBefore] = useState("");
  const [query, setQuery] = useState("");
  const debouncedQuery = useDebouncedValue(query, 300);
  const [mode, setMode] = useState<SearchMode>("keyword");
  // Semantic / hybrid embed the query, so they're gated while the local vector
  // store is degraded; keyword (FTS) stays available.
  const vectorDisabled = isVectorDegraded(sidecar, connectionMode) != null;
  // biome-ignore lint/correctness/useExhaustiveDependencies: only the degrade transition should reset the mode; depending on `mode` would loop
  useEffect(() => {
    if (vectorDisabled && isEmbeddingSearchMode(mode)) setMode("keyword");
  }, [vectorDisabled]);
  const [month, setMonth] = useState(() => {
    if (focus) return focus.month;
    const d = new Date();
    return yearMonthToKey(d.getFullYear(), d.getMonth() + 1);
  });
  const [selectedKey, setSelectedKey] = useState<string | null>(focus?.periodKey ?? null);
  const [generateOpen, setGenerateOpen] = useState(false);
  // Source-ID chips and the per-thread `Thread #{id}` link both pop the
  // ThreadDetail modal — reuses the same `OpenThreadState` shape that the
  // search-hit, personality, and chat paths share.
  const [openThread, setOpenThread] = useState<OpenThreadState | null>(null);
  // Token bumped on every memory-ref click so stale async resolutions can
  // detect they were superseded by a later click and skip their setState,
  // matching the Chat tab's source-pill click guard.
  const refClickToken = useRef(0);

  // When a fresh `focus` arrives after mount (e.g. a second chat source
  // pill clicked while the tab is already open), reapply it and signal
  // back so the parent can drop the seed.
  useEffect(() => {
    if (!focus) return;
    setKind(focus.kind);
    setView("calendar");
    setMonth(focus.month);
    setSelectedKey(focus.periodKey);
    onFocusConsumed?.();
  }, [focus, onFocusConsumed]);
  // A summary appears in the list, calendar-detail, and search caches, and the
  // sidebar count badge; invalidate every family so all views drop the row.
  // Mirrors the App-level "summary generation done" invalidation set.
  const del = useDeleteAction(deleteSummary, [
    ["summaries"],
    ["count-summaries"],
    ["summary-period-keys"],
    ["summary-search"],
  ]);

  // per-thread has no period_key, so the calendar view is meaningless there.
  const calendarKind: CalendarKind | null = kind === "per-thread" ? null : kind;
  const effectiveView: View = view === "calendar" && calendarKind == null ? "list" : view;
  const monthWindow = useMemo(() => monthBounds(month, timezone), [month, timezone]);

  const listQuery = useQuery({
    queryKey: ["summaries", kind, updatedAfter, updatedBefore, timezone],
    queryFn: () =>
      fetchSummaries(
        kind,
        localDateToEpochMs(updatedAfter, timezone),
        localDateToEpochMs(updatedBefore, timezone),
      ),
    enabled: effectiveView === "list",
  });

  // Calendar existence dots: the period_keys present in the shown month.
  const periodKeysQuery = useQuery({
    queryKey: ["summary-period-keys", kind, monthWindow.after, monthWindow.before],
    queryFn: () =>
      listSummaryPeriodKeys({
        kind: calendarKind as CalendarKind,
        updated_after_ms: monthWindow.after,
        updated_before_ms: monthWindow.before,
      }),
    enabled: effectiveView === "calendar" && calendarKind != null,
  });

  // Detail for the calendar-selected period: the whole month is fetched once
  // and the selected period_key is picked client-side, so switching days
  // within the month reuses the cache instead of refetching.
  const detailQuery = useQuery({
    queryKey: ["summaries", kind, monthWindow.after, monthWindow.before, "detail"],
    queryFn: () => fetchSummaries(kind, monthWindow.after, monthWindow.before),
    enabled: effectiveView === "calendar" && calendarKind != null && selectedKey != null,
  });

  const searchEnabled = effectiveView === "search" && debouncedQuery.trim().length > 0;
  const search = useQuery({
    queryKey: ["summary-search", kind, mode, debouncedQuery, updatedAfter, updatedBefore, timezone],
    enabled: searchEnabled,
    queryFn: () =>
      SEARCH_MODES[mode].fn({
        query_text: debouncedQuery.trim(),
        mode,
        user_id: SUMMARY_USER_ID,
        labels_any: [KIND_SEARCH_LABEL[kind]],
        created_after_ms: localDateToEpochMs(updatedAfter, timezone),
        created_before_ms: localDateToEpochMs(updatedBefore, timezone),
        limit: 50,
      }),
  });

  // A period_key can have several scope_keys (e.g. `_all` plus per-project),
  // so show every matching entry rather than the first.
  const selectedEntries = useMemo(
    () => detailQuery.data?.filter((e) => e.period_key === selectedKey) ?? [],
    [detailQuery.data, selectedKey],
  );

  // `Thread #{id}` link on per-thread cards, or a `source_thread_ids` chip
  // click. The synthesized ThreadSummary stub is enough for ThreadDetail;
  // its own memory query fills in the body.
  const handleOpenThread = useCallback((threadId: string) => {
    setOpenThread({ thread: synthesizeThreadSummary({ id: threadId }) });
    refClickToken.current += 1;
  }, []);

  // A `source_memory_ids` chip click. The cited memory may resolve to a
  // per-thread (open the conversation thread modal) or a period summary
  // (jump within Summaries to the cited calendar card). The async resolve
  // is guarded by a click token so a later click supersedes ours; a null
  // target (deleted memory, unknown external_id) leaves the UI untouched.
  const handleOpenMemoryRef = useCallback(async (memoryId: string) => {
    const myToken = ++refClickToken.current;
    const target = await resolveSummaryRefNavigation(memoryId);
    if (myToken !== refClickToken.current) return;
    if (!target) return;
    if (target.kind === "open-thread") {
      setOpenThread(target.open);
    } else {
      setKind(target.focus.kind);
      setView("calendar");
      setMonth(target.focus.month);
      setSelectedKey(target.focus.periodKey);
    }
  }, []);

  const refHandlers = useMemo(
    () => ({ onOpenThread: handleOpenThread, onOpenMemoryRef: handleOpenMemoryRef }),
    [handleOpenThread, handleOpenMemoryRef],
  );

  const count = subtitleCount(effectiveView, {
    list: listQuery.data?.length,
    search: searchEnabled ? (search.data?.length ?? 0) : undefined,
    calendar: periodKeysQuery.data?.length,
  });

  const generating = summaryProgress.busy;

  return (
    <SummaryRefHandlersProvider value={refHandlers}>
      <Toolbar
        title={t("summaries.title")}
        subtitle={count != null ? t("summaries.subtitleCount", { count }) : undefined}
        actions={
          <>
            <button
              type="button"
              className="btn"
              onClick={() => {
                // Only refetch the query backing the active view. `refetch()`
                // ignores `enabled: false`, so reloading the per-thread list
                // would otherwise fire periodKeysQuery with a null kind and
                // trip the Rust enum deserialization.
                if (effectiveView === "list") listQuery.refetch();
                else if (effectiveView === "search") {
                  if (searchEnabled) search.refetch();
                } else {
                  periodKeysQuery.refetch();
                  if (selectedKey != null) detailQuery.refetch();
                }
              }}
            >
              {t("common.reload")}
            </button>
            {generating ? (
              // Mirror the chat composer pattern (`src/pages/Chat.tsx`):
              // while busy, the primary action's slot is replaced by a
              // single "停止" button that fires the cancel API. Same
              // visual position, no layout shift, no hover-to-reveal
              // affordance to discover.
              <button
                type="button"
                className="btn secondary"
                onClick={() => void summaryProgress.cancel()}
                title={t("summaries.cancelTitle")}
              >
                {t("summaries.cancel")}
              </button>
            ) : (
              <button
                type="button"
                className="btn primary"
                onClick={() => setGenerateOpen(true)}
                title={t("summaries.generateTitle")}
              >
                {t("summaries.generate")}
              </button>
            )}
          </>
        }
      />
      <div className="content">
        <AnalysisProgress
          progress={summaryProgress.progress}
          error={null}
          onClose={summaryProgress.clear}
        />

        <div className="search-bar">
          <div className="segment">
            {(Object.keys(KIND_LABEL_KEYS) as SummaryKind[]).map((k) => (
              <button
                type="button"
                key={k}
                className={`segment-btn ${kind === k ? "active" : ""}`}
                onClick={() => {
                  setKind(k);
                  setSelectedKey(null);
                }}
              >
                {t(KIND_LABEL_KEYS[k])}
              </button>
            ))}
          </div>

          <div className="segment">
            <button
              type="button"
              className={`segment-btn ${effectiveView === "list" ? "active" : ""}`}
              onClick={() => setView("list")}
            >
              {t("summaries.view.list")}
            </button>
            <button
              type="button"
              className={`segment-btn ${effectiveView === "search" ? "active" : ""}`}
              onClick={() => setView("search")}
            >
              {t("summaries.view.search")}
            </button>
            <button
              type="button"
              className={`segment-btn ${effectiveView === "calendar" ? "active" : ""}`}
              onClick={() => setView("calendar")}
              disabled={calendarKind == null}
              title={calendarKind == null ? t("summaries.calendarDisabledTitle") : undefined}
            >
              {t("summaries.view.calendar")}
            </button>
          </div>

          {effectiveView === "search" && (
            <>
              <div className="segment">
                {(Object.keys(SEARCH_MODES) as SearchMode[]).map((m) => {
                  const disabled = vectorDisabled && isEmbeddingSearchMode(m);
                  return (
                    <button
                      type="button"
                      key={m}
                      className={`segment-btn ${mode === m ? "active" : ""}`}
                      onClick={() => setMode(m)}
                      disabled={disabled}
                      title={disabled ? t("search.modeDisabled") : t(`search.modeTitle.${m}`)}
                    >
                      {t(`search.mode.${m}`)}
                    </button>
                  );
                })}
              </div>
              <input
                type="text"
                className="text-input"
                placeholder={t("summaries.searchPlaceholder")}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                style={{ flex: 1, minWidth: 240 }}
              />
            </>
          )}

          {effectiveView !== "calendar" && (
            <>
              <DateInput
                value={updatedAfter}
                onChange={setUpdatedAfter}
                title={
                  effectiveView === "search"
                    ? t("summaries.date.createdAfter")
                    : t("summaries.date.updatedAfter")
                }
              />
              <DateInput
                value={updatedBefore}
                onChange={setUpdatedBefore}
                title={
                  effectiveView === "search"
                    ? t("summaries.date.createdBefore")
                    : t("summaries.date.updatedBefore")
                }
              />
            </>
          )}
        </div>

        {effectiveView === "list" && <ListView query={listQuery} onDelete={del.request} />}

        {effectiveView === "search" && (
          <SearchResults
            hits={search.data}
            isLoading={searchEnabled && search.isLoading}
            error={search.error as Error | null}
            mode={mode}
            enabled={searchEnabled}
          />
        )}

        {effectiveView === "calendar" && calendarKind != null && (
          <div className="sum-cal-layout">
            <SummaryCalendar
              kind={calendarKind}
              month={month}
              periodKeys={periodKeysQuery.data ?? []}
              selectedKey={selectedKey}
              onSelectKey={setSelectedKey}
              onMonthChange={(m) => {
                setMonth(m);
                setSelectedKey(null);
              }}
            />
            <div className="sum-cal-detail">
              {selectedKey == null && (
                <div className="empty-desc">{t("summaries.calendar.selectPrompt")}</div>
              )}
              {selectedKey != null && detailQuery.isLoading && (
                <div className="empty-desc">{t("common.loading")}</div>
              )}
              {selectedKey != null && !detailQuery.isLoading && selectedEntries.length === 0 && (
                <div className="empty-desc">
                  {t("summaries.calendar.noEntries", { key: selectedKey })}
                </div>
              )}
              <div className="sum-list">
                {selectedEntries.map((entry) => (
                  <SummaryCard
                    key={entry.memory_id}
                    entry={entry}
                    onDelete={() => del.request(entry.memory_id)}
                  />
                ))}
              </div>
            </div>
          </div>
        )}
      </div>

      {openThread && (
        <ThreadDetail
          thread={openThread.thread}
          highlight={openThread.highlight}
          onClose={() => setOpenThread(null)}
        />
      )}

      {generateOpen && (
        <SummaryGenerateDialog
          onClose={() => setGenerateOpen(false)}
          onStarted={(jobId) => summaryProgress.start(jobId)}
          initialKind={kind}
          sidecar={sidecar}
        />
      )}

      {del.pendingId != null && (
        <ConfirmDialog
          title={t("summaries.delete.title")}
          message={t("summaries.delete.message")}
          busy={del.busy}
          error={del.error}
          onConfirm={() => void del.confirm()}
          onCancel={del.cancel}
        />
      )}
    </SummaryRefHandlersProvider>
  );
}

function fetchSummaries(
  kind: SummaryKind,
  after: number | undefined,
  before: number | undefined,
): Promise<SummaryEntry[]> {
  return listSummaries({
    kind,
    limit: 200,
    updated_after_ms: after,
    updated_before_ms: before,
  });
}

function subtitleCount(
  view: View,
  counts: { list?: number; search?: number; calendar?: number },
): number | undefined {
  if (view === "list") return counts.list;
  if (view === "search") return counts.search;
  return counts.calendar;
}

function ListView({
  query,
  onDelete,
}: {
  query: ReturnType<typeof useQuery<SummaryEntry[]>>;
  onDelete: (memoryId: string) => void;
}) {
  const { t } = useTranslation();
  const { data, isLoading, error } = query;
  return (
    <>
      {isLoading && <div className="empty-desc">{t("common.loading")}</div>}
      {error && (
        <div style={{ color: "var(--danger)", fontSize: 12 }}>{(error as Error).message}</div>
      )}
      <div className="sum-list">
        {data?.map((entry) => (
          <SummaryCard
            key={entry.memory_id}
            entry={entry}
            onDelete={() => onDelete(entry.memory_id)}
          />
        ))}
      </div>
      {!isLoading && !error && (data?.length ?? 0) === 0 && (
        <div className="empty-state">
          <div className="empty-title">{t("summaries.list.emptyTitle")}</div>
          <div className="empty-desc">{t("summaries.list.emptyDesc")}</div>
        </div>
      )}
    </>
  );
}

function SummaryCard({ entry, onDelete }: { entry: SummaryEntry; onDelete: () => void }) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const { onOpenThread } = useSummaryRefHandlers();
  const content = parseSummaryContent(entry);
  // `_all` is the default scope and adds no signal; only surface a named scope.
  const scopeBadge = entry.scope_key && entry.scope_key !== "_all" ? entry.scope_key : null;
  const threadId = entry.thread_id;
  const isPerThreadLink = entry.kind === "per-thread" && threadId != null && onOpenThread != null;
  return (
    <div className="sum-card">
      <div className="sum-head">
        {isPerThreadLink ? (
          <button
            type="button"
            className="signal-thread-link"
            title={t("summaries.card.openThreadTitle")}
            onClick={() => onOpenThread(threadId)}
          >
            {t("summaries.fallbackTitle", { id: threadId })}
          </button>
        ) : (
          <span>
            {entry.period_key ??
              (threadId != null
                ? t("summaries.fallbackTitle", { id: threadId })
                : (entry.external_id ?? t("summaries.card.noId")))}
          </span>
        )}
        {scopeBadge && <span className="label-pill">{scopeBadge}</span>}
        <span style={{ marginLeft: "auto" }}>
          {formatDateTime(entry.updated_at_ms, locale, timezone)}
        </span>
        <button type="button" className="btn danger" onClick={onDelete}>
          {t("common.delete")}
        </button>
      </div>
      <SummaryBody parsed={content.parsed} raw={content.raw} />
    </div>
  );
}

function SearchResults({
  hits,
  isLoading,
  error,
  mode,
  enabled,
}: {
  hits: ThreadHit[] | undefined;
  isLoading: boolean;
  error: Error | null;
  mode: SearchMode;
  enabled: boolean;
}) {
  const { t } = useTranslation();
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
    <div className="sum-list">
      {hits.map((hit) => (
        <SearchHitCard key={hit.thread_id} hit={hit} />
      ))}
    </div>
  );
}

/** A summary search hit. The snippet is a raw-JSON excerpt from the matched
 *  memory, so the collapsed header shows the scraped title (falling back to
 *  the thread name); expanding fetches the full summary memory and renders it
 *  structured via `SummaryBody`. */
function SearchHitCard({ hit }: { hit: ThreadHit }) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const [open, setOpen] = useState(false);
  const detail = useQuery({
    queryKey: ["summary-hit", hit.thread_id],
    queryFn: () => findMemoriesByThreadId({ thread_id: hit.thread_id, limit: 1 }),
    enabled: open,
  });
  const content = detail.data?.[0]
    ? parseSummaryContent({ content_json: detail.data[0].content })
    : null;

  // Prefer the title from the fetched full content once expanded; before that,
  // scrape it from the (possibly truncated) snippet.
  const previewContent = content ?? parseSummaryContent({ content_json: hit.top_snippet });
  const title =
    extractSummaryTitle(previewContent) ??
    hit.thread_description ??
    t("summaries.fallbackTitle", { id: hit.thread_id });

  return (
    <div className="sum-card">
      <div className="sum-head">
        <span>{title}</span>
        <span style={{ marginLeft: "auto" }}>
          {t("summaries.hit.score", {
            count: hit.hit_count,
            score: hit.top_score.toFixed(3),
          })}{" "}
          · {formatDateTime(hit.top_created_at_ms, locale, timezone)}
        </span>
      </div>
      <details onToggle={(e) => setOpen(e.currentTarget.open)}>
        <summary className="sum-hit-summary">
          {open ? t("summaries.hit.close") : t("summaries.hit.show")}
        </summary>
        {detail.isLoading && <div className="empty-desc">{t("common.loading")}</div>}
        {detail.error && (
          <div style={{ color: "var(--danger)", fontSize: 12 }}>
            {(detail.error as Error).message}
          </div>
        )}
        {content && <SummaryBody parsed={content.parsed} raw={content.raw} />}
      </details>
    </div>
  );
}

/** Local-TZ epoch-ms bounds covering exactly the shown month. Delegates the
 *  ±1ms boundary nudging (strict-`>` after / inclusive-`<=` before) to the
 *  shared `dayRangeToEpochMs` so the rule lives in one place. */
function monthBounds(
  monthKey: string,
  timeZone?: string,
): { after: number | undefined; before: number | undefined } {
  const ym = monthKeyToYearMonth(monthKey);
  if (!ym) return { after: undefined, before: undefined };
  const firstDay = dayKey(new Date(ym.y, ym.m - 1, 1));
  // Day 0 of the next month is the last day of this month.
  const lastDay = dayKey(new Date(ym.y, ym.m, 0));
  return dayRangeToEpochMs(firstDay, lastDay, timeZone);
}
