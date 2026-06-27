import type { TFunction } from "i18next";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { IMPORT_STEPS, type ImportSnapshot, isImportBusy } from "@/hooks/useImportProgress";
import type { ImportStep, StepStatus } from "@/types/api";
import { Modal } from "./Modal";

// Mock (specs/tauri-mvp-mock.html toast section) uses the raw stage names
// verbatim; keep them so the displayed toast matches the design reference.
const STEP_LABEL: Record<ImportStep, string> = {
  "thread-import": "thread-import",
  "thread-summary": "thread-summary",
  "thread-personality": "thread-personality",
  reflection: "reflection",
};

export function ImportToast({
  snapshot,
  onClose,
  onCancel,
}: {
  snapshot: ImportSnapshot;
  onClose: () => void;
  /** Fire-and-forget cancel for the currently in-flight pipeline.
   *  Hidden once every step is terminal (the toast becomes a results
   *  panel and only offers Dismiss). */
  onCancel: () => Promise<void> | void;
}) {
  const { t } = useTranslation();
  // The step whose detail the user opted to inspect, surfaced in a separate
  // dialog so a long error (or dry-run summary) never overflows the compact
  // toast.
  const [detailStep, setDetailStep] = useState<ImportStep | null>(null);
  const busy = isImportBusy(snapshot);

  return (
    <>
      <div className="toast">
        <div className="toast-head">
          {t("importToast.head")}
          <button
            type="button"
            className="toast-close"
            onClick={onClose}
            aria-label={t("importToast.close")}
          >
            ×
          </button>
        </div>
        {IMPORT_STEPS.map((step) => {
          const s = snapshot.steps[step];
          // Offer the detail dialog when there is more to show than the
          // one-line status: failures (full error) and multi-line messages
          // (e.g. the dry-run per-source summary).
          // Warning carries the "成功 N / 失敗 M" digest worth surfacing as
          // a full detail view, same as a hard failure.
          const hasDetail =
            !!s.message &&
            (s.status === "failed" || s.status === "warning" || s.message.includes("\n"));
          return (
            <div key={step} className={`toast-step ${s.status}`}>
              <div className="toast-step-head">
                <span className="dot" />
                <span className="step-name">{STEP_LABEL[step]}</span>
                <span className="step-status">{statusLabel(s.status, s.message, t)}</span>
                {hasDetail && (
                  <button
                    type="button"
                    className="toast-detail-btn"
                    onClick={() => setDetailStep(step)}
                  >
                    {t("importToast.detail")}
                  </button>
                )}
              </div>
            </div>
          );
        })}
        <div
          style={{
            marginTop: 8,
            fontSize: 10,
            color: "var(--label-tertiary)",
          }}
        >
          job: {snapshot.job_id}
        </div>
        {busy && (
          // While any step is still active, surface a single Cancel
          // action. Once everything settles the toast turns into a
          // results panel (the ✕ in the header is the only dismiss
          // affordance) and the cancel row disappears.
          <div style={{ marginTop: 8, display: "flex", justifyContent: "flex-end" }}>
            <button
              type="button"
              className="btn secondary"
              onClick={() => void onCancel()}
              title={t("importToast.cancelHint")}
            >
              {t("importToast.cancel")}
            </button>
          </div>
        )}
      </div>

      {detailStep && (
        <Modal onClose={() => setDetailStep(null)} ariaLabel={t("importToast.detailDialogAria")}>
          <div className="modal-head">
            <div className="modal-title">
              {snapshot.steps[detailStep].status === "failed"
                ? t("importToast.detailTitleFailed", { step: STEP_LABEL[detailStep] })
                : snapshot.steps[detailStep].status === "warning"
                  ? t("importToast.detailTitleWarning", { step: STEP_LABEL[detailStep] })
                  : t("importToast.detailTitle", { step: STEP_LABEL[detailStep] })}
            </div>
          </div>
          <div className="modal-body">
            <pre className="error-detail">{snapshot.steps[detailStep].message}</pre>
          </div>
          <div className="modal-foot">
            <button type="button" className="btn" onClick={() => setDetailStep(null)}>
              {t("importToast.close")}
            </button>
          </div>
        </Modal>
      )}
    </>
  );
}

function statusLabel(status: StepStatus, message: string | null, t: TFunction): string {
  // Active steps surface the (already server-condensed) progress digest;
  // terminal/failed steps use a fixed label and push detail to the dialog.
  if (status === "active" && message) {
    return message;
  }
  // A waiting step that carries a message is an intentional skip
  // ("スキップ" / "dry-run" / "skipped: import failed") — show the reason
  // rather than the bare 待機中.
  if (status === "waiting" && message) {
    return message;
  }
  // Warning carries the "成功 N / 失敗 M" counter digest produced by the
  // workflow's final output. Surface it inline so the user sees the
  // partial-failure summary at a glance; the full detail (e.g. the last
  // error message) goes to the detail dialog via the 詳細 button.
  if (status === "warning" && message) {
    const firstLine = message.split("\n", 1)[0] ?? message;
    return `${labelForStatus(status, t)} · ${firstLine}`;
  }
  return labelForStatus(status, t);
}

function labelForStatus(status: StepStatus, t: TFunction): string {
  switch (status) {
    case "waiting":
      return t("importToast.statusWaiting");
    case "active":
      return t("importToast.statusActive");
    case "done":
      return t("importToast.statusDone");
    case "warning":
      return t("importToast.statusWarning");
    case "failed":
      return t("importToast.statusFailed");
  }
}
