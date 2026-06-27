import { useCallback, useEffect, useRef, useState } from "react";

/** Distance (in px) from the bottom at which we still consider the
 *  user "stuck to the latest message". Anything beyond that turns off
 *  auto-scroll until the user manually returns to the end. */
const STICK_THRESHOLD_PX = 80;

export interface UseStickToBottom {
  /** Attach to the scroll container as a callback ref. Using a
   *  callback (rather than `RefObject`) lets the hook react to the
   *  node being attached so the scroll listener gets registered
   *  immediately after mount. */
  containerRef: (el: HTMLDivElement | null) => void;
  /** True while the user is scrolled away from the latest message.
   *  Drives the "↓ jump to latest" affordance. */
  isPinnedAway: boolean;
  /** Programmatic jump to the bottom — called by the FAB and on submit
   *  so a new question always lands the user back at the stream. */
  scrollToBottom: () => void;
  /** Call after every change that could grow the scroll height (token
   *  delta, new turn, source list expanded, etc.). Auto-scrolls only
   *  when the user is still near the bottom. */
  notifyContentChanged: () => void;
}

/** Stick-to-bottom scroll controller for a streaming chat log.
 *
 *  Standard chat-UI behavior: auto-scroll while the user is at (or
 *  near) the bottom, but freeze it the moment they scroll up to read
 *  earlier content. A "↓ jump to latest" affordance lets them rejoin
 *  the stream when they're ready. Without this, every token delta
 *  re-targets `scrollIntoView` and the user can't read anything that
 *  has already scrolled past the viewport.
 */
export function useStickToBottom(): UseStickToBottom {
  const elRef = useRef<HTMLDivElement | null>(null);
  const stickyRef = useRef(true);
  const cleanupRef = useRef<(() => void) | null>(null);
  const [isPinnedAway, setIsPinnedAway] = useState(false);

  const isNearBottom = useCallback((el: HTMLDivElement) => {
    return el.scrollHeight - (el.scrollTop + el.clientHeight) <= STICK_THRESHOLD_PX;
  }, []);

  const scrollToBottom = useCallback(() => {
    const el = elRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    stickyRef.current = true;
    setIsPinnedAway(false);
  }, []);

  const notifyContentChanged = useCallback(() => {
    const el = elRef.current;
    if (!el) return;
    if (stickyRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, []);

  const containerRef = useCallback(
    (el: HTMLDivElement | null) => {
      cleanupRef.current?.();
      cleanupRef.current = null;
      elRef.current = el;
      if (!el) return;
      const handleScroll = () => {
        const near = isNearBottom(el);
        stickyRef.current = near;
        // Only flip the boolean when crossing the threshold so the FAB
        // doesn't flicker as the user scrolls within the near-bottom zone.
        setIsPinnedAway((prev) => (prev === !near ? prev : !near));
      };
      el.addEventListener("scroll", handleScroll, { passive: true });
      cleanupRef.current = () => el.removeEventListener("scroll", handleScroll);
    },
    [isNearBottom],
  );

  useEffect(() => {
    return () => {
      cleanupRef.current?.();
      cleanupRef.current = null;
    };
  }, []);

  return { containerRef, isPinnedAway, scrollToBottom, notifyContentChanged };
}
