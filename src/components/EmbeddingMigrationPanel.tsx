import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  getEmbeddingRebuildProgress,
  getEmbeddingSpace,
  inspectEmbeddingMigration,
  recoverEmbeddingMigration,
  startEmbeddingRebuild,
  startEmbeddingReconcile,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import {
  hasWriteRejections,
  rejectionReasonLabel,
  type SplitWriteRejections,
  splitWriteRejections,
} from "@/lib/embeddingWriteRejections";
import type {
  EmbeddingMigrationFailure,
  EmbeddingMigrationInspect,
  EmbeddingMigrationNextAction,
  EmbeddingMigrationResolution,
  EmbeddingMigrationResult,
  EmbeddingRebuildProgressView,
  EmbeddingSpaceView,
  EmbeddingVerificationCounts,
} from "@/types/api";

type MigrationMode = "local" | "remote" | "unknown";
type DestructiveAction = "restore" | "abandon";

interface MigrationSnapshot {
  state: string;
  nextAction: EmbeddingMigrationNextAction | null;
  resolution: EmbeddingMigrationResolution | null;
  attemptId: string | null;
  attemptStage: string | null;
  backupMode: "backup" | "none" | null;
  backup: string | null;
  counts: Partial<EmbeddingMigrationInspect> | null;
  failure: EmbeddingMigrationFailure | null;
}

const verificationCountKeys: (keyof EmbeddingVerificationCounts)[] = [
  "missing_memory_text",
  "stale_memory_text",
  "unverified_memory_text",
  "failed_memory_text",
  "missing_memory_media",
  "stale_memory_media",
  "unverified_memory_media",
  "failed_memory_media",
  "missing_thread",
  "stale_thread",
  "unverified_thread",
  "failed_thread",
  "missing_reflection_intent",
  "stale_reflection_intent",
  "unverified_reflection_intent",
  "failed_reflection_intent",
  "orphan",
];

function attemptIdOf(attempt: { attempt: string } | null | undefined): string | null {
  const value = attempt?.attempt;
  return value && value.trim().length > 0 ? value : null;
}

const MIGRATION_INSPECT_POLL_MS = 5000;

/**
 * Each inspection spawns the memories migration CLI and opens the database,
 * so poll only while the state can change on its own: an attempt or migration
 * is in flight, the tool asks to wait, or there is no usable observation yet.
 * Settled states change only through user actions, sidecar restarts, or
 * migration events, which invalidate the query explicitly.
 */
export function embeddingMigrationInspectRefetchMs(
  result: EmbeddingMigrationResult | undefined,
): number | false {
  const snapshot = snapshotOf(result);
  if (!snapshot) return MIGRATION_INSPECT_POLL_MS;
  const inFlight =
    snapshot.attemptId !== null ||
    snapshot.state === "migrating" ||
    snapshot.state === "unavailable" ||
    snapshot.nextAction === "wait";
  return inFlight ? MIGRATION_INSPECT_POLL_MS : false;
}

function snapshotOf(result: EmbeddingMigrationResult | undefined): MigrationSnapshot | null {
  if (!result) return null;
  if (result.kind === "inspect") {
    const inspect = result.data;
    return {
      state: inspect.state,
      nextAction: inspect.next_action,
      resolution: null,
      attemptId: attemptIdOf(inspect.attempt),
      attemptStage: inspect.attempt?.stage ?? null,
      backupMode: inspect.attempt?.backup_mode ?? null,
      backup: inspect.backup,
      counts: inspect,
      failure: null,
    };
  }
  if (result.kind === "failed") {
    const failure = result.data;
    return {
      state: failure.error_code,
      nextAction: null,
      resolution: failure.resolution,
      attemptId: attemptIdOf(failure.attempt),
      attemptStage: failure.attempt?.stage ?? null,
      backupMode: failure.attempt?.backup_mode ?? null,
      backup: failure.backup,
      counts: null,
      failure,
    };
  }
  return null;
}

function failedCount(snapshot: MigrationSnapshot): number | null {
  if (snapshot.failure?.verification_counts) {
    const counts = snapshot.failure.verification_counts as Partial<
      Record<keyof EmbeddingVerificationCounts, unknown>
    >;
    if (!verificationCountKeys.every((key) => isNonNegativeInteger(counts[key]))) return null;
    const verifiedCounts = counts as EmbeddingVerificationCounts;
    const failedByKind = [
      verifiedCounts.failed_memory_text,
      verifiedCounts.failed_memory_media,
      verifiedCounts.failed_thread,
      verifiedCounts.failed_reflection_intent,
    ];
    return failedByKind.reduce((total, count) => total + count, 0);
  }
  const transient = snapshot.counts?.failed_transient;
  const permanent = snapshot.counts?.failed_permanent;
  return isNonNegativeInteger(transient) && isNonNegativeInteger(permanent)
    ? transient + permanent
    : null;
}

function completeVerificationCounts(
  snapshot: MigrationSnapshot,
): EmbeddingVerificationCounts | null {
  const counts = snapshot.failure?.verification_counts as
    | Partial<Record<keyof EmbeddingVerificationCounts, unknown>>
    | null
    | undefined;
  if (!counts || !verificationCountKeys.every((key) => isNonNegativeInteger(counts[key]))) {
    return null;
  }
  return counts as EmbeddingVerificationCounts;
}

function hasValidRebuildFailurePair(failure: EmbeddingMigrationFailure): boolean {
  return (
    (failure.error_code === "rebuild_incomplete" && failure.resolution === "resume_rebuild") ||
    (failure.error_code === "rebuild_has_failures" && failure.resolution === "resolve_failures")
  );
}

function isFailureOnlyRebuildError(failure: EmbeddingMigrationFailure | null): boolean {
  return (
    failure?.error_code === "rebuild_has_failures" && failure.resolution === "resolve_failures"
  );
}

