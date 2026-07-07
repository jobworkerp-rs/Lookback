import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  deleteReflection,
  enqueueReflectionJob,
  searchReflections,
  searchReflectionsHybrid,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { DateInput } from "@/components/DateInput";
import { MarkdownBody } from "@/components/MarkdownMessage";
import { ThreadDetail } from "@/components/ThreadDetail";
import { Toolbar } from "@/components/Toolbar";
import { useDebouncedValue } from "@/hooks/useDebouncedValue";
import { useDeleteAction } from "@/hooks/useDeleteAction";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import type { ReflectionProgressHandle } from "@/hooks/useReflectionProgress";
import { isVectorDegraded, type SidecarStatus } from "@/hooks/useSidecarStatus";
import { useTimezone } from "@/hooks/useTimezone";
import { localDateToEpochMs } from "@/lib/dateInput";
import { formatDateTime } from "@/lib/localeFormat";
import {
  OUTCOME_FILTER_VALUES,
  outcomeLabel,
  reflectionAspectLabel,
  taskCategoryLabel,
} from "@/lib/searchTaxonomy";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type { ConnectionMode, ReflectionEntry, ThreadSummary } from "@/types/api";

export function Reflections({
  reflectionProgress,
  sidecar,
  connectionMode = "local",
}: {
  reflectionProgress: ReflectionProgressHandle;
  sidecar: SidecarStatus;
  connectionMode?: ConnectionMode | null;
}) {
  const { t } = useTranslation();
  // Date filter anchored to midnight in the display timezone (matches the
  // rendered card timestamps; see Threads for the boundary rationale).
  const timezone = useTimezone();
  const vectorDisabled = isVectorDegraded(sidecar, connectionMode) != null;
  const [selectedOutcomes, setSelectedOutcomes] = useState<number[]>([]);
  const [createdAfter, setCreatedAfter] = useState<string>("");
  const [queryText, setQueryText] = useState<string>("");
  const [enqueueError, setEnqueueError] = useState<string | null>(null);
  const [enqueueBusy, setEnqueueBusy] = useState(false);
  // Origin thread opened from a reflection card's link, shown as a modal
  // over this page (same pattern as Personality's SignalsDrawer).
  const [openThread, setOpenThread] = useState<ThreadSummary | null>(null);
  const del = useDeleteAction(deleteReflection, [["reflections"]]);

  const debouncedQueryText = useDebouncedValue(queryText, 300);
  const query = debouncedQueryText.trim();
  const createdAfterMs = localDateToEpochMs(createdAfter, timezone);
  const hasActiveFilter =
    query.length > 0 || selectedOutcomes.length > 0 || createdAfter.trim().length > 0;
  const reflections = useQuery({
    queryKey: ["reflections", selectedOutcomes, createdAfter, timezone],
    queryFn: () =>
      searchReflections({
        outcomes: selectedOutcomes,
        created_after_ms: createdAfterMs,
        limit: 200,
      }),
  });
  const hybridSearch = useQuery({
    queryKey: ["reflection-hybrid-search", selectedOutcomes, createdAfter, query, timezone],
    enabled: query.length > 0 && !vectorDisabled,
    queryFn: () =>
      searchReflectionsHybrid({
        query_text: query,
        outcomes: selectedOutcomes,
        created_after_ms: createdAfterMs,
        limit: 50,
      }),
  });
  const visibleReflections = useMemo(() => {
    if (query.length === 0) return reflections.data ?? [];
    if (!vectorDisabled && !hybridSearch.isSuccess) return [];
    if (!vectorDisabled && (hybridSearch.data?.length ?? 0) > 0) return hybridSearch.data ?? [];
    return (reflections.data ?? []).filter((entry) => reflectionMatchesQuery(entry, query));
  }, [hybridSearch.data, hybridSearch.isSuccess, reflections.data, query, vectorDisabled]);

  async function handleEnqueue() {
    setEnqueueError(null);
    setEnqueueBusy(true);
    try {
      const dispatchId = `reflection-${crypto.randomUUID()}`;
      const res = await enqueueReflectionJob({ dispatch_id: dispatchId });
      reflectionProgress.start(res.job_id_hint);
    } catch (e) {
      setEnqueueError((e as Error).message);
    } finally {
      setEnqueueBusy(false);
    }
  }

  const total = visibleReflections.length;
  const subtitle = useMemo(() => {
    if (reflections.isLoading || hybridSearch.isLoading) return t("common.loading");
    return t("reflections.subtitleCount", { count: total });
  }, [hybridSearch.isLoading, reflections.isLoading, total, t]);

  return (
    <>
      <Toolbar
        title={t("reflections.title")}
        subtitle={subtitle}
        actions={
          <>
            <button
              type="button"
              className="btn"
              onClick={() => reflections.refetch()}
              disabled={reflections.isFetching}
            >
              {t("common.reload")}
            </button>
            {reflectionProgress.progress?.status === "active" ? (
              <button
                type="button"
                className="btn danger"
                onClick={() => reflectionProgress.cancel()}
                title={t("reflections.cancelTitle")}
              >
                {t("reflections.cancel")}
              </button>
            ) : (
              <button
                type="button"
                className="btn primary"
                onClick={() => void handleEnqueue()}
                disabled={enqueueBusy}
                title={t("reflections.generateTitle")}
              >
                {enqueueBusy ? t("reflections.enqueuing") : t("reflections.generate")}
              </button>
            )}
          </>
        }
      />
      <div className="content">
        <div className="search-bar">
          <input
            type="text"
            className="text-input"
            placeholder={t("reflections.searchPlaceholder")}
            value={queryText}
            onChange={(e) => setQueryText(e.target.value)}
            title={t("reflections.searchTitle")}
            style={{ flex: 1, minWidth: 240 }}
          />
          <DateInput
            value={createdAfter}
            onChange={setCreatedAfter}
            title={t("reflections.createdAfterTitle")}
          />
          <div className="segment">
            {OUTCOME_FILTER_VALUES.map((value) => {
              const active = selectedOutcomes.includes(value);
              return (
                <button
                  key={value}
                  type="button"
                  className={`segment-btn ${active ? "active" : ""}`}
                  onClick={() => {
                    setSelectedOutcomes((prev) =>
                      prev.includes(value) ? prev.filter((v) => v !== value) : [...prev, value],
                    );
                  }}
                >
                  {outcomeLabel(t, value)}
                </button>
              );
            })}
            {selectedOutcomes.length > 0 && (
              <button type="button" className="segment-btn" onClick={() => setSelectedOutcomes([])}>
                ×
              </button>
            )}
          </div>
        </div>

        {enqueueError && (
          <div style={{ color: "var(--danger)", fontSize: 12, marginBottom: 8 }}>
            {enqueueError}
          </div>
        )}
        {reflectionProgress.progress && (
          <div
            className={`reflection-progress ${reflectionProgress.progress.status}`}
            style={{ fontSize: 12, marginBottom: 8 }}
          >
            <strong>
              {reflectionProgress.progress.status === "done"
                ? t("reflections.progress.done")
                : reflectionProgress.progress.status === "failed"
                  ? t("reflections.progress.failed")
                  : t("reflections.progress.active")}
            </strong>
            {reflectionProgress.progress.message && (
              <pre
                style={{
                  marginTop: 4,
                  fontSize: 11,
                  whiteSpace: "pre-wrap",
                  maxHeight: 160,
                  overflow: "auto",
                  background: "var(--fill-secondary)",
                  padding: "6px 8px",
                  borderRadius: 4,
                }}
              >
                {reflectionProgress.progress.message}
              </pre>
            )}
            {(reflectionProgress.progress.status === "done" ||
              reflectionProgress.progress.status === "failed") && (
              <button
                type="button"
                className="btn"
                style={{ marginTop: 4 }}
                onClick={reflectionProgress.clear}
              >
                {t("reflections.progress.close")}
              </button>
            )}
          </div>
        )}

        {(reflections.error || hybridSearch.error) && (
          <div style={{ color: "var(--danger)", fontSize: 12 }}>
            {((reflections.error ?? hybridSearch.error) as Error).message}
          </div>
        )}

        {visibleReflections.map((r) => (
          <ReflectionCard
            key={r.id}
            entry={r}
            onOpenThread={() =>
              setOpenThread(
                synthesizeThreadSummary({
                  id: r.origin_thread_id,
                  createdAtMs: r.created_at_ms,
                  updatedAtMs: r.created_at_ms,
                }),
              )
            }
            onDelete={() => del.request(r.id)}
          />
        ))}

        {!reflections.isLoading &&
          !hybridSearch.isLoading &&
          !reflections.error &&
          !hybridSearch.error &&
          total === 0 && (
            <div className="empty-state">
              <div className="empty-title">
                {hasActiveFilter ? t("reflections.noHits.title") : t("reflections.empty.title")}
              </div>
              <div className="empty-desc">
                {hasActiveFilter ? t("reflections.noHits.desc") : t("reflections.empty.desc")}
              </div>
            </div>
          )}
      </div>

      {openThread && <ThreadDetail thread={openThread} onClose={() => setOpenThread(null)} />}

      {del.pendingId != null && (
        <ConfirmDialog
          title={t("reflections.delete.title")}
          message={t("reflections.delete.message")}
          busy={del.busy}
          error={del.error}
          onConfirm={() => void del.confirm()}
          onCancel={del.cancel}
        />
      )}
    </>
  );
}

