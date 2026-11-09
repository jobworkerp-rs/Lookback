import type { ReactNode, RefObject } from "react";
import { useEffect, useRef } from "react";
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
  /** Semantic role for surfaces that require an alert dialog announcement. */
  role?: "dialog" | "alertdialog";
  /**
   * Opt in to focus management for dialogs that contain a self-contained
   * editing workflow. Existing callers keep the legacy focus behavior when
   * this is omitted.
   */
  focusManagement?: boolean;
  /** Element to focus when focus management is enabled. */
  initialFocusRef?: RefObject<HTMLElement | null>;
}

const FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [contenteditable="true"], [tabindex]:not([tabindex="-1"])';

const managedOverlays = new WeakSet<HTMLDivElement>();
const restorationTargets = new WeakMap<HTMLDivElement, HTMLElement | null>();
const DIALOG_SELECTOR = '[role="dialog"], [role="alertdialog"]';

function getFocusableElements(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    (element) =>
      !element.hidden &&
      !element.closest('[hidden], [aria-hidden="true"], fieldset:disabled') &&
      !element.matches(":disabled") &&
      (element.isContentEditable || element.tabIndex >= 0),
  );
}

function isTopmostOverlay(overlay: HTMLDivElement): boolean {
  const overlays = document.querySelectorAll<HTMLDivElement>(".modal-overlay");
  return overlays.item(overlays.length - 1) === overlay;
}

function getNextManagedOverlay(closingOverlay: HTMLDivElement): HTMLDivElement | null {
  const overlays = Array.from(document.querySelectorAll<HTMLDivElement>(".modal-overlay"));
  return (
    overlays
      .reverse()
      .find(
        (overlay) =>
          overlay !== closingOverlay &&
          !closingOverlay.contains(overlay) &&
          managedOverlays.has(overlay),
      ) ?? null
  );
}

function focusDialog(overlay: HTMLDivElement): void {
  const dialog = overlay.querySelector<HTMLElement>(DIALOG_SELECTOR);
  if (!dialog) return;
  const focusable = getFocusableElements(dialog);
  (focusable[0] ?? dialog).focus();
}

function canRestoreFocus(element: HTMLElement | null, dialog: HTMLElement): element is HTMLElement {
  return Boolean(
    element?.isConnected &&
      dialog.contains(element) &&
      !element.hidden &&
      !element.closest('[hidden], [aria-hidden="true"], fieldset:disabled') &&
      !element.matches(":disabled"),
  );
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
export function Modal({
  onClose,
  wide,
  dismissable = true,
  children,
  ariaLabel,
  role = "dialog",
  focusManagement = false,
  initialFocusRef,
}: ModalProps) {
  const overlayRef = useRef<HTMLDivElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const previouslyFocusedRef = useRef<HTMLElement | null>(null);
  const initialFocusRefRef = useRef(initialFocusRef);
  initialFocusRefRef.current = initialFocusRef;

  useEscape(dismissable && onClose ? onClose : () => {}, () => {
    // All modal overlays share the same stacking context, so the final overlay
    // in document order is visually frontmost. This keeps Escape scoped to one
    // modal even when callers render dialogs as siblings.
    const overlays = document.querySelectorAll<HTMLDivElement>(".modal-overlay");
    return overlays.item(overlays.length - 1) === overlayRef.current;
  });

  useEffect(() => {
    if (!focusManagement || !dialogRef.current || !overlayRef.current) return;

    const overlay = overlayRef.current;
    const dialog = dialogRef.current;
    managedOverlays.add(overlay);
    previouslyFocusedRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;

    if (isTopmostOverlay(overlay)) {
      if (!restorationTargets.has(overlay)) {
        restorationTargets.set(overlay, previouslyFocusedRef.current);
      }
      const target = initialFocusRefRef.current?.current;
      const focusable = getFocusableElements(dialog);
      (target && dialog.contains(target) ? target : (focusable[0] ?? dialog)).focus();
    }

    return () => {
      const previous = restorationTargets.get(overlay) ?? previouslyFocusedRef.current;
      const next = getNextManagedOverlay(overlay);
      managedOverlays.delete(overlay);
      restorationTargets.delete(overlay);
      if (next) {
        if (!restorationTargets.has(next)) restorationTargets.set(next, previous);
        const nextDialog = next.querySelector<HTMLElement>(DIALOG_SELECTOR);
        if (nextDialog && canRestoreFocus(previous, nextDialog)) {
          previous.focus();
        } else {
          focusDialog(next);
        }
      } else if (previous?.isConnected) {
        previous.focus();
      }
    };
  }, [focusManagement]);

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
      {/* biome-ignore lint/a11y/useAriaPropsSupportedByRole: role is a dialog or alertdialog by contract */}
      {/* biome-ignore lint/a11y/noStaticElementInteractions: the dialog owns keyboard focus management */}
      <div
        ref={dialogRef}
        className={`modal${wide ? " lg" : ""}`}
        role={role}
        aria-modal="true"
        aria-label={ariaLabel}
        tabIndex={-1}
        onKeyDown={(event) => {
          if (!focusManagement || event.key !== "Tab" || !overlayRef.current || !dialogRef.current)
            return;
          if (!isTopmostOverlay(overlayRef.current)) return;

          const focusable = getFocusableElements(dialogRef.current);
          if (focusable.length === 0) {
            event.preventDefault();
            dialogRef.current.focus();
            return;
          }

          const first = focusable[0];
          const last = focusable[focusable.length - 1];
          if (!first || !last) return;
          const active = document.activeElement;
          const activeIndex = active instanceof HTMLElement ? focusable.indexOf(active) : -1;
          if (event.shiftKey && activeIndex <= 0) {
            event.preventDefault();
            last.focus();
          } else if (
            !event.shiftKey &&
            (activeIndex === -1 || activeIndex === focusable.length - 1)
          ) {
            event.preventDefault();
            first.focus();
          }
        }}
      >
        {children}
      </div>
    </div>
  );
}
