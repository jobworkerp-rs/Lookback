import { useRef, useState } from "react";
import { startImportCancel } from "@/api";
import { isTerminalStepStatus } from "@/lib/stepStatus";
import type { ImportNoticeCode, ImportStep, ImportStepUpdate, StepStatus } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

const STEPS: ImportStep[] = ["thread-import", "thread-summary", "thread-personality", "reflection"];

interface ImportStepState {
  status: StepStatus;
  message: string | null;
  notice_code?: ImportNoticeCode;
}

function toImportStepState(update: ImportStepUpdate): ImportStepState {
  return {
    status: update.status,
    message: update.message,
    ...(update.notice_code ? { notice_code: update.notice_code } : {}),
  };
}

export interface ImportSnapshot {
  job_id: string;
  steps: Record<ImportStep, ImportStepState>;
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
  const snapshotsByJobId = useRef(new Map<string, ImportSnapshot>());

  useTauriEvent<ImportStepUpdate>("import://step", (update) => {
    setSnapshot((current) => {
      const cached = snapshotsByJobId.current.get(update.job_id);
      const base = cached ?? defaultSnapshot(update.job_id);
      // The same status, message, and notice can arrive for every keep-alive
      // chunk during streaming; short-circuit so downstream consumers do not
      // re-render on no-op updates.
      const prev = base.steps[update.step];
      // Tauri event delivery can race with command responses. Once a step has
      // reached a terminal state, a delayed progress event cannot represent a
      // new execution of that same step, so keep the terminal result visible.
      if (isTerminalStepStatus(prev.status) && !isTerminalStepStatus(update.status)) {
        return current;
      }
      if (
        prev.status === update.status &&
        prev.message === update.message &&
        prev.notice_code === update.notice_code
      ) {
        // The first event can legitimately equal the optimistic default. It
        // still establishes a snapshot for this dispatch.
        if (!cached) {
          snapshotsByJobId.current.set(update.job_id, base);
          return !current || current.job_id === update.job_id ? base : current;
        }
        return current;
      }
      const nextStep = toImportStepState(update);
      const next = {
        ...base,
        steps: {
          ...base.steps,
          [update.step]: nextStep,
        },
      };
      snapshotsByJobId.current.set(update.job_id, next);
      // The command response can arrive after this event. Keep a background
      // snapshot until reset associates it with the toast, without allowing a
      // late event from another dispatch to replace the visible job.
      return !current || current.job_id === update.job_id ? next : current;
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
      setSnapshot((current) => {
        if (current?.job_id === jobId) return current;
        const next = snapshotsByJobId.current.get(jobId) ?? defaultSnapshot(jobId);
        snapshotsByJobId.current.set(jobId, next);
        return next;
      }),
    clear: () => {
      snapshotsByJobId.current.clear();
      setSnapshot(null);
    },
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
