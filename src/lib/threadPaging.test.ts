import { describe, expect, it } from "vitest";
import type { MemoryRow } from "@/types/api";
import {
  flattenMemories,
  initialOffset,
  nextOffset,
  PAGE_SIZE,
  prependScrollTop,
  prevOffset,
} from "./threadPaging";

function row(id: string): MemoryRow {
  return { id, role: 1, content_type: 0, content: id, created_at_ms: 0 };
}

describe("threadPaging", () => {
  it("PAGE_SIZE is a positive page window", () => {
    expect(PAGE_SIZE).toBeGreaterThan(0);
  });

  describe("initialOffset", () => {
    it("starts at 0 when no highlight position (browse)", () => {
      expect(initialOffset(null)).toBe(0);
      expect(initialOffset(undefined)).toBe(0);
    });

    it("centers the page on the highlight position", () => {
      expect(initialOffset(350)).toBe(350 - Math.floor(PAGE_SIZE / 2));
    });

    it("clamps to 0 when the position is within half a page of the start", () => {
      expect(initialOffset(20)).toBe(0);
      expect(initialOffset(0)).toBe(0);
    });

    it("keeps the hit inside the first page window", () => {
      const pos = 350;
      const start = initialOffset(pos);
      expect(start).toBeLessThanOrEqual(pos);
      expect(pos).toBeLessThan(start + PAGE_SIZE);
    });
  });

  describe("nextOffset (downward)", () => {
    it("stops when the last page was short", () => {
      expect(nextOffset(PAGE_SIZE - 1, 0)).toBeUndefined();
    });

    it("advances by a page when the last page was full", () => {
      expect(nextOffset(PAGE_SIZE, 100)).toBe(100 + PAGE_SIZE);
    });

    it("stops when the next offset reaches the known total", () => {
      expect(nextOffset(PAGE_SIZE, 100, 100 + PAGE_SIZE)).toBeUndefined();
      expect(nextOffset(PAGE_SIZE, 100, 100 + PAGE_SIZE + 1)).toBe(100 + PAGE_SIZE);
    });
  });

  describe("prevOffset (upward)", () => {
    it("stops at the top", () => {
      expect(prevOffset(0)).toBeUndefined();
      expect(prevOffset(-10)).toBeUndefined();
    });

    it("steps back one page, clamped to 0", () => {
      expect(prevOffset(PAGE_SIZE * 2)).toBe(PAGE_SIZE);
      expect(prevOffset(Math.floor(PAGE_SIZE / 2))).toBe(0);
    });
  });

  describe("flattenMemories", () => {
    it("concatenates pages in order", () => {
      const out = flattenMemories([[row("1"), row("2")], [row("3")]]);
      expect(out.map((m) => m.id)).toEqual(["1", "2", "3"]);
    });

    it("dedups by id across page boundaries", () => {
      const out = flattenMemories([
        [row("1"), row("2")],
        [row("2"), row("3")],
      ]);
      expect(out.map((m) => m.id)).toEqual(["1", "2", "3"]);
    });

    it("handles empty input", () => {
      expect(flattenMemories([])).toEqual([]);
    });
  });

  describe("prependScrollTop", () => {
    it("preserves the distance from the bottom after prepend", () => {
      // old: scrollHeight 1000, scrollTop 200 -> bottom distance 800.
      // new: scrollHeight 1500 -> scrollTop should become 1500 - 800 = 700,
      // which equals oldTop + (newH - oldH) = 200 + 500.
      expect(prependScrollTop(1500, 800)).toBe(700);
    });
  });
});
