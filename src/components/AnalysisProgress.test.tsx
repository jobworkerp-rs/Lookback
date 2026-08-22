import { fireEvent, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { AnalysisProgress } from "./AnalysisProgress";

describe("AnalysisProgress", () => {
  it.each([
    ["done", "生成完了"],
    ["warning", "一部失敗"],
    ["failed", "生成失敗"],
  ] as const)("shows a close action for terminal %s progress", (status, label) => {
    i18n.changeLanguage("ja");
    const onClose = vi.fn();
    renderWithProviders(
      <AnalysisProgress
        progress={{ job_id: "summary-1", status, message: "daily failed" }}
        error={null}
        onClose={onClose}
      />,
    );

    expect(screen.getByText(label)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "閉じる" }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("keeps the close action hidden while progress is active", () => {
    i18n.changeLanguage("ja");
    renderWithProviders(
      <AnalysisProgress
        progress={{ job_id: "summary-1", status: "active", message: "running" }}
        error={null}
        onClose={vi.fn()}
      />,
    );

    expect(screen.queryByRole("button", { name: "閉じる" })).not.toBeInTheDocument();
  });
});
