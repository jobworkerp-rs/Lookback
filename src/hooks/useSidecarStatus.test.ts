import { describe, expect, it } from "vitest";
import type { SidecarErrorPayload, SidecarStartReport, SidecarStatusSnapshot } from "@/types/api";
import {
  applySnapshot,
  errorStatusFrom,
  hasLlmInitFailure,
  isVectorDegraded,
  readyStatusFrom,
  type SidecarStatus,
} from "./useSidecarStatus";

const report: SidecarStartReport = {
  jobworkerp_port: 9000,
  memories_port: 9010,
  conductor_port: 9020,
  mcp_server_port: null,
  warnings: [],
};

const readySnapshot: SidecarStatusSnapshot = { ready: report, failure: null };
const emptySnapshot: SidecarStatusSnapshot = { ready: null, failure: null };

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
    const next = applySnapshot(starting, { ready: null, failure });
    expect(next.phase).toBe("error");
    expect(next.failure).toEqual(failure);
  });

  it("prefers failure over ready when both are populated", () => {
    // A prior successful start followed by an embedding-swap rollback
    // failure leaves both fields populated; BootError must win so the
    // user is not handed a stale endpoints view of an unhealthy sidecar.
    const failure: SidecarErrorPayload = { kind: "raw", message: "swap failed" };
    const next = applySnapshot(starting, { ready: report, failure });
    expect(next.phase).toBe("error");
    expect(next.failure).toEqual(failure);
  });

  it("ignores a null snapshot (still starting)", () => {
    expect(applySnapshot(starting, null)).toBe(starting);
  });

  it("stays starting when both snapshot fields are null", () => {
    expect(applySnapshot(starting, emptySnapshot)).toBe(starting);
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
