import { useEffect } from "react";

/** Call `onClose` when Escape is pressed. Shared by overlay-style surfaces
 *  (Modal, drawers) so each one doesn't re-register the same listener. */
export function useEscape(onClose: () => void, canHandle: () => boolean = () => true): void {
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (event.key === "Escape" && canHandle()) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [canHandle, onClose]);
}
