import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  deleteSummary,
  findMemoriesByThreadId,
  getSummaryContent,
  getSummaryRecoveryStatus,
  listSummaries,
  listSummaryPathChildren,
  listSummaryPeriodKeys,
  parseSummaryContent,
  resetSummaryRecoveryState,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { DateInput } from "@/components/DateInput";
import { Modal } from "@/components/Modal";
import { StagedProgressPanel } from "@/components/StagedProgressPanel";
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
import { type StagedProgressHandle, SUMMARY_GENERATION_STEPS } from "@/hooks/useStagedProgress";
import { useTimezone } from "@/hooks/useTimezone";
import { localDateToEpochMs } from "@/lib/dateInput";
import { formatDateTime } from "@/lib/localeFormat";
import { isEmbeddingSearchMode, resolveSearchErrorHint, SEARCH_MODES } from "@/lib/searchModes";
import {
  loadPeriodSummaryDisplayPreference,
  type PeriodSummaryDisplayPreference,
  savePeriodSummaryDisplayPreference,
} from "@/lib/summaryDisplayPreference";
import { KIND_LABEL_KEYS } from "@/lib/summaryKind";
import {
  buildMonthGrid,
  isoWeekOfDate,
  monthKeyToYearMonth,
  yearMonthToKey,
} from "@/lib/summaryPeriod";
import { resolveSummaryRefNavigation } from "@/lib/summaryRefNav";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import { incrementUiMemoryCounter } from "@/lib/uiMemoryDiagnostics";
import type {
  ConnectionMode,
  SearchMode,
  SummaryEntry,
  SummaryKind,
  SummaryPathChildEntry,
  SummaryRecoveryRecord,
  ThreadHit,
} from "@/types/api";

// Summaries are lifecycle kinds owned by the same Lookback user as RAW
// memories. Their labels and memory kinds, not a synthetic owner, scope them.
const LOOKBACK_USER_ID = 1;

type View = "list" | "search" | "calendar";
type PublicSummaryKind = Exclude<SummaryKind, "per-thread">;
type CalendarKind = Exclude<SummaryKind, "per-thread" | "thread-group">;
type CalendarState = { month: string; selectedKey: string | null };
type CalendarStateByKind = Record<CalendarKind, CalendarState>;

/** Summary label that scopes full-text search to one public summary kind. */
const KIND_SEARCH_LABEL: Record<SummaryKind, string> = {
  "per-thread": "summary",
  "thread-group": "thread_group_summary",
  daily: "daily_summary",
  weekly: "weekly_summary",
  monthly: "monthly_summary",
};

/** Numeric values of llm_memory.data.MemoryKind, kept in sync with the proto. */
const KIND_SEARCH_MEMORY_KIND: Record<SummaryKind, number> = {
  "per-thread": 2,
  "thread-group": 8,
  daily: 3,
  weekly: 4,
  monthly: 5,
};

const PUBLIC_SUMMARY_KINDS: readonly PublicSummaryKind[] = [
  "thread-group",
  "daily",
  "weekly",
  "monthly",
];

/** Deep-link hint a caller (currently: the RAG chat tab's
 *  `period_summary` source pill) can pass so Summaries opens straight to
 *  the cited period card instead of the default group-summary / current-month
 *  view. `view` is fixed to `calendar` because that's the only view that
 *  honours `selectedKey`. */
export interface SummariesFocus {
  kind: CalendarKind;
  month: string;
  periodKey: string;
}

