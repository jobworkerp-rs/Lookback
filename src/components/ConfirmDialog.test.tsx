import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { ConfirmDialog } from "./ConfirmDialog";

describe("ConfirmDialog", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("calls onConfirm when the destructive button is clicked", () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    renderWithProviders(
      <ConfirmDialog
        title="削除しますか?"
        message="本文"
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );
    fireEvent.click(screen.getByText("削除する"));
    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onCancel).not.toHaveBeenCalled();
  });

  it("calls onCancel when the cancel button is clicked", () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    renderWithProviders(
      <ConfirmDialog
        title="削除しますか?"
        message="本文"
        onConfirm={onConfirm}
        onCancel={onCancel}
      />,
    );
    fireEvent.click(screen.getByText("キャンセル"));
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("disables both buttons and shows a busy label while busy", () => {
    const onConfirm = vi.fn();
    renderWithProviders(
      <ConfirmDialog
        title="削除しますか?"
        message="本文"
        busy
        onConfirm={onConfirm}
        onCancel={vi.fn()}
      />,
    );
    const confirm = screen.getByText("削除中…");
    const cancel = screen.getByText("キャンセル");
    expect(confirm).toBeDisabled();
    expect(cancel).toBeDisabled();
    // Clicks are ignored while busy (prevents double-submit).
    fireEvent.click(confirm);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("shows a custom busyLabel while busy instead of the default", () => {
    renderWithProviders(
      <ConfirmDialog
        title="実行を停止しますか?"
        message="本文"
        confirmLabel="停止"
        busy
        busyLabel="停止中…"
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(screen.getByText("停止中…")).toBeInTheDocument();
    expect(screen.queryByText("削除中…")).not.toBeInTheDocument();
  });

  it("falls back to 削除中… when busy without a busyLabel", () => {
    renderWithProviders(
      <ConfirmDialog
        title="削除しますか?"
        message="本文"
        busy
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(screen.getByText("削除中…")).toBeInTheDocument();
  });

  it("honors a custom confirm label and surfaces an error", () => {
    renderWithProviders(
      <ConfirmDialog
        title="スレッドを削除しますか?"
        message="本文"
        confirmLabel="スレッドを削除"
        error="削除に失敗しました"
        onConfirm={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    expect(screen.getByText("スレッドを削除")).toBeInTheDocument();
    expect(screen.getByText("削除に失敗しました")).toBeInTheDocument();
  });
});
