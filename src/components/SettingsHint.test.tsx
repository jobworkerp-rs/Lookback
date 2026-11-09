import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Modal } from "./Modal";
import { SettingsHint } from "./SettingsHint";

describe("SettingsHint", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("opens on hover and focus, and closes when those interactions end", () => {
    vi.useFakeTimers();
    renderHint();
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.mouseEnter(button);
    expect(screen.getByRole("tooltip")).toHaveTextContent("設定を保存すると反映されます");
    expect(button).toHaveAttribute("aria-describedby", screen.getByRole("tooltip").id);

    fireEvent.mouseLeave(button);
    act(() => vi.advanceTimersByTime(200));
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();

    fireEvent.focus(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.blur(button);
    act(() => vi.advanceTimersByTime(200));
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("keeps a hover tooltip open while the pointer moves onto it, then closes after leaving it", () => {
    vi.useFakeTimers();
    renderHint({ content: "長い説明" });
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.mouseEnter(button);
    const tooltip = screen.getByRole("tooltip");
    expect(getComputedStyle(tooltip).pointerEvents).toBe("auto");

    fireEvent.mouseLeave(button);
    fireEvent.mouseEnter(tooltip);
    act(() => vi.advanceTimersByTime(200));
    expect(screen.getByRole("tooltip")).toBeInTheDocument();

    fireEvent.mouseLeave(tooltip);
    act(() => vi.advanceTimersByTime(200));
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("keeps the tooltip readable while the trigger blurs over the tooltip", () => {
    vi.useFakeTimers();
    renderHint({ content: "長い説明" });
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.focus(button);
    const tooltip = screen.getByRole("tooltip");
    fireEvent.blur(button);
    fireEvent.mouseEnter(tooltip);
    act(() => vi.advanceTimersByTime(200));
    expect(screen.getByRole("tooltip")).toBeInTheDocument();

    fireEvent.wheel(tooltip);
    fireEvent.touchStart(tooltip);
    fireEvent.mouseLeave(tooltip);
    act(() => vi.advanceTimersByTime(200));
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("pins and unpins the explanation with a tap or keyboard activation", () => {
    renderHint();
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.click(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.mouseLeave(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();

    fireEvent.click(button);
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();

    fireEvent.keyDown(button, { key: "Enter" });
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.keyDown(button, { key: " " });
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("delegates activation and closes the explanation when onActivate is supplied", () => {
    const onActivate = vi.fn();
    renderHint({ onActivate });
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.mouseEnter(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.click(button);

    expect(onActivate).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("closes only the tooltip on Escape without closing its parent modal", () => {
    const onClose = vi.fn();
    render(
      <Modal ariaLabel="編集" onClose={onClose}>
        <SettingsHint label="設定の説明" content="設定を保存すると反映されます" />
      </Modal>,
    );
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.click(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.keyDown(window, { key: "Escape" });

    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog", { name: "編集" })).toBeInTheDocument();
  });

  it("lets the parent modal receive a second Escape after the tooltip is closed", () => {
    const onClose = vi.fn();
    render(
      <Modal ariaLabel="編集" onClose={onClose}>
        <SettingsHint label="設定の説明" content="設定を保存すると反映されます" />
      </Modal>,
    );
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.focus(button);
    fireEvent.keyDown(button, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();

    fireEvent.keyDown(button, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("hides the tooltip when a hovered and focused pinned hint is clicked twice", () => {
    renderHint();
    const button = screen.getByRole("button", { name: "設定の説明" });

    fireEvent.mouseEnter(button);
    fireEvent.focus(button);
    fireEvent.click(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();

    fireEvent.click(button);
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();

    fireEvent.mouseLeave(button);
    fireEvent.mouseEnter(button);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
  });

  it("clamps a portal tooltip inside the viewport", () => {
    Object.defineProperty(window, "innerWidth", { configurable: true, value: 320 });
    Object.defineProperty(window, "innerHeight", { configurable: true, value: 240 });
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.classList.contains("settings-hint-tooltip")) {
        return new DOMRect(0, 0, 140, 400);
      }
      return new DOMRect(280, 100, 32, 24);
    });

    renderHint();
    fireEvent.mouseEnter(screen.getByRole("button", { name: "設定の説明" }));
    const tooltip = screen.getByRole("tooltip");

    expect(tooltip.parentElement).toBe(document.body);
    expect(Number.parseFloat(tooltip.style.left)).toBeLessThanOrEqual(172);
    expect(Number.parseFloat(tooltip.style.top)).toBe(8);
  });
});

function renderHint(options: { content?: string; onActivate?: () => void } = {}) {
  return render(
    <SettingsHint
      label="設定の説明"
      content={options.content ?? "設定を保存すると反映されます"}
      tone="help"
      onActivate={options.onActivate}
    />,
  );
}
