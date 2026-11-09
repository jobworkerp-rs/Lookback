import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from "react";
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import "./SettingsHint.css";

export type SettingsHintTone = "help" | "warning" | "info";

export interface SettingsHintProps {
  label: string;
  content: ReactNode;
  tone?: SettingsHintTone;
  onActivate?: () => void;
}

interface TooltipPosition {
  left: number;
  top: number;
}

const VIEWPORT_PADDING = 8;
const TOOLTIP_GAP = 8;
const HOVER_CLOSE_DELAY = 120;

const useIsomorphicLayoutEffect = typeof window === "undefined" ? useEffect : useLayoutEffect;

export function SettingsHint({ label, content, tone = "help", onActivate }: SettingsHintProps) {
  const buttonRef = useRef<HTMLButtonElement>(null);
  const tooltipRef = useRef<HTMLDivElement>(null);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [tooltipHovered, setTooltipHovered] = useState(false);
  const [pinned, setPinned] = useState(false);
  const [closing, setClosing] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const [position, setPosition] = useState<TooltipPosition>({ left: 0, top: 0 });
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const interactionRef = useRef({
    hovered: false,
    focused: false,
    pinned: false,
    tooltipHovered: false,
  });
  const generatedId = useId().replace(/:/g, "");
  const tooltipId = `settings-hint-tooltip-${generatedId}`;
  const tooltipVisible = !dismissed && (hovered || focused || pinned || tooltipHovered || closing);
  interactionRef.current = { hovered, focused, pinned, tooltipHovered };

  const clearCloseTimer = useCallback(() => {
    if (closeTimerRef.current === null) return;
    clearTimeout(closeTimerRef.current);
    closeTimerRef.current = null;
  }, []);

  const dismissTooltip = useCallback(() => {
    clearCloseTimer();
    setPinned(false);
    setHovered(false);
    setFocused(false);
    setTooltipHovered(false);
    setClosing(false);
    setDismissed(true);
  }, [clearCloseTimer]);

  const scheduleClose = useCallback(() => {
    clearCloseTimer();
    setClosing(true);
    closeTimerRef.current = setTimeout(() => {
      closeTimerRef.current = null;
      const current = interactionRef.current;
      if (current.hovered || current.focused || current.pinned || current.tooltipHovered) {
        setClosing(false);
        return;
      }
      dismissTooltip();
    }, HOVER_CLOSE_DELAY);
  }, [clearCloseTimer, dismissTooltip]);

  useEffect(() => clearCloseTimer, [clearCloseTimer]);

  const updatePosition = useCallback(() => {
    if (
      !tooltipVisible ||
      typeof window === "undefined" ||
      typeof document === "undefined" ||
      !buttonRef.current ||
      !tooltipRef.current
    ) {
      return;
    }

    const anchor = buttonRef.current.getBoundingClientRect();
    const tooltip = tooltipRef.current.getBoundingClientRect();
    const viewportWidth = window.innerWidth || document.documentElement.clientWidth;
    const viewportHeight = window.innerHeight || document.documentElement.clientHeight;
    const tooltipWidth = tooltip.width;
    const tooltipHeight = tooltip.height;
    const maxLeft = Math.max(VIEWPORT_PADDING, viewportWidth - tooltipWidth - VIEWPORT_PADDING);
    const maxTop = Math.max(VIEWPORT_PADDING, viewportHeight - tooltipHeight - VIEWPORT_PADDING);

    const left = Math.min(Math.max(anchor.left, VIEWPORT_PADDING), maxLeft);
    let top = anchor.bottom + TOOLTIP_GAP;
    if (top > maxTop && anchor.top - tooltipHeight - TOOLTIP_GAP >= VIEWPORT_PADDING) {
      top = anchor.top - tooltipHeight - TOOLTIP_GAP;
    }
    top = Math.min(Math.max(top, VIEWPORT_PADDING), maxTop);

    if (!Number.isFinite(left) || !Number.isFinite(top)) return;
    setPosition({ left: Math.round(left), top: Math.round(top) });
  }, [tooltipVisible]);

  useIsomorphicLayoutEffect(() => {
    updatePosition();
  }, [updatePosition]);

  useEffect(() => {
    if (!tooltipVisible || typeof window === "undefined") return;

    const handleViewportChange = () => updatePosition();
    window.addEventListener("resize", handleViewportChange);
    window.addEventListener("scroll", handleViewportChange, true);
    return () => {
      window.removeEventListener("resize", handleViewportChange);
      window.removeEventListener("scroll", handleViewportChange, true);
    };
  }, [tooltipVisible, updatePosition]);

  useEffect(() => {
    if (!tooltipVisible || typeof window === "undefined") return;

    const handleEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      dismissTooltip();
    };
    window.addEventListener("keydown", handleEscape, true);
    return () => window.removeEventListener("keydown", handleEscape, true);
  }, [tooltipVisible, dismissTooltip]);

  const activate = () => {
    clearCloseTimer();
    if (onActivate) {
      onActivate();
      setPinned(false);
      setHovered(false);
      setFocused(false);
      setTooltipHovered(false);
      setClosing(false);
      setDismissed(true);
      return;
    }
    const nextPinned = !pinned;
    setPinned(nextPinned);
    setDismissed(!nextPinned);
  };

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (event.key === "Escape" && tooltipVisible) {
      event.preventDefault();
      event.stopPropagation();
      dismissTooltip();
      return;
    }
    if ((event.key === "Enter" || event.key === " ") && !event.repeat) {
      event.preventDefault();
      activate();
    }
  };

  const tooltip =
    tooltipVisible && typeof document !== "undefined" && document.body
      ? createPortal(
          <div
            ref={tooltipRef}
            id={tooltipId}
            className={`settings-hint-tooltip settings-hint-tooltip--${tone}`}
            role="tooltip"
            style={{ left: position.left, top: position.top }}
            onMouseEnter={() => {
              clearCloseTimer();
              setClosing(false);
              setDismissed(false);
              setTooltipHovered(true);
            }}
            onMouseLeave={() => {
              setTooltipHovered(false);
              scheduleClose();
            }}
            onPointerEnter={() => {
              clearCloseTimer();
              setClosing(false);
              setDismissed(false);
              setTooltipHovered(true);
            }}
            onPointerLeave={() => {
              setTooltipHovered(false);
              scheduleClose();
            }}
          >
            {content}
          </div>,
          document.body,
        )
      : null;

  return (
    <>
      <span className={`settings-hint settings-hint--${tone}`}>
        <button
          ref={buttonRef}
          type="button"
          className="settings-hint-button"
          aria-label={label}
          aria-describedby={tooltipVisible ? tooltipId : undefined}
          aria-pressed={onActivate ? undefined : pinned}
          onMouseEnter={() => {
            clearCloseTimer();
            setClosing(false);
            setDismissed(false);
            setHovered(true);
          }}
          onMouseLeave={() => {
            setHovered(false);
            scheduleClose();
          }}
          onPointerEnter={() => {
            clearCloseTimer();
            setClosing(false);
            setDismissed(false);
            setHovered(true);
          }}
          onPointerLeave={() => {
            setHovered(false);
            scheduleClose();
          }}
          onFocus={() => {
            clearCloseTimer();
            setClosing(false);
            setDismissed(false);
            setFocused(true);
          }}
          onBlur={() => {
            setFocused(false);
            scheduleClose();
          }}
          onClick={activate}
          onKeyDown={handleKeyDown}
        >
          <HintIcon tone={tone} />
        </button>
      </span>
      {tooltip}
    </>
  );
}

function HintIcon({ tone }: { tone: SettingsHintTone }) {
  if (tone === "warning") {
    return (
      <svg className="settings-hint-icon" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
        <path d="M12 3 2.5 20h19L12 3Z" fill="none" stroke="currentColor" strokeWidth="2" />
        <path d="M12 9v5m0 3h.01" fill="none" stroke="currentColor" strokeWidth="2" />
      </svg>
    );
  }

  if (tone === "info") {
    return (
      <svg className="settings-hint-icon" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
        <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" strokeWidth="2" />
        <path d="M12 11v6m0-9h.01" fill="none" stroke="currentColor" strokeWidth="2" />
      </svg>
    );
  }

  return (
    <svg className="settings-hint-icon" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
      <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" strokeWidth="2" />
      <path
        d="M9.8 9a2.3 2.3 0 1 1 3.7 1.8c-.9.7-1.5 1.1-1.5 2.2m0 3h.01"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
      />
    </svg>
  );
}
