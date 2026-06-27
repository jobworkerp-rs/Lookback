import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { DateInput } from "./DateInput";

describe("DateInput", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("marks an empty value as unselected (no has-value class, no clear button)", () => {
    renderWithProviders(<DateInput value="" onChange={() => {}} title="from" />);
    const input = screen.getByTitle("from");
    expect(input).not.toHaveClass("has-value");
    expect(screen.queryByLabelText("日付をクリア")).toBeNull();
  });

  it("adds has-value and shows the clear button once a date is set", () => {
    renderWithProviders(<DateInput value="2026-05-23" onChange={() => {}} title="from" />);
    const input = screen.getByTitle("from");
    expect(input).toHaveClass("has-value");
    expect(screen.getByLabelText("日付をクリア")).toBeInTheDocument();
  });

  it("clears the value when the clear button is clicked", () => {
    const onChange = vi.fn();
    renderWithProviders(<DateInput value="2026-05-23" onChange={onChange} />);
    fireEvent.click(screen.getByLabelText("日付をクリア"));
    expect(onChange).toHaveBeenCalledWith("");
  });

  it("propagates the picked date through onChange", () => {
    const onChange = vi.fn();
    renderWithProviders(<DateInput value="" onChange={onChange} title="from" />);
    fireEvent.change(screen.getByTitle("from"), { target: { value: "2026-01-15" } });
    expect(onChange).toHaveBeenCalledWith("2026-01-15");
  });

  it("hides the clear button while disabled even with a value", () => {
    renderWithProviders(<DateInput value="2026-05-23" onChange={() => {}} title="from" disabled />);
    expect(screen.getByTitle("from")).toBeDisabled();
    expect(screen.queryByLabelText("日付をクリア")).toBeNull();
  });
});
