import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { ImportSnapshot } from "@/hooks/useImportProgress";
import { IMPORT_STEPS } from "@/hooks/useStagedProgress";
import type { ImportStep } from "@/types/api";
import { Modal } from "./Modal";
import { StagedProgressPanel } from "./StagedProgressPanel";

export function ImportProgressPanel({
  snapshot,
  onClose,
}: {
  snapshot: ImportSnapshot;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [detailStep, setDetailStep] = useState<ImportStep | null>(null);
  const detail = detailStep ? snapshot.steps[detailStep] : null;
  return (
    <>
      <StagedProgressPanel
        snapshot={snapshot}
        title={t("importToast.head")}
        steps={IMPORT_STEPS}
        statusNamespace="importToast"
        onClose={onClose}
        messageFor={(step, value, translatedStatus) => {
          if (value.notice_code === "no-importable-log-sources") {
            return t("importToast.noticeNoImportableLogSources");
          }
          if (
            value.message &&
            (value.status === "active" ||
              value.status === "waiting" ||
              value.status === "cancelled" ||
              (step === "thread-group-summary" && value.message === "対象スレッドなし"))
          ) {
            return value.message;
          }

          const hasDetail =
            !!value.message &&
            (value.status === "failed" ||
              value.status === "warning" ||
              value.message.includes("\n"));
          const detailButton = hasDetail ? (
            <button
              type="button"
              className="import-progress-detail-btn"
              onClick={() => setDetailStep(step as ImportStep)}
            >
              {t("importToast.detail")}
            </button>
          ) : null;

          if (value.status === "warning" && value.message) {
            const firstLine = value.message.split("\n", 1)[0] ?? value.message;
            return (
              <>
                {translatedStatus} · {firstLine} {detailButton}
              </>
            );
          }
          if (value.status === "failed" && value.message) {
            return (
              <>
                {translatedStatus} {detailButton}
              </>
            );
          }
          if (value.status === "done" && value.message?.includes("\n")) {
            const firstLine = value.message.split("\n", 1)[0] ?? value.message;
            return (
              <>
                {translatedStatus} · {firstLine} {detailButton}
              </>
            );
          }
          return null;
        }}
      />
      {detail && detailStep && (
        <Modal onClose={() => setDetailStep(null)} ariaLabel={t("importToast.detailDialogAria")}>
          <div className="modal-head">
            <div className="modal-title">
              {detail.status === "failed"
                ? t("importToast.detailTitleFailed", { step: detailStep })
                : detail.status === "warning"
                  ? t("importToast.detailTitleWarning", { step: detailStep })
                  : t("importToast.detailTitle", { step: detailStep })}
            </div>
          </div>
          <div className="modal-body">
            <pre className="import-progress-detail">{detail.message}</pre>
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
