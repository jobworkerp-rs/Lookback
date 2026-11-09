import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { MonthPicker } from "./MonthPicker";

describe("MonthPicker", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("reports the picked year-month as YYYY-MM", () => {
    const onChange = vi.fn();
    renderWithProviders(<MonthPicker value="2026-09" onChange={onChange} title="開始" />);

    fireEvent.click(screen.getByLabelText("開始"));
    expect(screen.getByText("2026年")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "7月" }));
    expect(onChange).toHaveBeenCalledWith("2026-07");
    expect(document.querySelector(".picker-popover")).toBeNull();
  });

  it("moves the displayed year", () => {
    const onChange = vi.fn();
    renderWithProviders(<MonthPicker value="2026-01" onChange={onChange} title="開始" />);

    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.click(screen.getByTitle("前の年"));
    expect(screen.getByText("2025年")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "12月" }));
    expect(onChange).toHaveBeenCalledWith("2025-12");
  });

  it("opens on the current year and shows a placeholder when empty", () => {
    vi.useFakeTimers({ now: new Date(2026, 9, 16) });
    renderWithProviders(<MonthPicker value="" onChange={() => {}} title="終了" />);
    const trigger = screen.getByLabelText("終了");

    expect(trigger).toHaveTextContent("—");
    expect(screen.queryByLabelText("月をクリア")).toBeNull();
    fireEvent.click(trigger);
    expect(screen.getByText("2026年")).toBeInTheDocument();
    vi.useRealTimers();
  });

  it("marks the selected month and clears it", () => {
    const onChange = vi.fn();
    renderWithProviders(<MonthPicker value="2026-07" onChange={onChange} title="開始" />);

    expect(screen.getByLabelText("開始")).toHaveTextContent("2026-07");
    fireEvent.click(screen.getByLabelText("開始"));
    expect(screen.getByRole("button", { name: "7月" })).toHaveClass("selected");
    fireEvent.click(screen.getByLabelText("月をクリア"));
    expect(onChange).toHaveBeenCalledWith("");
  });
});
