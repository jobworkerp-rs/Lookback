import { type QueryKey, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { errorMessage } from "@/lib/errorMessage";

export interface DeleteActionHandle {
  /** Id of the item awaiting confirmation, or null when no dialog is open. */
  pendingId: string | null;
  /** True while the delete RPC + cache invalidation are in flight. */
  busy: boolean;
  /** Message from the last failed attempt, surfaced in the confirm dialog. */
  error: string | null;
  /** Open the confirm dialog for `id` (clears any prior error). */
  request(id: string): void;
  /** Dismiss the dialog without deleting. */
  cancel(): void;
  /** Run the delete for the pending id, then invalidate caches. Resolves true
   *  on success (dialog closed), false if the delete failed (dialog stays open
   *  showing `error`). */
  confirm(): Promise<boolean>;
}

/**
 * Shared state machine for a confirm-then-delete action, feeding `ConfirmDialog`'s
 * `busy` / `error` / `onConfirm` / `onCancel` directly. Centralizes the
 * delete → invalidate → reset flow so each list page doesn't hand-roll the
 * same `useState` triple + try/finally (and so the cache-invalidation key set
 * for an entity lives in exactly one call site).
 *
 * `mutationFn` performs the delete; `invalidateKeys` are invalidated together
 * (parallel) on success — pass every query family the entity appears in (mirror
 * the App-level "generation done" invalidation for that entity).
 */
export function useDeleteAction(
  mutationFn: (id: string) => Promise<void>,
  invalidateKeys: QueryKey[],
): DeleteActionHandle {
  const [pendingId, setPendingId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const queryClient = useQueryClient();

  return {
    pendingId,
    busy,
    error,
    request(id) {
      setError(null);
      setPendingId(id);
    },
    cancel() {
      setPendingId(null);
    },
    async confirm() {
      if (pendingId == null) return false;
      setBusy(true);
      setError(null);
      try {
        await mutationFn(pendingId);
        await Promise.all(
          invalidateKeys.map((queryKey) => queryClient.invalidateQueries({ queryKey })),
        );
        setPendingId(null);
        return true;
      } catch (e) {
        setError(errorMessage(e));
        return false;
      } finally {
        setBusy(false);
      }
    },
  };
}
