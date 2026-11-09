import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { reextractRejectedCandidate } from "@/api";
import type { AnalysisStepUpdate } from "@/types/api";
import { useRejectedCandidateReextract } from "./useRejectedCandidateReextract";

let stepHandler: ((event: AnalysisStepUpdate) => void) | null = null;
vi.mock("./useTauriEvent", () => ({
  useTauriEvent: vi.fn((_event: string, handler: typeof stepHandler) => {
    stepHandler = handler;
  }),
}));
vi.mock("@/api", () => ({ reextractRejectedCandidate: vi.fn() }));

function wrapper({ children }: { children: ReactNode }) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

describe("useRejectedCandidateReextract", () => {
  beforeEach(() => {
    stepHandler = null;
    vi.mocked(reextractRejectedCandidate).mockReset();
  });

  it("keeps a later rejected candidate when an earlier re-extract succeeds", async () => {
    let resolveDispatch: ((value: { job_id_hint: string }) => void) | undefined;
    vi.mocked(reextractRejectedCandidate).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveDispatch = resolve;
        }),
    );
    const { result } = renderHook(() => useRejectedCandidateReextract(), { wrapper });
    act(() => result.current.setRejectedCandidate("candidate-a"));
    act(() => result.current.reextract());
    await waitFor(() => expect(reextractRejectedCandidate).toHaveBeenCalled());
    await act(async () => resolveDispatch?.({ job_id_hint: "job-a" }));
    act(() => result.current.setRejectedCandidate("candidate-b"));
    act(() => stepHandler?.({ job_id: "job-a", status: "done", message: null }));

    expect(result.current.rejectedCandidateId).toBe("candidate-b");
    expect(result.current.error).toBeNull();
    expect(result.current.isReextracting).toBe(false);
  });

  it("keeps a terminal error received before the command response", async () => {
    let resolveDispatch: ((value: { job_id_hint: string }) => void) | undefined;
    vi.mocked(reextractRejectedCandidate).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveDispatch = resolve;
        }),
    );
    const { result } = renderHook(() => useRejectedCandidateReextract(), { wrapper });
    act(() => result.current.setRejectedCandidate("candidate-a"));
    act(() => result.current.reextract());
    await waitFor(() => expect(reextractRejectedCandidate).toHaveBeenCalled());
    act(() => stepHandler?.({ job_id: "job-a", status: "failed", message: "snapshot mismatch" }));
    await act(async () => resolveDispatch?.({ job_id_hint: "job-a" }));

    expect(result.current.error).toBe("snapshot mismatch");
    expect(result.current.isReextracting).toBe(false);
  });

  it("clears the matching rejected candidate after a successful re-extract", async () => {
    vi.mocked(reextractRejectedCandidate).mockResolvedValue({ job_id_hint: "job-a" });
    const { result } = renderHook(() => useRejectedCandidateReextract(), { wrapper });
    act(() => result.current.setRejectedCandidate("candidate-a"));
    act(() => result.current.reextract());
    await waitFor(() => expect(result.current.isReextracting).toBe(true));
    act(() => stepHandler?.({ job_id: "job-a", status: "done", message: null }));

    expect(result.current.rejectedCandidateId).toBeNull();
    expect(result.current.error).toBeNull();
    expect(result.current.isReextracting).toBe(false);
  });
});
