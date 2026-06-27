import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef } from "react";

/**
 * Subscribe to a Tauri event for the lifetime of the component. The
 * handler is stored in a ref so callers don't need to memoize it; the
 * underlying `listen()` is set up once and torn down on unmount.
 *
 * `listen()` rejections (rare — usually a malformed event name) are
 * logged instead of being swallowed as an unhandled promise rejection.
 */
export function useTauriEvent<T>(event: string, handler: (payload: T) => void): void {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    listen<T>(event, (e) => handlerRef.current(e.payload))
      .then((u) => {
        if (cancelled) u();
        else unlisten = u;
      })
      .catch((err) => {
        console.error(`failed to subscribe to ${event}`, err);
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [event]);
}
