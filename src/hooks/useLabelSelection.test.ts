import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useLabelSelection } from "./useLabelSelection";

describe("useLabelSelection", () => {
  it("toggles a single label on and off", () => {
    const { result } = renderHook(() => useLabelSelection());

    act(() => result.current.toggleLabel("agent:codex"));
    expect(result.current.selectedLabels).toEqual(["agent:codex"]);

    act(() => result.current.toggleLabel("agent:codex"));
    expect(result.current.selectedLabels).toEqual([]);
  });

  it("toggles many labels on and off without duplicating entries", () => {
    const { result } = renderHook(() => useLabelSelection(["existing"]));

    act(() => result.current.toggleManyLabels(["a", "b", "existing"], true));
    expect(new Set(result.current.selectedLabels)).toEqual(new Set(["existing", "a", "b"]));

    act(() => result.current.toggleManyLabels(["a", "existing"], false));
    expect(result.current.selectedLabels).toEqual(["b"]);
  });

  it("exposes a sorted view independent of toggle order", () => {
    const { result } = renderHook(() => useLabelSelection());

    act(() => {
      result.current.toggleLabel("zeta");
      result.current.toggleLabel("alpha");
    });

    expect(result.current.sortedLabels).toEqual(["alpha", "zeta"]);
  });
});
