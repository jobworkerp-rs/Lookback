import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

const POPOVER_WIDTH_PX = 238;
const POPOVER_MAX_HEIGHT_PX = 270;
const VIEWPORT_GUTTER_PX = 8;

export interface PickerFieldProps {
  value: string;
  title: string;
  clearLabel: string;
  onClear: () => void;
  disabled?: boolean;
  /** Called right before the popover opens, e.g. to show the page of `value`. */
  onOpen?: () => void;
  /** Popover body; `close` dismisses it after a pick. */
  children: (close: () => void) => ReactNode;
}

/**
 * Trigger button plus a body-level popover for app-drawn pickers. Native
 * date/month inputs are not usable in every webview (WebKitGTK never reports
 * typed month values to React), so pickers render their own grid inside this
 * shell.
 */
export function PickerField({
  value,
  title,
  clearLabel,
  onClear,
  disabled,
  onOpen,
  children,
}: PickerFieldProps) {
  const pickerRef = useRef<HTMLSpanElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ top: 0, left: 0 });

  const updatePosition = useCallback(() => {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const popupWidth = Math.min(
      POPOVER_WIDTH_PX,
      Math.max(0, window.innerWidth - VIEWPORT_GUTTER_PX * 2),
    );
    const maxLeft = Math.max(
      VIEWPORT_GUTTER_PX,
      window.innerWidth - popupWidth - VIEWPORT_GUTTER_PX,
    );
    const left = Math.min(Math.max(rect.left, VIEWPORT_GUTTER_PX), maxLeft);
    const popupHeight = Math.min(
      POPOVER_MAX_HEIGHT_PX,
      Math.max(0, window.innerHeight - VIEWPORT_GUTTER_PX * 2),
    );
    const belowTop = rect.bottom + 4;
    const top =
      belowTop + popupHeight <= window.innerHeight - VIEWPORT_GUTTER_PX
        ? belowTop
        : Math.max(VIEWPORT_GUTTER_PX, rect.top - popupHeight - 4);
    setPosition({ top, left });
  }, []);

  useEffect(() => {
    if (!open) return;
    updatePosition();
    const dismissOnOutsideClick = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (pickerRef.current?.contains(target) || popoverRef.current?.contains(target)) return;
      setOpen(false);
    };
    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // The parent modal also closes on Escape. Consume it here while the
      // picker is open so Escape dismisses only this transient surface.
      event.preventDefault();
      event.stopPropagation();
      setOpen(false);
    };
    window.addEventListener("resize", updatePosition);
    window.addEventListener("scroll", updatePosition, true);
    document.addEventListener("mousedown", dismissOnOutsideClick);
    document.addEventListener("keydown", dismissOnEscape);
    return () => {
      window.removeEventListener("resize", updatePosition);
      window.removeEventListener("scroll", updatePosition, true);
      document.removeEventListener("mousedown", dismissOnOutsideClick);
      document.removeEventListener("keydown", dismissOnEscape);
    };
  }, [open, updatePosition]);

  function togglePicker() {
    if (open) {
      setOpen(false);
      return;
    }
    onOpen?.();
    updatePosition();
    setOpen(true);
  }

  const popover = open ? (
    <div ref={popoverRef} className="picker-popover" style={position}>
      {children(() => setOpen(false))}
    </div>
  ) : null;

  return (
    <span ref={pickerRef} className="picker">
      <button
        ref={triggerRef}
        type="button"
        className={`text-input picker-trigger ${value ? "has-value" : ""}`}
        aria-label={title}
        aria-expanded={open}
        disabled={disabled}
        onClick={togglePicker}
      >
        {value || "—"}
      </button>
      {value && !disabled && (
        <button
          type="button"
          className="picker-clear"
          aria-label={clearLabel}
          title={clearLabel}
          onClick={onClear}
        >
          ×
        </button>
      )}
      {typeof document !== "undefined" && popover && createPortal(popover, document.body)}
    </span>
  );
}
