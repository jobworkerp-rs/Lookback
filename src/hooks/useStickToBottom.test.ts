import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useStickToBottom } from "./useStickToBottom";

/** jsdom leaves these as 0 by default. Layout-sensitive hooks need
 *  concrete numbers so the near-bottom threshold logic can be exercised. */
function attachScrollContainer(
  el: HTMLDivElement,
  {
    scrollHeight,
    clientHeight,
    scrollTop,
  }: { scrollHeight: number; clientHeight: number; scrollTop: number },
) {
  Object.defineProperty(el, "scrollHeight", { configurable: true, value: scrollHeight });
  Object.defineProperty(el, "clientHeight", { configurable: true, value: clientHeight });
  // scrollTop is writable in jsdom; assign it directly so the hook's
  // notify path can mutate it too.
  el.scrollTop = scrollTop;
}

describe("useStickToBottom", () => {
  it("auto-scrolls when content grows while the user is at the bottom", () => {
    const { result } = renderHook(() => useStickToBottom());
    const el = document.createElement("div");
    attachScrollContainer(el, { scrollHeight: 500, clientHeight: 300, scrollTop: 200 });
    // 500 - (200 + 300) = 0 → at the bottom.
    act(() => {
      result.current.containerRef(el);
    });
    // Simulate the stream growing: scrollHeight jumps from 500 to 800.
    Object.defineProperty(el, "scrollHeight", { configurable: true, value: 800 });
    act(() => {
      result.current.notifyContentChanged();
    });
    expect(el.scrollTop).toBe(800);
    expect(result.current.isPinnedAway).toBe(false);
  });

  it("freezes auto-scroll once the user scrolls away from the bottom", () => {
    const { result } = renderHook(() => useStickToBottom());
    const el = document.createElement("div");
    attachScrollContainer(el, { scrollHeight: 1000, clientHeight: 300, scrollTop: 700 });
    act(() => {
      result.current.containerRef(el);
    });
    // User scrolls up: scrollTop=200 leaves 500px below the viewport,
    // well past the 80px threshold.
    el.scrollTop = 200;
    act(() => {
      el.dispatchEvent(new Event("scroll"));
    });
    expect(result.current.isPinnedAway).toBe(true);
    // Stream grows; auto-scroll must NOT re-target the user.
    Object.defineProperty(el, "scrollHeight", { configurable: true, value: 1500 });
    act(() => {
      result.current.notifyContentChanged();
    });
    expect(el.scrollTop).toBe(200);
  });

  it("re-enables auto-scroll after scrollToBottom is called", () => {
    const { result } = renderHook(() => useStickToBottom());
    const el = document.createElement("div");
    attachScrollContainer(el, { scrollHeight: 1000, clientHeight: 300, scrollTop: 100 });
    act(() => {
      result.current.containerRef(el);
    });
    act(() => {
      el.dispatchEvent(new Event("scroll"));
    });
    expect(result.current.isPinnedAway).toBe(true);
    act(() => {
      result.current.scrollToBottom();
    });
    expect(el.scrollTop).toBe(1000);
    expect(result.current.isPinnedAway).toBe(false);
    // And new content keeps tracking the tail again.
    Object.defineProperty(el, "scrollHeight", { configurable: true, value: 1300 });
    act(() => {
      result.current.notifyContentChanged();
    });
    expect(el.scrollTop).toBe(1300);
  });
});
