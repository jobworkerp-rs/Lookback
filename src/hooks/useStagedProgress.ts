import { useEffect, useRef, useState } from "react";
import { analysisCancel, getActiveStagedProgress, startImportCancel } from "@/api";
import { incrementUiMemoryCounter } from "@/lib/uiMemoryDiagnostics";
import type {
  ImportStep,
  ImportStepState,
  StagedProgressKind,
  StagedProgressSnapshot,
} from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

export const IMPORT_STEPS: readonly ImportStep[] = [
  "thread-import",
  "thread-summary",
  "thread-group-summary",
  "thread-personality",
  "reflection",
];

export const SUMMARY_GENERATION_STEPS = [
  "thread-summary",
  "thread-group-summary",
  "daily",
  "weekly",
  "monthly",
] as const;

const STEPS_BY_KIND = {
  import: IMPORT_STEPS,
  "summary-generation": SUMMARY_GENERATION_STEPS,
} as const;

export type ImportSnapshot = StagedProgressSnapshot<"import">;

export interface StagedProgressHandle<K extends StagedProgressKind> {
  snapshot: StagedProgressSnapshot<K> | null;
  busy: boolean;
  reset: (jobId: string) => void;
  /** Clear only the expected run when provided, protecting a newer run. */
  clear: (expectedJobId?: string) => void;
  dismissToast: () => void;
  toastVisible: boolean;
  cancel: () => Promise<void>;
}

export function defaultStagedSnapshot<K extends StagedProgressKind>(
  kind: K,
  jobId: string,
): StagedProgressSnapshot<K> {
  const steps: Record<string, ImportStepState> = {};
  for (const step of STEPS_BY_KIND[kind]) {
    steps[step] = { status: "waiting", message: null };
  }
  // Import has always shown its first step as active while the dispatch
  // response is in flight; summary generation waits for its first event.
  if (kind === "import") steps["thread-import"] = { status: "active", message: null };
  return {
    kind,
    job_id: jobId,
    sequence: 0,
    steps: steps as StagedProgressSnapshot<K>["steps"],
  };
}

export function isStagedProgressBusy(snapshot: StagedProgressSnapshot | null): boolean {
  if (snapshot == null) return false;
  return Object.values(snapshot.steps).some(
    (step) => step.status === "active" || (step.status === "waiting" && !step.message),
  );
}

/** One state machine serves both staged process kinds. The backend registry
 *  owns per-step rules (no terminal reverts, no repeated digests); this hook
 *  only orders whole snapshots by their per-job sequence. */
export function useStagedProgress<K extends StagedProgressKind>(kind: K): StagedProgressHandle<K> {
  const [snapshot, setSnapshot] = useState<StagedProgressSnapshot<K> | null>(null);
  const [toastVisible, setToastVisible] = useState(false);
  const current = useRef<StagedProgressSnapshot<K> | null>(null);
  const snapshotsByJobId = useRef(new Map<string, StagedProgressSnapshot<K>>());
  const authoritativeSequenceByJobId = useRef(new Map<string, number>());
  const selectedJobId = useRef<string | null>(null);
  const acceptedUpdates = useRef(0);

  const publish = (next: StagedProgressSnapshot<K> | null) => {
    current.current = next;
    setSnapshot(next);
  };

  const applySnapshot = (incoming: StagedProgressSnapshot) => {
    if (incoming.kind !== kind) return;
    if (kind === "summary-generation") {
      incrementUiMemoryCounter("staged_received_total");
    }
    const selectionMismatch =
      selectedJobId.current != null && incoming.job_id !== selectedJobId.current;
    if (selectionMismatch && kind !== "import") return;

    const cached = snapshotsByJobId.current.get(incoming.job_id);
    const prior = cached ?? (current.current?.job_id === incoming.job_id ? current.current : null);
    if (prior && incoming.sequence < prior.sequence) return;

    const authoritativeSequence = authoritativeSequenceByJobId.current.get(incoming.job_id);
    if (authoritativeSequence != null && incoming.sequence <= authoritativeSequence) return;

    const next = incoming as StagedProgressSnapshot<K>;
    snapshotsByJobId.current.set(next.job_id, next);
    authoritativeSequenceByJobId.current.set(next.job_id, next.sequence);
    acceptedUpdates.current += 1;
    if (selectionMismatch) return;

    const visible = current.current;
    if (
      visible == null ||
      visible.job_id === next.job_id ||
      (selectedJobId.current == null && !isStagedProgressBusy(visible))
    ) {
      if (visible?.job_id !== next.job_id) selectedJobId.current = next.job_id;
      if (kind === "summary-generation") {
        incrementUiMemoryCounter("staged_published_total");
      }
      publish(next);
    }
  };

  useTauriEvent<StagedProgressSnapshot>("staged://step", applySnapshot);

  // Hydration is intentionally one-shot. Any accepted event (or a pre-dispatch
  // reset) wins over a slower response so old state cannot reappear afterwards.
  // biome-ignore lint/correctness/useExhaustiveDependencies: hydration is tied to the fixed kind of this hook instance
  useEffect(() => {
    const updatesAtRequest = acceptedUpdates.current;
    let disposed = false;
    void getActiveStagedProgress(kind)
      .catch(() => null)
      .then((restored) => {
        if (disposed || acceptedUpdates.current !== updatesAtRequest || restored == null) return;
        applySnapshot(restored);
      });
    return () => {
      disposed = true;
    };
  }, []);

  return {
    snapshot,
    busy: isStagedProgressBusy(snapshot),
    toastVisible,
    reset(jobId) {
      acceptedUpdates.current += 1;
      selectedJobId.current = jobId;
      const next = snapshotsByJobId.current.get(jobId) ?? defaultStagedSnapshot(kind, jobId);
      snapshotsByJobId.current.set(jobId, next);
      publish(next);
      if (kind === "import") setToastVisible(true);
    },
    clear(expectedJobId) {
      if (expectedJobId != null && current.current?.job_id !== expectedJobId) return;
      if (
        expectedJobId != null &&
        (authoritativeSequenceByJobId.current.has(expectedJobId) ||
          (current.current?.sequence ?? 0) > 0)
      ) {
        return;
      }
      selectedJobId.current = null;
      acceptedUpdates.current += 1;
      snapshotsByJobId.current.clear();
      authoritativeSequenceByJobId.current.clear();
      publish(null);
      setToastVisible(false);
    },
    dismissToast() {
      setToastVisible(false);
    },
    async cancel() {
      const jobId = current.current?.job_id;
      if (!jobId) return;
      if (kind === "import") await startImportCancel(jobId);
      else await analysisCancel(jobId);
    },
  };
}

export function useSummaryGenerationProgress() {
  return useStagedProgress("summary-generation");
}
