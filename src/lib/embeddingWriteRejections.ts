import type { TFunction } from "i18next";

/**
 * Reasons for which Memories correctly discarded a result that was
 * superseded while it was being generated. A newer generation covers the
 * same entity, so the user has nothing to fix.
 */
const ADVISORY_REJECTIONS: ReadonlySet<string> = new Set([
  "source_version_mismatch",
  "attempt_mismatch",
]);

export type RejectionCount = [reason: string, count: number];

export interface SplitWriteRejections {
  advisories: RejectionCount[];
  warnings: RejectionCount[];
}

/**
 * Split the per-reason counters into advisories and warnings, dropping
 * zero counts. Unknown reasons are warnings so a reason added by a newer
 * Memories is never silently hidden.
 */
export function splitWriteRejections(
  record: Record<string, number> | undefined,
): SplitWriteRejections {
  const advisories: RejectionCount[] = [];
  const warnings: RejectionCount[] = [];
  for (const [reason, count] of Object.entries(record ?? {}).sort(([a], [b]) =>
    a.localeCompare(b),
  )) {
    if (count <= 0) continue;
    (ADVISORY_REJECTIONS.has(reason) ? advisories : warnings).push([reason, count]);
  }
  return { advisories, warnings };
}

export function hasWriteRejections({ advisories, warnings }: SplitWriteRejections): boolean {
  return advisories.length > 0 || warnings.length > 0;
}

export function rejectionReasonLabel(t: TFunction, reason: string): string {
  return t(`settings.embeddingWriteRejections.reasons.${reason}`, {
    defaultValue: t("settings.embeddingWriteRejections.unknownReason", { name: reason }),
  });
}
