import type { StepStatus } from "@/types/api";

/** Returns whether a workflow step can no longer receive progress updates. */
export function isTerminalStepStatus(status: StepStatus): boolean {
  return status === "done" || status === "warning" || status === "failed";
}
