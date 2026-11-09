import { useTranslation } from "react-i18next";
import type { StepStreamProgress } from "@/hooks/useStepStreamProgress";
import { isTerminalStepStatus } from "@/lib/stepStatus";

const STATUS_LABEL_KEYS: Partial<Record<StepStreamProgress["status"], string>> = {
  done: "analysis.done",
  warning: "analysis.warning",
  failed: "analysis.failed",
  cancelled: "analysis.cancelled",
};

/**
 * Inline progress/error block shared by the summary, personality and
 * reflection dispatch buttons.
 */
export function AnalysisProgress({
  progress,
  error,
  onClose,
}: {
  progress: StepStreamProgress | null;
  error: string | null;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const terminal = progress != null && isTerminalStepStatus(progress.status);
  return (
    <>
      {error && (
        <div style={{ color: "var(--danger)", fontSize: 12, marginBottom: 8 }}>{error}</div>
      )}
      {progress && (
        <div
          className={`reflection-progress ${progress.status}`}
          style={{ fontSize: 12, marginBottom: 8 }}
        >
          <strong>{t(STATUS_LABEL_KEYS[progress.status] ?? "analysis.running")}</strong>
          {progress.message && (
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
              {progress.message}
            </pre>
          )}
          {terminal && (
            <button type="button" className="btn" style={{ marginTop: 4 }} onClick={onClose}>
              {t("analysis.close")}
            </button>
          )}
        </div>
      )}
    </>
  );
}
