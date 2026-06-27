import { useCallback, useRef, useState } from "react";
import { reflectionCancel } from "@/api";
import type { ReflectionStepUpdate } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

export interface ReflectionProgress {
  job_id: string;
  status: ReflectionStepUpdate["status"];
  message: string | null;
}

/** Return shape of {@link useReflectionProgress}, lifted so App can own the
 *  hook and pass it to the Reflections page. */
export interface ReflectionProgressHandle {
  progress: ReflectionProgress | null;
  start(jobId: string): void;
  cancel(): void;
  clear(): void;
}

/**
 * Subscribe to `reflection://step` events fired by the Rust side as the
 * `memories-reflection-batch` workflow streams. `start(jobId)` opens a
 * fresh slot the next event will populate; `cancel()` sends the cancel
 * command; `clear()` dismisses progress.
 *
 * Only the latest `job_id` is tracked — concurrent dispatches would
 * each have their own toast in a future iteration.
 */
export function useReflectionProgress(): ReflectionProgressHandle {
  const [progress, setProgress] = useState<ReflectionProgress | null>(null);
  const dispatchIdRef = useRef<string | null>(null);

  useTauriEvent<ReflectionStepUpdate>("reflection://step", (p) => {
    setProgress((prev) => {
      if (prev == null) return null;
      if (prev.job_id !== p.job_id) return prev;
      if (prev.status === p.status && prev.message === p.message) return prev;
      return { job_id: p.job_id, status: p.status, message: p.message };
    });
  });

  const cancel = useCallback(() => {
    const id = dispatchIdRef.current;
    if (id) {
      void reflectionCancel(id);
    }
  }, []);

  return {
    progress,
    start(jobId: string) {
      dispatchIdRef.current = jobId;
      setProgress({ job_id: jobId, status: "active", message: "dispatching" });
    },
    cancel,
    clear() {
      dispatchIdRef.current = null;
      setProgress(null);
    },
  };
}