function verificationTotal(snapshot: MigrationSnapshot, keys: string[]): number | null {
  const counts = snapshot.failure?.verification_counts as
    | Record<string, unknown>
    | null
    | undefined;
  if (!counts) return null;
  const values = keys.map((key) => counts[key]);
  return values.every(isNonNegativeInteger) ? values.reduce((sum, count) => sum + count, 0) : null;
}

function isNonNegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function concreteBackup(snapshot: MigrationSnapshot): boolean {
  return snapshot.backupMode === "backup" && Boolean(snapshot.backup?.trim());
}

function hasAttempt(snapshot: MigrationSnapshot): boolean {
  return Boolean(snapshot.attemptId && snapshot.attemptStage && snapshot.backupMode);
}

function canRestore(snapshot: MigrationSnapshot): boolean {
  return (
    hasAttempt(snapshot) &&
    concreteBackup(snapshot) &&
    ["replace", "rebuild_pending", "commit", "restoring"].includes(snapshot.attemptStage ?? "")
  );
}

function canAbandon(snapshot: MigrationSnapshot): boolean {
  if (!hasAttempt(snapshot)) return false;
  if (snapshot.attemptStage === "backup") return true;
  return (
    snapshot.backupMode === "none" &&
    ["replace", "rebuild_pending", "discarding"].includes(snapshot.attemptStage ?? "")
  );
}

function canResume(snapshot: MigrationSnapshot): boolean {
  return hasAttempt(snapshot) && ["backup", "replace"].includes(snapshot.attemptStage ?? "");
}

function expectedInspectAction(snapshot: MigrationSnapshot): string | null {
  if (!hasAttempt(snapshot)) return null;
  switch (snapshot.attemptStage) {
    case "backup":
      return "resume_or_abandon";
    case "replace":
      return snapshot.backupMode === "backup" ? "resume_or_restore" : "resume_or_abandon";
    case "rebuild_pending":
      return "start_rebuild";
    case "commit":
      return "finalize";
    case "restoring":
      return "restore";
    case "discarding":
      return "abandon";
    default:
      return null;
  }
}

function inspectActions(snapshot: MigrationSnapshot): Set<string> {
  const actions = new Set<string>();
  const hasInspectedAttempt = hasAttempt(snapshot);

  if (snapshot.state === "migrating") {
    if (!hasInspectedAttempt || expectedInspectAction(snapshot) !== snapshot.nextAction) {
      return actions;
    }
    switch (snapshot.attemptStage) {
      case "backup":
        actions.add("resume_or_abandon");
        // A backup-stage attempt has not replaced any vectors yet; abandon is
        // safe even when the attempt was configured to create a backup.
        actions.add("abandon");
        break;
      case "replace":
        if (snapshot.nextAction === "resume_or_restore" && concreteBackup(snapshot)) {
          actions.add("resume_or_restore");
          actions.add("restore");
        } else if (snapshot.nextAction === "resume_or_abandon") {
          actions.add("resume_or_abandon");
          actions.add("abandon");
        }
        break;
      case "rebuild_pending": {
        actions.add("start_rebuild");
        if (canRestore(snapshot)) actions.add("restore");
        if (canAbandon(snapshot)) actions.add("abandon");
        const failed = failedCount(snapshot);
        if (failed != null && failed > 0) {
          actions.add("resolve_failed");
        }
        // Inspect may not expose verification totals here. The initial finalize
        // is authorized by the confirmed attempt plus matching completion
        // progress; Memories performs the authoritative verification.
        actions.add("finalize");
        break;
      }
      case "commit":
        actions.add("finalize");
        if (canRestore(snapshot)) actions.add("restore");
        break;
      case "restoring":
        if (canRestore(snapshot)) actions.add("restore");
        break;
      case "discarding":
        if (canAbandon(snapshot)) actions.add("abandon");
        break;
    }
    return actions;
  }

  if (
    snapshot.state === "failed" &&
    snapshot.nextAction === "resolve_failed" &&
    !hasInspectedAttempt
  ) {
    actions.add("resolve_failed");
  } else if (
    (snapshot.state === "incomplete" || snapshot.state === "unverified") &&
    snapshot.nextAction === "reconcile" &&
    !hasInspectedAttempt
  ) {
    actions.add("reconcile");
  } else if (
    (snapshot.state === "rebuild_inconsistent" || snapshot.state === "unavailable") &&
    ((snapshot.nextAction === "restore" && canRestore(snapshot)) ||
      (snapshot.nextAction === "abandon" && canAbandon(snapshot)))
  ) {
    actions.add(snapshot.nextAction);
  }
  return actions;
}

function failureActions(snapshot: MigrationSnapshot): Set<string> {
  const actions = new Set<string>();
  const resolution = snapshot.failure?.resolution;
  if (!resolution) return actions;

  switch (resolution) {
    case "finalize_required":
      if (hasAttempt(snapshot) && snapshot.attemptStage === "commit") actions.add("finalize");
      break;
    case "restore_required":
    case "apply_restore_required":
      if (canRestore(snapshot)) actions.add("restore");
      break;
    case "abandon_required":
      if (canAbandon(snapshot)) actions.add("abandon");
      break;
    case "resume_or_abandon":
      if (hasAttempt(snapshot) && ["backup", "replace"].includes(snapshot.attemptStage ?? "")) {
        actions.add("resume_or_abandon");
        if (canAbandon(snapshot)) actions.add("abandon");
      }
      break;
    case "resume_or_restore":
      if (canResume(snapshot) && canRestore(snapshot) && snapshot.attemptStage === "replace") {
        actions.add("resume_or_restore");
        actions.add("restore");
      }
      break;
    case "continue_or_restore":
      if (snapshot.attemptStage === "rebuild_pending" && canRestore(snapshot)) {
        actions.add("start_rebuild");
        actions.add("restore");
      }
      break;
    case "continue_or_abandon":
      if (snapshot.attemptStage === "rebuild_pending" && canAbandon(snapshot)) {
        actions.add("start_rebuild");
        actions.add("abandon");
      }
      break;
    case "resume_rebuild":
      if (snapshot.attemptStage === "rebuild_pending" && hasAttempt(snapshot)) {
        actions.add("start_rebuild");
      }
      break;
    case "resolve_failures":
      if (!hasAttempt(snapshot)) {
        actions.add("resolve_failed");
      } else if (snapshot.attemptStage === "rebuild_pending" && failedCount(snapshot) != null) {
        actions.add("resolve_failed");
        actions.add("finalize");
      }
      break;
  }
  return actions;
}

