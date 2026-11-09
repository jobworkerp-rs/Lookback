import { useEffect, useRef, useState } from "react";
import { getSidecarStatus } from "@/api";
import type {
  ConnectionMode,
  IssueTrackerAvailabilitySnapshot,
  SidecarEndpoints,
  SidecarErrorPayload,
  SidecarStartReport,
  SidecarStatusSnapshot,
  SidecarWarning,
  SidecarWarningKind,
} from "@/types/api";
import { useTauriEvent } from "./useTauriEvent";

export type SidecarPhase = "starting" | "ready" | "error";

export interface SidecarStatus {
  phase: SidecarPhase;
  endpoints?: SidecarEndpoints;
  warnings: SidecarWarning[];
  /** True only while the memories migration tool is changing the local database. */
  databaseMigrationInProgress?: boolean;
  /** Live primary/Gateway/mutation availability of the local Issue tracker. */
  issueTrackerAvailability?: IssueTrackerAvailabilitySnapshot;
  /**
   * Failure payload from the most recent `sidecar://error` event.
   * Present iff `phase === "error"`. The BootError component branches
   * on `failure.kind` (then on `failure.failure.code`) to render
   * code-specific recovery actions.
   */
  failure?: SidecarErrorPayload;
}

/** Map a `SidecarStartReport` to the ready status (splits the flattened endpoints). */
export function readyStatusFrom(report: SidecarStartReport): SidecarStatus {
  const { warnings = [], ...endpoints } = report;
  return {
    phase: "ready",
    endpoints: endpoints as SidecarEndpoints,
    warnings,
    databaseMigrationInProgress: false,
  };
}

/** Normalise a `sidecar://error` payload into the SidecarStatus error
 * shape. Both Rust emit sites (`stage_and_start_sidecars` and the
 * embedding-swap rollback) lift through `SidecarErrorPayload`, so this
 * is a 1:1 wrap — kept as a function only so the event handler reads
 * symmetrically with `readyStatusFrom`. */
export function errorStatusFrom(payload: SidecarErrorPayload): SidecarStatus {
  return { phase: "error", warnings: [], failure: payload, databaseMigrationInProgress: false };
}

/**
 * Fold a mount-time `get_sidecar_status` snapshot into the current status.
 *
 * The snapshot closes the race window where Rust emitted a lifecycle event
 * before the React listener attached. Once ready, it also supplies the live
 * health snapshot used for long-running sidecars such as Issue Tracker.
 *
 * Snapshot.failure takes precedence over snapshot.ready: a sidecar that
 * later failed must surface BootError even if a prior successful start
 * left a `last_report` behind (e.g. embedding-swap rollback path).
 */
export function applySnapshot(
  prev: SidecarStatus,
  snapshot: SidecarStatusSnapshot | null,
): SidecarStatus {
  if (prev.phase === "error" || snapshot == null) return prev;
  if (snapshot.failure) return errorStatusFrom(snapshot.failure);
  if (snapshot.ready) {
    const ready = readyStatusFrom(snapshot.ready);
    const next = snapshot.issue_tracker_availability
      ? { ...ready, issueTrackerAvailability: snapshot.issue_tracker_availability }
      : ready;
    // Polled every second: reuse `prev` when nothing changed so the app does
    // not re-render on every tick.
    return sameStatus(prev, next) ? prev : next;
  }
  const availability = snapshot.issue_tracker_availability ?? prev.issueTrackerAvailability;
  if (
    Boolean(prev.databaseMigrationInProgress) === snapshot.database_migration_in_progress &&
    availability === prev.issueTrackerAvailability
  ) {
    return prev;
  }
  return {
    ...prev,
    databaseMigrationInProgress: snapshot.database_migration_in_progress,
    ...(availability ? { issueTrackerAvailability: availability } : {}),
  };
}

