import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { Modal } from "./Modal";
import { MondayDatePicker } from "./MondayDatePicker";

describe("MondayDatePicker", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("shows a Monday-first calendar and reports the selected day", () => {
    const onChange = vi.fn();
    renderWithProviders(<MondayDatePicker value="2026-05-18" onChange={onChange} title="開始" />);

    fireEvent.click(screen.getByLabelText("開始"));
    expect(screen.getAllByText("月")[0]).toBeInTheDocument();
    expect(screen.getAllByText("日")[0]).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "19" }));
    expect(onChange).toHaveBeenCalledWith("2026-05-19");
  });

  it("moves the displayed month", () => {
    renderWithProviders(<MondayDatePicker value="2026-05-18" onChange={() => {}} title="開始" />);

    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.click(screen.getByTitle("次の月"));
    expect(screen.getByText("2026年6月")).toBeInTheDocument();
  });

  it("renders the calendar outside its scrollable parent", () => {
    renderWithProviders(<MondayDatePicker value="2026-05-18" onChange={() => {}} title="開始" />);

    fireEvent.click(screen.getByLabelText("開始"));
    expect(document.querySelector(".picker-popover")?.parentElement).toBe(document.body);
  });

  it("dismisses without changing the date", () => {
    renderWithProviders(<MondayDatePicker value="2026-05-18" onChange={() => {}} title="開始" />);
    const trigger = screen.getByLabelText("開始");

    fireEvent.click(trigger);
    fireEvent.click(trigger);
    expect(document.querySelector(".picker-popover")).toBeNull();

    fireEvent.click(trigger);
    fireEvent.mouseDown(document.body);
    expect(document.querySelector(".picker-popover")).toBeNull();

    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(document.querySelector(".picker-popover")).toBeNull();
  });

  it("keeps only the newly opened range picker visible", () => {
    renderWithProviders(
      <>
        <MondayDatePicker value="2026-05-18" onChange={() => {}} title="開始" />
        <MondayDatePicker value="2026-05-19" onChange={() => {}} title="終了" />
      </>,
    );

    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.mouseDown(screen.getByLabelText("終了"));
    fireEvent.click(screen.getByLabelText("終了"));
    expect(document.querySelectorAll(".picker-popover")).toHaveLength(1);
  });

  it("consumes Escape so its parent modal remains open", () => {
    const onClose = vi.fn();
    renderWithProviders(
      <Modal onClose={onClose} ariaLabel="dialog">
        <MondayDatePicker value="2026-05-18" onChange={() => {}} title="開始" />
      </Modal>,
    );

    fireEvent.click(screen.getByLabelText("開始"));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(document.querySelector(".picker-popover")).toBeNull();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("keeps the popup inside a narrow viewport", () => {
    const viewportWidth = vi.spyOn(window, "innerWidth", "get").mockReturnValue(390);
    renderWithProviders(<MondayDatePicker value="2026-05-18" onChange={() => {}} title="終了" />);
    const trigger = screen.getByLabelText("終了");
    const triggerRect = vi.spyOn(trigger, "getBoundingClientRect").mockReturnValue({
      bottom: 40,
      left: 300,
    } as DOMRect);

    fireEvent.click(trigger);
    expect(document.querySelector(".picker-popover")).toHaveStyle({ left: "144px" });

    triggerRect.mockRestore();
    viewportWidth.mockRestore();
  });

  it("opens upward when there is not enough space below the trigger", () => {
    const viewportHeight = vi.spyOn(window, "innerHeight", "get").mockReturnValue(300);
    renderWithProviders(<MondayDatePicker value="2026-05-18" onChange={() => {}} title="終了" />);
    const trigger = screen.getByLabelText("終了");
    const triggerRect = vi.spyOn(trigger, "getBoundingClientRect").mockReturnValue({
      bottom: 280,
      left: 20,
      top: 250,
    } as DOMRect);

    fireEvent.click(trigger);
    expect(document.querySelector(".picker-popover")).toHaveStyle({ top: "8px" });

    triggerRect.mockRestore();
    viewportHeight.mockRestore();
  });
});