/** Apply the CLI state/action contract before rendering or invoking controls. */
function authorizedActions(snapshot: MigrationSnapshot): Set<string> {
  return snapshot.failure ? failureActions(snapshot) : inspectActions(snapshot);
}

function contractProblem(result: EmbeddingMigrationResult | undefined): string | null {
  if (!result) return null;
  if (result.kind !== "inspect" && result.kind !== "failed") {
    return `unexpected result kind: ${result.kind}`;
  }
  const snapshot = snapshotOf(result);
  if (!snapshot) return "missing migration inspection";
  const hasInspectedAttempt = hasAttempt(snapshot);
  if (result.kind === "inspect") {
    const action = result.data.next_action;
    if (snapshot.state === "migrating") {
      if (!hasInspectedAttempt || expectedInspectAction(snapshot) !== action) {
        return `inconsistent inspection state/action: ${snapshot.state}/${action}`;
      }
      if (
        (action === "resume_or_restore" && !concreteBackup(snapshot)) ||
        (snapshot.attemptStage === "restoring" && !canRestore(snapshot)) ||
        (snapshot.attemptStage === "discarding" && !canAbandon(snapshot))
      ) {
        return `inconsistent inspection state/action: ${snapshot.state}/${action}`;
      }
      return null;
    }
    if (snapshot.state === "consistent") {
      return action === "none" && !hasInspectedAttempt
        ? null
        : `inconsistent inspection state/action: ${snapshot.state}/${action}`;
    }
    if (snapshot.state === "incomplete" || snapshot.state === "unverified") {
      return action === "reconcile" && !hasInspectedAttempt
        ? null
        : `inconsistent inspection state/action: ${snapshot.state}/${action}`;
    }
    if (snapshot.state === "failed") {
      return action === "resolve_failed" && !hasInspectedAttempt
        ? null
        : `inconsistent inspection state/action: ${snapshot.state}/${action}`;
    }
    if (snapshot.state === "unknown") {
      return action === "switch" && !hasInspectedAttempt
        ? null
        : `inconsistent inspection state/action: ${snapshot.state}/${action}`;
    }
    if (
      (snapshot.state === "rebuild_inconsistent" || snapshot.state === "unavailable") &&
      (action === "restore" || action === "abandon")
    ) {
      return inspectActions(snapshot).has(action)
        ? null
        : `inconsistent inspection state/action: ${snapshot.state}/${action}`;
    }
    if (
      [
        "start_rebuild",
        "resolve_failed",
        "reconcile",
        "finalize",
        "restore",
        "abandon",
        "resume_or_restore",
        "resume_or_abandon",
      ].includes(action)
    ) {
      return `inconsistent inspection state/action: ${snapshot.state}/${action}`;
    }
    return null;
  }

  const resolution = result.data.resolution;
  const failure = result.data;
  const isRebuildFailureResponse =
    failure.error_code === "rebuild_incomplete" ||
    failure.error_code === "rebuild_has_failures" ||
    resolution === "resume_rebuild" ||
    resolution === "resolve_failures";
  if (isRebuildFailureResponse && !hasValidRebuildFailurePair(failure)) {
    return "inconsistent rebuild failure code/resolution pair";
  }
  if (
    isRebuildFailureResponse &&
    (!hasInspectedAttempt || snapshot.attemptStage !== "rebuild_pending")
  ) {
    return "rebuild verification failure requires its rebuild-pending attempt";
  }
  if (
    failure.error_code === "rebuild_has_failures" &&
    (completeVerificationCounts(snapshot) === null || (failedCount(snapshot) ?? 0) <= 0)
  ) {
    return "rebuild_has_failures requires complete positive verification failure counts";
  }
  if (
    resolution === "finalize_required" &&
    (!hasInspectedAttempt || snapshot.attemptStage !== "commit")
  ) {
    return "finalization requires an inspected commit-stage attempt";
  }
  if (
    resolution === "finalize_required" &&
    result.data.verification_counts != null &&
    failedCount(snapshot) === null
  ) {
    return "finalization response contains incomplete verification failure counts";
  }
  if (
    (resolution === "restore_required" || resolution === "apply_restore_required") &&
    !canRestore(snapshot)
  ) {
    return "restoration requires a stage-authorized attempt and concrete backup";
  }
  if (resolution === "abandon_required" && !canAbandon(snapshot)) {
    return "abandonment requires a stage-authorized attempt";
  }
  if (
    resolution === "resume_or_restore" &&
    (!snapshot.attemptStage || snapshot.attemptStage !== "replace" || !canRestore(snapshot))
  ) {
    return "resume/restore recovery requires a replace-stage attempt and concrete backup";
  }
  if (
    resolution === "resume_or_abandon" &&
    (!hasInspectedAttempt || !["backup", "replace"].includes(snapshot.attemptStage ?? ""))
  ) {
    return "resume/abandon recovery requires a backup- or replace-stage attempt";
  }
  if (
    resolution === "continue_or_restore" &&
    (snapshot.attemptStage !== "rebuild_pending" || !canRestore(snapshot))
  ) {
    return "continue/restore recovery requires a rebuild-pending attempt and concrete backup";
  }
  if (
    resolution === "continue_or_abandon" &&
    (snapshot.attemptStage !== "rebuild_pending" || !canAbandon(snapshot))
  ) {
    return "continue/abandon recovery requires a no-backup rebuild-pending attempt";
  }
  if (
    (resolution === "resume_rebuild" || resolution === "resolve_failures") &&
    hasInspectedAttempt &&
    snapshot.attemptStage !== "rebuild_pending"
  ) {
    return "retrying migration failures requires a rebuild-pending attempt";
  }
  if (
    resolution === "resolve_failures" &&
    hasInspectedAttempt &&
    snapshot.attemptStage === "rebuild_pending" &&
    completeVerificationCounts(snapshot) === null
  ) {
    return "resolving migration failures requires complete verification counts";
  }
  return null;
}

