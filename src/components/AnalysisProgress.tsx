import { useTranslation } from "react-i18next";
import type { StepStreamProgress } from "@/hooks/useStepStreamProgress";

/**
 * Inline progress/error block for the standalone summary / personality
 * dispatch buttons. Mirrors the reflection-progress block in Reflections.tsx
 * but is shared by the two analysis pages.
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
  const terminal = progress?.status === "done" || progress?.status === "failed";
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
          <strong>
            {progress.status === "done"
              ? t("analysis.done")
              : progress.status === "failed"
                ? t("analysis.failed")
                : t("analysis.running")}
          </strong>
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
