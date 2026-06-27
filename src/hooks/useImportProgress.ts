import { useState } from "react";
import { startImportCancel } from "@/api";
import type { ImportStep, ImportStepUpdate, StepStatus } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

const STEPS: ImportStep[] = ["thread-import", "thread-summary", "thread-personality", "reflection"];

export interface ImportSnapshot {
  job_id: string;
  steps: Record<ImportStep, { status: StepStatus; message: string | null }>;
}

export function defaultSnapshot(jobId: string): ImportSnapshot {
  const steps = {} as ImportSnapshot["steps"];
  for (const step of STEPS) {
    steps[step] = { status: "waiting", message: null };
  }
  // First step starts active so the toast feels immediately responsive.
  steps["thread-import"] = { status: "active", message: null };
  return { job_id: jobId, steps };
}

/** A run is "busy" while any step is still active. The toast uses this to
 *  swap the Dismiss button for a Cancel button (the Chat-tab pattern). */
export function isImportBusy(snapshot: ImportSnapshot | null): boolean {
  if (snapshot == null) return false;
  return Object.values(snapshot.steps).some((s) => s.status === "active");
}

export function useImportProgress(): {
  snapshot: ImportSnapshot | null;
  busy: boolean;
  reset: (jobId: string) => void;
  clear: () => void;
  cancel: () => Promise<void>;
} {
  const [snapshot, setSnapshot] = useState<ImportSnapshot | null>(null);

  useTauriEvent<ImportStepUpdate>("import://step", (update) => {
    setSnapshot((current) => {
      if (!current || current.job_id !== update.job_id) {
        const next = defaultSnapshot(update.job_id);
        next.steps[update.step] = {
          status: update.status,
          message: update.message,
        };
        return next;
      }
      // Same status + same message arrives for every keep-alive chunk
      // during streaming; short-circuit so downstream consumers don't
      // re-render on no-op updates.
      const prev = current.steps[update.step];
      if (prev.status === update.status && prev.message === update.message) {
        return current;
      }
      return {
        ...current,
        steps: {
          ...current.steps,
          [update.step]: {
            status: update.status,
            message: update.message,
          },
        },
      };
    });
  });

  return {
    snapshot,
    busy: isImportBusy(snapshot),
    // Idempotent: if an event for this job has already populated the snapshot,
    // keep that state. Otherwise the listener can race ahead of the awaited
    // `startImport` promise (dry-run / immediate failure) and we'd clobber a
    // terminal `done` / `failed` with `active` here.
    reset: (jobId: string) =>
      setSnapshot((current) => (current?.job_id === jobId ? current : defaultSnapshot(jobId))),
    clear: () => setSnapshot(null),
    /** Fire-and-forget cancel against the dispatch id parked in the
     *  snapshot. Idempotent server-side: a no-op if the run already
     *  finished, so calling it on a settled toast is harmless. */
    cancel: async () => {
      const jobId = snapshot?.job_id;
      if (!jobId) return;
      await startImportCancel(jobId);
    },
  };
}

export const IMPORT_STEPS = STEPS;
