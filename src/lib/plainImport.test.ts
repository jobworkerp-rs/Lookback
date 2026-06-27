import { afterEach, describe, expect, it } from "vitest";
import {
  DEFAULT_PLAIN_STRATEGY,
  isValidPlainSourceName,
  loadPlainThreadStrategy,
  PLAIN_STRATEGY_STORAGE_KEY,
  savePlainThreadStrategy,
} from "./plainImport";

afterEach(() => {
  localStorage.clear();
});

describe("loadPlainThreadStrategy / savePlainThreadStrategy", () => {
  it("defaults to per-dir when unset", () => {
    expect(loadPlainThreadStrategy()).toBe(DEFAULT_PLAIN_STRATEGY);
    expect(DEFAULT_PLAIN_STRATEGY).toBe("per-dir");
  });

  it("defaults for a malformed stored value", () => {
    localStorage.setItem(PLAIN_STRATEGY_STORAGE_KEY, "per-week");
    expect(loadPlainThreadStrategy()).toBe(DEFAULT_PLAIN_STRATEGY);
  });

  it("round-trips a saved strategy", () => {
    savePlainThreadStrategy("single");
    expect(loadPlainThreadStrategy()).toBe("single");
    savePlainThreadStrategy("per-file");
    expect(loadPlainThreadStrategy()).toBe("per-file");
  });
});

describe("isValidPlainSourceName", () => {
  it("accepts lowercase alnum with - and _ up to 32 chars", () => {
    expect(isValidPlainSourceName("notes")).toBe(true);
    expect(isValidPlainSourceName("obsidian-private")).toBe(true);
    expect(isValidPlainSourceName("notes_01")).toBe(true);
    expect(isValidPlainSourceName("a".repeat(32))).toBe(true);
  });

  it("rejects empty, too long, uppercase, or punctuation", () => {
    expect(isValidPlainSourceName("")).toBe(false);
    expect(isValidPlainSourceName("a".repeat(33))).toBe(false);
    expect(isValidPlainSourceName("Notes")).toBe(false);
    expect(isValidPlainSourceName("dot.name")).toBe(false);
    expect(isValidPlainSourceName("with space")).toBe(false);
  });
});
