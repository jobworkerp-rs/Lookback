import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type { AppSettingsResponse, SetTimezoneRequest } from "@/types/api";
import { TimezoneCard } from "./TimezoneCard";

const getAppSettings = vi.fn();
const listTimezones = vi.fn();

vi.mock("@/api", () => ({
  getAppSettings: () => getAppSettings(),
  listTimezones: () => listTimezones(),
  // Extra Settings APIs are harmless here; keep the mock shape broad so this
  // focused test stays isolated from Settings imports when components move.
  applySettings: () => Promise.resolve({ restarted: true, backup_path: null }),
  createDataRoot: () => Promise.resolve(),
  getConnectionConfig: () => Promise.resolve(null),
  getEmbeddingSettings: () => Promise.resolve(null),
  getLlmSettings: () => Promise.resolve(null),
  getMcpSettings: () => Promise.resolve(null),
  getMemoryEmbeddingStats: () => Promise.resolve(null),
  getModelStatus: () => Promise.resolve({ llm: null, embedding: null }),
  getSettings: () => Promise.resolve(null),
  listEmbeddingPresets: () => Promise.resolve([]),
  listLlmPresets: () => Promise.resolve([]),
  purgeAllData: () => Promise.resolve({ warnings: [] }),
  readSidecarLog: () =>
    Promise.resolve({ file_name: "", content: "", truncated: false, file_size: 0 }),
  redispatchMemoryEmbeddings: () => Promise.resolve(null),
  retryModelSetup: () => Promise.resolve(),
  setConnectionConfig: () => Promise.resolve(),
  setDataRoot: () => Promise.resolve(),
  validateDataRoot: () => Promise.resolve(null),
}));

function appSettings(timezone: string | null): AppSettingsResponse {
  return {
    hf_home_mode: "data_root",
    hf_home_path: null,
    data_root_override: null,
    timezone,
    effective_timezone: timezone ?? "Asia/Tokyo",
    resolved: {
      current_data_root: "/tmp/lookback",
      default_data_root: "/tmp/lookback",
      effective_hf_home: "/tmp/lookback/models",
      pending_data_root: "/tmp/lookback",
    },
  };
}

const ZONES = ["Asia/Tokyo", "Europe/Paris", "UTC"];

function renderCard(onDirtyChange = vi.fn(), resetSignal = 0, disabled = false) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrap = (signal: number): ReactElement => (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <TimezoneCard onDirtyChange={onDirtyChange} resetSignal={signal} disabled={disabled} />
      </QueryClientProvider>
    </I18nextProvider>
  );
  const utils = render(wrap(resetSignal));
  // Re-render keeps the SAME QueryClient so the app-settings cache survives
  // (a fresh client would drop the seed and the select would go blank).
  const rerenderSignal = (signal: number) => utils.rerender(wrap(signal));
  return { onDirtyChange, rerenderSignal, ...utils };
}

function lastPayload(onDirty: ReturnType<typeof vi.fn>): SetTimezoneRequest | null {
  return onDirty.mock.lastCall?.[0] ?? null;
}
function lastEdited(onDirty: ReturnType<typeof vi.fn>): boolean {
  return onDirty.mock.lastCall?.[1] ?? false;
}