function recoveryActionAuthorized(
  snapshot: MigrationSnapshot,
  action: DestructiveAction | "resume" | "finalize",
): boolean {
  const actions = authorizedActions(snapshot);
  if (action === "resume") {
    return actions.has("resume_or_abandon") || actions.has("resume_or_restore");
  }
  if (action === "restore") {
    return actions.has("restore") || actions.has("resume_or_restore");
  }
  if (action === "abandon") {
    return actions.has("abandon") || actions.has("resume_or_abandon");
  }
  return actions.has("finalize");
}

function migrationResultError(result: EmbeddingMigrationResult): string | null {
  if (result.kind === "failed") {
    return `${result.data.command} (${result.data.stage}: ${result.data.error_code})`;
  }
  if (result.kind === "plan" && !result.data.executable) {
    return result.data.reason ?? null;
  }
  return null;
}

function operationResultAttempt(result: EmbeddingMigrationResult): string | null {
  if (
    result.kind === "switched" ||
    result.kind === "finalized" ||
    result.kind === "restored" ||
    result.kind === "abandoned"
  ) {
    return result.data.attempt;
  }
  return null;
}

function expectedOperationKind(action: DestructiveAction | "resume" | "finalize") {
  if (action === "resume") return "switched";
  if (action === "finalize") return "finalized";
  if (action === "restore") return "restored";
  return "abandoned";
}

/** Rejected embedding writes since Memories started; only some need action. */
function WriteRejectionRecord({ rejections }: { rejections: SplitWriteRejections }) {
  const { t } = useTranslation();
  if (!hasWriteRejections(rejections)) return null;
  const rows = [
    ...rejections.warnings.map((entry) => [...entry, true] as const),
    ...rejections.advisories.map((entry) => [...entry, false] as const),
  ];
  return (
    <div style={{ fontSize: 12, display: "grid", gap: 4, marginTop: 8 }}>
      <div style={{ fontWeight: 600 }}>{t("settings.embeddingWriteRejections.title")}</div>
      <div className="settings-card-desc" style={{ marginBottom: 0 }}>
        {t("settings.embeddingWriteRejections.desc")}
      </div>
      {rows.map(([reason, count, warning]) => (
        <div key={reason} style={warning ? { color: "var(--warning)" } : undefined}>
          {t(
            warning
              ? "settings.embeddingWriteRejections.warningCount"
              : "settings.embeddingWriteRejections.count",
            { reason: rejectionReasonLabel(t, reason), count },
          )}
        </div>
      ))}
    </div>
  );
}

