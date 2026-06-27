import { useQuery } from "@tanstack/react-query";
import { useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  deletePersonalityProfile,
  deletePersonalitySignal,
  enqueuePersonalityJob,
  enqueuePersonalityMergeJob,
  findMemoryThreadPosition,
  getPersonality,
  listPersonalitySignals,
  parsePersonalityContent,
  parsePersonalitySignalContent,
} from "@/api";
import { AnalysisProgress } from "@/components/AnalysisProgress";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { MemoryRefs } from "@/components/MemoryRefs";
import type { Route } from "@/components/Sidebar";
import { type OpenThreadState, ThreadDetail } from "@/components/ThreadDetail";
import { Toolbar } from "@/components/Toolbar";
import { useDeleteAction } from "@/hooks/useDeleteAction";
import { useEscape } from "@/hooks/useEscape";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import type { StepStreamProgressHandle } from "@/hooks/useStepStreamProgress";
import { resolveThreadHighlight } from "@/lib/chatSourceNav";
import { formatDateTime, formatNumber } from "@/lib/localeFormat";
import {
  formatProfileContent,
  formatSignalContent,
  PERSONA_CATEGORY_LABELS,
  rawSignalFallback,
  type SignalCategoryView,
} from "@/lib/personalitySignal";
import { buildPersonaStats, PERSONA_CATEGORIES } from "@/lib/personaStats";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import type { PersonalityProfileContent, PersonalitySignal, ThreadSummary } from "@/types/api";

/// Shared with App.tsx so the sidebar's thread count and the Personality
/// tab dedupe their `get_personality` fetch under React Query.
export const PERSONALITY_QUERY_KEY = ["personality", 1] as const;

