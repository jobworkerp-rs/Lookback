import { describe, expect, it, vi } from "vitest";
import type { SelectedMemorySnapshot } from "@/types/api";

const { getSelectedMemoryLimitsFromApi } = vi.hoisted(() => ({
  getSelectedMemoryLimitsFromApi: vi.fn(),
}));
vi.mock("@/api", () => ({ getSelectedMemoryLimits: getSelectedMemoryLimitsFromApi }));

import {
  DEFAULT_SELECTED_MEMORY_LIMITS,
  getSelectedMemoryLimits,
  normalizeSelectedMemories,
  projectSelectedMemoryContent,
  validateSelectedMemories,
} from "./selectedMemory";

const memory = (overrides: Partial<SelectedMemorySnapshot> = {}): SelectedMemorySnapshot => ({
  memory_id: "9007199254740993",
  kind: "daily",
  content: JSON.stringify({ title: "作業", source_memory_ids: ["42", "43"], body: "本文" }),
  captured_at_ms: 1,
  ...overrides,
});

describe("selected memory validation", () => {
  it("loads limits from the Tauri command instead of duplicating them in callers", async () => {
    const limits = { ...DEFAULT_SELECTED_MEMORY_LIMITS, max_direct_ids_total: 500 };
    getSelectedMemoryLimitsFromApi.mockResolvedValueOnce(limits);
    await expect(getSelectedMemoryLimits()).resolves.toEqual(limits);
    expect(getSelectedMemoryLimitsFromApi).toHaveBeenCalledOnce();
  });

  it("accepts ten complete snapshots and preserves order", () => {
    const input = Array.from({ length: 10 }, (_, i) => memory({ memory_id: String(i + 1) }));
    expect(validateSelectedMemories(input, DEFAULT_SELECTED_MEMORY_LIMITS)).toEqual(input);
  });

  it("accepts reflection snapshots while keeping unknown kinds invalid", () => {
    expect(validateSelectedMemories([memory({ kind: "reflection" })])).toHaveLength(1);
    expect(() => validateSelectedMemories([memory({ kind: "unknown" as never })])).toThrow(
      "invalid selected memory kind",
    );
  });

  it("rejects unsafe wire ids and the eleventh item", () => {
    expect(() =>
      validateSelectedMemories([memory({ memory_id: "01" })], DEFAULT_SELECTED_MEMORY_LIMITS),
    ).toThrow();
    const input = Array.from({ length: 11 }, (_, i) => memory({ memory_id: String(i + 1) }));
    expect(() => validateSelectedMemories(input, DEFAULT_SELECTED_MEMORY_LIMITS)).toThrow();
  });
});

describe("selected memory projection", () => {
  it("removes structured source id arrays without changing the snapshot", () => {
    const selected = memory();
    const projected = projectSelectedMemoryContent(selected.content);
    expect(projected.content).toContain("本文");
    expect(projected.content).not.toContain("source_memory_ids");
    expect(selected.content).toContain("source_memory_ids");
  });

  it("does not treat ids embedded in plain text as callable sources", () => {
    const projected = projectSelectedMemoryContent("参照 memory_id=123 と thread_id=456");
    expect(projected.allowedMemoryIds).toEqual([]);
    expect(projected.allowedThreadIds).toEqual([]);
    expect(projected.content).toContain("memory_id=123");
  });

  it("deduplicates structured source ids found in the content", () => {
    const selected = memory({
      content: JSON.stringify({ source_memory_ids: ["5", "5", "6"], source_thread_ids: ["7"] }),
    });
    const projected = projectSelectedMemoryContent(selected.content);
    expect(projected.allowedMemoryIds).toEqual(["5", "6"]);
    expect(projected.allowedThreadIds).toEqual(["7"]);
  });

  it("keeps the summary while marking source ids truncated", () => {
    const selected = memory({
      source_memory_ids: Array.from({ length: 101 }, (_, i) => String(i + 1)),
    });
    const normalized = normalizeSelectedMemories([selected]);
    expect(normalized).toHaveLength(1);
    expect(normalized[0]?.source_memory_ids).toHaveLength(100);
    expect(normalized[0]?.source_ids_truncated).toBe(true);
  });

  it("deduplicates direct source ids across the selected set", () => {
    const normalized = normalizeSelectedMemories([
      memory({ memory_id: "1", source_memory_ids: ["50"] }),
      memory({ memory_id: "2", source_memory_ids: ["50", "51"] }),
    ]);
    expect(normalized[1]?.source_memory_ids).toEqual(["51"]);
    expect(normalized[1]?.source_ids_truncated).toBe(true);
  });

  it("allows later selections to adopt source ids excluded by an earlier per-memory limit", () => {
    const limits = { ...DEFAULT_SELECTED_MEMORY_LIMITS, max_direct_ids_per_memory: 1 };
    const normalized = normalizeSelectedMemories(
      [
        memory({ memory_id: "1", source_memory_ids: ["50", "51"] }),
        memory({ memory_id: "2", source_memory_ids: ["51"] }),
      ],
      limits,
    );
    expect(normalized[0]?.source_memory_ids).toEqual(["50"]);
    expect(normalized[1]?.source_memory_ids).toEqual(["51"]);
  });

  it("allows later selections to adopt source threads excluded by an earlier per-memory limit", () => {
    const limits = { ...DEFAULT_SELECTED_MEMORY_LIMITS, max_direct_ids_per_memory: 1 };
    const normalized = normalizeSelectedMemories(
      [
        memory({ memory_id: "1", source_thread_ids: ["60", "61"] }),
        memory({ memory_id: "2", source_thread_ids: ["61"] }),
      ],
      limits,
    );
    expect(normalized[0]?.source_thread_ids).toEqual(["60"]);
    expect(normalized[1]?.source_thread_ids).toEqual(["61"]);
  });

  it("keeps matching IDs when they belong to different source namespaces", () => {
    const normalized = normalizeSelectedMemories([
      memory({ memory_id: "1", source_memory_ids: ["42"] }),
      memory({ memory_id: "2", source_thread_ids: ["42"] }),
    ]);
    expect(normalized[0]?.source_memory_ids).toEqual(["42"]);
    expect(normalized[1]?.source_thread_ids).toEqual(["42"]);
  });
});
