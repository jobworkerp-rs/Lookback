import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type KeyboardEvent, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import * as api from "@/api";
import {
  deleteThread,
  findCoOccurringLabels,
  findDistinctLabels,
  findThread,
  listThreads,
  resolveMemoryReference,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { DateInput } from "@/components/DateInput";
import { ImportProgressPanel } from "@/components/ImportProgressPanel";
import { LabelFilter } from "@/components/LabelFilter";
import { MarkdownBody } from "@/components/MarkdownMessage";
import { Modal } from "@/components/Modal";
import { SummaryBody } from "@/components/SummaryBody";
import { ThreadDetail, type ThreadHighlight } from "@/components/ThreadDetail";
import { Toolbar } from "@/components/Toolbar";
import { useDebouncedValue } from "@/hooks/useDebouncedValue";
import { useDeleteAction } from "@/hooks/useDeleteAction";
import type { ImportSnapshot } from "@/hooks/useImportProgress";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import { isVectorDegraded, type SidecarStatus } from "@/hooks/useSidecarStatus";
import type { StepStreamProgressHandle } from "@/hooks/useStepStreamProgress";
import { useTimezone } from "@/hooks/useTimezone";
import { localDateToEpochMs } from "@/lib/dateInput";
import { sortLabelsByPrefixPriority } from "@/lib/labelPrefix";
import { formatDateTime } from "@/lib/localeFormat";
import {
  PERSONALITY_QUERY_KEY,
  threadGroupSummaryBatchKey,
  threadGroupSummaryKey,
} from "@/lib/queryKeys";
import { isEmbeddingSearchMode, resolveSearchErrorHint, SEARCH_MODES } from "@/lib/searchModes";
import { parseThreadDescription } from "@/lib/threadDescription";
import {
  fetchThreadGroupSummaries,
  groupDisplayTitle,
  isThreadGroupSnapshotChangedError,
  isThreadGroupUnsupportedError,
  liveThreadId,
  relatedGroups,
  threadGroupSummaryState,
  threadGroupTreeProjection,
} from "@/lib/threadGroupView";
import { parseThreadReference } from "@/lib/threadReference";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type {
  ConnectionMode,
  LabelMatch,
  MemoryReference,
  SearchMode,
  ThreadGroupLineageView,
  ThreadGroupListItem,
  ThreadGroupRecoveryStatus,
  ThreadGroupSearchPage,
  ThreadGroupSearchResultView,
  ThreadGroupSummaryView,
  ThreadHit,
  ThreadSummary,
} from "@/types/api";

export interface ThreadsPageProps {
  onOpenImport: () => void;
  sidecar: SidecarStatus;
  memoriesMode?: ConnectionMode | null;
  importSnapshot?: ImportSnapshot | null;
  importBusy?: boolean;
  onCancelImport?: () => Promise<void>;
  onClearImportProgress?: () => void;
  initialGroupId?: string | null;
  onInitialGroupConsumed?: () => void;
  summaryProgress?: StepStreamProgressHandle;
}

type Mode = "browse" | SearchMode;
type ThreadsView = "groups" | "threads";

const threadGroupApiAvailable = typeof api.listThreadGroups === "function";

interface Selection {
  thread: ThreadSummary;
  // Set when opened from a search hit, so ThreadDetail can scroll to it.
  highlight?: ThreadHighlight;
}

type ImportCancelFeedback =
  | { kind: "idle" }
  | { kind: "requesting" }
  | { kind: "requested" }
  | { kind: "failed"; error: string };

function ImportCancelAction({ onCancel }: { onCancel?: () => Promise<void> }) {
  const { t } = useTranslation();
  const [feedback, setFeedback] = useState<ImportCancelFeedback>({ kind: "idle" });
  const requestPending = useRef(false);
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  async function requestCancel(): Promise<void> {
    if (!onCancel || requestPending.current) return;
    requestPending.current = true;
    setFeedback({ kind: "requesting" });
    try {
      await onCancel();
      if (mounted.current) setFeedback({ kind: "requested" });
    } catch (error) {
      if (mounted.current) {
        setFeedback({
          kind: "failed",
          error: error instanceof Error ? error.message : String(error),
        });
      }
    } finally {
      requestPending.current = false;
    }
  }

  return (
    <>
      <button
        type="button"
        className="btn secondary"
        onClick={() => void requestCancel()}
        disabled={feedback.kind === "requesting" || feedback.kind === "requested"}
        title={t("importToast.cancelHint")}
      >
        {feedback.kind === "requesting"
          ? t("importToast.cancelRequesting")
          : t("importToast.cancel")}
      </button>
      {feedback.kind === "requested" && (
        <span role="status" className="toolbar-subtitle">
          {t("importToast.cancelRequested")}
        </span>
      )}
      {feedback.kind === "failed" && (
        <span role="alert" style={{ color: "var(--danger)", fontSize: 11 }}>
          {t("importToast.cancelRequestFailed", { error: feedback.error })}
        </span>
      )}
    </>
  );
}

export function Threads({
  onOpenImport,
  sidecar,
  memoriesMode = "local",
  importSnapshot = null,
  importBusy = false,
  onCancelImport,
  onClearImportProgress,
  initialGroupId = null,
  onInitialGroupConsumed,
  summaryProgress,
}: ThreadsPageProps) {
  const { t } = useTranslation();
  // Date filters are anchored to midnight in the display timezone so the search
  // boundary matches the timestamps the list renders (both go through the same
  // zone); without this a thread shown as "4/30" could leak into a "5/1 onward"
  // search when the OS zone differs from the configured one.
  const timezone = useTimezone();
  // Semantic / hybrid embed the query, so they're unavailable while the local
  // vector store is degraded. Keyword (FTS) and browse stay usable.
  const vectorDisabled = isVectorDegraded(sidecar, memoriesMode) != null;
  const [threadsView, setThreadsView] = useState<ThreadsView>(
    threadGroupApiAvailable ? "groups" : "threads",
  );
  const [allowCrossMember, setAllowCrossMember] = useState(false);
  const [groupCursor, setGroupCursor] = useState<{
    conditionKey: string;
    token: string | null;
  }>({ conditionKey: "", token: null });
  const [selectedGroupId, setSelectedGroupId] = useState<string | null>(null);
  const [selectedGroupOwnerUserId, setSelectedGroupOwnerUserId] = useState<number | undefined>();
  const [selected, setSelected] = useState<Selection | null>(null);
  const [filter, setFilter] = useState("");
  const debouncedFilter = useDebouncedValue(filter, 300);
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
  useEffect(() => {
    if (initialGroupId != null) {
      setThreadsView("groups");
      setSelectedGroupOwnerUserId(undefined);
      setSelectedGroupId(initialGroupId);
      onInitialGroupConsumed?.();
    }
  }, [initialGroupId, onInitialGroupConsumed]);
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
  // are separated by memory kind, so the raw-thread list must not include them.
  // They are NOT cascade-deleted, so their caches are intentionally left alone.
  // Label aggregates (distinct + co-occurring) ARE invalidated — removing the
  // last thread carrying a label should drop it from the filter bar.
  const del = useDeleteAction(deleteThread, [
    ["threads"],
    ["thread-search"],
    ["distinct-labels"],
    ["co-occurring-labels"],
    PERSONALITY_QUERY_KEY,
    // Sidebar counts are event-driven, and a deleted member can drop a group
    // out of the summary count.
    ["count-thread-group-summaries"],
    ["count-thread-groups"],
    ["thread-row"],
    ["memory-reference"],
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
    enabled: threadsView === "threads" || threadsView === "groups",
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
    enabled: (threadsView === "threads" || threadsView === "groups") && useCoOccurring,
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
    enabled: threadsView === "threads" && mode === "browse",
  });

  const groupBrowse = mode === "browse";
  const groupSearchEnabled =
    threadsView === "groups" &&
    threadGroupApiAvailable &&
    (groupBrowse || debouncedQuery.trim().length > 0);
  const showGroupMatchCount = !groupBrowse && debouncedQuery.trim().length > 0;
  const groupSearchConditionKey = JSON.stringify([
    threadsView,
    groupBrowse,
    mode,
    debouncedQuery,
    createdAfter,
    createdBefore,
    sortedLabels,
    effectiveMatch ?? "any",
    allowCrossMember,
    timezone,
  ]);
  // Keep an old cursor in state for the next-page action, but never send it
  // while the condition key is changing. This avoids one stale RPC between a
  // control change and the reset effect below.
  const groupPageToken =
    groupCursor.conditionKey === groupSearchConditionKey ? groupCursor.token : null;
  const groupSearch = useQuery<ThreadGroupSearchPage>({
    queryKey: [
      "thread-group-search",
      groupSearchConditionKey,
      groupBrowse,
      mode,
      debouncedQuery,
      createdAfter,
      createdBefore,
      sortedLabels,
      effectiveMatch ?? "any",
      allowCrossMember,
      timezone,
      groupPageToken,
    ],
    enabled: groupSearchEnabled,
    queryFn: () =>
      api.searchThreadGroups({
        query_text: groupBrowse ? "" : debouncedQuery.trim(),
        mode: (groupBrowse ? "keyword" : mode) as SearchMode,
        created_after_ms: localDateToEpochMs(createdAfter, timezone),
        created_before_ms: localDateToEpochMs(createdBefore, timezone),
        ...labelArgs,
        allow_cross_member: allowCrossMember,
        page_size: 50,
        page_token: groupPageToken ?? undefined,
      }),
  });
  useEffect(() => {
    if (isThreadGroupSnapshotChangedError(groupSearch.error)) {
      setGroupCursor({ conditionKey: groupSearchConditionKey, token: null });
    }
  }, [groupSearch.error, groupSearchConditionKey]);
  const groupLineage = useQuery({
    queryKey: ["thread-group", selectedGroupId, selectedGroupOwnerUserId],
    queryFn: () => api.findThreadGroup(selectedGroupId as string, selectedGroupOwnerUserId),
    enabled: selectedGroupId != null,
  });
  const groupSummary = useQuery({
    queryKey: threadGroupSummaryKey(selectedGroupId, selectedGroupOwnerUserId),
    queryFn: () => api.findThreadGroupSummary(selectedGroupId as string, selectedGroupOwnerUserId),
    enabled: selectedGroupId != null,
  });
  const groupRecovery = useQuery<ThreadGroupRecoveryStatus>({
    queryKey: ["thread-group-recovery-status"],
    queryFn: api.getThreadGroupRecoveryStatus,
    enabled: threadGroupApiAvailable,
    retry: false,
  });
  const [retryingGroupId, setRetryingGroupId] = useState<string | null>(null);
  const [groupRetryError, setGroupRetryError] = useState<string | null>(null);
  const [resolvingGroupId, setResolvingGroupId] = useState<string | null>(null);
  const [groupResolutionError, setGroupResolutionError] = useState<string | null>(null);

  const retryGroupSummary = async (group: ThreadGroupListItem) => {
    setRetryingGroupId(group.id);
    setGroupRetryError(null);
    try {
      const response = await api.enqueueThreadGroupSummaryJob({
        dispatch_id: crypto.randomUUID(),
        target_group_id: group.id,
        group_owner_user_id: group.user_id,
        expected_membership_snapshot_digest: group.membership_snapshot_digest,
        force_resummarize: true,
      });
      summaryProgress?.start(response.job_id_hint);
    } catch (error) {
      setGroupRetryError(error instanceof Error ? error.message : String(error));
    } finally {
      setRetryingGroupId(null);
    }
  };

  const retryGroupIssueResolution = async (group: ThreadGroupListItem) => {
    setResolvingGroupId(group.id);
    setGroupResolutionError(null);
    try {
      const response = await api.enqueueThreadGroupIssueResolutionJob({
        dispatch_id: crypto.randomUUID(),
        target_group_id: group.id,
        group_owner_user_id: group.user_id,
        expected_membership_snapshot_digest: group.membership_snapshot_digest,
      });
      summaryProgress?.start(response.job_id_hint);
    } catch (error) {
      setGroupResolutionError(error instanceof Error ? error.message : String(error));
    } finally {
      setResolvingGroupId(null);
    }
  };

  // A cursor belongs to one complete search condition. Changing any control
  // starts a fresh keyset walk instead of asking the backend to continue a
  // cursor issued for a different result set.
  useEffect(() => {
    setGroupCursor({ conditionKey: groupSearchConditionKey, token: null });
  }, [groupSearchConditionKey]);

  // Empty query disables the search query so we don't fire an
  // RPC with empty text the moment the user picks a mode.
  const searchEnabled =
    threadsView === "threads" && mode !== "browse" && debouncedQuery.trim().length > 0;
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

  // Direct ID lookup belongs only to Browse. Search modes retain their
  // established query semantics, including numeric full-text queries.
  const reference = useMemo(
    () => (mode === "browse" ? parseThreadReference(debouncedFilter) : null),
    [mode, debouncedFilter],
  );
  // Keep fetched candidates hidden until the input that initiated the lookup
  // still represents the same target. The request remains debounced, while a
  // user can never click a stale result during the debounce window.
  const currentReference = useMemo(
    () => (mode === "browse" ? parseThreadReference(filter) : null),
    [mode, filter],
  );
  const displayedReference =
    reference?.kind === currentReference?.kind && reference?.id === currentReference?.id
      ? reference
      : null;
  const referenceId = reference?.id ?? "";
  const threadReference = useQuery({
    queryKey: ["thread-row", referenceId],
    queryFn: () => findThread(referenceId),
    enabled: reference != null && reference.kind !== "memory",
  });
  const memoryReference = useQuery({
    queryKey: ["memory-reference", referenceId],
    queryFn: () => resolveMemoryReference(referenceId),
    enabled: reference != null && reference.kind !== "thread",
  });

  const filtered = useMemo(() => {
    if (!threads.data) return [];
    // An exact ID is navigation intent, not a title/label filter. Keep the
    // browse list available below its independent ID candidate section.
    if (parseThreadReference(filter)) return threads.data;
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
  const subtitle =
    threadsView === "groups"
      ? t("threads.groups.subtitle", {
          count: groupSearch.data?.results.length ?? 0,
        })
      : isSearching
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
              onClick={() => {
                if (threadsView === "groups") {
                  void groupSearch.refetch();
                } else if (isSearching) void search.refetch();
                else void threads.refetch();
              }}
              disabled={
                threadsView === "groups"
                  ? groupSearch.isFetching
                  : isSearching
                    ? search.isFetching
                    : threads.isFetching
              }
            >
              {t("common.reload")}
            </button>
            {importBusy ? (
              <ImportCancelAction
                key={importSnapshot?.job_id ?? "unknown"}
                onCancel={onCancelImport}
              />
            ) : (
              <button type="button" className="btn primary" onClick={onOpenImport}>
                {t("common.import")}
              </button>
            )}
          </>
        }
      />
      <div className="content">
        {importSnapshot && (
          <ImportProgressPanel
            key={importSnapshot.job_id}
            snapshot={importSnapshot}
            onClose={() => onClearImportProgress?.()}
          />
        )}
        <div className="segment" style={{ marginBottom: 10 }}>
          <button
            type="button"
            className={`segment-btn ${threadsView === "groups" ? "active" : ""}`}
            onClick={() => setThreadsView("groups")}
            disabled={!threadGroupApiAvailable}
          >
            {t("threads.view.groups")}
          </button>
          <button
            type="button"
            className={`segment-btn ${threadsView === "threads" ? "active" : ""}`}
            onClick={() => setThreadsView("threads")}
          >
            {t("threads.view.threads")}
          </button>
        </div>

        <SearchControls
          mode={mode}
          isSearching={isSearching}
          vectorDisabled={vectorDisabled}
          query={query}
          filter={filter}
          createdAfter={createdAfter}
          createdBefore={createdBefore}
          showBrowseFilter={threadsView === "threads"}
          showGroupFilters={threadsView === "groups"}
          showCrossMember={threadsView === "groups"}
          allowCrossMember={allowCrossMember}
          onModeChange={setMode}
          onQueryChange={setQuery}
          onFilterChange={setFilter}
          onCreatedAfterChange={setCreatedAfter}
          onCreatedBeforeChange={setCreatedBefore}
          onAllowCrossMemberChange={setAllowCrossMember}
        />

        <LabelFilter
          labels={(labelAggregate.data ?? []).map(({ label, thread_count: count }) => ({
            label,
            count,
          }))}
          coOccurringLabels={
            useCoOccurring
              ? coOccurringQuery.data?.map(({ label, thread_count: count }) => ({
                  label,
                  count,
                }))
              : undefined
          }
          selected={selectedLabels}
          match={labelMatch}
          onToggle={toggleLabel}
          onToggleMany={toggleManyLabels}
          onSetMatch={setLabelMatch}
          isLoading={labelAggregate.isLoading}
        />

        {threadsView === "groups" ? (
          <ThreadGroupsView
            searchQuery={groupSearch}
            searchEnabled={groupSearchEnabled}
            showMatchingThreads={showGroupMatchCount}
            onNextPage={() => {
              const token = groupSearch.data?.next_page_token;
              if (token) setGroupCursor({ conditionKey: groupSearchConditionKey, token });
            }}
            onSelect={(group) => {
              setSelectedGroupOwnerUserId(group.user_id);
              setSelectedGroupId(group.id);
            }}
            recovery={groupRecovery.data}
            summaryGenerating={summaryProgress?.busy ?? false}
            retryingGroupId={retryingGroupId}
          />
        ) : isSearching ? (
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
            reference={displayedReference}
            threadReference={
              displayedReference?.kind === "memory" ? undefined : threadReference.data
            }
            threadReferenceError={
              displayedReference?.kind === "memory" ? null : (threadReference.error as Error | null)
            }
            threadReferenceLoading={
              displayedReference?.kind !== "memory" && threadReference.isLoading
            }
            memoryReference={
              displayedReference?.kind === "thread" ? undefined : memoryReference.data
            }
            memoryReferenceError={
              displayedReference?.kind === "thread" ? null : (memoryReference.error as Error | null)
            }
            memoryReferenceLoading={
              displayedReference?.kind !== "thread" && memoryReference.isLoading
            }
            data={filtered}
            isLoading={threads.isLoading}
            error={threads.error as Error | null}
            onSelect={(thread) => setSelected({ thread })}
            onOpenImport={onOpenImport}
            selectedLabels={selectedLabels}
            onToggleLabel={toggleLabel}
            onSelectReference={(selection) => setSelected(selection)}
          />
        )}
      </div>

      {selectedGroupId && (
        <ThreadGroupDetail
          lineage={groupLineage.data}
          summary={groupSummary.data}
          recovery={groupRecovery.data}
          summaryGenerating={summaryProgress?.busy ?? false}
          retrying={retryingGroupId === selectedGroupId}
          retryError={groupRetryError}
          resolvingIssueResolution={resolvingGroupId === selectedGroupId}
          issueResolutionError={groupResolutionError}
          isLoading={groupLineage.isLoading || groupSummary.isLoading}
          error={(groupLineage.error ?? groupSummary.error) as Error | null}
          onClose={() => {
            setSelectedGroupId(null);
            setSelectedGroupOwnerUserId(undefined);
          }}
          onOpenThread={(thread) => setSelected({ thread })}
          onOpenGroup={setSelectedGroupId}
          onRetry={() => {
            if (groupLineage.data) void retryGroupSummary(groupLineage.data.group);
          }}
          onRetryIssueResolution={() => {
            if (groupLineage.data) void retryGroupIssueResolution(groupLineage.data.group);
          }}
        />
      )}

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

function SearchControls({
  mode,
  isSearching,
  vectorDisabled,
  query,
  filter,
  createdAfter,
  createdBefore,
  showBrowseFilter,
  showGroupFilters,
  showCrossMember,
  allowCrossMember,
  onModeChange,
  onQueryChange,
  onFilterChange,
  onCreatedAfterChange,
  onCreatedBeforeChange,
  onAllowCrossMemberChange,
}: {
  mode: Mode;
  isSearching: boolean;
  vectorDisabled: boolean;
  query: string;
  filter: string;
  createdAfter: string;
  createdBefore: string;
  showBrowseFilter: boolean;
  showGroupFilters: boolean;
  showCrossMember: boolean;
  allowCrossMember: boolean;
  onModeChange: (mode: Mode) => void;
  onQueryChange: (query: string) => void;
  onFilterChange: (filter: string) => void;
  onCreatedAfterChange: (date: string) => void;
  onCreatedBeforeChange: (date: string) => void;
  onAllowCrossMemberChange: (enabled: boolean) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="search-bar">
      <div className="segment">
        <button
          type="button"
          className={`segment-btn ${mode === "browse" ? "active" : ""}`}
          onClick={() => onModeChange("browse")}
        >
          {t("search.mode.browse")}
        </button>
        <button
          type="button"
          className={`segment-btn ${mode === "keyword" ? "active" : ""}`}
          onClick={() => onModeChange("keyword")}
          title={t("search.modeTitle.keyword")}
        >
          {t("search.mode.keyword")}
        </button>
        <button
          type="button"
          className={`segment-btn ${mode === "semantic" ? "active" : ""}`}
          onClick={() => onModeChange("semantic")}
          disabled={vectorDisabled}
          title={vectorDisabled ? t("search.modeDisabled") : t("search.modeTitle.semantic")}
        >
          {t("search.mode.semantic")}
        </button>
        <button
          type="button"
          className={`segment-btn ${mode === "hybrid" ? "active" : ""}`}
          onClick={() => onModeChange("hybrid")}
          disabled={vectorDisabled}
          title={vectorDisabled ? t("search.modeDisabled") : t("search.modeTitle.hybrid")}
        >
          {t("search.mode.hybrid")}
        </button>
      </div>
      {isSearching || showGroupFilters ? (
        <>
          {isSearching && (
            <input
              type="text"
              className="text-input"
              placeholder={t("threads.searchPlaceholder")}
              value={query}
              onChange={(event) => onQueryChange(event.target.value)}
              style={{ flex: 1, minWidth: 240 }}
            />
          )}
          <DateInput
            value={createdAfter}
            onChange={onCreatedAfterChange}
            title={t("threads.createdAfterTitle")}
          />
          <DateInput
            value={createdBefore}
            onChange={onCreatedBeforeChange}
            title={t("threads.createdBeforeTitle")}
          />
          {isSearching && showCrossMember && (
            <label className="label-filter-toggle">
              <input
                type="checkbox"
                checked={allowCrossMember}
                onChange={(event) => onAllowCrossMemberChange(event.target.checked)}
              />
              {t("threads.groups.allowCrossMember")}
            </label>
          )}
        </>
      ) : (
        showBrowseFilter && (
          <input
            type="text"
            className="text-input"
            placeholder={t("threads.filterPlaceholder")}
            value={filter}
            onChange={(event) => onFilterChange(event.target.value)}
            style={{ flex: 1 }}
          />
        )
      )}
    </div>
  );
}

function ThreadGroupsView({
  searchQuery,
  searchEnabled,
  showMatchingThreads,
  onSelect,
  onNextPage,
  recovery,
  summaryGenerating,
  retryingGroupId,
}: {
  searchQuery: {
    data?: ThreadGroupSearchPage;
    isLoading: boolean;
    error: Error | null;
  };
  searchEnabled: boolean;
  showMatchingThreads: boolean;
  onSelect: (group: ThreadGroupListItem) => void;
  onNextPage: () => void;
  recovery?: ThreadGroupRecoveryStatus;
  summaryGenerating: boolean;
  retryingGroupId: string | null;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const groups = searchQuery.data?.results.map((result) => result.group) ?? [];
  const activeQuery = searchQuery;
  const searchResultsByGroupId = new Map<string, ThreadGroupSearchResultView>();
  for (const result of searchQuery.data?.results ?? []) {
    if (!searchResultsByGroupId.has(result.group.id)) {
      searchResultsByGroupId.set(result.group.id, result);
    }
  }
  const failedGroupIds = new Set(recovery?.records.map((record) => record.group_id));
  const queryClient = useQueryClient();
  // One batched read for every card on the page; each result seeds the
  // per-group entry the detail pane reads.
  const summaryBatch = useQuery({
    queryKey: threadGroupSummaryBatchKey(groups),
    queryFn: async () => {
      const summaries = await fetchThreadGroupSummaries(groups, api.findThreadGroupSummaries);
      for (const group of groups) {
        queryClient.setQueryData(
          threadGroupSummaryKey(group.id, group.user_id),
          summaries.get(group.id) ?? null,
        );
      }
      return summaries;
    },
    enabled: !activeQuery.error && groups.length > 0,
  });
  if (activeQuery.error && isThreadGroupUnsupportedError(activeQuery.error)) {
    return (
      <div className="empty-state" role="alert">
        <div className="empty-title">{t("threads.groups.unsupported")}</div>
      </div>
    );
  }
  if (activeQuery.error) {
    return (
      <div className="empty-state" role="alert">
        <div className="empty-title">{t("threads.groups.error")}</div>
        <div className="empty-desc">{activeQuery.error.message}</div>
      </div>
    );
  }
  if (activeQuery.isLoading) return <div className="empty-desc">{t("common.loading")}</div>;
  if (groups.length === 0) {
    return (
      <div className="empty-state">
        <div className="empty-title">{t("threads.groups.emptyTitle")}</div>
        <div className="empty-desc">{t("threads.groups.emptyDesc")}</div>
      </div>
    );
  }
  return (
    <div className="thread-list">
      {groups.map((group) => {
        const result = searchResultsByGroupId.get(group.id);
        const matchingThreadCount =
          showMatchingThreads && result
            ? new Set(result.witnesses.map((witness) => witness.thread_id)).size
            : 0;
        return (
          <button
            type="button"
            className="thread-card"
            key={group.id}
            onClick={() => onSelect(group)}
            style={{ textAlign: "left" }}
          >
            <div className="thread-card-head">
              <span>
                {group.latest_activity_at_ms == null
                  ? "—"
                  : formatDateTime(group.latest_activity_at_ms, locale, timezone)}
              </span>
            </div>
            <div className="thread-title">{groupDisplayTitle(group)}</div>
            <div className="thread-card-foot">
              {showMatchingThreads && matchingThreadCount > 0 && (
                <span className="label-pill">
                  {t("threads.groups.matchingThreads", { count: matchingThreadCount })}
                </span>
              )}
              <span className="label-pill">
                {t("threads.groups.threadCount", { count: group.active_member_count })}
              </span>
              {group.deleted_member_count > 0 && (
                <span className="label-pill">
                  {t("threads.groups.deletedMembers", { count: group.deleted_member_count })}
                </span>
              )}
              {(() => {
                const state = threadGroupSummaryState({
                  group,
                  summary: summaryBatch.data?.get(group.id),
                  generating: summaryGenerating || retryingGroupId === group.id,
                  failed: failedGroupIds.has(group.id),
                });
                return state === "generating" ? (
                  <span className="label-pill">{t("threads.groups.summaryGenerating")}</span>
                ) : state === "failed" ? (
                  <span className="label-pill">{t("threads.groups.summaryFailed")}</span>
                ) : state === "latest" ? (
                  <span className="label-pill">{t("threads.groups.summaryLatest")}</span>
                ) : state === "stale" ? (
                  <span className="label-pill">{t("threads.groups.summaryStale")}</span>
                ) : summaryBatch.isError ? (
                  <span className="label-pill">{t("threads.groups.summaryError")}</span>
                ) : null;
              })()}
            </div>
          </button>
        );
      })}
      {searchEnabled && searchQuery.data?.next_page_token && (
        <button
          type="button"
          className="btn secondary"
          onClick={onNextPage}
          disabled={searchQuery.isLoading}
        >
          {t("threads.groups.nextPage")}
        </button>
      )}
    </div>
  );
}

function ThreadGroupDetail({
  lineage,
  summary,
  recovery,
  summaryGenerating,
  retrying,
  retryError,
  resolvingIssueResolution,
  issueResolutionError,
  isLoading,
  error,
  onClose,
  onOpenThread,
  onOpenGroup,
  onRetry,
  onRetryIssueResolution,
}: {
  lineage: ThreadGroupLineageView | undefined;
  summary: ThreadGroupSummaryView | null | undefined;
  recovery?: ThreadGroupRecoveryStatus;
  summaryGenerating: boolean;
  retrying: boolean;
  retryError: string | null;
  resolvingIssueResolution: boolean;
  issueResolutionError: string | null;
  isLoading: boolean;
  error: Error | null;
  onClose: () => void;
  onOpenThread: (thread: ThreadSummary) => void;
  onOpenGroup: (groupId: string) => void;
  onRetry: () => void;
  onRetryIssueResolution: () => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const showGroupSummary = lineage?.group.active_member_count !== 1;
  const related = lineage ? relatedGroups(lineage.group.id, lineage.relations) : [];
  const treeProjection = lineage
    ? threadGroupTreeProjection(lineage.members, lineage.relations, lineage.unresolved)
    : null;
  const parentByChild = treeProjection?.parentByChild ?? new Map<string, string>();
  const treeRows = (treeProjection?.members ?? []).map((member) => {
    let depth = 0;
    let current = liveThreadId(member);
    const visited = new Set<string>();
    while (current != null && parentByChild.has(current) && !visited.has(current)) {
      visited.add(current);
      current = parentByChild.get(current) ?? null;
      depth += 1;
    }
    return { member, depth: Math.min(depth, 8) };
  });
  const summaryState = lineage
    ? threadGroupSummaryState({
        group: lineage.group,
        summary,
        generating: summaryGenerating || retrying,
        failed: recovery?.records.some((record) => record.group_id === lineage.group.id) ?? false,
      })
    : null;
  const parsedSummary = summary ? api.parseSummaryContent(summary) : null;
  return (
    <Modal wide onClose={onClose} ariaLabel={t("threads.groups.lineageTitle")}>
      <div className="modal-head">
        <div className="modal-title">
          {lineage ? groupDisplayTitle(lineage.group) : t("threads.groups.lineageTitle")}
        </div>
      </div>
      <div className="modal-body">
        {isLoading && <div className="empty-desc">{t("threads.groups.loading")}</div>}
        {error && (
          <div className="form-error" role="alert">
            {isThreadGroupUnsupportedError(error) ? t("threads.groups.unsupported") : error.message}
          </div>
        )}
        {lineage && (
          <>
            {showGroupSummary && (
              <>
                <h3>{t("threads.groups.summaryTitle")}</h3>
                {summaryState && (
                  <div className="label-pill">
                    {t(`threads.groups.summaryState.${summaryState}`)}
                  </div>
                )}
                {summary && parsedSummary ? (
                  <SummaryBody parsed={parsedSummary.parsed} raw={parsedSummary.raw} />
                ) : (
                  <div className="empty-desc">{t("threads.groups.summaryEmpty")}</div>
                )}
                {summaryState === "failed" && (
                  <div className="form-error" role="alert">
                    {t("threads.groups.summaryRetryHint")}
                  </div>
                )}
                {retryError && (
                  <div className="form-error" role="alert">
                    {retryError}
                  </div>
                )}
                <button
                  type="button"
                  className="btn secondary"
                  onClick={onRetry}
                  disabled={summaryGenerating || retrying}
                >
                  {retrying
                    ? t("threads.groups.summaryRetrying")
                    : t("threads.groups.summaryRetry")}
                </button>
              </>
            )}
            <div className="thread-group-action-list">
              <button
                type="button"
                className="btn secondary"
                onClick={onRetryIssueResolution}
                disabled={summaryGenerating || resolvingIssueResolution}
              >
                {resolvingIssueResolution
                  ? t("threads.groups.issueResolutionRetrying")
                  : t("threads.groups.issueResolutionRetry")}
              </button>
              <div className="empty-desc">{t("threads.groups.issueResolutionHint")}</div>
            </div>
            {issueResolutionError && (
              <div className="form-error" role="alert">
                {issueResolutionError}
              </div>
            )}
            <h3>{t("threads.groups.treeTitle")}</h3>
            <div className="thread-list">
              {treeRows.map(({ member, depth }) => {
                const threadId = liveThreadId(member);
                const display = member.display;
                const description = parseThreadDescription(
                  display?.description,
                  member.thread_canonical_key,
                );
                const treeRelation = threadId
                  ? treeProjection?.relationByChild.get(threadId)
                  : null;
                const inferredRelation = threadId
                  ? treeProjection?.inferredRelationByChild.get(threadId)
                  : null;
                const relationLabel = treeRelation
                  ? t(
                      `threads.groups.relationType.${treeRelation.relation_type}`,
                      t("threads.groups.relationType.unknown"),
                    )
                  : inferredRelation
                    ? t("threads.groups.treeInferred")
                    : t("threads.groups.treeRoot");
                const thread = threadId
                  ? synthesizeThreadSummary({
                      id: threadId,
                      description: display?.description,
                      createdAtMs: display?.created_at_ms ?? undefined,
                      updatedAtMs: display?.last_message_at_ms ?? undefined,
                    })
                  : null;
                return thread ? (
                  <div
                    className="thread-hit-card"
                    key={`${member.thread_canonical_key}:${member.state}`}
                    style={{ marginLeft: `${depth * 16}px` }}
                  >
                    <button
                      type="button"
                      className="thread-tree-open-button"
                      onClick={() => onOpenThread(thread)}
                    >
                      <div className="thread-hit-head">
                        <span className="thread-tree-title">{description.title}</span>
                        {inferredRelation && !treeRelation && (
                          <span className="thread-tree-inferred">
                            {t("threads.groups.inferredBadge")}
                          </span>
                        )}
                        <span className="thread-tree-open">
                          {t("threads.groups.openRelatedThread", {
                            relation: relationLabel,
                            id: threadId,
                          })}
                        </span>
                      </div>
                    </button>
                    {description.body && (
                      <div className="thread-tree-summary-body message-body">
                        <div className="thread-tree-summary-label">
                          {t("threads.groups.memberSummaryTitle")}
                        </div>
                        <MarkdownBody>{description.body}</MarkdownBody>
                      </div>
                    )}
                  </div>
                ) : (
                  <div
                    className="thread-hit-card thread-hit-card-static"
                    key={`${member.thread_canonical_key}:${member.state}`}
                    style={{ marginLeft: `${depth * 16}px` }}
                  >
                    <div className="thread-hit-head">
                      <span className="thread-tree-title">{description.title}</span>
                      <span>{t("threads.groups.deleted")}</span>
                    </div>
                    <div className="thread-hit-snippet">{member.state}</div>
                  </div>
                );
              })}
            </div>
            {related.length > 0 && (
              <>
                <h3>{t("threads.groups.relatedGroupsTitle")}</h3>
                <div className="thread-group-action-list">
                  {related.map(({ groupId }) => (
                    <button
                      type="button"
                      className="btn secondary"
                      key={groupId}
                      onClick={() => onOpenGroup(groupId)}
                    >
                      {t("threads.groups.openRelatedGroup", { id: groupId })}
                    </button>
                  ))}
                </div>
              </>
            )}
            <details className="thread-group-technical-info">
              <summary>{t("threads.groups.technicalTitle")}</summary>
              <dl className="empty-desc">
                <dt>{t("threads.groups.technicalGroupId")}</dt>
                <dd>{lineage.group.id}</dd>
                <dt>{t("threads.groups.technicalCanonicalKey")}</dt>
                <dd>{lineage.group.group_canonical_key}</dd>
                <dt>{t("threads.groups.technicalStatus")}</dt>
                <dd>{lineage.group.status}</dd>
                <dt>{t("threads.groups.technicalAuthority")}</dt>
                <dd>{lineage.group.grouping_authority}</dd>
                <dt>{t("threads.groups.technicalSnapshot")}</dt>
                <dd>{lineage.group.membership_snapshot_digest}</dd>
                <dt>{t("threads.groups.technicalCounts")}</dt>
                <dd>
                  {lineage.group.active_member_count} / {lineage.group.deleted_member_count} /{" "}
                  {lineage.group.unresolved_count}
                </dd>
                {summary && (
                  <>
                    <dt>{t("threads.groups.summaryDigest")}</dt>
                    <dd>{summary.membership_snapshot_digest ?? "—"}</dd>
                    <dt>{t("threads.groups.summaryUpdated")}</dt>
                    <dd>{formatDateTime(summary.updated_at_ms, locale, timezone)}</dd>
                    {summary.member_summary_count != null && (
                      <>
                        <dt>{t("threads.groups.summaryMemberCount")}</dt>
                        <dd>{summary.member_summary_count}</dd>
                      </>
                    )}
                  </>
                )}
              </dl>
            </details>
          </>
        )}
      </div>
      <div className="modal-foot">
        <button type="button" className="btn" onClick={onClose}>
          {t("threads.groups.close")}
        </button>
      </div>
    </Modal>
  );
}

function BrowseList({
  reference,
  threadReference,
  threadReferenceError,
  threadReferenceLoading,
  memoryReference,
  memoryReferenceError,
  memoryReferenceLoading,
  data,
  isLoading,
  error,
  onSelect,
  onOpenImport,
  selectedLabels,
  onToggleLabel,
  onSelectReference,
}: {
  reference: ReturnType<typeof parseThreadReference>;
  threadReference: ThreadSummary | null | undefined;
  threadReferenceError: Error | null;
  threadReferenceLoading: boolean;
  memoryReference: MemoryReference | null | undefined;
  memoryReferenceError: Error | null;
  memoryReferenceLoading: boolean;
  data: ThreadSummary[];
  isLoading: boolean;
  error: Error | null;
  onSelect: (thread: ThreadSummary) => void;
  onOpenImport: () => void;
  selectedLabels: string[];
  onToggleLabel: (label: string) => void;
  onSelectReference: (selection: Selection) => void;
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
      {reference && (
        <IdReferenceResults
          reference={reference}
          thread={threadReference}
          threadError={threadReferenceError}
          threadLoading={threadReferenceLoading}
          memory={memoryReference}
          memoryError={memoryReferenceError}
          memoryLoading={memoryReferenceLoading}
          onSelect={onSelectReference}
        />
      )}
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

function IdReferenceResults({
  reference,
  thread,
  threadError,
  threadLoading,
  memory,
  memoryError,
  memoryLoading,
  onSelect,
}: {
  reference: NonNullable<ReturnType<typeof parseThreadReference>>;
  thread: ThreadSummary | null | undefined;
  threadError: Error | null;
  threadLoading: boolean;
  memory: MemoryReference | null | undefined;
  memoryError: Error | null;
  memoryLoading: boolean;
  onSelect: (selection: Selection) => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const checksThread = reference.kind !== "memory";
  const checksMemory = reference.kind !== "thread";
  const done =
    (!checksThread || (!threadLoading && (thread !== undefined || threadError != null))) &&
    (!checksMemory || (!memoryLoading && (memory !== undefined || memoryError != null)));
  const noHit = done && !thread && !memory && !threadError && !memoryError;
  return (
    <section className="id-reference-results" aria-label={t("threads.idLookup.title")}>
      <div className="id-reference-title">{t("threads.idLookup.title")}</div>
      {(threadLoading || memoryLoading) && <div className="empty-desc">{t("common.loading")}</div>}
      {threadError && <div className="id-reference-error">{threadError.message}</div>}
      {memoryError && <div className="id-reference-error">{memoryError.message}</div>}
      {thread && (
        <button type="button" className="thread-hit-card" onClick={() => onSelect({ thread })}>
          <div className="thread-hit-head">
            <span>
              {
                parseThreadDescription(
                  thread.description,
                  t("threads.fallbackTitle", { id: thread.id }),
                ).title
              }
            </span>
            <span>{t("threads.idLookup.threadMatch")}</span>
          </div>
          <div className="thread-hit-snippet">
            {thread.channel ?? t("threadDetail.noChannel")} ·{" "}
            {formatDateTime(thread.updated_at_ms, locale, timezone)}
          </div>
        </button>
      )}
      {memory && (
        <div className="thread-hit-card id-memory-card">
          <div className="thread-hit-head">
            <span>{t("threads.idLookup.memoryTitle", { id: memory.memory.id })}</span>
            <span>{t("threads.idLookup.memoryMatch")}</span>
          </div>
          <div className="thread-hit-snippet">
            {memory.memory.content.replace(/\s+/g, " ").slice(0, 160)}
          </div>
          {memory.locations.length === 0 ? (
            <div className="empty-desc">{t("threads.idLookup.memoryNoThread")}</div>
          ) : (
            <div className="id-memory-locations">
              {memory.locations.map((location) => (
                <button
                  type="button"
                  className="btn secondary"
                  key={location.thread.id}
                  onClick={() =>
                    onSelect({
                      thread: location.thread,
                      highlight: {
                        memoryId: memory.memory.id,
                        position: location.position,
                        threadTotal: location.thread_total,
                      },
                    })
                  }
                >
                  {t("threads.idLookup.openMemoryThread", { id: location.thread.id })}
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      {noHit && (
        <div className="empty-desc">{t("threads.idLookup.noHit", { id: reference.id })}</div>
      )}
    </section>
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
