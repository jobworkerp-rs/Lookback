import type { TFunction } from "i18next";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { isStagedProgressBusy } from "@/hooks/useStagedProgress";
import type { ImportStepState, StagedProgressSnapshot, StepStatus } from "@/types/api";

const DEFAULT_STATUS_KEYS: Record<StepStatus, string> = {
  waiting: "statusWaiting",
  active: "statusActive",
  done: "statusDone",
  warning: "statusWarning",
  failed: "statusFailed",
  cancelled: "statusCancelled",
};

type OverallStatus = StepStatus | "partial";

const ANALYSIS_STATUS_KEYS: Record<OverallStatus, string> = {
  waiting: "waiting",
  active: "running",
  done: "done",
  warning: "warning",
  failed: "failed",
  cancelled: "cancelled",
  partial: "partial",
};

export function StagedProgressPanel<K extends StagedProgressSnapshot["kind"]>({
  snapshot,
  title,
  labels,
  steps,
  statusNamespace,
  showOverall = false,
  onClose,
  closeLabel,
  messageFor,
}: {
  snapshot: StagedProgressSnapshot<K>;
  title: string;
  /** Display names per step; a step without one is shown by its key. */
  labels?: Readonly<Record<string, string>>;
  steps?: readonly string[];
  statusNamespace: string;
  showOverall?: boolean;
  onClose?: () => void;
  closeLabel?: string;
  messageFor?: (
    step: string,
    value: ImportStepState,
    translatedStatus: string,
    t: TFunction,
  ) => ReactNode | null;
}) {
  const { t } = useTranslation();
  const busy = isStagedProgressBusy(snapshot);
  const effectiveStatusKeys: Record<OverallStatus, string> =
    statusNamespace === "analysis"
      ? ANALYSIS_STATUS_KEYS
      : { ...DEFAULT_STATUS_KEYS, partial: DEFAULT_STATUS_KEYS.warning };
  const overall = overallStatus(snapshot);
  const overallKey = effectiveStatusKeys[overall];
  const orderedSteps = steps ?? Object.keys(snapshot.steps);
  const snapshotSteps = snapshot.steps as Record<string, ImportStepState>;

  return (
    <div className="import-progress-panel staged-progress-panel" role="status">
      <strong>{title}</strong>
      {showOverall && (
        <div className={`import-progress-row staged-progress-overall ${overall}`}>
          <strong>{t(`${statusNamespace}.${overallKey}`)}</strong>
        </div>
      )}
      {orderedSteps.map((step) => {
        const value = snapshotSteps[step];
        if (!value) return null;
        const translatedStatus = t(`${statusNamespace}.${effectiveStatusKeys[value.status]}`);
        const display = messageFor
          ? messageFor(step, value, translatedStatus, t)
          : defaultMessage(value, translatedStatus);
        return (
          <div key={step} className={`import-progress-row staged-progress-row ${value.status}`}>
            <span>{labels?.[step] ?? step}</span>
            <span>{display ?? translatedStatus}</span>
          </div>
        );
      })}
      {onClose && !busy && (
        <button type="button" className="btn" onClick={onClose}>
          {closeLabel ?? t(`${statusNamespace}.close`)}
        </button>
      )}
    </div>
  );
}

export function overallStatus(snapshot: StagedProgressSnapshot): OverallStatus {
  if (isStagedProgressBusy(snapshot)) return "active";
  const statuses = Object.values(snapshot.steps).map((step) => step.status);
  if (statuses.includes("cancelled")) return "cancelled";
  if (statuses.includes("failed")) {
    return statuses.includes("done") || statuses.includes("warning") ? "partial" : "failed";
  }
  if (statuses.includes("warning")) return "warning";
  return "done";
}

function defaultMessage(value: ImportStepState, translatedStatus: string): string | null {
  if (
    (value.status === "active" ||
      value.status === "waiting" ||
      value.status === "cancelled" ||
      value.status === "done") &&
    value.message
  ) {
    return value.message;
  }
  if (value.status === "warning" && value.message) {
    return `${translatedStatus} · ${value.message.split("\n", 1)[0] ?? value.message}`;
  }
  if (value.status === "failed" && value.message) {
    return `${translatedStatus}: ${value.message}`;
  }
  return null;
}