export function Summaries({
  summaryGenerationProgress,
  sidecar,
  memoriesMode = "local",
  focus,
  onFocusConsumed,
  onNavigateThreadGroup,
}: {
  summaryGenerationProgress: StagedProgressHandle<"summary-generation">;
  sidecar: SidecarStatus;
  memoriesMode?: ConnectionMode | null;
  /** One-shot focus seed: applied once on mount or whenever a new value
   *  arrives, then `onFocusConsumed` clears it so navigating around the
   *  tab freely doesn't keep snapping back. */
  focus?: SummariesFocus | null;
  onFocusConsumed?: () => void;
  onNavigateThreadGroup?: (groupId: string) => void;
}) {
  const { t } = useTranslation();
  const summaryRecovery = useQuery({
    queryKey: ["summary-recovery-status"],
    queryFn: getSummaryRecoveryStatus,
    staleTime: 30_000,
  });
  // Date filters anchored to midnight in the display timezone so the range
  // boundary matches the rendered timestamps (see Threads for the rationale).
  const timezone = useTimezone();
  const [kind, setKind] = useState<PublicSummaryKind>(focus?.kind ?? "thread-group");
  const [confirmRecoveryReset, setConfirmRecoveryReset] = useState(false);
  const [recoveryDetailsOpen, setRecoveryDetailsOpen] = useState(false);
  const [view, setView] = useState<View>(focus ? "calendar" : "list");
  const [updatedAfter, setUpdatedAfter] = useState("");
  const [updatedBefore, setUpdatedBefore] = useState("");
  const [query, setQuery] = useState("");
  const debouncedQuery = useDebouncedValue(query, 300);
  const [mode, setMode] = useState<SearchMode>("keyword");
  const [periodDisplayPreference, setPeriodDisplayPreference] =
    useState<PeriodSummaryDisplayPreference>(loadPeriodSummaryDisplayPreference);
  // Semantic / hybrid embed the query, so they're gated while the local vector
  // store is degraded; keyword (FTS) stays available.
  const vectorDisabled = isVectorDegraded(sidecar, memoriesMode) != null;
  // biome-ignore lint/correctness/useExhaustiveDependencies: only the degrade transition should reset the mode; depending on `mode` would loop
  useEffect(() => {
    if (vectorDisabled && isEmbeddingSearchMode(mode)) setMode("keyword");
  }, [vectorDisabled]);
  const [calendarStateByKind, setCalendarStateByKind] = useState<CalendarStateByKind>(() => {
    const d = new Date();
    const currentMonth = yearMonthToKey(d.getFullYear(), d.getMonth() + 1);
    const emptyState = { month: currentMonth, selectedKey: null };
    return {
      daily:
        focus?.kind === "daily" ? { month: focus.month, selectedKey: focus.periodKey } : emptyState,
      weekly:
        focus?.kind === "weekly"
          ? { month: focus.month, selectedKey: focus.periodKey }
          : emptyState,
      monthly:
        focus?.kind === "monthly"
          ? { month: focus.month, selectedKey: focus.periodKey }
          : emptyState,
    };
  });
  const [lastCalendarKind, setLastCalendarKind] = useState<CalendarKind>(focus?.kind ?? "daily");
  const [generateOpen, setGenerateOpen] = useState(false);
  // Legacy source-ID chips and per-thread links both pop the ThreadDetail
  // modal — reuses the same OpenThreadState shape as other navigation paths.
  const [openThread, setOpenThread] = useState<OpenThreadState | null>(null);
  // Token bumped on every memory-ref click so stale async resolutions can
  // detect they were superseded by a later click and skip their setState,
  // matching the Chat tab's source-pill click guard.
  const refClickToken = useRef(0);

  // A summary appears in the list, calendar-detail, and search caches, and the
  // sidebar count badge; invalidate every family so all views drop the row.
  // Mirrors the App-level "summary generation done" invalidation set.
  const del = useDeleteAction(deleteSummary, [
    ["summaries"],
    ["count-thread-group-summaries"],
    ["summary-period-keys"],
    ["summary-search"],
    ["summary-distinct-labels"],
    ["summary-co-occurring-labels"],
  ]);

  // Thread-group summaries have no period_key, so the calendar view is
  // meaningless for that public kind.
  const calendarKind: CalendarKind | null = kind === "thread-group" ? null : kind;
  const periodDisplay = useMemo(
    () =>
      calendarKind == null
        ? undefined
        : {
            initialMode: periodDisplayPreference.remember
              ? periodDisplayPreference.mode
              : ("collapsed" as const),
            onBulkModeChange: (mode: PeriodSummaryDisplayPreference["mode"]) => {
              if (!periodDisplayPreference.remember) return;
              const next = { ...periodDisplayPreference, mode };
              setPeriodDisplayPreference(next);
              savePeriodSummaryDisplayPreference(next);
            },
          },
    [calendarKind, periodDisplayPreference],
  );
  const effectiveView: View = view === "calendar" && calendarKind == null ? "list" : view;
  const calendarState = calendarKind == null ? null : calendarStateByKind[calendarKind];
  const month = calendarState?.month ?? "";
  const selectedKey = calendarState?.selectedKey ?? null;
  const calendarPeriodKeyPrefixes = useMemo(
    () => (calendarKind == null ? [] : periodKeyPrefixesForMonth(calendarKind, month)),
    [calendarKind, month],
  );

  const updateCalendarState = useCallback(
    (targetKind: CalendarKind, next: Partial<CalendarState>) => {
      setCalendarStateByKind((current) => ({
        ...current,
        [targetKind]: { ...current[targetKind], ...next },
      }));
    },
    [],
  );
  const openCalendar = useCallback(
    (targetKind: CalendarKind, state?: Partial<CalendarState>) => {
      setKind(targetKind);
      setView("calendar");
      setLastCalendarKind(targetKind);
      if (state) updateCalendarState(targetKind, state);
    },
    [updateCalendarState],
  );
  // When a fresh `focus` arrives after mount (e.g. a second chat source
  // pill clicked while the tab is already open), reapply it and signal
  // back so the parent can drop the seed.
  useEffect(() => {
    if (!focus) return;
    openCalendar(focus.kind, { month: focus.month, selectedKey: focus.periodKey });
    onFocusConsumed?.();
  }, [focus, onFocusConsumed, openCalendar]);
  const searchLabels = useMemo(() => [KIND_SEARCH_LABEL[kind]], [kind]);

  const listQuery = useQuery({
    queryKey: ["summaries", kind, updatedAfter, updatedBefore, timezone],
    queryFn: () =>
      fetchSummaries(kind, {
        after: localDateToEpochMs(updatedAfter, timezone),
        before: localDateToEpochMs(updatedBefore, timezone),
      }),
    enabled: effectiveView === "list",
  });

  // Calendar existence dots: the period_keys present in the shown month.
  const periodKeysQuery = useQuery({
    queryKey: ["summary-period-keys", kind, calendarPeriodKeyPrefixes],
    queryFn: () =>
      listSummaryPeriodKeys({
        kind: calendarKind as CalendarKind,
        period_key_prefixes: calendarPeriodKeyPrefixes,
      }),
    enabled: effectiveView === "calendar" && calendarKind != null,
  });

  // Detail is selected by the external-id period token, rather than its
  // updated_at. A period summary's timestamp comes from its source memories
  // and need not fall in the calendar month named by that token.
  const detailQuery = useQuery({
    queryKey: ["summaries", kind, selectedKey, "detail"],
    queryFn: () => fetchSummaries(kind, { period_key: selectedKey ?? undefined }),
    enabled:
      effectiveView === "calendar" &&
      calendarKind != null &&
      selectedKey != null &&
      selectedKey !== "",
  });

  const searchEnabled = effectiveView === "search" && debouncedQuery.trim().length > 0;
  const search = useQuery({
    queryKey: ["summary-search", kind, mode, debouncedQuery, updatedAfter, updatedBefore, timezone],
    enabled: searchEnabled,
    queryFn: () =>
      SEARCH_MODES[mode].fn({
        query_text: debouncedQuery.trim(),
        mode,
        user_id: LOOKBACK_USER_ID,
        memory_kinds: [KIND_SEARCH_MEMORY_KIND[kind]],
        labels_any: searchLabels,
        label_match: undefined,
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
  // A recovery record is intentionally not a synthetic summary.  It may be
  // the only evidence for a period whose overall Memory was never persisted,
  // so expose it separately in the calendar without inflating summary counts.
  const selectedRecoveryRecords = useMemo(
    () =>
      summaryRecovery.data?.records.filter(
        (record) => record.period_kind === calendarKind && record.period_key === selectedKey,
      ) ?? [],
    [calendarKind, selectedKey, summaryRecovery.data?.records],
  );
  const warningPeriodKeys = useMemo(
    () =>
      summaryRecovery.data?.records
        .filter((record) => record.period_kind === calendarKind)
        .map((record) => record.period_key) ?? [],
    [calendarKind, summaryRecovery.data?.records],
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
  const handleOpenMemoryRef = useCallback(
    async (memoryId: string) => {
      const myToken = ++refClickToken.current;
      const target = await resolveSummaryRefNavigation(memoryId);
      if (myToken !== refClickToken.current) return;
      if (!target) return;
      if (target.kind === "open-thread") {
        setOpenThread(target.open);
      } else if (target.kind === "open-thread-group") {
        onNavigateThreadGroup?.(target.groupId);
      } else {
        openCalendar(target.focus.kind, {
          month: target.focus.month,
          selectedKey: target.focus.periodKey,
        });
      }
    },
    [onNavigateThreadGroup, openCalendar],
  );

  const refHandlers = useMemo(
    () => ({ onOpenThread: handleOpenThread, onOpenMemoryRef: handleOpenMemoryRef }),
    [handleOpenThread, handleOpenMemoryRef],
  );

  const count = subtitleCount(effectiveView, {
    list: listQuery.data?.filter((entry) => entry.kind !== "per-thread").length,
    search: searchEnabled ? (search.data?.length ?? 0) : undefined,
    calendar: periodKeysQuery.data?.length,
  });

  const generating = summaryGenerationProgress.busy;
  const startGeneration = () => setGenerateOpen(true);

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
                // ignores `enabled: false`, so reloading the inactive period list
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
            {summaryRecovery.data?.health === "healthy" &&
              summaryRecovery.data.record_count > 0 && (
                <button
                  type="button"
                  className="btn summary-recovery-indicator"
                  onClick={() => setRecoveryDetailsOpen(true)}
                  title={t("summaries.recoveryDetailsButton", {
                    count: summaryRecovery.data.record_count,
                  })}
                  aria-label={t("summaries.recoveryDetailsButton", {
                    count: summaryRecovery.data.record_count,
                  })}
                >
                  <span className="summary-recovery-indicator-icon" aria-hidden="true">
                    ⓘ
                  </span>
                  <span>{summaryRecovery.data.record_count}</span>
                </button>
              )}
            {generating ? (
              // Mirror the chat composer pattern (`src/pages/Chat.tsx`):
              // while busy, the primary action's slot is replaced by a
              // single "停止" button that fires the cancel API. Same
              // visual position, no layout shift, no hover-to-reveal
              // affordance to discover.
              <button
                type="button"
                className="btn secondary"
                onClick={() => void summaryGenerationProgress.cancel()}
                title={t("summaries.cancelTitle")}
              >
                {t("summaries.cancel")}
              </button>
            ) : (
              <button
                type="button"
                className="btn primary"
                onClick={() => void startGeneration()}
                title={t("summaries.generateTitle")}
              >
                {t("summaries.generate")}
              </button>
            )}
          </>
        }
      />
      <div className="content">
        {summaryRecovery.data?.health !== undefined &&
          summaryRecovery.data.health !== "healthy" && (
            <div className="form-error" role="alert">
              {t("summaries.recoveryStateError")}
              {summaryRecovery.data.detail ? `: ${summaryRecovery.data.detail}` : ""}
              {summaryRecovery.data.health === "corrupt" && (
                <button
                  type="button"
                  className="btn secondary"
                  onClick={() => setConfirmRecoveryReset(true)}
                >
                  {t("summaries.recoveryReset")}
                </button>
              )}
            </div>
          )}
        {summaryGenerationProgress.snapshot && (
          <StagedProgressPanel
            snapshot={summaryGenerationProgress.snapshot}
            title={t("summaryGen.progressTitle")}
            labels={summaryStageLabels(t)}
            steps={SUMMARY_GENERATION_STEPS}
            statusNamespace="analysis"
            showOverall
            onClose={() => summaryGenerationProgress.clear()}
          />
        )}

        {confirmRecoveryReset && (
          <ConfirmDialog
            title={t("summaries.recoveryResetTitle")}
            message={t("summaries.recoveryResetMessage")}
            confirmLabel={t("summaries.recoveryReset")}
            onCancel={() => setConfirmRecoveryReset(false)}
            onConfirm={() => {
              setConfirmRecoveryReset(false);
              void resetSummaryRecoveryState().then(() => summaryRecovery.refetch());
            }}
          />
        )}

        {recoveryDetailsOpen &&
          summaryRecovery.data?.health === "healthy" &&
          summaryRecovery.data.record_count > 0 && (
            <Modal
              onClose={() => setRecoveryDetailsOpen(false)}
              ariaLabel={t("summaries.recoveryDetailsTitle")}
            >
              <div className="modal-head">
                <div className="modal-title">{t("summaries.recoveryDetailsTitle")}</div>
              </div>
              <div className="modal-body">
                <p className="summary-recovery-details-description">
                  {t("summaries.recoveryPending", { count: summaryRecovery.data.record_count })}
                </p>
                <ul className="summary-recovery-details-list">
                  {summaryRecovery.data.records.map((record) => (
                    <li key={summaryRecoveryRecordKey(record)}>
                      <div className="summary-recovery-record">
                        {formatSummaryRecoveryRecord(record, t)}
                      </div>
                      {record.detail && (
                        <div className="summary-recovery-record-detail">{record.detail}</div>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
              <div className="modal-foot">
                <button type="button" className="btn" onClick={() => setRecoveryDetailsOpen(false)}>
                  {t("summaries.recoveryDetailsClose")}
                </button>
              </div>
            </Modal>
          )}

        <div className="search-bar">
          <div className="segment">
            {PUBLIC_SUMMARY_KINDS.map((k) => (
              <button
                type="button"
                key={k}
                className={`segment-btn ${kind === k ? "active" : ""}`}
                onClick={() => {
                  setKind(k);
                  if (k !== "thread-group") {
                    setLastCalendarKind(k);
                    if (kind !== k) {
                      updateCalendarState(k, { selectedKey: null });
                    }
                  }
                }}
              >
                {t(KIND_LABEL_KEYS[k])}
              </button>
            ))}
          </div>

          {calendarKind != null && (
            <label className="summary-display-preference">
              <input
                type="checkbox"
                checked={periodDisplayPreference.remember}
                onChange={(event) => {
                  const next = { ...periodDisplayPreference, remember: event.target.checked };
                  setPeriodDisplayPreference(next);
                  savePeriodSummaryDisplayPreference(next);
                }}
              />
              <span>
                <strong>{t("summaries.structured.remember")}</strong>
                <small>{t("summaries.structured.rememberDescription")}</small>
              </span>
            </label>
          )}

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
              onClick={() => {
                if (calendarKind == null) setKind(lastCalendarKind);
                setView("calendar");
              }}
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

        {effectiveView === "list" && (
          <ListView
            query={listQuery}
            onDelete={del.request}
            periodDisplay={periodDisplay}
            onNavigateThreadGroup={onNavigateThreadGroup}
          />
        )}

        {effectiveView === "search" && (
          <SearchResults
            hits={search.data}
            isLoading={searchEnabled && search.isLoading}
            error={search.error as Error | null}
            mode={mode}
            enabled={searchEnabled}
            periodDisplay={periodDisplay}
            onNavigateThreadGroup={onNavigateThreadGroup}
          />
        )}

        {effectiveView === "calendar" && calendarKind != null && (
          <div className="sum-cal-layout">
            <SummaryCalendar
              kind={calendarKind}
              month={month}
              periodKeys={periodKeysQuery.data ?? []}
              warningPeriodKeys={warningPeriodKeys}
              selectedKey={selectedKey}
              onSelectKey={(key) => updateCalendarState(calendarKind, { selectedKey: key })}
              onMonthChange={(m) => {
                updateCalendarState(calendarKind, { month: m, selectedKey: null });
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
                <>
                  <div className="empty-desc">
                    {selectedRecoveryRecords.length > 0
                      ? t("summaries.calendar.incomplete")
                      : t("summaries.calendar.noEntries", { key: selectedKey })}
                  </div>
                  {selectedRecoveryRecords.length > 0 && (
                    <ul className="empty-desc">
                      {selectedRecoveryRecords.map((record) => (
                        <li
                          key={summaryRecoveryRecordKey(record)}
                          title={record.path_label ?? undefined}
                        >
                          {formatSummaryRecoveryRecord(record, t)}
                        </li>
                      ))}
                    </ul>
                  )}
                </>
              )}
              <div className="sum-list">
                <SummaryCardList
                  entries={selectedEntries}
                  onDelete={del.request}
                  periodDisplay={periodDisplay}
                />
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
          onStart={summaryGenerationProgress.reset}
          onDispatchError={(dispatchId) => summaryGenerationProgress.clear(dispatchId)}
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

function summaryStageLabels(t: (key: string) => string): Record<string, string> {
  return {
    "thread-summary": t("summaryGen.stageThreadSummary"),
    "thread-group-summary": t("summaryGen.stageThreadGroupSummary"),
    daily: t(KIND_LABEL_KEYS.daily),
    weekly: t(KIND_LABEL_KEYS.weekly),
    monthly: t(KIND_LABEL_KEYS.monthly),
  };
}

function summaryRecoveryRecordKey(record: SummaryRecoveryRecord): string {
  return [
    record.memories_endpoint_id,
    record.owner_id,
    record.source_scope_digest,
    record.period_kind,
    record.period_key,
    record.rollup_scope,
    record.path_bucket_digest ?? "overall",
  ].join(":");
}

function formatSummaryRecoveryRecord(
  record: SummaryRecoveryRecord,
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  const path =
    record.rollup_scope === "path"
      ? formatSummaryPath(
          record.path_bucket_digest
            ? {
                path_bucket_digest: record.path_bucket_digest,
                path_bucket_kind: record.path_bucket_kind ?? "unassigned",
                path_label: record.path_label,
              }
            : null,
          t,
        )
      : t("summaries.card.rollup.overall");
  return t("summaries.recoveryRecord", {
    kind: record.period_kind,
    period: record.period_key,
    path,
    stage: record.stage,
    reason: record.reason_code,
  });
}

function fetchSummaries(
  kind: SummaryKind,
  options: {
    after?: number;
    before?: number;
    labels_any?: string[];
    period_key?: string;
  } = {},
): Promise<SummaryEntry[]> {
  return listSummaries({
    kind,
    // A period detail is filtered after the kind-only RPC response so it
    // remains compatible with external-id format changes. Do not cap that
    // response before its exact period key has been selected.
    ...(options.period_key === undefined ? { limit: 200 } : {}),
    ...(options.after !== undefined ? { updated_after_ms: options.after } : {}),
    ...(options.before !== undefined ? { updated_before_ms: options.before } : {}),
    ...(options.labels_any !== undefined ? { labels_any: options.labels_any } : {}),
    ...(options.period_key !== undefined ? { period_key: options.period_key } : {}),
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
  periodDisplay,
  onNavigateThreadGroup,
}: {
  query: ReturnType<typeof useQuery<SummaryEntry[]>>;
  onDelete: (memoryId: string) => void;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
  onNavigateThreadGroup?: (groupId: string) => void;
}) {
  const { t } = useTranslation();
  const { data, isLoading, error } = query;
  const publicEntries = (data ?? []).filter((entry) => entry.kind !== "per-thread");
  return (
    <>
      {isLoading && <div className="empty-desc">{t("common.loading")}</div>}
      {error && (
        <div style={{ color: "var(--danger)", fontSize: 12 }}>{(error as Error).message}</div>
      )}
      <div className="sum-list">
        <SummaryCardList
          entries={publicEntries}
          onDelete={onDelete}
          periodDisplay={periodDisplay}
          onNavigateThreadGroup={onNavigateThreadGroup}
        />
      </div>
      {!isLoading && !error && publicEntries.length === 0 && (
        <div className="empty-state">
          <div className="empty-title">{t("summaries.list.emptyTitle")}</div>
          <div className="empty-desc">{t("summaries.list.emptyDesc")}</div>
        </div>
      )}
    </>
  );
}

/**
 * A legacy `_all` row remains directly visible when it is the only known
 * representation of a period.  When a v2 row for that period also exists but
 * is incomplete, keep the legacy body available as a collapsed previous
 * representation instead of presenting two competing primary cards.
 */
function SummaryCardList({
  entries,
  onDelete,
  periodDisplay,
  onNavigateThreadGroup,
}: {
  entries: SummaryEntry[];
  onDelete: (memoryId: string) => void;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
  onNavigateThreadGroup?: (groupId: string) => void;
}) {
  incrementUiMemoryCounter("summary_list_render_attempts_total");
  const { t } = useTranslation();
  const publicEntries = entries.filter((entry) => entry.kind !== "per-thread");
  // Keyed lookups keep this linear; a period page can hold hundreds of rows.
  const periodKey = (entry: SummaryEntry) => JSON.stringify([entry.kind, entry.period_key]);
  const currentPeriods = new Set(entries.filter((candidate) => !candidate.legacy).map(periodKey));
  const legacyPrevious = publicEntries.filter(
    (entry) => entry.legacy && entry.scope_key === "_all" && currentPeriods.has(periodKey(entry)),
  );
  const legacyPreviousSet = new Set(legacyPrevious);
  const primary = publicEntries.filter((entry) => !legacyPreviousSet.has(entry));
  const cards = (rows: SummaryEntry[]) =>
    rows.map((entry) => (
      <SummaryCard
        key={entry.memory_id}
        entry={entry}
        onDelete={() => onDelete(entry.memory_id)}
        periodDisplay={periodDisplay}
        onNavigateThreadGroup={onNavigateThreadGroup}
      />
    ));
  return (
    <>
      {cards(primary)}
      {legacyPrevious.length > 0 && (
        <details className="summary-path-disclosure">
          <summary>{t("summaries.card.previousLegacy", { count: legacyPrevious.length })}</summary>
          {cards(legacyPrevious)}
        </details>
      )}
    </>
  );
}

function SummaryCard({
  entry,
  onDelete,
  periodDisplay,
  onNavigateThreadGroup,
}: {
  entry: SummaryEntry;
  onDelete: () => void;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
  onNavigateThreadGroup?: (groupId: string) => void;
}) {
  incrementUiMemoryCounter("summary_card_render_attempts_total");
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const { onOpenThread } = useSummaryRefHandlers();
  const content = parseSummaryContent(entry);
  // `_all` is the default scope and adds no signal; only surface a named scope.
  const scopeBadge = entry.scope_key && entry.scope_key !== "_all" ? entry.scope_key : null;
  const rollupBadge =
    entry.rollup_scope == null ? null : t(`summaries.card.rollup.${entry.rollup_scope}`);
  const completionBadge =
    entry.completion_status === "incomplete"
      ? t("summaries.card.incomplete", { count: entry.missing_path_bucket_count ?? 0 })
      : null;
  const consistencyBadge =
    entry.generation_consistency && entry.generation_consistency !== "aligned"
      ? t(`summaries.card.generation.${entry.generation_consistency}`)
      : null;
  const fallbackBadge =
    (entry.fallback_path_bucket_digests?.length ?? 0) > 0
      ? t("summaries.card.fallback", { count: entry.fallback_path_bucket_digests?.length ?? 0 })
      : null;
  const missingBadge =
    (entry.missing_path_bucket_digests?.length ?? 0) > 0
      ? t("summaries.card.missing", { count: entry.missing_path_bucket_digests?.length ?? 0 })
      : null;
  const failedAttemptBadge =
    entry.last_material_attempt_status === "failed" &&
    entry.last_material_attempt_generation_id != null &&
    entry.last_material_attempt_generation_id !== entry.generation_id
      ? t("summaries.card.latestAttemptFailed")
      : null;
  const contextTruncatedBadge =
    entry.context_truncated && (entry.omitted_source_count ?? 0) > 0
      ? t("summaries.card.contextTruncated", { count: entry.omitted_source_count })
      : null;
  const threadId = entry.thread_id;
  const isPerThreadLink = entry.kind === "per-thread" && threadId != null && onOpenThread != null;
  const isGroupLink =
    entry.kind === "thread-group" && entry.group_id != null && onNavigateThreadGroup != null;
  return (
    <div className="sum-card">
      <div className="sum-head">
        {isGroupLink ? (
          <button
            type="button"
            className="signal-thread-link"
            title={t("summaries.card.openGroupTitle")}
            onClick={() => onNavigateThreadGroup(entry.group_id as string)}
          >
            {t("summaries.groupFallbackTitle", { id: entry.group_id })}
          </button>
        ) : isPerThreadLink ? (
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
        {rollupBadge && <span className="label-pill">{rollupBadge}</span>}
        {scopeBadge && <span className="label-pill">{scopeBadge}</span>}
        {completionBadge && <span className="label-pill">{completionBadge}</span>}
        {consistencyBadge && <span className="label-pill">{consistencyBadge}</span>}
        {fallbackBadge && <span className="label-pill">{fallbackBadge}</span>}
        {missingBadge && <span className="label-pill">{missingBadge}</span>}
        {failedAttemptBadge && <span className="label-pill">{failedAttemptBadge}</span>}
        {contextTruncatedBadge && <span className="label-pill">{contextTruncatedBadge}</span>}
        <span style={{ marginLeft: "auto" }}>
          {formatDateTime(entry.updated_at_ms, locale, timezone)}
        </span>
        <button type="button" className="btn danger" onClick={onDelete}>
          {t("common.delete")}
        </button>
      </div>
      <SummaryBody parsed={content.parsed} raw={content.raw} periodDisplay={periodDisplay} />
      {(entry.path_bucket_diagnostics?.length ?? 0) > 0 && (
        <details className="summary-path-disclosure">
          <summary>{t("summaries.pathSummaries.diagnostics")}</summary>
          <ul className="empty-desc">
            {entry.path_bucket_diagnostics?.map((diagnostic) => {
              const path = formatSummaryPath(
                {
                  path_bucket_digest: diagnostic.path_bucket_digest,
                  path_bucket_kind: diagnostic.path_bucket_kind,
                  path_label: diagnostic.path_label,
                },
                t,
              );
              return (
                <li key={diagnostic.path_bucket_digest} title={diagnostic.path_label ?? undefined}>
                  {path}: {diagnostic.statuses.join(", ")}
                </li>
              );
            })}
          </ul>
        </details>
      )}
      {entry.rollup_scope === "overall" &&
        entry.source_scope_digest != null &&
        entry.period_key != null &&
        entry.kind !== "per-thread" &&
        (entry.path_summary_count ?? 0) >= 2 && (
          <PathSummaryDisclosure entry={entry} periodDisplay={periodDisplay} />
        )}
    </div>
  );
}

function PathSummaryDisclosure({
  entry,
  periodDisplay,
}: {
  entry: SummaryEntry;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const children = useQuery({
    queryKey: ["summary-path-children", entry.kind, entry.period_key, entry.source_scope_digest],
    queryFn: () =>
      listSummaryPathChildren({
        kind: entry.kind as CalendarKind,
        period_key: entry.period_key as string,
        source_scope_digest: entry.source_scope_digest as string,
      }),
    enabled: open,
  });
  return (
    <details
      className="summary-path-disclosure summary-path-section"
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary>
        <span className="summary-path-section-heading">
          <span className="summary-path-section-icon" aria-hidden="true">
            ⌘
          </span>
          <span>
            <strong>
              {t("summaries.pathSummaries.title", { count: entry.path_summary_count })}
            </strong>
            <small>{t("summaries.pathSummaries.description")}</small>
          </span>
        </span>
      </summary>
      <div className="summary-path-section-content">
        {children.isLoading && <p className="empty-desc">{t("common.loading")}</p>}
        {children.error && <p className="chat-error">{(children.error as Error).message}</p>}
        {children.data?.map((child) => (
          <PathSummaryRow key={child.memory_id} entry={child} periodDisplay={periodDisplay} />
        ))}
        {!children.isLoading && !children.error && children.data?.length === 0 && (
          <p className="empty-desc">{t("summaries.pathSummaries.empty")}</p>
        )}
      </div>
    </details>
  );
}

function PathSummaryRow({
  entry,
  periodDisplay,
}: {
  entry: SummaryPathChildEntry;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const content = useQuery({
    queryKey: ["summary-content", entry.memory_id],
    queryFn: () => getSummaryContent(entry.memory_id),
    enabled: open,
  });
  const path = formatSummaryPath(entry.path_bucket, t);
  const parsed =
    content.data == null ? null : parseSummaryContent({ ...entry, content_json: content.data });
  return (
    <details
      className="summary-path-row"
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary title={entry.path_bucket?.path_label ?? path}>
        <span className="summary-path-row-heading">
          <span className="summary-path-row-icon" aria-hidden="true">
            ⌁
          </span>
          <span className="summary-path-row-path">{path}</span>
        </span>
        {entry.aggregation_mode === "passthrough" && (
          <span className="label-pill">{t("summaries.pathSummaries.passthrough")}</span>
        )}
      </summary>
      <div className="summary-path-row-content">
        {content.isLoading && <p className="empty-desc">{t("common.loading")}</p>}
        {content.error && <p className="chat-error">{(content.error as Error).message}</p>}
        {parsed && (
          <SummaryBody parsed={parsed.parsed} raw={parsed.raw} periodDisplay={periodDisplay} />
        )}
      </div>
    </details>
  );
}

function formatSummaryPath(
  bucket: SummaryEntry["path_bucket"],
  t: (key: string) => string,
): string {
  if (bucket?.path_bucket_kind === "unassigned") return t("summaries.pathSummaries.unassigned");
  if (bucket?.path_bucket_kind === "ambiguous") return t("summaries.pathSummaries.ambiguous");
  const raw = bucket?.path_label?.replace(/^path:/, "") ?? "";
  if (raw.length <= 48) return raw;
  const parts = raw.split("/").filter(Boolean);
  return parts.length > 1 ? `…/${parts.slice(-2).join("/")}` : `…${raw.slice(-47)}`;
}

function SearchResults({
  hits,
  isLoading,
  error,
  mode,
  enabled,
  periodDisplay,
  onNavigateThreadGroup,
}: {
  hits: ThreadHit[] | undefined;
  isLoading: boolean;
  error: Error | null;
  mode: SearchMode;
  enabled: boolean;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
  onNavigateThreadGroup?: (groupId: string) => void;
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
        <SearchHitCard
          key={hit.thread_id}
          hit={hit}
          periodDisplay={periodDisplay}
          onNavigateThreadGroup={onNavigateThreadGroup}
        />
      ))}
    </div>
  );
}

/** A summary search hit. The snippet is a raw-JSON excerpt from the matched
 *  memory, so the collapsed header shows the scraped title (falling back to
 *  the thread name); expanding fetches the full summary memory and renders it
 *  structured via `SummaryBody`. */
function SearchHitCard({
  hit,
  periodDisplay,
  onNavigateThreadGroup,
}: {
  hit: ThreadHit;
  periodDisplay?: React.ComponentProps<typeof SummaryBody>["periodDisplay"];
  onNavigateThreadGroup?: (groupId: string) => void;
}) {
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
    (hit.group_id != null ? t("summaries.groupFallbackTitle", { id: hit.group_id }) : null) ??
    hit.thread_description ??
    t("summaries.fallbackTitle", { id: hit.thread_id });
  const groupLink = hit.group_id != null && onNavigateThreadGroup != null;

  return (
    <div className="sum-card">
      <div className="sum-head">
        {groupLink ? (
          <button
            type="button"
            className="signal-thread-link"
            title={t("summaries.card.openGroupTitle")}
            onClick={() => onNavigateThreadGroup(hit.group_id as string)}
          >
            {title}
          </button>
        ) : (
          <span>{title}</span>
        )}
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
        {content && (
          <SummaryBody parsed={content.parsed} raw={content.raw} periodDisplay={periodDisplay} />
        )}
      </details>
    </div>
  );
}

/** Prefixes that cover exactly the calendar period tokens drawn for `month`.
 *  They intentionally derive from `external_id`, not Memory timestamps. */
export function periodKeyPrefixesForMonth(kind: CalendarKind, monthKey: string): string[] {
  if (kind === "daily") return [`${monthKey}-`];
  if (kind === "monthly") return [monthKey];
  const ym = monthKeyToYearMonth(monthKey);
  if (!ym) return [];
  return [
    ...new Set(buildMonthGrid(ym.y, ym.m).flatMap((day) => (day ? [isoWeekOfDate(day)] : []))),
  ];
}