describe("TimezoneCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getAppSettings.mockReset();
    listTimezones.mockReset();
    listTimezones.mockResolvedValue(ZONES);
  });

  it("reports null payload and edited=false when the selection matches persisted (Auto)", async () => {
    getAppSettings.mockResolvedValue(appSettings(null));
    const { onDirtyChange } = renderCard();
    await waitFor(() => expect(screen.getByRole("combobox")).toBeInTheDocument());
    // Seeded to Auto == persisted null → clean.
    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(false));
    expect(lastPayload(onDirtyChange)).toBeNull();
  });

  it("reports the picked zone when a zone is selected from Auto", async () => {
    getAppSettings.mockResolvedValue(appSettings(null));
    const { onDirtyChange } = renderCard();
    const select = await screen.findByRole("combobox");
    await waitFor(() =>
      expect(screen.getByRole("option", { name: "Europe/Paris" })).toBeInTheDocument(),
    );

    fireEvent.change(select, { target: { value: "Europe/Paris" } });

    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(true));
    expect(lastPayload(onDirtyChange)).toEqual({ timezone: "Europe/Paris" });
  });

  it("disables edits and reports clean while remote connection mode is active", async () => {
    getAppSettings.mockResolvedValue(appSettings(null));
    const { onDirtyChange } = renderCard(vi.fn(), 0, true);
    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe(""));

    expect(select).toBeDisabled();
    expect(screen.getByText(i18n.t("settings.timezone.remoteDisabled"))).toBeInTheDocument();
    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(false));
    expect(lastPayload(onDirtyChange)).toBeNull();
  });

  it("reports a null timezone (Auto) when switching back from a persisted explicit zone", async () => {
    getAppSettings.mockResolvedValue(appSettings("Europe/Paris"));
    const { onDirtyChange } = renderCard();
    const select = await screen.findByRole("combobox");
    // Seeded to the persisted explicit zone → clean initially.
    await waitFor(() => expect((select as HTMLSelectElement).value).toBe("Europe/Paris"));

    // Select the "Auto" option (empty value).
    fireEvent.change(select, { target: { value: "" } });

    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(true));
    expect(lastPayload(onDirtyChange)).toEqual({ timezone: null });
  });

  it("re-seeds and clears dirty on resetSignal", async () => {
    getAppSettings.mockResolvedValue(appSettings(null));
    const onDirtyChange = vi.fn();
    const { rerenderSignal } = renderCard(onDirtyChange, 0);
    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    // Wait for the app-settings seed (select seeded to Auto == "") before
    // editing, so the change is measured against the persisted value.
    await waitFor(() => expect(select.value).toBe(""));
    fireEvent.change(select, { target: { value: "UTC" } });
    await waitFor(() => expect(select.value).toBe("UTC"));
    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(true));

    // Bumping resetSignal must re-seed the select back to persisted (Auto).
    rerenderSignal(1);
    await waitFor(() => expect(select.value).toBe(""));
    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(false));
  });

  it("keeps a persisted zone selectable even when it is missing from the host list", async () => {
    getAppSettings.mockResolvedValue(appSettings("Etc/Custom"));
    listTimezones.mockResolvedValue(["Asia/Tokyo"]);
    renderCard();

    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe("Etc/Custom"));
    expect(screen.getByRole("option", { name: "Etc/Custom" })).toBeInTheDocument();
  });

  it("allows manual timezone entry when the host timezone list is empty", async () => {
    getAppSettings.mockResolvedValue(appSettings(null));
    listTimezones.mockResolvedValue([]);
    const { onDirtyChange } = renderCard();
    const input = await screen.findByPlaceholderText("Asia/Tokyo");
    await screen.findByText("Asia/Tokyo");

    fireEvent.change(input, { target: { value: "America/New_York" } });

    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(true));
    expect(lastPayload(onDirtyChange)).toEqual({ timezone: "America/New_York" });
  });

  it("maps a cleared manual timezone back to Auto", async () => {
    getAppSettings.mockResolvedValue(appSettings("America/New_York"));
    listTimezones.mockResolvedValue([]);
    const { onDirtyChange } = renderCard();
    const input = (await screen.findByPlaceholderText("Asia/Tokyo")) as HTMLInputElement;
    await waitFor(() => expect(input.value).toBe("America/New_York"));

    fireEvent.change(input, { target: { value: "" } });

    await waitFor(() => expect(lastEdited(onDirtyChange)).toBe(true));
    expect(lastPayload(onDirtyChange)).toEqual({ timezone: null });
  });

  it("renders the localized title in ja and en", async () => {
    getAppSettings.mockResolvedValue(appSettings(null));
    renderCard();
    // Title and label both read "タイムゾーン"; scope to the card title node.
    await waitFor(() =>
      expect(document.querySelector(".settings-card-title")?.textContent).toBe("タイムゾーン"),
    );

    await i18n.changeLanguage("en");
    renderCard();
    await waitFor(() =>
      expect(
        Array.from(document.querySelectorAll(".settings-card-title")).some(
          (el) => el.textContent === "Timezone",
        ),
      ).toBe(true),
    );
  });
});
