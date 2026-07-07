import { renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { TimezoneContext, useTimezone } from "./useTimezone";

describe("useTimezone", () => {
  it("returns undefined with no provider (leaf views stay QueryClient-agnostic)", () => {
    const { result } = renderHook(() => useTimezone());
    expect(result.current).toBeUndefined();
  });

  it("returns the provided timezone", () => {
    const wrapper = ({ children }: { children: ReactNode }) => (
      <TimezoneContext.Provider value="Europe/Paris">{children}</TimezoneContext.Provider>
    );
    const { result } = renderHook(() => useTimezone(), { wrapper });
    expect(result.current).toBe("Europe/Paris");
  });
});