/** Recovery actions are always gated by a fresh Memories inspection and its attempt ID. */
export function EmbeddingMigrationPanel({
  mode,
  compact = false,
}: {
  mode: MigrationMode;
  compact?: boolean;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [operationError, setOperationError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<DestructiveAction | null>(null);
  const [acceptedFailed, setAcceptedFailed] = useState("");
  const [finalizeFailure, setFinalizeFailure] = useState<EmbeddingMigrationFailure | null>(null);
  const inspection = useQuery({
    queryKey: ["embedding-migration-inspect"],
    queryFn: inspectEmbeddingMigration,
    enabled: mode === "local",
    refetchInterval: (query) =>
      mode === "local" ? embeddingMigrationInspectRefetchMs(query.state.data) : false,
  });
  const snapshot = snapshotOf(inspection.data);
  const attemptId = snapshot?.attemptId ?? null;
  const rebuildProgress = useQuery<EmbeddingRebuildProgressView>({
    queryKey: ["embedding-rebuild-progress", "migration-panel", attemptId],
    queryFn: () => getEmbeddingRebuildProgress(attemptId ?? ""),
    enabled: mode === "local" && attemptId !== null && snapshot?.attemptStage === "rebuild_pending",
    refetchInterval:
      mode === "local" && snapshot?.attemptStage === "rebuild_pending" ? 3000 : false,
    retry: false,
  });
  // Remote servers are observed read-only for their write record.
  const spaceObservable = mode !== "unknown";
  const space = useQuery<EmbeddingSpaceView>({
    queryKey: ["embedding-space", "migration-panel", mode],
    queryFn: getEmbeddingSpace,
    enabled: spaceObservable,
    // Rejection counters grow while the app runs.
    refetchInterval: spaceObservable ? 5000 : false,
  });
  const writeRejections = splitWriteRejections(space.data?.write_rejections);

  // Keep rendering the last known state after a transient refresh error, but
  // do not authorize an operation from an observation that is no longer fresh.
  const currentSnapshot = snapshot && !inspection.isError ? snapshot : null;
  const inspectionProblem = contractProblem(inspection.data);
  const inspectedActions =
    currentSnapshot && !inspectionProblem ? authorizedActions(currentSnapshot) : new Set<string>();
  const cachedFinalizeFailure = finalizeFailure
    ? snapshotOf({ kind: "failed", data: finalizeFailure })
    : null;
  const queriedFinalizeFailure =
    snapshot?.failure && hasValidRebuildFailurePair(snapshot.failure) ? snapshot : null;
  const candidateFinalizeFailure = cachedFinalizeFailure ?? queriedFinalizeFailure;
  const activeFinalizeFailure =
    currentSnapshot &&
    currentSnapshot.attemptStage === "rebuild_pending" &&
    currentSnapshot.attemptId !== null &&
    candidateFinalizeFailure?.attemptId === currentSnapshot.attemptId &&
    candidateFinalizeFailure.attemptStage === "rebuild_pending" &&
    candidateFinalizeFailure.backupMode === currentSnapshot.backupMode &&
    candidateFinalizeFailure.failure !== null &&
    hasValidRebuildFailurePair(candidateFinalizeFailure.failure) &&
    (!isFailureOnlyRebuildError(candidateFinalizeFailure.failure) ||
      (completeVerificationCounts(candidateFinalizeFailure) !== null &&
        (failedCount(candidateFinalizeFailure) ?? 0) > 0))
      ? candidateFinalizeFailure
      : null;
  const actions = new Set(inspectedActions);
  if (activeFinalizeFailure?.failure) {
    if (isFailureOnlyRebuildError(activeFinalizeFailure.failure)) {
      actions.delete("start_rebuild");
      actions.add("resolve_failed");
    } else {
      actions.delete("resolve_failed");
      actions.add("start_rebuild");
    }
  }
  const failureCountsSnapshot =
    activeFinalizeFailure ?? (snapshot?.failure?.verification_counts ? snapshot : null);
  const failedSnapshot = failureCountsSnapshot ?? snapshot;
  const failed = failedSnapshot ? failedCount(failedSnapshot) : null;
  const perKindVerificationCounts = failureCountsSnapshot
    ? completeVerificationCounts(failureCountsSnapshot)
    : null;
  const exactAcceptedCount = failed != null && acceptedFailed === String(failed);
  const matchingProgress =
    snapshot?.attemptStage === "rebuild_pending" &&
    attemptId &&
    rebuildProgress.data?.attempt_id === attemptId
      ? rebuildProgress.data
      : null;
  const finalizeReady =
    actions.has("finalize") &&
    (snapshot?.attemptStage === "commit" ||
      (snapshot?.attemptStage === "rebuild_pending" &&
        matchingProgress?.completion_expected === true &&
        !rebuildProgress.isError));
  const rebuildFinalizeReady = finalizeReady && snapshot?.attemptStage === "rebuild_pending";
  const commitFinalizeReady = finalizeReady && snapshot?.attemptStage === "commit";
  const firstRebuildFinalizeReady = rebuildFinalizeReady && activeFinalizeFailure === null;
  const approveFailedFinalizeReady =
    rebuildFinalizeReady &&
    isFailureOnlyRebuildError(activeFinalizeFailure?.failure ?? null) &&
    failed != null &&
    failed > 0;

  useEffect(() => {
    if (!finalizeFailure || inspection.isPending || inspection.isError) return;
    const latest = snapshotOf(inspection.data);
    const sameFailureAuthority =
      latest?.state === "migrating" && latest.nextAction === "start_rebuild";
    const sameFailureResolution =
      latest?.failure !== null &&
      latest?.failure !== undefined &&
      hasValidRebuildFailurePair(latest.failure);
    if (
      !latest ||
      latest.attemptId !== attemptIdOf(finalizeFailure.attempt) ||
      latest.attemptStage !== "rebuild_pending" ||
      latest.backupMode !== finalizeFailure.attempt?.backup_mode ||
      (!sameFailureAuthority && !sameFailureResolution)
    ) {
      setFinalizeFailure(null);
      setAcceptedFailed("");
      setOperationError(null);
    }
  }, [finalizeFailure, inspection.data, inspection.isError, inspection.isPending]);

  const refetchSnapshot = async (expectedAttemptId: string | null) => {
    const latest = await inspection.refetch();
    const latestSnapshot = snapshotOf(latest.data);
    if (latest.isError || !latestSnapshot) {
      throw new Error(t("settings.embeddingMigration.freshInspectFailed"));
    }
    if (latestSnapshot.attemptId !== expectedAttemptId) {
      throw new Error(t("settings.embeddingMigration.stateChanged"));
    }
    if (contractProblem(latest.data)) {
      throw new Error(t("settings.embeddingMigration.inconsistentState"));
    }
    return latestSnapshot;
  };

  const invalidateState = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["embedding-migration-inspect"] }),
      queryClient.invalidateQueries({ queryKey: ["embedding-space"] }),
      queryClient.invalidateQueries({ queryKey: ["embedding-rebuild-progress"] }),
      queryClient.invalidateQueries({ queryKey: ["embedding-reconcile-progress"] }),
    ]);
  };

  const runRecovery = async (action: DestructiveAction | "resume" | "finalize") => {
    if (!attemptId) return;
    setBusy(true);
    setOperationError(null);
    let operationAttempted = false;
    let acknowledgeFailedCount: number | null = null;
    try {
      const latest = await refetchSnapshot(attemptId);
      if (!recoveryActionAuthorized(latest, action)) {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      if (action === "restore" && !canRestore(latest)) {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      if (action === "abandon" && !canAbandon(latest)) {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      if (action === "finalize") {
        if (activeFinalizeFailure && latest.attemptStage !== "rebuild_pending") {
          throw new Error(t("settings.embeddingMigration.stateChanged"));
        }
        if (latest.attemptStage === "rebuild_pending") {
          if (activeFinalizeFailure) {
            if (!isFailureOnlyRebuildError(activeFinalizeFailure.failure)) {
              throw new Error(t("settings.embeddingMigration.stateChanged"));
            }
            const verifiedCounts = completeVerificationCounts(activeFinalizeFailure);
            const failureCount = failedCount(activeFinalizeFailure);
            if (
              activeFinalizeFailure.attemptId !== latest.attemptId ||
              activeFinalizeFailure.backupMode !== latest.backupMode ||
              verifiedCounts === null ||
              failureCount === null ||
              failureCount <= 0 ||
              acceptedFailed !== String(failureCount)
            ) {
              throw new Error(t("settings.embeddingMigration.stateChanged"));
            }
            acknowledgeFailedCount = failureCount;
          }
          const latestProgress = await rebuildProgress.refetch();
          if (
            latestProgress.isError ||
            latestProgress.data?.attempt_id !== latest.attemptId ||
            latestProgress.data.completion_expected !== true
          ) {
            throw new Error(t("settings.embeddingMigration.progressNotComplete"));
          }
        } else if (latest.attemptStage !== "commit") {
          throw new Error(t("settings.embeddingMigration.stateChanged"));
        }
      }
      operationAttempted = true;
      const response = await recoverEmbeddingMigration({
        action,
        attempt_id: latest.attemptId ?? attemptId,
        ...(action === "finalize" && acknowledgeFailedCount !== null
          ? { accept_failed: acknowledgeFailedCount }
          : {}),
      });
      const responseProblem =
        response.result.kind === "failed" ? contractProblem(response.result) : null;
      if (responseProblem) throw new Error(responseProblem);
      if (
        action === "finalize" &&
        latest.attemptStage === "rebuild_pending" &&
        response.result.kind === "failed" &&
        hasValidRebuildFailurePair(response.result.data) &&
        response.result.data.attempt?.stage === "rebuild_pending" &&
        attemptIdOf(response.result.data.attempt) === latest.attemptId &&
        response.result.data.attempt?.backup_mode === latest.backupMode
      ) {
        setFinalizeFailure(response.result.data);
        setAcceptedFailed("");
        setConfirming(null);
        setOperationError(migrationResultError(response.result));
        await invalidateState();
        return;
      }
      const resultError = migrationResultError(response.result);
      if (resultError) throw new Error(resultError);
      const expectedKind = expectedOperationKind(action);
      if (
        response.result.kind !== expectedKind ||
        operationResultAttempt(response.result) !== attemptId
      ) {
        throw new Error(t("settings.embeddingMigration.unexpectedOperationResult"));
      }
      if (!response.restarted) {
        throw new Error(t("settings.embeddingMigration.restartFailed"));
      }
      setAcceptedFailed("");
      setFinalizeFailure(null);
      setConfirming(null);
      await invalidateState();
    } catch (error) {
      setConfirming(null);
      setOperationError(error instanceof Error ? error.message : String(error));
      if (operationAttempted) await invalidateState();
    } finally {
      setBusy(false);
    }
  };

  const retryFailed = async () => {
    if (!snapshot) return;
    setBusy(true);
    setOperationError(null);
    let operationAttempted = false;
    try {
      const latest = await refetchSnapshot(attemptId);
      const lastFailure = activeFinalizeFailure?.failure ?? null;
      const sameAttemptFailure =
        lastFailure !== null &&
        activeFinalizeFailure?.attemptId === latest.attemptId &&
        activeFinalizeFailure.attemptStage === latest.attemptStage &&
        activeFinalizeFailure.backupMode === latest.backupMode;
      const retryFailureOnly =
        activeFinalizeFailure !== null &&
        sameAttemptFailure &&
        isFailureOnlyRebuildError(lastFailure) &&
        latest.state === "migrating" &&
        latest.nextAction === "start_rebuild" &&
        latest.attemptStage === "rebuild_pending" &&
        completeVerificationCounts(activeFinalizeFailure) !== null &&
        (failedCount(activeFinalizeFailure) ?? 0) > 0;
      const latestActions = authorizedActions(latest);
      const retryFromInspectionCounts =
        latest.attemptId !== null &&
        latest.attemptStage === "rebuild_pending" &&
        latestActions.has("resolve_failed") &&
        (failedCount(latest) ?? 0) > 0 &&
        !(sameAttemptFailure && lastFailure?.error_code === "rebuild_incomplete");
      if (
        !(latest.attemptId === null && latestActions.has("resolve_failed")) &&
        !retryFromInspectionCounts &&
        !retryFailureOnly
      ) {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      if (latest.attemptId === null) {
        operationAttempted = true;
        await startEmbeddingReconcile({ retry_failed: true });
      } else if (latest.attemptStage === "rebuild_pending") {
        operationAttempted = true;
        const result = await startEmbeddingRebuild({
          attempt_id: latest.attemptId,
          retry_failed: true,
        });
        if (result.attempt_id !== latest.attemptId) {
          throw new Error(t("settings.embeddingMigration.unexpectedOperationResult"));
        }
        setFinalizeFailure(null);
        setAcceptedFailed("");
      } else {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      await invalidateState();
    } catch (error) {
      setOperationError(error instanceof Error ? error.message : String(error));
      if (operationAttempted) await invalidateState();
    } finally {
      setBusy(false);
    }
  };

  const startRebuild = async () => {
    if (!attemptId) return;
    setBusy(true);
    setOperationError(null);
    let operationAttempted = false;
    try {
      const latest = await refetchSnapshot(attemptId);
      if (!authorizedActions(latest).has("start_rebuild")) {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      const rebuildAttemptId = latest.attemptId ?? attemptId;
      operationAttempted = true;
      const result = await startEmbeddingRebuild({ attempt_id: rebuildAttemptId });
      if (result.attempt_id !== rebuildAttemptId) {
        throw new Error(t("settings.embeddingMigration.unexpectedOperationResult"));
      }
      setFinalizeFailure(null);
      setAcceptedFailed("");
      await invalidateState();
    } catch (error) {
      setOperationError(error instanceof Error ? error.message : String(error));
      if (operationAttempted) await invalidateState();
    } finally {
      setBusy(false);
    }
  };

  const startReconcile = async () => {
    setBusy(true);
    setOperationError(null);
    let operationAttempted = false;
    try {
      const latest = await refetchSnapshot(null);
      if (!authorizedActions(latest).has("reconcile")) {
        throw new Error(t("settings.embeddingMigration.stateChanged"));
      }
      operationAttempted = true;
      await startEmbeddingReconcile();
      await invalidateState();
    } catch (error) {
      setOperationError(error instanceof Error ? error.message : String(error));
      if (operationAttempted) await invalidateState();
    } finally {
      setBusy(false);
    }
  };

  if (mode === "unknown") {
    return (
      <section className="settings-card" aria-label={t("settings.embeddingMigration.title")}>
        <div className="settings-card-title">{t("settings.embeddingMigration.title")}</div>
        <div className="settings-card-desc">
          {t("settings.embeddingMigration.connectionModeUnknown")}
        </div>
      </section>
    );
  }

  if (mode === "remote") {
    // Nothing on a remote server can be acted on here, so an empty card is noise.
    if (!hasWriteRejections(writeRejections)) return null;
    return (
      <section className="settings-card" aria-label={t("settings.embeddingMigration.title")}>
        <div className="settings-card-title">{t("settings.embeddingMigration.title")}</div>
        <div className="settings-card-desc">{t("settings.embeddingMigration.remoteReadOnly")}</div>
        <WriteRejectionRecord rejections={writeRejections} />
      </section>
    );
  }

  const tableMarkers = (space.data?.tables ?? [])
    .map((table) => table.record?.migration_marker)
    .filter((marker): marker is NonNullable<typeof marker> => marker != null);

  return (
    <section
      className="settings-card"
      aria-label={t("settings.embeddingMigration.title")}
      style={compact ? { marginTop: 8 } : undefined}
    >
      <div className="settings-card-title">{t("settings.embeddingMigration.title")}</div>
      {!compact && (
        <div className="settings-card-desc">{t("settings.embeddingMigration.description")}</div>
      )}
      {inspection.isPending && <div role="status">{t("settings.embeddingMigration.loading")}</div>}
      {inspection.isError && (
        <div role="alert" style={{ color: "var(--danger)" }}>
          {t("settings.embeddingMigration.error", { error: String(inspection.error) })}
        </div>
      )}
      {inspectionProblem && snapshot && (
        <div role="alert">
          {t("settings.embeddingMigration.invalidInspection", { error: inspectionProblem })}
        </div>
      )}
      {snapshot && (
        <div style={{ fontSize: 12, display: "grid", gap: 4 }}>
          <div>{t("settings.embeddingMigration.state", { state: snapshot.state })}</div>
          {snapshot.attemptId && (
            <div>
              {t("settings.embeddingMigration.attempt", {
                attempt: snapshot.attemptId,
                stage: snapshot.attemptStage ?? t("settings.embeddingMigration.unknown"),
              })}
            </div>
          )}
          {(snapshot.nextAction || snapshot.resolution) && (
            <div>
              {t("settings.embeddingMigration.nextAction", {
                action: snapshot.nextAction ?? snapshot.resolution,
              })}
            </div>
          )}
          {failureCountsSnapshot?.resolution && (
            <div role="alert">
              {t("settings.embeddingMigration.failureResolution", {
                resolution: failureCountsSnapshot.resolution,
              })}
            </div>
          )}
          {snapshot.counts && !failureCountsSnapshot && (
            <div>
              {t("settings.embeddingMigration.counts", {
                required: snapshot.counts.required ?? t("settings.embeddingMigration.unknown"),
                complete: snapshot.counts.complete ?? t("settings.embeddingMigration.unknown"),
                missing: snapshot.counts.missing ?? t("settings.embeddingMigration.unknown"),
                stale: snapshot.counts.stale ?? t("settings.embeddingMigration.unknown"),
                unverified: snapshot.counts.unverified ?? t("settings.embeddingMigration.unknown"),
                failedTransient:
                  snapshot.counts.failed_transient ?? t("settings.embeddingMigration.unknown"),
                failedPermanent:
                  snapshot.counts.failed_permanent ?? t("settings.embeddingMigration.unknown"),
                orphan: snapshot.counts.orphan ?? t("settings.embeddingMigration.unknown"),
              })}
            </div>
          )}
          {failureCountsSnapshot?.failure?.verification_counts && (
            <div role="alert">
              {t("settings.embeddingMigration.counts", {
                required: t("settings.embeddingMigration.unknown"),
                complete: t("settings.embeddingMigration.unknown"),
                missing:
                  verificationTotal(failureCountsSnapshot, [
                    "missing_memory_text",
                    "missing_memory_media",
                    "missing_thread",
                    "missing_reflection_intent",
                  ]) ?? t("settings.embeddingMigration.unknown"),
                stale:
                  verificationTotal(failureCountsSnapshot, [
                    "stale_memory_text",
                    "stale_memory_media",
                    "stale_thread",
                    "stale_reflection_intent",
                  ]) ?? t("settings.embeddingMigration.unknown"),
                unverified:
                  verificationTotal(failureCountsSnapshot, [
                    "unverified_memory_text",
                    "unverified_memory_media",
                    "unverified_thread",
                    "unverified_reflection_intent",
                  ]) ?? t("settings.embeddingMigration.unknown"),
                failedTransient: t("settings.embeddingMigration.unknown"),
                failedPermanent: failed ?? t("settings.embeddingMigration.unknown"),
                orphan:
                  verificationTotal(failureCountsSnapshot, ["orphan"]) ??
                  t("settings.embeddingMigration.unknown"),
              })}
            </div>
          )}
          {perKindVerificationCounts && (
            <div role="alert">
              {t("settings.embeddingMigration.verificationFailuresByKind", {
                memoryText: perKindVerificationCounts.failed_memory_text,
                memoryMedia: perKindVerificationCounts.failed_memory_media,
                thread: perKindVerificationCounts.failed_thread,
                reflectionIntent: perKindVerificationCounts.failed_reflection_intent,
              })}
            </div>
          )}
          {snapshot.backup && (
            <div>{t("settings.embeddingMigration.backup", { backup: snapshot.backup })}</div>
          )}
          {tableMarkers.map((marker) => (
            <div key={`${marker.attempt_id}:${marker.state}`}>
              {t("settings.embeddingMigration.attempt", {
                attempt: marker.attempt_id,
                stage: marker.state,
              })}
            </div>
          ))}
        </div>
      )}
      {inspection.data && !snapshot && (
        <div role="alert">
          {t("settings.embeddingMigration.invalidInspection", {
            error: inspectionProblem ?? t("settings.embeddingMigration.unknown"),
          })}
        </div>
      )}
      {rebuildProgress.isError && snapshot?.attemptStage === "rebuild_pending" && attemptId && (
        <div role="alert">
          {t("settings.embeddingSpaceStatus.progressError", {
            error: String(rebuildProgress.error),
          })}
        </div>
      )}
      {matchingProgress?.completion_expected && (
        <div role="status">{t("settings.embeddingSpaceStatus.completionExpected")}</div>
      )}
      {snapshot?.attemptStage === "rebuild_pending" &&
        rebuildProgress.data &&
        attemptId &&
        rebuildProgress.data.attempt_id !== attemptId && (
          <div role="alert">{t("settings.embeddingMigration.staleProgress")}</div>
        )}
      {actions.has("resolve_failed") && (
        <button type="button" className="btn" disabled={busy} onClick={() => void retryFailed()}>
          {busy
            ? t("settings.embeddingMigration.retrying")
            : t("settings.embeddingMigration.retryFailed")}
        </button>
      )}
      {attemptId && actions.has("start_rebuild") && (
        <button type="button" className="btn" disabled={busy} onClick={() => void startRebuild()}>
          {t("settings.embeddingMigration.startRebuild")}
        </button>
      )}
      {actions.has("reconcile") && (
        <button type="button" className="btn" disabled={busy} onClick={() => void startReconcile()}>
          {t("settings.embeddingMigration.reconcile")}
        </button>
      )}
      {attemptId && (actions.has("resume_or_abandon") || actions.has("resume_or_restore")) && (
        <button
          type="button"
          className="btn"
          disabled={busy}
          onClick={() => void runRecovery("resume")}
        >
          {t("settings.embeddingMigration.resume")}
        </button>
      )}
      {attemptId &&
        snapshot &&
        concreteBackup(snapshot) &&
        (actions.has("restore") || actions.has("resume_or_restore")) && (
          <button
            type="button"
            className="btn danger"
            disabled={busy}
            onClick={() => setConfirming("restore")}
          >
            {t("settings.embeddingMigration.restore")}
          </button>
        )}
      {attemptId && actions.has("abandon") && (
        <button
          type="button"
          className="btn danger"
          disabled={busy}
          onClick={() => setConfirming("abandon")}
        >
          {t("settings.embeddingMigration.abandon")}
        </button>
      )}
      {attemptId && commitFinalizeReady && (
        <button
          type="button"
          className="btn primary"
          disabled={busy}
          onClick={() => void runRecovery("finalize")}
        >
          {t("settings.embeddingMigration.finalize")}
        </button>
      )}
      {attemptId && firstRebuildFinalizeReady && (
        <button
          type="button"
          className="btn primary"
          disabled={busy}
          onClick={() => void runRecovery("finalize")}
        >
          {t("settings.embeddingMigration.finalize")}
        </button>
      )}
      {attemptId && approveFailedFinalizeReady && failed != null && (
        <div style={{ display: "grid", gap: 4 }}>
          <div role="alert">
            {t("settings.embeddingMigration.acceptFailedHint", { count: failed })}
          </div>
          <input
            type="number"
            min={0}
            aria-label={t("settings.embeddingMigration.acceptFailedPlaceholder", { count: failed })}
            placeholder={t("settings.embeddingMigration.acceptFailedPlaceholder", {
              count: failed,
            })}
            value={acceptedFailed}
            onChange={(event) => setAcceptedFailed(event.target.value)}
          />
          <button
            type="button"
            className="btn danger"
            disabled={busy || !exactAcceptedCount}
            onClick={() => void runRecovery("finalize")}
          >
            {t("settings.embeddingMigration.acceptFailed", { count: failed })}
          </button>
        </div>
      )}
      {operationError && (
        <div role="alert" style={{ color: "var(--danger)" }}>
          {t("settings.embeddingMigration.operationError", { error: operationError })}
        </div>
      )}
      <div className="settings-row">
        <div className="settings-row-label" />
        <button
          type="button"
          className="btn"
          disabled={inspection.isFetching || busy}
          onClick={() => void inspection.refetch()}
        >
          {t("settings.embeddingMigration.reload")}
        </button>
      </div>
      <WriteRejectionRecord rejections={writeRejections} />
      {confirming && (
        <ConfirmDialog
          title={t(
            `settings.embeddingMigration.confirm${confirming === "restore" ? "Restore" : "Abandon"}Title`,
          )}
          message={t(
            `settings.embeddingMigration.confirm${confirming === "restore" ? "Restore" : "Abandon"}`,
          )}
          confirmLabel={t(
            `settings.embeddingMigration.confirm${confirming === "restore" ? "Restore" : "Abandon"}Action`,
          )}
          busy={busy}
          onConfirm={() => void runRecovery(confirming)}
          onCancel={() => setConfirming(null)}
        />
      )}
    </section>
  );
}
