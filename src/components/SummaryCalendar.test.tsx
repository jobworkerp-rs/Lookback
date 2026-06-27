import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { SummaryCalendar } from "./SummaryCalendar";

describe("SummaryCalendar", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("marks daily cells with a summary as clickable and reports the key", () => {
    const onSelectKey = vi.fn();
    renderWithProviders(
      <SummaryCalendar
        kind="daily"
        month="2026-05"
        periodKeys={["2026-05-18"]}
        selectedKey={null}
        onSelectKey={onSelectKey}
        onMonthChange={vi.fn()}
      />,
    );
    // The day-18 cell is enabled (has a summary); day 19 is disabled.
    const cells = screen.getAllByRole("button");
    const day18 = cells.find((b) => b.textContent?.trim().startsWith("18"));
    expect(day18).toBeDefined();
    expect(day18).not.toBeDisabled();
    if (day18) fireEvent.click(day18);
    expect(onSelectKey).toHaveBeenCalledWith("2026-05-18");
  });

  it("disables daily cells without a summary", () => {
    renderWithProviders(
      <SummaryCalendar
        kind="daily"
        month="2026-05"
        periodKeys={[]}
        selectedKey={null}
        onSelectKey={vi.fn()}
        onMonthChange={vi.fn()}
      />,
    );
    const cells = screen.getAllByRole("button");
    const day10 = cells.find((b) => b.textContent?.trim().startsWith("10"));
    expect(day10).toBeDisabled();
  });

  it("reports the ISO week key when a weekly cell is clicked", () => {
    const onSelectKey = vi.fn();
    renderWithProviders(
      <SummaryCalendar
        kind="weekly"
        month="2026-05"
        periodKeys={["2026-W21"]}
        selectedKey={null}
        onSelectKey={onSelectKey}
        onMonthChange={vi.fn()}
      />,
    );
    // 2026-05-18 (Monday) belongs to ISO week 2026-W21.
    const cells = screen.getAllByRole("button");
    const day18 = cells.find((b) => b.textContent?.trim().startsWith("18"));
    if (day18) fireEvent.click(day18);
    expect(onSelectKey).toHaveBeenCalledWith("2026-W21");
  });

  it("offers a month-level select button for the monthly kind", () => {
    const onSelectKey = vi.fn();
    renderWithProviders(
      <SummaryCalendar
        kind="monthly"
        month="2026-05"
        periodKeys={["2026-05"]}
        selectedKey={null}
        onSelectKey={onSelectKey}
        onMonthChange={vi.fn()}
      />,
    );
    const monthBtn = screen.getByText("この月の要約");
    fireEvent.click(monthBtn);
    expect(onSelectKey).toHaveBeenCalledWith("2026-05");
  });

  it("navigates months via the prev/next controls", () => {
    const onMonthChange = vi.fn();
    renderWithProviders(
      <SummaryCalendar
        kind="daily"
        month="2026-05"
        periodKeys={[]}
        selectedKey={null}
        onSelectKey={vi.fn()}
        onMonthChange={onMonthChange}
      />,
    );
    fireEvent.click(screen.getByTitle("次の月"));
    expect(onMonthChange).toHaveBeenCalledWith("2026-06");
    fireEvent.click(screen.getByTitle("前の月"));
    expect(onMonthChange).toHaveBeenCalledWith("2026-04");
  });
});
