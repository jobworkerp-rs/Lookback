import { describe, expect, it } from "vitest";
import { isTerminalStepStatus } from "./stepStatus";

describe("isTerminalStepStatus", () => {
  it.each(["done", "warning", "failed"] as const)("recognizes %s as terminal", (status) => {
    expect(isTerminalStepStatus(status)).toBe(true);
  });

  it.each(["waiting", "active"] as const)("keeps %s non-terminal", (status) => {
    expect(isTerminalStepStatus(status)).toBe(false);
  });
});
