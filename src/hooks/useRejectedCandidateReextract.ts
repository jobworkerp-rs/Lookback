import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";
import { reextractRejectedCandidate } from "@/api";
import { invalidateIssueQueries } from "@/pages/issues/queryInvalidation";
import type { AnalysisStepUpdate } from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

type TerminalStep = Pick<AnalysisStepUpdate, "job_id" | "status" | "message"> & {
  status: "done" | "failed";
};

type ActiveReextract = {
  jobId: string;
  candidateId: string;
};

type CandidateError = {
  candidateId: string;
  message: string;
};

/** Tracks the single explicit re-extract affordance shown after a rejection. */
export function useRejectedCandidateReextract() {
  const queryClient = useQueryClient();
  const [rejectedCandidateId, setRejectedCandidateId] = useState<string | null>(null);
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [candidateError, setCandidateError] = useState<CandidateError | null>(null);
  const rejectedCandidateRef = useRef<string | null>(null);
  const activeJobRef = useRef<ActiveReextract | null>(null);
  const dispatchPendingRef = useRef(false);
  const pendingTerminalsRef = useRef(new Map<string, TerminalStep>());

  const setRejectedCandidate = (candidateId: string | null) => {
    rejectedCandidateRef.current = candidateId;
    setRejectedCandidateId(candidateId);
  };

  const settle = (terminal: TerminalStep) => {
    const activeJob = activeJobRef.current;
    if (activeJob?.jobId !== terminal.job_id) return;
    activeJobRef.current = null;
    setActiveJobId(null);
    if (terminal.status === "failed") {
      if (rejectedCandidateRef.current === activeJob.candidateId) {
        setCandidateError({
          candidateId: activeJob.candidateId,
          message: terminal.message ?? "再抽出に失敗しました",
        });
      }
      return;
    }
    void invalidateIssueQueries(queryClient, "all");
    if (rejectedCandidateRef.current === activeJob.candidateId) setRejectedCandidate(null);
  };

  const mutation = useMutation({
    mutationFn: reextractRejectedCandidate,
    onMutate: () => {
      dispatchPendingRef.current = true;
      pendingTerminalsRef.current.clear();
      setCandidateError(null);
    },
    onSuccess: ({ job_id_hint }, candidateId) => {
      dispatchPendingRef.current = false;
      activeJobRef.current = { jobId: job_id_hint, candidateId };
      setActiveJobId(job_id_hint);
      const terminal = pendingTerminalsRef.current.get(job_id_hint);
      pendingTerminalsRef.current.clear();
      if (terminal) settle(terminal);
    },
    onError: (error, candidateId) => {
      dispatchPendingRef.current = false;
      pendingTerminalsRef.current.clear();
      setCandidateError({ candidateId, message: error.message });
    },
  });

  useTauriEvent<AnalysisStepUpdate>("summary://step", (event) => {
    if (event.status !== "done" && event.status !== "failed") return;
    const terminal: TerminalStep = {
      job_id: event.job_id,
      status: event.status as TerminalStep["status"],
      message: event.message,
    };
    if (activeJobRef.current?.jobId === event.job_id) {
      settle(terminal);
    } else if (dispatchPendingRef.current) {
      pendingTerminalsRef.current.set(event.job_id, terminal);
    }
  });

  return {
    rejectedCandidateId,
    setRejectedCandidate,
    reextract: () => {
      if (rejectedCandidateId) mutation.mutate(rejectedCandidateId);
    },
    isReextracting: mutation.isPending || activeJobId !== null,
    error: candidateError?.candidateId === rejectedCandidateId ? candidateError.message : null,
  };
}