export function Personality({
  personalityProgress,
  onNavigate,
}: {
  personalityProgress: StepStreamProgressHandle;
  onNavigate: (route: Route) => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const [enqueueError, setEnqueueError] = useState<string | null>(null);
  const [enqueueBusy, setEnqueueBusy] = useState(false);
  // `Force 再抽出` — when checked, the next generate run passes
  // `force_reextract: true` so it re-extracts every eligible thread instead of
  // skipping ones that already have a stored signal. Cleared after each
  // dispatch so a forced run is an explicit, single-shot action.
  const [forceReextract, setForceReextract] = useState(false);
  const [signalsOpen, setSignalsOpen] = useState(false);
  // A profile memory chip opens the source thread scrolled to that memory,
  // reusing the same ThreadDetail machinery as the signals drawer.
  const [openThread, setOpenThread] = useState<OpenThreadState | null>(null);
  const gridRef = useRef<HTMLDivElement>(null);
  // Deleting the merged profile only affects the profile query (which also
  // carries the sidebar thread count).
  const profileDel = useDeleteAction(deletePersonalityProfile, [PERSONALITY_QUERY_KEY]);

  const personality = useQuery({
    queryKey: PERSONALITY_QUERY_KEY,
    queryFn: () => getPersonality({ user_id: 1 }),
  });

  async function handleEnqueue() {
    setEnqueueError(null);
    if (forceReextract) {
      const ok = window.confirm(t("personality.forceReextractConfirm"));
      if (!ok) return;
    }
    setEnqueueBusy(true);
    try {
      // Dispatch id doubles as the cancel key; the Stop button below
      // sends it to analysis_cancel which targets the same in-flight map
      // entry the backend registered for this dispatch.
      const dispatch_id = crypto.randomUUID();
      const res = await enqueuePersonalityJob({
        force_reextract: forceReextract,
        dispatch_id,
      });
      personalityProgress.start(res.job_id_hint);
    } catch (e) {
      setEnqueueError((e as Error).message);
    } finally {
      // Drop the force flag whether the dispatch succeeded or failed.
      // Leaving it ticked on a thrown dispatch confused users: the next
      // click triggered the confirm dialog again unexpectedly because
      // the checkbox still read as "Force" while the user had only
      // intended to retry. Forces are explicit one-shots; the user can
      // re-check it for an explicit retry.
      setForceReextract(false);
      setEnqueueBusy(false);
    }
  }

  // Re-run ONLY the Layer-2 merge against the existing signals — no
  // per-thread fan-out. Use when an earlier full run produced layer-1
  // signals but no profile (e.g. external-LLM 429 storm during per-thread
  // extraction left the merge step orphaned). `force_remerge` is bridged
  // from the same Force checkbox: a checked Force here means "bypass the
  // merge's eligibility short-circuit and rebuild the profile even though
  // signal.updated_at hasn't moved" — the relevant override for a re-run
  // against unchanged sources. Skipped on Force without a confirmation
  // (unlike Force re-extract) because no LLM fan-out is involved — a
  // merge is a single LLM call.
  async function handleMergeOnly() {
    setEnqueueError(null);
    setEnqueueBusy(true);
    try {
      const dispatch_id = crypto.randomUUID();
      const res = await enqueuePersonalityMergeJob({
        force_remerge: forceReextract,
        dispatch_id,
      });
      personalityProgress.start(res.job_id_hint);
    } catch (e) {
      setEnqueueError((e as Error).message);
    } finally {
      // Mirror handleEnqueue: drop the Force flag so a follow-up click is
      // explicit. Skipping this would leave the checkbox stuck Force-on
      // and the next 「マージのみ」 click silently runs as force_remerge,
      // which the user may not have intended.
      setForceReextract(false);
      setEnqueueBusy(false);
    }
  }

  const generating = personalityProgress.busy;

  const parsed = useMemo<PersonalityProfileContent | null>(() => {
    const p = personality.data?.profile;
    return p ? parsePersonalityContent(p) : null;
  }, [personality.data]);

  const stats = useMemo(
    () =>
      buildPersonaStats({
        threads: personality.data?.thread_count ?? 0,
        signals: personality.data?.signal_count ?? 0,
        content: parsed,
      }),
    [personality.data, parsed],
  );

  // Flattened, display-ready category views. The profile's list/object fields
  // are arrays-of-objects / objects (not strings), so they MUST be flattened
  // before render — passing them raw to JSX threw "Objects are not valid as a
  // React child" and blanked the whole app.
  const profileViews = useMemo(() => (parsed ? formatProfileContent(parsed) : []), [parsed]);

  // Follow a profile entry's memory chip through the memory row's hydrated
  // thread_ids. Profile entries aggregate multiple source threads, so their
  // JSON cannot safely tell which thread a particular memory_id belongs to.
  async function openProfileMemory(memoryId: string) {
    const pos = await findMemoryThreadPosition({ memory_id: memoryId });
    if (!pos) return;
    const when = personality.data?.profile?.updated_at_ms ?? Date.now();
    const thread = synthesizeThreadSummary({
      id: pos.thread_id,
      createdAtMs: when,
      updatedAtMs: when,
    });
    setOpenThread({
      thread,
      highlight: {
        memoryId,
        position: pos.position,
        threadTotal: pos.thread_total,
      },
    });
  }

  const threadsBadge =
    personality.data?.thread_count_truncated && stats.threads > 0
      ? `${formatNumber(stats.threads, locale)}+`
      : formatNumber(stats.threads, locale);

  return (
    <>
      <Toolbar
        title={t("personality.title")}
        subtitle={
          personality.isLoading
            ? t("common.loading")
            : personality.data?.profile
              ? t("personality.updatedAt", {
                  time: formatDateTime(personality.data.profile.updated_at_ms, locale),
                })
              : t("personality.profileNotGenerated")
        }
        actions={
          <>
            <button
              type="button"
              className="btn"
              onClick={() => personality.refetch()}
              disabled={personality.isFetching}
            >
              {t("common.reload")}
            </button>
            <label
              style={{
                display: "inline-flex",
                alignItems: "center",
                gap: 4,
                fontSize: 12,
                color: forceReextract ? "var(--danger)" : "var(--muted)",
                cursor: enqueueBusy || generating ? "not-allowed" : "pointer",
                userSelect: "none",
              }}
              title={t("personality.forceReextractHint")}
            >
              <input
                type="checkbox"
                checked={forceReextract}
                onChange={(e) => setForceReextract(e.target.checked)}
                disabled={enqueueBusy || generating}
              />
              {t("personality.forceReextract")}
            </label>
            {generating ? (
              // Chat composer pattern: while busy, the slot becomes a
              // Stop button. enqueueBusy is the brief window before the
              // dispatch promise resolves and `generating` flips — keep
              // it as a disabled state on the Stop button so a double
              // click can't race the registration.
              <button
                type="button"
                className="btn secondary"
                onClick={() => void personalityProgress.cancel()}
                disabled={enqueueBusy}
                title={t("personality.stopHint")}
              >
                {t("personality.stop")}
              </button>
            ) : (
              <>
                <button
                  type="button"
                  className="btn primary"
                  onClick={() => void handleEnqueue()}
                  disabled={enqueueBusy}
                  title={t("personality.generateHint")}
                >
                  {enqueueBusy
                    ? t("personality.enqueuing")
                    : forceReextract
                      ? t("personality.generateForce")
                      : t("personality.generate")}
                </button>
                <button
                  type="button"
                  className="btn"
                  onClick={() => void handleMergeOnly()}
                  disabled={enqueueBusy}
                  title={t("personality.mergeOnlyHint")}
                >
                  {forceReextract ? t("personality.mergeForce") : t("personality.mergeOnly")}
                </button>
              </>
            )}
          </>
        }
      />
      <div className="content">
        <AnalysisProgress
          progress={personalityProgress.progress}
          error={enqueueError}
          onClose={personalityProgress.clear}
        />
        {personality.error && (
          <div style={{ color: "var(--danger)", fontSize: 12, marginBottom: 8 }}>
            {(personality.error as Error).message}
          </div>
        )}
        <div className="persona-head">
          <Stat
            label={t("personality.statThreads")}
            value={threadsBadge}
            onClick={() => onNavigate("threads")}
            hint={t("personality.statThreadsHint")}
          />
          <Stat
            label={t("personality.statSignals")}
            value={formatNumber(stats.signals, locale)}
            onClick={stats.signals > 0 ? () => setSignalsOpen(true) : undefined}
            hint={stats.signals > 0 ? t("personality.statSignalsHint") : undefined}
          />
          <Stat
            label={t("personality.statCategories")}
            value={`${stats.categories} / ${PERSONA_CATEGORIES}`}
            onClick={
              parsed
                ? () => gridRef.current?.scrollIntoView({ behavior: "smooth", block: "start" })
                : undefined
            }
            hint={parsed ? t("personality.statCategoriesHint") : undefined}
          />
          <Stat label={t("personality.statProfileVersion")} value={stats.profile_version} />
        </div>

        {!personality.isLoading && !personality.error && !parsed && (
          <div className="empty-state">
            <div className="empty-title">{t("personality.emptyTitle")}</div>
            <div className="empty-desc">{t("personality.emptyDesc")}</div>
          </div>
        )}

        {parsed && (
          <>
            <div style={{ display: "flex", marginBottom: 8 }}>
              <button
                type="button"
                className="btn danger"
                style={{ marginLeft: "auto" }}
                onClick={() => {
                  const id = personality.data?.profile?.memory_id;
                  if (id != null) profileDel.request(id);
                }}
              >
                {t("personality.deleteProfile")}
              </button>
            </div>
            <div className="persona-grid" ref={gridRef}>
              {PERSONA_CATEGORY_LABELS.map((label) => (
                <ProfileCategory
                  key={label}
                  label={label}
                  view={profileViews.find((v) => v.label === label) ?? null}
                  onOpenMemory={openProfileMemory}
                />
              ))}
            </div>
          </>
        )}
      </div>

      {openThread && (
        <ThreadDetail
          thread={openThread.thread}
          highlight={openThread.highlight}
          onClose={() => setOpenThread(null)}
        />
      )}

      {signalsOpen && <SignalsDrawer onClose={() => setSignalsOpen(false)} />}

      {profileDel.pendingId != null && (
        <ConfirmDialog
          title={t("personality.deleteProfileConfirmTitle")}
          message={t("personality.deleteProfileConfirmMessage")}
          confirmLabel={t("personality.deleteProfile")}
          busy={profileDel.busy}
          error={profileDel.error}
          onConfirm={() => void profileDel.confirm()}
          onCancel={profileDel.cancel}
        />
      )}
    </>
  );
}

function Stat({
  label,
  value,
  onClick,
  hint,
}: {
  label: string;
  value: string;
  onClick?: () => void;
  hint?: string;
}) {
  const content = (
    <>
      <div className="persona-stat-label">{label}</div>
      <div className="persona-stat-value">{value}</div>
    </>
  );
  if (onClick) {
    return (
      <button type="button" className="persona-stat clickable" onClick={onClick} title={hint}>
        {content}
      </button>
    );
  }
  return <div className="persona-stat">{content}</div>;
}

/**
 * Right-side drawer listing the layer-1 personality signals that back the
 * merged profile. Lazily fetches on open (the badge already carries the
 * count, so we defer the heavier per-thread fetch until the user drills in).
 * A clicked row opens the source conversation thread via ThreadDetail.
 */

const SIGNALS_QUERY_KEY = ["personality-signals", 1] as const;

function SignalsDrawer({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const [openThread, setOpenThread] = useState<OpenThreadState | null>(null);
  // Deleting a signal refreshes the drawer list and the profile query (whose
  // signal-count badge reflects it).
  const del = useDeleteAction(deletePersonalitySignal, [SIGNALS_QUERY_KEY, PERSONALITY_QUERY_KEY]);

  const signals = useQuery({
    queryKey: SIGNALS_QUERY_KEY,
    queryFn: () => listPersonalitySignals({ user_id: 1 }),
  });

  useEscape(onClose);

  const rows = signals.data ?? [];

  // Follow a `memory_ids` link into the signal's source thread.
  async function openMemory(signal: PersonalitySignal, memoryId: string) {
    setOpenThread({
      thread: signalToThreadSummary(signal),
      highlight: await resolveThreadHighlight(signal.source_thread_id, memoryId),
    });
  }

  return (
    <>
      {/* biome-ignore lint/a11y/useKeyWithClickEvents: Escape handled via useEscape */}
      {/* biome-ignore lint/a11y/noStaticElementInteractions: backdrop, not an interactive control */}
      <div className="drawer-overlay" onClick={onClose} />
      <aside className="signal-drawer" aria-label={t("personality.signalsDrawerAria")}>
        <div className="signal-drawer-head">
          <div>
            <div className="signal-drawer-title">{t("personality.signalsDrawerTitle")}</div>
            <div className="signal-drawer-subtitle">
              {t("personality.signalsUsedCount", {
                count: rows.length,
                formatted: formatNumber(rows.length, locale),
              })}
            </div>
          </div>
          <button type="button" className="btn" onClick={onClose}>
            {t("personality.close")}
          </button>
        </div>
        <div className="signal-drawer-body">
          {signals.isLoading && <div className="empty-desc">{t("common.loading")}</div>}
          {signals.error && (
            <div style={{ color: "var(--danger)", fontSize: 12 }}>
              {(signals.error as Error).message}
            </div>
          )}
          {!signals.isLoading && !signals.error && rows.length === 0 && (
            <div className="empty-state">
              <div className="empty-title">{t("personality.signalsEmptyTitle")}</div>
              <div className="empty-desc">{t("personality.signalsEmptyDesc")}</div>
            </div>
          )}
          {rows.map((signal) => (
            <SignalRow
              key={signal.memory_id}
              signal={signal}
              onOpenThread={() => setOpenThread({ thread: signalToThreadSummary(signal) })}
              onOpenMemory={(memoryId) => void openMemory(signal, memoryId)}
              onDelete={() => del.request(signal.memory_id)}
            />
          ))}
        </div>
      </aside>

      {openThread && (
        <ThreadDetail
          thread={openThread.thread}
          highlight={openThread.highlight}
          onClose={() => setOpenThread(null)}
        />
      )}

      {del.pendingId != null && (
        <ConfirmDialog
          title={t("personality.deleteSignalConfirmTitle")}
          message={t("personality.deleteSignalConfirmMessage")}
          busy={del.busy}
          error={del.error}
          onConfirm={() => void del.confirm()}
          onCancel={del.cancel}
        />
      )}
    </>
  );
}

function SignalRow({
  signal,
  onOpenThread,
  onOpenMemory,
  onDelete,
}: {
  signal: PersonalitySignal;
  onOpenThread: () => void;
  onOpenMemory: (memoryId: string) => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const views = useMemo(() => formatSignalContent(parsePersonalitySignalContent(signal)), [signal]);
  const rawFallback = useMemo(
    () => (views.length === 0 ? rawSignalFallback(signal.content_json) : null),
    [views.length, signal.content_json],
  );
  return (
    <div className="signal-row">
      <div className="signal-row-head">
        <button type="button" className="signal-thread-link" onClick={onOpenThread}>
          {t("personality.threadFallback", { id: signal.source_thread_id })}
        </button>
        <span className="signal-row-date">{formatDateTime(signal.updated_at_ms, locale)}</span>
        <button
          type="button"
          className="btn danger"
          style={{ marginLeft: "auto" }}
          onClick={onDelete}
        >
          {t("common.delete")}
        </button>
      </div>
      {views.length > 0 ? (
        <div className="signal-categories">
          {views.map((view) => (
            <div key={view.label} className="signal-category">
              <div className="signal-category-title">{view.label}</div>
              <ul className="signal-item-list">
                {view.items.map((item) => (
                  <li key={item.primary}>
                    <span className="signal-item-primary">{item.primary}</span>
                    {item.detail && <span className="signal-item-detail">{item.detail}</span>}
                    <MemoryRefs ids={item.memoryIds} onOpen={onOpenMemory} />
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
      ) : rawFallback ? (
        <pre className="signal-raw-fallback">{rawFallback}</pre>
      ) : (
        <div className="persona-category-empty">{t("personality.noDisplayableContent")}</div>
      )}
    </div>
  );
}

function signalToThreadSummary(signal: PersonalitySignal): ThreadSummary {
  return synthesizeThreadSummary({
    id: signal.source_thread_id,
    createdAtMs: signal.updated_at_ms,
    updatedAtMs: signal.updated_at_ms,
  });
}

// A `weight` badge (profile-only) distinguishes this from the signal drawer's
// item layout; the chips reuse MemoryRefs and resolve their thread on click.
function ProfileCategory({
  label,
  view,
  onOpenMemory,
}: {
  label: string;
  view: SignalCategoryView | null;
  onOpenMemory: (memoryId: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="persona-category">
      <div className="persona-category-title">{label}</div>
      <div className="persona-category-body">
        {view ? (
          <ul className="signal-item-list">
            {view.items.map((item) => (
              <li key={item.primary}>
                <span className="signal-item-primary">
                  {item.primary}
                  {item.weight && <span className="persona-weight-badge">{item.weight}</span>}
                </span>
                {item.detail && <span className="signal-item-detail">{item.detail}</span>}
                <MemoryRefs
                  ids={item.memoryIds}
                  onOpen={item.memoryIds && item.memoryIds.length > 0 ? onOpenMemory : undefined}
                />
              </li>
            ))}
          </ul>
        ) : (
          <div className="persona-category-empty">{t("personality.noInfo")}</div>
        )}
      </div>
    </div>
  );
}