// Statuses are plain JSON data built in a fixed key order, so a serialized
// comparison is an exact structural equality check.
function sameStatus(a: SidecarStatus, b: SidecarStatus): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Subscribe to lifecycle events emitted by `sidecar::lifecycle::Sidecars`.
 * `sidecar://ready` carries a `SidecarStartReport` (endpoints + non-fatal
 * warnings); `sidecar://error` fires only on hard startup failure.
 *
 * On mount we fetch `get_sidecar_status` to close the lifecycle-event race.
 * We keep polling after readiness so runtime sidecar failures and recoveries
 * are reflected without restarting the React application.
 */
export function useSidecarStatus(): SidecarStatus {
  const [status, setStatus] = useState<SidecarStatus>({ phase: "starting", warnings: [] });
  const phaseRef = useRef<SidecarPhase>("starting");

  useTauriEvent<SidecarStartReport>("sidecar://ready", (payload) => {
    const next = readyStatusFrom(payload);
    phaseRef.current = next.phase;
    setStatus(next);
  });
  useTauriEvent<SidecarErrorPayload>("sidecar://error", (payload) => {
    const next = errorStatusFrom(payload);
    phaseRef.current = next.phase;
    setStatus(next);
  });

  useEffect(() => {
    let cancelled = false;
    let poll: number | undefined;
    const scheduleNext = () => {
      if (!cancelled && phaseRef.current !== "error") {
        poll = window.setTimeout(refresh, phaseRef.current === "starting" ? 500 : 1_000);
      }
    };
    const refresh = () => {
      getSidecarStatus()
        .then((snapshot) => {
          if (!cancelled) {
            setStatus((prev) => {
              const next = applySnapshot(prev, snapshot);
              phaseRef.current = next.phase;
              return next;
            });
          }
        })
        .catch((err) => {
          console.error("get_sidecar_status failed", err);
        })
        .finally(() => {
          scheduleNext();
        });
    };
    refresh();
    return () => {
      cancelled = true;
      if (poll !== undefined) window.clearTimeout(poll);
    };
  }, []);

  return status;
}

/// Convenience predicate used by ImportDialog / Sidebar so a UI surface
/// can disable LLM-tagged flows without re-checking the warning kind
/// string at every call site. Keep the `blocking` set in sync with Rust's
/// `Sidecars::llm_blocking_error` (sidecar/lifecycle.rs) — both layers must
/// agree on which warnings mean "LLM init failed".
export function hasLlmInitFailure(status: SidecarStatus): boolean {
  const blocking: SidecarWarningKind[] = [
    "worker_apply_failed",
    "plugins_stage_failed",
    "remote_jobworkerp_unavailable",
  ];
  return status.warnings.some((w) => blocking.includes(w.kind));
}

/** Dimension-mismatch diagnostics for a degraded (vector-disabled) start. */
export interface VectorDegradedInfo {
  expectedDim?: number;
  actualDim?: number;
}

/**
 * When the sidecar reported a `vector_store_degraded` warning, return the
 * mismatch dimensions (parsed from `SidecarWarning.detail`, a JSON blob
 * mirroring Rust's `DegradedInfo`); otherwise `null`. A present-but-unparsable
 * detail still returns `{}` — degraded is true, we just can't show the dims.
 * Mirror of `hasLlmInitFailure`: pages read this to disable embedding-dependent
 * search modes and App renders the degraded banner off it. Only a local
 * memories reports this warning.
 */
export function isVectorDegraded(
  status: SidecarStatus,
  memoriesMode: ConnectionMode | null = "local",
): VectorDegradedInfo | null {
  if (memoriesMode == null) return null;
  if (memoriesMode === "remote") return null;
  const warning = status.warnings.find((w) => w.kind === "vector_store_degraded");
  if (!warning) return null;
  if (!warning.detail) return {};
  try {
    const parsed = JSON.parse(warning.detail) as {
      expected_dim?: number;
      actual_dim?: number;
    };
    return { expectedDim: parsed.expected_dim, actualDim: parsed.actual_dim };
  } catch {
    return {};
  }
}