function reflectionMatchesQuery(entry: ReflectionEntry, query: string): boolean {
  const haystack = [
    entry.summary,
    entry.task_intent,
    entry.mitigation_hint ?? "",
    ...entry.lessons,
    ...entry.key_decisions,
    ...entry.success_factors,
  ]
    .join("\n")
    .toLocaleLowerCase();
  const tokens = query
    .toLocaleLowerCase()
    .split(/\s+/)
    .map((token) => token.trim())
    .filter(Boolean);
  return tokens.every((token) => haystack.includes(token));
}

export function ReflectionCard({
  entry,
  onOpenThread,
  onDelete,
}: {
  entry: ReflectionEntry;
  onOpenThread: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  return (
    <div className="reflection-card">
      <div className="reflection-card-head">
        <span className="reflection-tag">{taskCategoryLabel(t, entry.task_category)}</span>
        <span className="reflection-tag">{outcomeLabel(t, entry.outcome)}</span>
        <span className="reflection-tag">{reflectionAspectLabel(t, entry.reflection_aspect)}</span>
        <span style={{ marginLeft: "auto" }}>
          <span className="reflection-score">{entry.score.toFixed(2)}</span> ·{" "}
          {formatDateTime(entry.created_at_ms, locale, timezone)}
        </span>
      </div>
      {entry.task_intent && (
        <div
          className="message-body"
          style={{ fontSize: 12, color: "var(--label-secondary)", marginBottom: 4 }}
        >
          <MarkdownBody>{entry.task_intent}</MarkdownBody>
        </div>
      )}
      {/* The reflector emits markdown (headings, lists, `code`) in the long-form
          fields, so render them through the shared MarkdownBody instead of as
          plain text. `message-body` carries the compact heading/list/code styles. */}
      <div className="reflection-summary message-body">
        <MarkdownBody>{entry.summary}</MarkdownBody>
      </div>
      <div style={{ marginTop: 6, display: "flex", alignItems: "center", gap: 12 }}>
        <button type="button" className="signal-thread-link" onClick={onOpenThread}>
          {t("reflections.card.openThread")}
        </button>
        <button
          type="button"
          className="btn danger"
          style={{ marginLeft: "auto" }}
          onClick={onDelete}
        >
          {t("common.delete")}
        </button>
      </div>
      <ReflectionList title={t("reflections.card.lessons")} items={entry.lessons} />
      <ReflectionList title={t("reflections.card.keyDecisions")} items={entry.key_decisions} />
      {entry.mitigation_hint && (
        <div className="reflection-section">
          <div className="reflection-section-title">{t("reflections.card.mitigationHint")}</div>
          <div className="message-body" style={{ fontSize: 12 }}>
            <MarkdownBody>{entry.mitigation_hint}</MarkdownBody>
          </div>
        </div>
      )}
    </div>
  );
}

/** A labelled bullet list whose items may carry inline markdown. Capped at 5
 *  to keep a card scannable (same cap the plain-text version used). */
function ReflectionList({ title, items }: { title: string; items: string[] }) {
  if (items.length === 0) return null;
  return (
    <div className="reflection-section">
      <div className="reflection-section-title">{title}</div>
      <ul className="reflection-section-list message-body">
        {items.slice(0, 5).map((item) => (
          <li key={item}>
            <MarkdownBody>{item}</MarkdownBody>
          </li>
        ))}
      </ul>
    </div>
  );
}
