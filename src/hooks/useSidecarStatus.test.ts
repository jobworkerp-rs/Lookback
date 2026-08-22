import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getSidecarStatus } from "@/api";
import type { SidecarErrorPayload, SidecarStartReport, SidecarStatusSnapshot } from "@/types/api";
import {
  applySnapshot,
  errorStatusFrom,
  hasLlmInitFailure,
  isVectorDegraded,
  readyStatusFrom,
  type SidecarStatus,
  useSidecarStatus,
} from "./useSidecarStatus";

vi.mock("@/api", () => ({ getSidecarStatus: vi.fn() }));
vi.mock("./useTauriEvent", () => ({ useTauriEvent: vi.fn() }));

const mockedGetSidecarStatus = vi.mocked(getSidecarStatus);

const report: SidecarStartReport = {
  jobworkerp_port: 9000,
  memories_port: 9010,
  conductor_port: 9020,
  mcp_server_port: null,
  warnings: [],
};

const readySnapshot: SidecarStatusSnapshot = {
  ready: report,
  failure: null,
  database_migration_in_progress: false,
};
const emptySnapshot: SidecarStatusSnapshot = {
  ready: null,
  failure: null,
  database_migration_in_progress: false,
};

describe("readyStatusFrom", () => {
  it("splits the flattened endpoints out of the report", () => {
    const status = readyStatusFrom(report);
    expect(status.phase).toBe("ready");
    expect(status.endpoints).toEqual({
      jobworkerp_port: 9000,
      memories_port: 9010,
      conductor_port: 9020,
      mcp_server_port: null,
    });
    expect(status.warnings).toEqual([]);
  });

  it("carries warnings through", () => {
    const status = readyStatusFrom({
      ...report,
      warnings: [{ kind: "plugins_stage_failed", message: "no dylib", detail: null }],
    });
    expect(status.warnings).toHaveLength(1);
    expect(hasLlmInitFailure(status)).toBe(true);
  });
});

describe("isVectorDegraded", () => {
  const withWarnings = (warnings: SidecarStatus["warnings"]): SidecarStatus => ({
    phase: "ready",
    warnings,
  });

  it("returns null when there is no degraded warning", () => {
    expect(isVectorDegraded(withWarnings([]))).toBeNull();
    expect(
      isVectorDegraded(withWarnings([{ kind: "worker_apply_failed", message: "x", detail: null }])),
    ).toBeNull();
  });

  it("parses expected/actual dims out of the detail JSON", () => {
    const status = withWarnings([
      {
        kind: "vector_store_degraded",
        message: "degraded",
        detail: JSON.stringify({
          reason: "embedding_dimension_mismatch",
          expected_dim: 2048,
          actual_dim: 768,
        }),
      },
    ]);
    expect(isVectorDegraded(status)).toEqual({ expectedDim: 2048, actualDim: 768 });
  });

  it("returns an empty object (still degraded) when detail is missing or unparsable", () => {
    expect(
      isVectorDegraded(
        withWarnings([{ kind: "vector_store_degraded", message: "d", detail: null }]),
      ),
    ).toEqual({});
    expect(
      isVectorDegraded(
        withWarnings([{ kind: "vector_store_degraded", message: "d", detail: "{not json" }]),
      ),
    ).toEqual({});
  });

  it("ignores local degraded warnings while the active connection is remote", () => {
    const status = withWarnings([{ kind: "vector_store_degraded", message: "d", detail: null }]);
    expect(isVectorDegraded(status, "remote")).toBeNull();
    expect(isVectorDegraded(status, "local")).toEqual({});
  });

  it("does not assume local degraded while the connection mode is still unknown", () => {
    const status = withWarnings([{ kind: "vector_store_degraded", message: "d", detail: null }]);
    expect(isVectorDegraded(status, null)).toBeNull();
  });
});

