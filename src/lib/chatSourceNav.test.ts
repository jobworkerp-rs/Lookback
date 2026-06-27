import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@/api", () => ({
  findMemoryPosition: vi.fn(),
}));

import { findMemoryPosition } from "@/api";
import { resolveThreadHighlight } from "./chatSourceNav";

const mocked = vi.mocked(findMemoryPosition);

afterEach(() => {
  mocked.mockReset();
});

describe("resolveThreadHighlight", () => {
  it("returns position + threadTotal when the lookup succeeds", async () => {
    mocked.mockResolvedValue({ position: 12, thread_total: 50 });
    const h = await resolveThreadHighlight("thr-1", "mem-1");
    expect(h).toEqual({ memoryId: "mem-1", position: 12, threadTotal: 50 });
  });

  it("falls back to memoryId-only when the API returns null", async () => {
    mocked.mockResolvedValue(null);
    const h = await resolveThreadHighlight("thr-1", "mem-1");
    expect(h).toEqual({ memoryId: "mem-1" });
  });

  it("falls back to memoryId-only when the API throws", async () => {
    mocked.mockRejectedValue(new Error("network down"));
    const h = await resolveThreadHighlight("thr-1", "mem-1");
    expect(h).toEqual({ memoryId: "mem-1" });
  });
});
