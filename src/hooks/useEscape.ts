import { useEffect } from "react";

/** Call `onClose` when Escape is pressed. Shared by overlay-style surfaces
 *  (Modal, drawers) so each one doesn't re-register the same listener. */
export function useEscape(onClose: () => void): void {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);
}
