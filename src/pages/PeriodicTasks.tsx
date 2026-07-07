import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  cancelPeriodicExecution,
  createPeriodicTask,
  deletePeriodicTask,
  listPeriodicExecutionHistory,
  listPeriodicTaskStatuses,
  listPeriodicTasks,
  setEnabledPeriodicTask,
  updatePeriodicTask,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Modal } from "@/components/Modal";
import { Toolbar } from "@/components/Toolbar";
import { useLocaleTag } from "@/hooks/useLocaleTag";
import { useTimezone } from "@/hooks/useTimezone";
import { errorMessage } from "@/lib/errorMessage";
import {
  cronPreview,
  DEFAULT_PERIODIC_DRAFT,
  draftFromTask,
  formatExecutionTime,
  PERIODIC_INTERVAL_HOUR_OPTIONS,
  PERIODIC_SOURCE_OPTIONS,
  type PeriodicTaskDraft,
  periodicKindLabel,
  periodicSourceLabel,
  periodicStatusLabel,
  periodicStatusVariant,
  stableSchedulerIds,
  toTaskArgs,
  validatePeriodicDraft,
} from "@/lib/periodicTask";
import type {
  PeriodicExecutionHistoryEntry,
  PeriodicExecutionStatus,
  PeriodicExecutionSummary,
  PeriodicTaskEntry,
} from "@/types/api";

// i18n keys for weekday labels, indexed by weekday value (0=Sunday).
const WEEKDAY_KEYS = [
  "periodic.weekday.sun",
  "periodic.weekday.mon",
  "periodic.weekday.tue",
  "periodic.weekday.wed",
  "periodic.weekday.thu",
  "periodic.weekday.fri",
  "periodic.weekday.sat",
] as const;

// Poll fast while something is in flight, slow once everything has settled.
const ACTIVE_REFETCH_MS = 5_000;
const IDLE_REFETCH_MS = 30_000;

