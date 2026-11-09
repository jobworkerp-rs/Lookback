import { useTranslation } from "react-i18next";
import type { BackgroundUnavailableReason } from "@/api";
import { Modal } from "@/components/Modal";

interface CloseChoiceDialogProps {
  open: boolean;
  backgroundAvailable: boolean;
  backgroundUnavailableReason?: BackgroundUnavailableReason | null;
  busy: boolean;
  error: string | null;
  onQuit: () => void;
  onBackground: () => void;
  onCancel: () => void;
}

export function CloseChoiceDialog({
  open,
  backgroundAvailable,
  busy,
  error,
  onQuit,
  onBackground,
  onCancel,
}: CloseChoiceDialogProps) {
  const { t } = useTranslation();
  if (!open) return null;

  return (
    <Modal onClose={onCancel} dismissable={!busy} ariaLabel={t("app.closeChoice.title")}>
      <div className="modal-head">
        <div className="modal-title">{t("app.closeChoice.title")}</div>
      </div>
      <div className="modal-body" style={{ fontSize: 12 }}>
        <p>{t(backgroundAvailable ? "app.closeChoice.message" : "app.closeChoice.quitMessage")}</p>
        {error && (
          <div role="alert" style={{ marginTop: 8, color: "var(--danger)" }}>
            {error}
          </div>
        )}
      </div>
      <div className="modal-foot">
        <button type="button" className="btn danger" onClick={onQuit} disabled={busy}>
          {busy ? t("app.closeChoice.busy") : t("app.closeChoice.quit")}
        </button>
        {backgroundAvailable && (
          <button type="button" className="btn" onClick={onBackground} disabled={busy}>
            {t("app.closeChoice.background")}
          </button>
        )}
        <button type="button" className="btn" onClick={onCancel} disabled={busy}>
          {t("app.closeChoice.cancel")}
        </button>
      </div>
    </Modal>
  );
}
