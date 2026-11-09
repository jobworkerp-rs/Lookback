import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type { AppSettingsResponse, MigrationBackupSettings } from "@/types/api";
import { MigrationBackupCard } from "./MigrationBackupCard";

const getAppSettings = vi.fn();

vi.mock("@/api", () => ({
  getAppSettings: () => getAppSettings(),
}));

function appSettings(migration_backup: MigrationBackupSettings): AppSettingsResponse {
  return {
    hf_home_mode: "data_root",
    hf_home_path: null,
    data_root_override: null,
    timezone: null,
    effective_timezone: "Asia/Tokyo",
    migration_backup,
    resolved: {
      current_data_root: "/tmp/lookback",
      default_data_root: "/tmp/lookback",
      effective_hf_home: "/tmp/lookback/models",
      pending_data_root: "/tmp/lookback",
    },
  };
}

function renderCard(onDirtyChange = vi.fn(), resetSignal = 0) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrap = (signal: number): ReactElement => (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <MigrationBackupCard onDirtyChange={onDirtyChange} resetSignal={signal} />
      </QueryClientProvider>
    </I18nextProvider>
  );
  const utils = render(wrap(resetSignal));
  const rerenderSignal = (signal: number) => utils.rerender(wrap(signal));
  return { onDirtyChange, rerenderSignal, ...utils };
}

const lastCall = (onDirty: ReturnType<typeof vi.fn>) =>
  (onDirty.mock.lastCall ?? [null, false]) as [MigrationBackupSettings | null, boolean];

beforeEach(() => {
  i18n.changeLanguage("ja");
  getAppSettings.mockReset();
  getAppSettings.mockResolvedValue(appSettings({ enabled: true, keep: 1 }));
});

describe("MigrationBackupCard", () => {
  it("seeds from the saved setting and reports no change", async () => {
    const { onDirtyChange } = renderCard();

    const toggle = await screen.findByRole("checkbox", { name: /移行前にバックアップを作成/ });
    await waitFor(() => expect(toggle).toBeChecked());
    expect(screen.getByLabelText("保持するバックアップ数")).toHaveValue(1);
    expect(lastCall(onDirtyChange)).toEqual([null, false]);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("warns and reports a payload when the backup is turned off", async () => {
    const { onDirtyChange } = renderCard();
    const toggle = await screen.findByRole("checkbox", { name: /移行前にバックアップを作成/ });
    await waitFor(() => expect(toggle).toBeChecked());

    fireEvent.click(toggle);

    expect(screen.getByRole("alert")).toHaveTextContent(/復元できません/);
    expect(screen.getByLabelText("保持するバックアップ数")).toBeDisabled();
    expect(lastCall(onDirtyChange)).toEqual([{ enabled: false, keep: 1 }, true]);
  });

  it("accepts zero kept backups and rejects invalid counts without a payload", async () => {
    const { onDirtyChange } = renderCard();
    const keep = await screen.findByLabelText("保持するバックアップ数");
    await waitFor(() => expect(keep).toHaveValue(1));

    fireEvent.change(keep, { target: { value: "0" } });
    expect(lastCall(onDirtyChange)).toEqual([{ enabled: true, keep: 0 }, true]);

    fireEvent.change(keep, { target: { value: "-1" } });
    expect(lastCall(onDirtyChange)).toEqual([null, true]);
    expect(screen.getByText(/0 以上 100 以下の整数/)).toBeInTheDocument();

    fireEvent.change(keep, { target: { value: "1.5" } });
    expect(lastCall(onDirtyChange)).toEqual([null, true]);
  });

  it("resets to the saved value on discard", async () => {
    const { onDirtyChange, rerenderSignal } = renderCard();
    const keep = await screen.findByLabelText("保持するバックアップ数");
    await waitFor(() => expect(keep).toHaveValue(1));
    fireEvent.change(keep, { target: { value: "3" } });
    expect(lastCall(onDirtyChange)).toEqual([{ enabled: true, keep: 3 }, true]);

    rerenderSignal(1);

    await waitFor(() => expect(keep).toHaveValue(1));
    expect(lastCall(onDirtyChange)).toEqual([null, false]);
  });
});
