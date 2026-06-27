import { useState } from "react";
import { analysisCancel } from "@/api";
import type { AnalysisStepUpdate } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

export interface StepStreamProgress {
  job_id: string;
  status: AnalysisStepUpdate["status"];
  message: string | null;
}

/** Return shape of {@link useStepStreamProgress}, lifted so a parent (App)
 *  can own the hook and pass it to the page that renders the progress. */
export interface StepStreamProgressHandle {
  progress: StepStreamProgress | null;
  /** True while the dispatch is still running (status === "active"). The
   *  page uses this to swap the Generate button for a Stop button — the
   *  same pattern Chat composer uses. */
  busy: boolean;
  start(jobId: string): void;
  clear(): void;
  /** Fire-and-forget cancel of the currently-tracked dispatch.
   *  Idempotent: a no-op if no slot is open or the job already settled. */
  cancel(): Promise<void>;
}

function isTerminal(status: StepStreamProgress["status"]): boolean {
  return status === "done" || status === "failed";
}

/**
 * Generic single-job progress tracker for the standalone analysis
 * dispatches (`summary://step` / `personality://step`). Parametrized by
 * event name so the two analysis buttons share one implementation.
 * `start(jobId)` opens a slot; `clear()` dismisses.
 */
export function useStepStreamProgress(eventName: string): StepStreamProgressHandle {
  const [progress, setProgress] = useState<StepStreamProgress | null>(null);

  useTauriEvent<AnalysisStepUpdate>(eventName, (p) => {
    setProgress((prev) => {
      // The listener is mounted before the button is clicked, and the Rust
      // side can emit (esp. a fast `failed` when dispatch_stream errors
      // immediately) before the awaited enqueue* promise resolves and calls
      // `start`. Accept the event as the slot in two cases so a fast
      // failure / completion isn't lost (and `start` doesn't later clobber it
      // back to "active" — `start` is idempotent for the same job_id):
      //   - no slot yet (first dispatch, or after clear), or
      //   - a *new* job_id while the previous slot is already terminal (the
      //     user re-ran without dismissing the prior done/failed result).
      // A different job_id while the prior slot is still active is ignored to
      // avoid interleaving two concurrent dispatches into one slot.
      if (prev == null || (prev.job_id !== p.job_id && isTerminal(prev.status))) {
        return { job_id: p.job_id, status: p.status, message: p.message };
      }
      if (prev.job_id !== p.job_id) return prev;
      // Repeated same-status/same-message chunks during streaming are no-ops.
      if (prev.status === p.status && prev.message === p.message) return prev;
      return { job_id: p.job_id, status: p.status, message: p.message };
    });
  });

  return {
    progress,
    busy: progress?.status === "active",
    // Idempotent: if a terminal/active event for this job already populated the
    // slot (the listener raced ahead of the enqueue* promise), keep it instead
    // of resetting to "active".
    start(jobId: string) {
      setProgress((prev) =>
        prev?.job_id === jobId ? prev : { job_id: jobId, status: "active", message: "dispatching" },
      );
    },
    clear() {
      setProgress(null);
    },
    cancel: async () => {
      const jobId = progress?.job_id;
      if (!jobId) return;
      await analysisCancel(jobId);
    },
  };
}

export function useSummaryProgress() {
  return useStepStreamProgress("summary://step");
}

export function usePersonalityProgress() {
  return useStepStreamProgress("personality://step");
}
