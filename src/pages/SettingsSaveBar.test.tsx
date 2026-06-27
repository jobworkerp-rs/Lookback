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

  it("always promises a sidecar restart", () => {
    // Every save scope (LLM / embedding / HF_HOME) restarts the sidecar. The
    // backend may hot-reload some External-only swaps in place, but the bar
    // can't tell which, so it always shows the restart copy (harmless
    // over-warning, never a false "no restart").
    renderWithProviders(
      <SettingsSaveBar
        dirtyCount={1}
        resetsVectordb={false}
        saving={false}
        onDiscard={noop}
        onSave={noop}
      />,
    );
    expect(screen.getByText("保存すると sidecar が再起動されます。")).toBeInTheDocument();
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
});
