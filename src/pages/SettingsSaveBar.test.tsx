import { fireEvent, render, screen } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { SettingsSaveBar } from "./SettingsSaveBar";

const noop = () => {};

describe("SettingsSaveBar", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("renders nothing when no card is dirty", () => {
    const { container } = renderWithProviders(
      <SettingsSaveBar
        dirtyCount={0}
        resetsVectordb={false}
        saving={false}
        onDiscard={noop}
        onSave={noop}
      />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("shows the dirty count and fires onSave / onDiscard", () => {
    const onSave = vi.fn();
    const onDiscard = vi.fn();
    renderWithProviders(
      <SettingsSaveBar
        dirtyCount={2}
        resetsVectordb={false}
        saving={false}
        onDiscard={onDiscard}
        onSave={onSave}
      />,
    );
    expect(screen.getByText(/未保存の変更があります \(2件\)/)).toBeInTheDocument();
    fireEvent.click(screen.getByText("保存して適用 (再起動)"));
    expect(onSave).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByText("破棄"));
    expect(onDiscard).toHaveBeenCalledTimes(1);
  });

  it("surfaces the vectordb-reset warning only when the change resets the vectordb", () => {
    const { rerender } = render(
      <I18nextProvider i18n={i18n}>
        <SettingsSaveBar
          dirtyCount={1}
          resetsVectordb={false}
          saving={false}
          onDiscard={noop}
          onSave={noop}
        />
      </I18nextProvider>,
    );
    expect(screen.queryByText(/vectordb はリセット/)).not.toBeInTheDocument();

    rerender(
      <I18nextProvider i18n={i18n}>
        <SettingsSaveBar
          dirtyCount={1}
          resetsVectordb={true}
          saving={false}
          onDiscard={noop}
          onSave={noop}
        />
      </I18nextProvider>,
    );
    expect(screen.getByText(/vectordb はリセット/)).toBeInTheDocument();
  });

  it("warns that only required sidecars restart and jobworkerp jobs stop", () => {
    renderWithProviders(
      <SettingsSaveBar
        dirtyCount={1}
        resetsVectordb={false}
        saving={false}
        onDiscard={noop}
        onSave={noop}
      />,
    );
    expect(
      screen.getByText(
        /必要な sidecar を再起動する場合、jobworkerp で実行中の生成・インポート・要約などは停止します/,
      ),
    ).toBeInTheDocument();
    expect(screen.getByText("保存して適用 (再起動)")).toBeInTheDocument();
  });

  it("disables the buttons while saving", () => {
    renderWithProviders(
      <SettingsSaveBar
        dirtyCount={1}
        resetsVectordb={false}
        saving={true}
        onDiscard={noop}
        onSave={noop}
      />,
    );
    expect(screen.getByText("保存中…")).toBeDisabled();
    expect(screen.getByText("破棄")).toBeDisabled();
  });

  it("does not imply a restart for coding-agent-only changes", () => {
    renderWithProviders(
      <SettingsSaveBar
        dirtyCount={1}
        resetsVectordb={false}
        saving={false}
        showRestartHint={false}
        onDiscard={noop}
        onSave={noop}
      />,
    );
    expect(screen.queryByText(/必要な sidecar を再起動/)).not.toBeInTheDocument();
    expect(screen.getByText("保存して適用")).toBeInTheDocument();
  });

  it("disables save and explains an edited invalid form", () => {
    const onSave = vi.fn();
    renderWithProviders(
      <SettingsSaveBar
        dirtyCount={1}
        resetsVectordb={false}
        saving={false}
        saveDisabled
        saveDisabledReason="入力内容を確認してから保存してください。"
        onDiscard={noop}
        onSave={onSave}
      />,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("入力内容を確認してから保存してください。");
    const save = screen.getByRole("button", { name: "保存して適用 (再起動)" });
    expect(save).toBeDisabled();
    fireEvent.click(save);
    expect(onSave).not.toHaveBeenCalled();
  });
});