export function PeriodicTasks() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState<PeriodicTaskEntry | null>(null);
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<PeriodicTaskEntry | null>(null);
  const [historyFor, setHistoryFor] = useState<PeriodicTaskEntry | null>(null);
  const tasks = useQuery({
    queryKey: ["periodic-tasks"],
    queryFn: () => listPeriodicTasks({ limit: 100, offset: 0 }),
  });

  // Stable (deduped + sorted) ids keep the query key from churning when the
  // list reorders; card display order still follows `tasks.data`.
  const schedulerIds = useMemo(
    () => stableSchedulerIds((tasks.data ?? []).map((t) => t.id)),
    [tasks.data],
  );
  const statuses = useQuery({
    queryKey: ["periodic-task-statuses", schedulerIds],
    queryFn: () => listPeriodicTaskStatuses(schedulerIds),
    enabled: schedulerIds.length > 0,
    refetchInterval: (q) => {
      const data = q.state.data;
      // Refetch fast on error / no data too, so a transient failure recovers.
      if (!data) return ACTIVE_REFETCH_MS;
      return data.some((s) => s.active) ? ACTIVE_REFETCH_MS : IDLE_REFETCH_MS;
    },
  });
  const statusById = useMemo(() => {
    const map = new Map<string, PeriodicExecutionSummary>();
    for (const s of statuses.data ?? []) map.set(s.scheduler_id, s);
    return map;
  }, [statuses.data]);

  const invalidateStatuses = () =>
    queryClient.invalidateQueries({ queryKey: ["periodic-task-statuses"] });

  const enableMutation = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      setEnabledPeriodicTask(id, enabled),
    onSuccess: () => {
      void invalidateStatuses();
      return queryClient.invalidateQueries({ queryKey: ["periodic-tasks"] });
    },
  });
  const deleteMutation = useMutation({
    mutationFn: (id: string) => deletePeriodicTask(id),
    onSuccess: (_data, id) => {
      // A deleted scheduler's history is gone; drop its cache and close the
      // modal if it was open for that scheduler.
      queryClient.removeQueries({ queryKey: ["periodic-execution-history", id] });
      if (historyFor?.id === id) setHistoryFor(null);
      setDeleting(null);
      void invalidateStatuses();
      return queryClient.invalidateQueries({ queryKey: ["periodic-tasks"] });
    },
  });

  return (
    <>
      <Toolbar
        title={t("periodic.title")}
        subtitle={t("periodic.subtitle")}
        actions={
          <button type="button" className="btn primary" onClick={() => setCreating(true)}>
            {t("periodic.new")}
          </button>
        }
      />

      <div className="content">
        <div className="settings-card periodic-intro">
          <div>
            <div className="settings-card-title">{t("periodic.intro.title")}</div>
            <div className="settings-card-desc">{t("periodic.intro.desc")}</div>
          </div>
        </div>

        <div className="thread-list periodic-list">
          {tasks.isPending && <div className="empty-desc">{t("common.loading")}</div>}
          {tasks.isError && <div className="form-error">{t("periodic.loadError")}</div>}
          {tasks.data?.length === 0 && (
            <div className="empty-state">
              <div className="empty-title">{t("periodic.empty.title")}</div>
              <div className="empty-desc">{t("periodic.empty.desc")}</div>
            </div>
          )}
          {tasks.data?.map((task) => {
            const locked = task.status !== "supported";
            const summary = statusById.get(task.id);
            return (
              <div key={task.id} className="thread-card periodic-task-card">
                <div className="periodic-task-main">
                  <div className="thread-card-head periodic-task-head">
                    <span className="thread-source">
                      {task.task
                        ? periodicSourceLabel(t, task.task.source, task.task.sources)
                        : t("periodic.card.unknownSource")}
                    </span>
                    <span>
                      {task.task
                        ? periodicKindLabel(t, task.task.task_kind)
                        : t("periodic.card.unsupportedFormat")}
                    </span>
                    {task.status === "unsupported" && (
                      <span className="label-pill periodic-unsupported">unsupported</span>
                    )}
                    <span className="periodic-enabled-state">
                      {task.enabled ? t("periodic.enabled") : t("periodic.disabled")}
                    </span>
                  </div>
                  <div className="thread-title">{task.name || t("periodic.card.unnamed")}</div>
                  <div className="thread-description">
                    <code>{task.crontab}</code>
                    {task.task && (
                      <>
                        <span className="periodic-meta-separator">·</span>
                        <span>
                          {t("periodic.card.lookback", { count: task.task.lookback_days })}
                        </span>
                      </>
                    )}
                  </div>
                  <PeriodicStatusRow summary={summary} statusError={statuses.isError} />
                </div>
                <div className="periodic-task-actions">
                  <div className="periodic-task-actions-config">
                    <label className="periodic-toggle-label">
                      <input
                        type="checkbox"
                        checked={task.enabled}
                        disabled={locked || enableMutation.isPending}
                        onChange={(e) =>
                          enableMutation.mutate({ id: task.id, enabled: e.currentTarget.checked })
                        }
                      />
                      {t("periodic.enabled")}
                    </label>
                    <button
                      type="button"
                      className="btn"
                      disabled={locked}
                      onClick={() => setEditing(task)}
                    >
                      {t("periodic.edit")}
                    </button>
                    <button type="button" className="btn danger" onClick={() => setDeleting(task)}>
                      {t("common.delete")}
                    </button>
                  </div>
                  <div className="periodic-task-actions-exec">
                    <button type="button" className="btn" onClick={() => setHistoryFor(task)}>
                      {t("periodic.history")}
                    </button>
                    {summary?.cancelable && summary.runtime && (
                      <CancelButton
                        task={task}
                        executionRefId={summary.runtime.execution_ref_id}
                        onCancelled={invalidateStatuses}
                      />
                    )}
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      {(creating || editing) && (
        <PeriodicTaskDialog
          entry={editing}
          onClose={() => {
            setCreating(false);
            setEditing(null);
          }}
          onSaved={() => {
            void invalidateStatuses();
            return queryClient.invalidateQueries({ queryKey: ["periodic-tasks"] });
          }}
        />
      )}

      {deleting && (
        <ConfirmDialog
          title={t("periodic.delete.title")}
          message={t("periodic.delete.message", { name: deleting.name })}
          confirmLabel={t("common.delete")}
          busy={deleteMutation.isPending}
          onConfirm={() => deleteMutation.mutate(deleting.id)}
          onCancel={() => setDeleting(null)}
        />
      )}

      {historyFor && <PeriodicHistoryModal task={historyFor} onClose={() => setHistoryFor(null)} />}
    </>
  );
}

function PeriodicStatusRow({
  summary,
  statusError,
}: {
  summary: PeriodicExecutionSummary | undefined;
  statusError: boolean;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  if (!summary) {
    // A missing summary means either the query is still loading OR it rejected
    // as a whole (e.g. conductor unreachable, so no per-scheduler element came
    // back). Distinguish them: an errored query shows 状態を取得できません, not a
    // permanent 取得中… that hides the failure (README "状態を取得できません").
    if (statusError) return <UnavailableRow />;
    return (
      <div className="periodic-status-row periodic-status-loading">
        {t("periodic.status.latestLoading")}
      </div>
    );
  }
  if (summary.status === "not_started") {
    return (
      <div className="periodic-status-row">
        <StatusBadge status={summary.status} />
        <span className="periodic-status-detail">{t("periodic.status.notStarted")}</span>
      </div>
    );
  }
  if (summary.status === "unavailable" || !summary.runtime) {
    return <UnavailableRow error={summary.error} />;
  }
  const rt = summary.runtime;
  const time = formatExecutionTime(rt.triggered_at_ms, locale, timezone);
  const detail = rt.detail ?? rt.enqueue_error ?? null;
  return (
    <div className="periodic-status-row">
      <StatusBadge status={summary.status} />
      <span className="periodic-status-detail">
        {time && <>{t("periodic.status.latest", { time })}</>}
        {detail && <> · {detail}</>}
        {rt.job_id && <> · {t("periodic.status.job", { jobId: rt.job_id })}</>}
      </span>
    </div>
  );
}

function UnavailableRow({ error }: { error?: string | null }) {
  const { t } = useTranslation();
  return (
    <div className="periodic-status-row">
      <StatusBadge status="unavailable" />
      <span className="periodic-status-detail">
        {t("periodic.status.unavailable")}
        {error ? ` · ${error}` : ""}
      </span>
    </div>
  );
}

function StatusBadge({ status }: { status: PeriodicExecutionStatus }) {
  const { t } = useTranslation();
  return (
    <span className={`label-pill periodic-status-${periodicStatusVariant(status)}`}>
      {periodicStatusLabel(t, status)}
    </span>
  );
}

function CancelButton({
  task,
  executionRefId,
  onCancelled,
}: {
  task: PeriodicTaskEntry;
  executionRefId: string;
  onCancelled: () => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [confirming, setConfirming] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mutation = useMutation({
    mutationFn: () => cancelPeriodicExecution(executionRefId),
    onSuccess: () => {
      setConfirming(false);
      setError(null);
      onCancelled();
      // Reflect 取消中 / terminal via refetch, not optimistic update.
      void queryClient.invalidateQueries({
        queryKey: ["periodic-execution-history", task.id],
      });
    },
    onError: (e) => setError(errorMessage(e)),
  });
  return (
    <>
      <button
        type="button"
        className="btn danger"
        onClick={() => {
          setError(null);
          setConfirming(true);
        }}
      >
        {t("periodic.cancel.button")}
      </button>
      {confirming && (
        <ConfirmDialog
          title={t("periodic.cancel.title")}
          message={t("periodic.cancel.message", { name: task.name })}
          confirmLabel={t("periodic.cancel.button")}
          busyLabel={t("periodic.cancel.busy")}
          busy={mutation.isPending}
          error={error}
          onConfirm={() => mutation.mutate()}
          onCancel={() => setConfirming(false)}
        />
      )}
    </>
  );
}

function PeriodicHistoryModal({ task, onClose }: { task: PeriodicTaskEntry; onClose: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const history = useQuery({
    queryKey: ["periodic-execution-history", task.id, 20],
    queryFn: () => listPeriodicExecutionHistory(task.id, 20),
    refetchInterval: (q) => (q.state.data?.some((e) => e.active) ? ACTIVE_REFETCH_MS : false),
  });
  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["periodic-execution-history", task.id] });

  return (
    <Modal onClose={onClose} ariaLabel={t("periodic.historyModal.title", { name: task.name })} wide>
      <div className="modal-head">
        <div className="modal-title">{t("periodic.historyModal.title", { name: task.name })}</div>
        <button type="button" className="btn" onClick={() => history.refetch()}>
          {t("periodic.historyModal.refresh")}
        </button>
      </div>
      <div className="modal-body">
        {history.isPending && <div className="empty-desc">{t("common.loading")}</div>}
        {history.isError && (
          <div className="form-error">{t("periodic.historyModal.loadError")}</div>
        )}
        {history.data?.length === 0 && (
          <div className="empty-desc">{t("periodic.historyModal.empty")}</div>
        )}
        <div className="periodic-history-list">
          {history.data?.map((entry) => (
            <PeriodicHistoryRow
              key={entry.execution_ref_id}
              task={task}
              entry={entry}
              onCancelled={invalidate}
            />
          ))}
        </div>
      </div>
      <div className="modal-foot">
        <button type="button" className="btn" onClick={onClose}>
          {t("periodic.historyModal.close")}
        </button>
      </div>
    </Modal>
  );
}

function PeriodicHistoryRow({
  task,
  entry,
  onCancelled,
}: {
  task: PeriodicTaskEntry;
  entry: PeriodicExecutionHistoryEntry;
  onCancelled: () => void;
}) {
  const { t } = useTranslation();
  const locale = useLocaleTag();
  const timezone = useTimezone();
  const time = formatExecutionTime(entry.triggered_at_ms, locale, timezone);
  const detail = entry.detail ?? entry.enqueue_error ?? null;
  return (
    <div className="periodic-history-row">
      <div className="periodic-history-row-head">
        <StatusBadge status={entry.status} />
        {time && <span className="periodic-history-time">{time}</span>}
        {entry.job_id && (
          <span className="periodic-history-job">
            {t("periodic.status.job", { jobId: entry.job_id })}
          </span>
        )}
        {entry.cancelable && (
          <CancelButton
            task={task}
            executionRefId={entry.execution_ref_id}
            onCancelled={onCancelled}
          />
        )}
      </div>
      {entry.trigger_context_json && (
        <code className="periodic-history-context">{entry.trigger_context_json}</code>
      )}
      {detail && <div className="periodic-history-detail">{detail}</div>}
    </div>
  );
}

function PeriodicTaskDialog({
  entry,
  onClose,
  onSaved,
}: {
  entry: PeriodicTaskEntry | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState<PeriodicTaskDraft>(
    entry?.task ? draftFromTask(entry.task) : DEFAULT_PERIODIC_DRAFT,
  );
  const task = useMemo(() => toTaskArgs(draft), [draft]);
  const errors = validatePeriodicDraft(draft);
  const mutation = useMutation({
    mutationFn: async () => {
      if (entry) {
        await updatePeriodicTask({ id: entry.id, task, enabled: entry.enabled, description: null });
      } else {
        await createPeriodicTask({ task, enabled: true, description: null });
      }
    },
    onSuccess: () => {
      onSaved();
      onClose();
    },
  });

  const set = <K extends keyof PeriodicTaskDraft>(key: K, value: PeriodicTaskDraft[K]) =>
    setDraft((prev) => ({ ...prev, [key]: value }));

  const setIntervalMode = (mode: PeriodicTaskDraft["interval_mode"]) =>
    setDraft((prev) => ({
      ...prev,
      interval_mode: mode,
      interval_value:
        mode === "hours" &&
        !PERIODIC_INTERVAL_HOUR_OPTIONS.some((hours) => hours === prev.interval_value)
          ? 24
          : Math.max(1, prev.interval_value),
    }));

  const setRunSummaryDaily = (checked: boolean) =>
    setDraft((prev) => ({
      ...prev,
      run_summary_daily: checked,
      run_summary_thread: checked ? true : prev.run_summary_thread,
    }));

  const setRunSummaryThread = (checked: boolean) =>
    setDraft((prev) => ({
      ...prev,
      run_summary_thread: prev.run_summary_daily ? true : checked,
    }));

  return (
    <Modal
      onClose={onClose}
      ariaLabel={entry ? t("periodic.dialog.editTitle") : t("periodic.dialog.createTitle")}
    >
      <div className="modal-head">
        <div className="modal-title">
          {entry ? t("periodic.dialog.editTitle") : t("periodic.dialog.createTitle")}
        </div>
      </div>
      <div className="modal-body">
        <div className="periodic-form-grid">
          <label className="field periodic-field-span">
            <span className="field-label">{t("periodic.dialog.name")}</span>
            <input value={draft.name} onChange={(e) => set("name", e.currentTarget.value)} />
          </label>
          <label className="field">
            <span className="field-label">{t("periodic.dialog.source")}</span>
            <select value={draft.source} onChange={(e) => set("source", e.currentTarget.value)}>
              {PERIODIC_SOURCE_OPTIONS.map((source) => (
                <option key={source.value} value={source.value}>
                  {source.label}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span className="field-label">{t("periodic.dialog.kind")}</span>
            <select
              value={draft.task_kind}
              onChange={(e) =>
                set("task_kind", e.currentTarget.value as PeriodicTaskDraft["task_kind"])
              }
            >
              <option value="regular">{t("periodic.dialog.kindRegular")}</option>
              <option value="weekly">{t("periodic.dialog.kindWeekly")}</option>
              <option value="monthly">{t("periodic.dialog.kindMonthly")}</option>
            </select>
          </label>
          <label className="field">
            <span className="field-label">{t("periodic.dialog.hour")}</span>
            <input
              type="number"
              min={0}
              max={23}
              value={draft.hour}
              onChange={(e) => set("hour", Number(e.currentTarget.value))}
            />
          </label>
          <label className="field">
            <span className="field-label">{t("periodic.dialog.minute")}</span>
            <input
              type="number"
              min={0}
              max={59}
              value={draft.minute}
              onChange={(e) => set("minute", Number(e.currentTarget.value))}
            />
          </label>
          {draft.task_kind === "regular" && (
            <>
              <label className="field">
                <span className="field-label">{t("periodic.dialog.intervalUnit")}</span>
                <select
                  value={draft.interval_mode}
                  onChange={(e) =>
                    setIntervalMode(e.currentTarget.value as PeriodicTaskDraft["interval_mode"])
                  }
                >
                  <option value="hours">{t("periodic.dialog.intervalUnitHours")}</option>
                  <option value="days">{t("periodic.dialog.intervalUnitDays")}</option>
                </select>
              </label>
              {draft.interval_mode === "hours" ? (
                <label className="field">
                  <span className="field-label">{t("periodic.dialog.interval")}</span>
                  <select
                    value={draft.interval_value}
                    onChange={(e) => set("interval_value", Number(e.currentTarget.value))}
                  >
                    {PERIODIC_INTERVAL_HOUR_OPTIONS.map((hours) => (
                      <option key={hours} value={hours}>
                        {t("periodic.dialog.hoursOption", { count: hours })}
                      </option>
                    ))}
                  </select>
                </label>
              ) : (
                <label className="field">
                  <span className="field-label">{t("periodic.dialog.interval")}</span>
                  <input
                    type="number"
                    min={1}
                    value={draft.interval_value}
                    onChange={(e) => set("interval_value", Number(e.currentTarget.value))}
                  />
                </label>
              )}
            </>
          )}
          {draft.task_kind === "regular" && (
            <div className="field periodic-field-span">
              <span className="field-label">{t("periodic.dialog.outputs")}</span>
              <div className="periodic-checkbox-grid">
                <label className="checkbox-row">
                  <input
                    type="checkbox"
                    checked={draft.run_summary_thread || draft.run_summary_daily}
                    disabled={draft.run_summary_daily}
                    onChange={(e) => setRunSummaryThread(e.currentTarget.checked)}
                  />
                  {t("periodic.dialog.summaryThread")}
                </label>
                <label className="checkbox-row">
                  <input
                    type="checkbox"
                    checked={draft.run_summary_daily}
                    onChange={(e) => setRunSummaryDaily(e.currentTarget.checked)}
                  />
                  {t("periodic.dialog.summaryDaily")}
                </label>
                <label className="checkbox-row">
                  <input
                    type="checkbox"
                    checked={draft.run_personality}
                    onChange={(e) => set("run_personality", e.currentTarget.checked)}
                  />
                  {t("periodic.dialog.personality")}
                </label>
                <label className="checkbox-row">
                  <input
                    type="checkbox"
                    checked={draft.run_reflection}
                    onChange={(e) => set("run_reflection", e.currentTarget.checked)}
                  />
                  {t("periodic.dialog.reflection")}
                </label>
              </div>
            </div>
          )}
          {draft.task_kind === "weekly" && (
            <>
              <div className="field periodic-field-span">
                <span className="field-label">{t("periodic.dialog.outputs")}</span>
                <label className="checkbox-row">
                  <input type="checkbox" checked readOnly />
                  {t("periodic.dialog.summaryWeekly")}
                </label>
              </div>
              <label className="field">
                <span className="field-label">{t("periodic.dialog.weekday")}</span>
                <select
                  value={draft.weekly_day}
                  onChange={(e) => set("weekly_day", Number(e.currentTarget.value))}
                >
                  {WEEKDAY_KEYS.map((key, value) => (
                    <option key={key} value={value}>
                      {t(key)}
                    </option>
                  ))}
                </select>
              </label>
            </>
          )}
          {draft.task_kind === "monthly" && (
            <>
              <div className="field periodic-field-span">
                <span className="field-label">{t("periodic.dialog.outputs")}</span>
                <label className="checkbox-row">
                  <input type="checkbox" checked readOnly />
                  {t("periodic.dialog.summaryMonthly")}
                </label>
              </div>
              <label className="field">
                <span className="field-label">{t("periodic.dialog.monthlyDay")}</span>
                <input
                  type="number"
                  min={1}
                  max={28}
                  value={draft.monthly_day}
                  onChange={(e) => set("monthly_day", Number(e.currentTarget.value))}
                />
              </label>
            </>
          )}
          <label className="field">
            <span className="field-label">{t("periodic.dialog.lookbackDays")}</span>
            <input
              type="number"
              min={1}
              value={draft.lookback_days}
              onChange={(e) => set("lookback_days", Number(e.currentTarget.value))}
            />
          </label>
        </div>

        <div className="periodic-cron-preview">
          <span className="field-label">cron</span>
          <code>{cronPreview(t, task)}</code>
        </div>
        {errors.length > 0 && errors[0] && <div className="form-error">{t(errors[0])}</div>}
      </div>

      <div className="modal-foot">
        <button type="button" className="btn" onClick={onClose}>
          {t("common.cancel")}
        </button>
        <button
          type="button"
          className="btn primary"
          disabled={errors.length > 0 || mutation.isPending}
          onClick={() => mutation.mutate()}
        >
          {t("periodic.dialog.save")}
        </button>
      </div>
    </Modal>
  );
}
