import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Modal } from "@/components/Modal";

export interface ConfirmDialogProps {
  title: string;
  /** Body text or richer node (e.g. a code block of what will be acted on). */
  message: ReactNode;
  /** Label for the confirm button. Defaults to the localized "削除する". */
  confirmLabel?: string;
  /** Disables both buttons and swaps the confirm label to a busy state. */
  busy?: boolean;
  /** Busy-state confirm label. Defaults to the localized "削除中…"; pass e.g.
   *  "停止中…" for a non-delete destructive action so the busy text matches
   *  `confirmLabel`. */
  busyLabel?: string;
  /** Optional error surfaced under the body (e.g. a failed RPC). */
  error?: string | null;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * Confirmation modal for a destructive / irreversible action. Generalizes the
 * Settings purge dialog so every such affordance (delete summary / reflection /
 * thread / personality, stop a running periodic execution, …) shares one
 * accessible modal with consistent danger styling. Built on `Modal`, which
 * already owns Escape / click-outside close.
 */
export function ConfirmDialog({
  title,
  message,
  confirmLabel,
  busy = false,
  busyLabel,
  error,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const { t } = useTranslation();
  return (
    <Modal onClose={onCancel} ariaLabel={title}>
      <div className="modal-head">
        <div className="modal-title">{title}</div>
      </div>
      <div className="modal-body" style={{ fontSize: 12 }}>
        {message}
        {error && <div style={{ marginTop: 8, color: "var(--danger)", fontSize: 11 }}>{error}</div>}
      </div>
      <div className="modal-foot">
        <button type="button" className="btn" onClick={onCancel} disabled={busy}>
          {t("common.cancel")}
        </button>
        <button type="button" className="btn danger" onClick={onConfirm} disabled={busy}>
          {busy
            ? (busyLabel ?? t("confirm.busyLabel"))
            : (confirmLabel ?? t("confirm.confirmLabel"))}
        </button>
      </div>
    </Modal>
  );
}