describe("applySnapshot", () => {
  const starting: SidecarStatus = { phase: "starting", warnings: [] };

  it("promotes starting -> ready from a snapshot", () => {
    const next = applySnapshot(starting, readySnapshot);
    expect(next.phase).toBe("ready");
    expect(next.endpoints?.jobworkerp_port).toBe(9000);
  });

  it("promotes starting -> error when snapshot carries a structured failure", () => {
    // Closes the race window where memories crashes at init *before*
    // the React listener attached. Without this branch the BootError UI
    // was unreachable for a startup failure that beat hook mount —
    // exactly the regression the reviewer flagged.
    const failure: SidecarErrorPayload = {
      kind: "structured",
      failure: {
        code: "lancedb_schema_mismatch",
        table: "memories",
        uri: "/x",
        expected_dim: 2048,
        actual_dim: 768,
        expected_fingerprint: "",
        actual_fingerprint: "",
      },
    };
    const next = applySnapshot(starting, {
      ready: null,
      failure,
      database_migration_in_progress: true,
    });
    expect(next.phase).toBe("error");
    expect(next.failure).toEqual(failure);
  });

  it("prefers failure over ready when both are populated", () => {
    // A prior successful start followed by an embedding-swap rollback
    // failure leaves both fields populated; BootError must win so the
    // user is not handed a stale endpoints view of an unhealthy sidecar.
    const failure: SidecarErrorPayload = { kind: "raw", message: "swap failed" };
    const next = applySnapshot(starting, {
      ready: report,
      failure,
      database_migration_in_progress: true,
    });
    expect(next.phase).toBe("error");
    expect(next.failure).toEqual(failure);
  });

  it("ignores a null snapshot (still starting)", () => {
    expect(applySnapshot(starting, null)).toBe(starting);
  });

  it("stays starting when both snapshot fields are null", () => {
    expect(applySnapshot(starting, emptySnapshot)).toBe(starting);
  });

  it("marks only the starting screen while the database migration gate runs", () => {
    const next = applySnapshot(starting, {
      ready: null,
      failure: null,
      database_migration_in_progress: true,
    });
    expect(next).toEqual({
      phase: "starting",
      warnings: [],
      databaseMigrationInProgress: true,
    });
  });

  it("clears the migration indication once the gate snapshot completes", () => {
    const migrating: SidecarStatus = {
      phase: "starting",
      warnings: [],
      databaseMigrationInProgress: true,
    };
    expect(applySnapshot(migrating, emptySnapshot)).toEqual({
      ...migrating,
      databaseMigrationInProgress: false,
    });
  });

  it("does not override an already-ready status (event won the race)", () => {
    const ready = readyStatusFrom(report);
    expect(applySnapshot(ready, readySnapshot)).toBe(ready);
  });

  it("does not override an error status", () => {
    const errored: SidecarStatus = {
      phase: "error",
      warnings: [],
      failure: { kind: "raw", message: "boom" },
    };
    expect(applySnapshot(errored, readySnapshot)).toBe(errored);
  });
});

describe("errorStatusFrom", () => {
  it("lifts a structured failure into status.failure", () => {
    const payload: SidecarErrorPayload = {
      kind: "structured",
      failure: {
        code: "lancedb_schema_mismatch",
        table: "memories",
        uri: "/x",
        expected_dim: 2048,
        actual_dim: 768,
        expected_fingerprint: "",
        actual_fingerprint: "",
      },
    };
    const status = errorStatusFrom(payload);
    expect(status.phase).toBe("error");
    expect(status.failure).toEqual(payload);
  });

  it("passes a raw payload through unchanged", () => {
    const payload: SidecarErrorPayload = { kind: "raw", message: "oops" };
    const status = errorStatusFrom(payload);
    expect(status.failure).toEqual(payload);
  });
});

describe("useSidecarStatus polling", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
  });

  it("polls serially while starting and stops after ready", async () => {
    vi.useFakeTimers();
    mockedGetSidecarStatus
      .mockResolvedValueOnce(emptySnapshot)
      .mockResolvedValueOnce(readySnapshot);

    const { result } = renderHook(() => useSidecarStatus());
    await act(async () => {});
    expect(mockedGetSidecarStatus).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });
    expect(mockedGetSidecarStatus).toHaveBeenCalledTimes(2);
    expect(result.current.phase).toBe("ready");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(mockedGetSidecarStatus).toHaveBeenCalledTimes(2);
  });

  it("stops polling after an error snapshot", async () => {
    vi.useFakeTimers();
    const failure: SidecarErrorPayload = { kind: "raw", message: "boom" };
    mockedGetSidecarStatus.mockResolvedValue({
      ready: null,
      failure,
      database_migration_in_progress: false,
    });

    const { result } = renderHook(() => useSidecarStatus());
    await act(async () => {});
    expect(result.current.phase).toBe("error");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(mockedGetSidecarStatus).toHaveBeenCalledTimes(1);
  });

  it("cancels the pending poll when unmounted", async () => {
    vi.useFakeTimers();
    mockedGetSidecarStatus.mockResolvedValue(emptySnapshot);

    const { unmount } = renderHook(() => useSidecarStatus());
    await act(async () => {});
    unmount();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(mockedGetSidecarStatus).toHaveBeenCalledTimes(1);
  });
});
