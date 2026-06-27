import type { ThreadSummary } from "@/types/api";

/**
 * Build a minimal ThreadSummary for opening ThreadDetail when the full row
 * isn't in hand (a search hit, a personality signal, a chat source pill).
 * ThreadDetail only needs `id` to fetch memories; description/labels fill
 * in from the modal's own fetch. `user_id` is the app-wide default tenant
 * ("1"). `createdAtMs` / `updatedAtMs` default to `Date.now()` when the
 * caller has no better timestamp to offer.
 */
export function synthesizeThreadSummary(args: {
  id: string;
  description?: string | null;
  createdAtMs?: number;
  updatedAtMs?: number;
}): ThreadSummary {
  const now = Date.now();
  return {
    id: args.id,
    user_id: "1",
    description: args.description ?? null,
    channel: null,
    labels: [],
    created_at_ms: args.createdAtMs ?? now,
    updated_at_ms: args.updatedAtMs ?? now,
  };
}
