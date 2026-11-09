import type { ReactNode } from "react";
import { useRef } from "react";
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
  /** Label for the cancel action. Defaults to the localized "キャンセル". */
  cancelLabel?: string;
  /** Visual intent of the primary action. Defaults to the destructive variant. */
  confirmVariant?: "danger" | "primary";
  /** Disables only the primary action, for example while validation is incomplete. */
  confirmDisabled?: boolean;
  /** Optional secondary action shown between cancel and confirm. */
  secondaryAction?: {
    label: string;
    onClick: () => void;
    disabled?: boolean;
  };
  /** Opt in to Modal's focus trap and focus restoration. */
  focusManagement?: boolean;
  /** Semantic role for confirmations that need an alert dialog announcement. */
  role?: "dialog" | "alertdialog";
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
  cancelLabel,
  confirmVariant = "danger",
  confirmDisabled = false,
  secondaryAction,
  focusManagement = false,
  role = "dialog",
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const { t } = useTranslation();
  const cancelRef = useRef<HTMLButtonElement>(null);
  return (
    <Modal
      onClose={onCancel}
      ariaLabel={title}
      role={role}
      focusManagement={focusManagement}
      initialFocusRef={focusManagement ? cancelRef : undefined}
    >
      <div className="modal-head">
        <div className="modal-title">{title}</div>
      </div>
      <div className="modal-body" style={{ fontSize: 12 }}>
        {message}
        {error && <div style={{ marginTop: 8, color: "var(--danger)", fontSize: 11 }}>{error}</div>}
      </div>
      <div className="modal-foot">
        <button ref={cancelRef} type="button" className="btn" onClick={onCancel} disabled={busy}>
          {cancelLabel ?? t("common.cancel")}
        </button>
        {secondaryAction && (
          <button
            type="button"
            className="btn"
            onClick={secondaryAction.onClick}
            disabled={busy || secondaryAction.disabled}
          >
            {secondaryAction.label}
          </button>
        )}
        <button
          type="button"
          className={`btn ${confirmVariant}`}
          onClick={onConfirm}
          disabled={busy || confirmDisabled}
        >
          {busy
            ? (busyLabel ?? t("confirm.busyLabel"))
            : (confirmLabel ?? t("confirm.confirmLabel"))}
        </button>
      </div>
    </Modal>
  );
}
