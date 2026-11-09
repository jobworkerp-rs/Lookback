import {
  defaultStagedSnapshot,
  IMPORT_STEPS,
  type ImportSnapshot,
  isStagedProgressBusy,
  useStagedProgress,
} from "./useStagedProgress";

export { IMPORT_STEPS, type ImportSnapshot };

/** Kept as the import-facing constructor for existing panels and fixtures. */
export function defaultSnapshot(jobId: string): ImportSnapshot {
  return defaultStagedSnapshot("import", jobId);
}

export function isImportBusy(snapshot: ImportSnapshot | null): boolean {
  return isStagedProgressBusy(snapshot);
}

export function useImportProgress() {
  return useStagedProgress("import");
}
