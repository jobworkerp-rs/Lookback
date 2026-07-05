import type { QueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import { refreshGeneratedCaches, refreshImportedThreadCaches } from "@/lib/generatedRefresh";
import type { StepStatus } from "@/types/api";
import type { ImportSnapshot } from "./useImportProgress";

function storesMayHaveChanged(status: StepStatus | undefined): boolean {
  return status === "done" || status === "warning";
}

function refreshKey(jobId: string | undefined, status: StepStatus | undefined): string | null {
  if (!jobId || !storesMayHaveChanged(status)) return null;
  return `${jobId}:${status}`;
}

export function useImportRefresh(queryClient: QueryClient, snapshot: ImportSnapshot | null): void {
  const jobId = snapshot?.job_id;
  const threadImportKey = refreshKey(jobId, snapshot?.steps["thread-import"].status);
  useEffect(() => {
    if (threadImportKey == null) return;
    refreshImportedThreadCaches(queryClient);
  }, [threadImportKey, queryClient]);

  const summaryKey = refreshKey(jobId, snapshot?.steps["thread-summary"].status);
  useEffect(() => {
    if (summaryKey == null) return;
    refreshGeneratedCaches(queryClient, ["thread_summary"]);
  }, [summaryKey, queryClient]);

  const personalityKey = refreshKey(jobId, snapshot?.steps["thread-personality"].status);
  useEffect(() => {
    if (personalityKey == null) return;
    refreshGeneratedCaches(queryClient, ["personality"]);
  }, [personalityKey, queryClient]);

  const reflectionKey = refreshKey(jobId, snapshot?.steps.reflection.status);
  useEffect(() => {
    if (reflectionKey == null) return;
    refreshGeneratedCaches(queryClient, ["reflection"]);
  }, [reflectionKey, queryClient]);
}
