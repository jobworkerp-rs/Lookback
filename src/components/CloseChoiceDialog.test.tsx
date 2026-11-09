import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { CloseChoiceDialog } from "./CloseChoiceDialog";

describe("CloseChoiceDialog", () => {
  beforeEach(() => {
    void i18n.changeLanguage("ja");
  });

  it("offers quit, background, and cancel in Japanese and delegates each choice", () => {
    const onQuit = vi.fn();
    const onBackground = vi.fn();
    const onCancel = vi.fn();
    const view = renderWithProviders(
      <CloseChoiceDialog
        open
        backgroundAvailable
        busy={false}
        error={null}
        onQuit={onQuit}
        onBackground={onBackground}
        onCancel={onCancel}
      />,
    );

    expect(screen.getByRole("dialog", { name: "Lookback を終了しますか？" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "バックグラウンドで実行" }));
    expect(onBackground).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "アプリを終了" }));
    expect(onQuit).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));
    expect(onCancel).toHaveBeenCalledTimes(1);

    void i18n.changeLanguage("en");
    view.rerender(
      <CloseChoiceDialog
        open
        backgroundAvailable
        busy={false}
        error={null}
        onQuit={onQuit}
        onBackground={onBackground}
        onCancel={onCancel}
      />,
    );
    expect(screen.getByRole("dialog", { name: "Quit Lookback?" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run in background" })).toBeInTheDocument();
  });

  it.each(["ja", "en"])(
    "hides unavailable background and asks only about quitting in %s",
    (locale) => {
      void i18n.changeLanguage(locale);
      const onQuit = vi.fn();
      const onBackground = vi.fn();
      const onCancel = vi.fn();
      renderWithProviders(
        <CloseChoiceDialog
          open
          backgroundAvailable={false}
          backgroundUnavailableReason="unverified"
          busy
          error="Could not stop cleanly."
          onQuit={onQuit}
          onBackground={onBackground}
          onCancel={onCancel}
        />,
      );

      expect(
        screen.queryByRole("button", { name: /バックグラウンドで実行|Run in background/ }),
      ).not.toBeInTheDocument();
      expect(
        screen.getByText(locale === "ja" ? "アプリを終了しますか？" : "Quit the app?"),
      ).toBeInTheDocument();
      expect(screen.queryByRole("note")).not.toBeInTheDocument();
      expect(screen.getByText("Could not stop cleanly.")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /終了処理中|Quitting/ })).toBeDisabled();
      expect(screen.getByRole("button", { name: /キャンセル|Cancel/ })).toBeDisabled();
      expect(onBackground).not.toHaveBeenCalled();
      expect(onQuit).not.toHaveBeenCalled();
      expect(onCancel).not.toHaveBeenCalled();
    },
  );
});
