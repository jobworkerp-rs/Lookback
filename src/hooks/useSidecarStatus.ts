import { useEffect, useState } from "react";
import { getSidecarStatus } from "@/api";
import type {
  ConnectionMode,
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
  return { phase: "ready", endpoints: endpoints as SidecarEndpoints, warnings };
}

/** Normalise a `sidecar://error` payload into the SidecarStatus error
 * shape. Both Rust emit sites (`stage_and_start_sidecars` and the
 * embedding-swap rollback) lift through `SidecarErrorPayload`, so this
 * is a 1:1 wrap — kept as a function only so the event handler reads
 * symmetrically with `readyStatusFrom`. */
export function errorStatusFrom(payload: SidecarErrorPayload): SidecarStatus {
  return { phase: "error", warnings: [], failure: payload };
}

/**
 * Fold a mount-time `get_sidecar_status` snapshot into the current status.
 *
 * The snapshot is consulted only while still in the `starting` phase —
 * if either lifecycle event already arrived we keep that, since the
 * event is the live source of truth. The snapshot's purpose is to
 * close the race window where Rust emitted the event *before* the
 * React listener attached, i.e. when memories crashed at init faster
 * than the boot screen could mount its hooks.
 *
 * Snapshot.failure takes precedence over snapshot.ready: a sidecar that
 * later failed must surface BootError even if a prior successful start
 * left a `last_report` behind (e.g. embedding-swap rollback path).
 */
export function applySnapshot(
  prev: SidecarStatus,
  snapshot: SidecarStatusSnapshot | null,
): SidecarStatus {
  if (prev.phase !== "starting" || snapshot == null) return prev;
  if (snapshot.failure) return errorStatusFrom(snapshot.failure);
  if (snapshot.ready) return readyStatusFrom(snapshot.ready);
  return prev;
}

/**
 * Subscribe to lifecycle events emitted by `sidecar::lifecycle::Sidecars`.
 * `sidecar://ready` carries a `SidecarStartReport` (endpoints + non-fatal
 * warnings); `sidecar://error` fires only on hard startup failure.
 *
 * On mount we also fetch `get_sidecar_status` once: a hook mounted after
 * the one-shot `sidecar://ready` event already fired would otherwise stay
 * stuck in `starting`. The snapshot only fills the gap (see `applySnapshot`).
 */
export function useSidecarStatus(): SidecarStatus {
  const [status, setStatus] = useState<SidecarStatus>({ phase: "starting", warnings: [] });

  useTauriEvent<SidecarStartReport>("sidecar://ready", (payload) => {
    setStatus(readyStatusFrom(payload));
  });
  useTauriEvent<SidecarErrorPayload>("sidecar://error", (payload) => {
    setStatus(errorStatusFrom(payload));
  });

  useEffect(() => {
    let cancelled = false;
    getSidecarStatus()
      .then((snapshot) => {
        if (!cancelled) setStatus((prev) => applySnapshot(prev, snapshot));
      })
      .catch((err) => {
        console.error("get_sidecar_status failed", err);
      });
    return () => {
      cancelled = true;
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
  const blocking: SidecarWarningKind[] = ["worker_apply_failed", "plugins_stage_failed"];
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
 * search modes and App renders the degraded banner off it.
 */
export function isVectorDegraded(
  status: SidecarStatus,
  connectionMode: ConnectionMode | null = "local",
): VectorDegradedInfo | null {
  if (connectionMode == null) return null;
  if (connectionMode === "remote") return null;
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
