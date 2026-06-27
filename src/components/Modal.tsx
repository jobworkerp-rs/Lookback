import type { ReactNode } from "react";
import { useRef } from "react";
import { useEscape } from "@/hooks/useEscape";

export interface ModalProps {
  /** Invoked on Escape / click-outside. Required unless `dismissable` is
   *  false, in which case the modal blocks every standard close
   *  affordance and the caller doesn't need a handler. */
  onClose?: () => void;
  /** Wider variant matches the mock's thread detail modal. */
  wide?: boolean;
  /** Defaults to `true`. Set to `false` for non-dismissable blockers
   *  (e.g. a long-running save spinner the user must not close mid-
   *  flight). When `false`, Escape and click-outside are inert and
   *  `onClose` is ignored. */
  dismissable?: boolean;
  children: ReactNode;
  ariaLabel?: string;
}

/**
 * Shared modal shell. Owns the click-outside / Escape-to-close affordance
 * so individual dialogs (Import, ThreadDetail, Settings purge) don't each
 * reinvent it and don't accumulate a11y violations.
 *
 * Click-outside is implemented by checking whether the click target is the
 * overlay element itself (not a descendant) — this avoids putting an
 * `onClick` handler on a static `<div>` (which a11y linters flag) and
 * also avoids needing `stopPropagation()` inside the inner card.
 *
 * Set `dismissable={false}` to model a blocking progress dialog; the
 * Escape + overlay click paths become inert so the caller's `onClose`
 * (if any) is never invoked — making the "no close" contract explicit
 * in the type rather than hidden behind a no-op handler.
 */
export function Modal({ onClose, wide, dismissable = true, children, ariaLabel }: ModalProps) {
  const overlayRef = useRef<HTMLDivElement>(null);

  useEscape(dismissable && onClose ? onClose : () => {});

  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents: Escape is handled at document level above
    // biome-ignore lint/a11y/noStaticElementInteractions: the outer overlay is a backdrop, not an interactive control
    <div
      ref={overlayRef}
      className="modal-overlay"
      onClick={(e) => {
        if (dismissable && onClose && e.target === overlayRef.current) {
          onClose();
        }
      }}
    >
      <div
        className={`modal${wide ? " lg" : ""}`}
        role="dialog"
        aria-modal="true"
        aria-label={ariaLabel}
        tabIndex={-1}
      >
        {children}
      </div>
    </div>
  );
}
