import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type { AppSettingsResponse, DataRootValidation } from "@/types/api";
import { DataRootCard, HfHomeCard } from "./Settings";

const getAppSettings = vi.fn();
const setDataRoot = vi.fn();
const validateDataRoot = vi.fn();
const createDataRoot = vi.fn();
const openDialog = vi.fn();

vi.mock("@/api", () => ({
  // HfHomeCard now only reads getAppSettings (it reports its payload via
  // onDirtyChange instead of saving itself); DataRootCard still owns its
  // own save. Match the surface of the real module so neither card
  // observes an undefined export.
  getAppSettings: () => getAppSettings(),
  setDataRoot: (path: unknown) => setDataRoot(path),
  validateDataRoot: (path: unknown) => validateDataRoot(path),
  createDataRoot: (path: unknown) => createDataRoot(path),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (opts: unknown) => openDialog(opts),
}));

const APP_SETTINGS_DEFAULT: AppSettingsResponse = {
  hf_home_mode: "data_root",
  hf_home_path: null,
  data_root_override: null,
  resolved: {
    current_data_root: "/Users/u/Library/Application Support/lookback",
    default_data_root: "/Users/u/Library/Application Support/lookback",
    effective_hf_home: "/Users/u/Library/Application Support/lookback/models",
    pending_data_root: "/Users/u/Library/Application Support/lookback",
  },
};

const APP_SETTINGS_WITH_OVERRIDE: AppSettingsResponse = {
  hf_home_mode: "custom",
  hf_home_path: "/Volumes/Ext/hf",
  data_root_override: "/Volumes/Ext/lookback",
  resolved: {
    current_data_root: "/Users/u/Library/Application Support/lookback",
    default_data_root: "/Users/u/Library/Application Support/lookback",
    effective_hf_home: "/Volumes/Ext/hf",
    pending_data_root: "/Volumes/Ext/lookback",
  },
};

function renderUi(ui: ReactElement) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
}

describe("HfHomeCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getAppSettings.mockReset();
    openDialog.mockReset();
  });

  // The card now lifts its save payload up via onDirtyChange instead of
  // saving itself. `resetSignal=0` means "no discard requested".
  function renderHf(
    onDirtyChange = vi.fn(),
    props: Partial<React.ComponentProps<typeof HfHomeCard>> = {},
  ) {
    renderUi(<HfHomeCard onDirtyChange={onDirtyChange} resetSignal={0} {...props} />);
    return onDirtyChange;
  }

  it("seeds the mode from the loaded settings and shows the effective path", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    renderHf();
    // The effective-path read-only field reflects what the sidecar is
    // actually populating — this is the load-bearing invariant for the
    // "preparing vs ready" badge in the Models card.
    await screen.findByDisplayValue("/Users/u/Library/Application Support/lookback/models");
    // App data dir 配下 button is the active segment when mode=data_root.
    expect(screen.getByText("App data dir 配下").className).toContain("active");
  });

  it("reports a null payload while the card is unchanged", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    const onDirty = renderHf();
    await screen.findByDisplayValue("/Users/u/Library/Application Support/lookback/models");
    // After the seed effect settles, the most recent report must be null
    // (clean) — the save bar must not count an untouched card.
    await waitFor(() => expect(onDirty).toHaveBeenCalled());
    expect(onDirty.mock.lastCall?.[0]).toBeNull();
  });

  it("reports the chosen mode and (for custom) the path as a payload", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    const onDirty = renderHf();
    await screen.findByDisplayValue("/Users/u/Library/Application Support/lookback/models");
    fireEvent.click(screen.getByText("カスタムパス"));
    const pathInput = await screen.findByPlaceholderText("/Volumes/Ext/hf");
    fireEvent.change(pathInput, { target: { value: "/Volumes/Ext/hf" } });
    await waitFor(() =>
      expect(onDirty.mock.lastCall?.[0]).toEqual({ mode: "custom", path: "/Volumes/Ext/hf" }),
    );
  });

  it("reports null while the custom path is empty (cannot apply)", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    const onDirty = renderHf();
    await screen.findByDisplayValue("/Users/u/Library/Application Support/lookback/models");
    fireEvent.click(screen.getByText("カスタムパス"));
    // Custom selected but path still empty → incomplete → null payload.
    await waitFor(() => expect(onDirty.mock.lastCall?.[0]).toBeNull());
    expect(screen.getByText("カスタムパスを入力してください。")).toBeInTheDocument();
  });

  it("reports path:null when the chosen mode is not custom", async () => {
    // Regression: an earlier draft sent `path: ""` for non-custom modes,
    // which the backend rejected as "empty path".
    getAppSettings.mockResolvedValue(APP_SETTINGS_WITH_OVERRIDE);
    const onDirty = renderHf();
    // Seeded with custom + /Volumes/Ext/hf. Switch back to data_root.
    await screen.findByDisplayValue("/Volumes/Ext/hf");
    fireEvent.click(screen.getByText("App data dir 配下"));
    await waitFor(() =>
      expect(onDirty.mock.lastCall?.[0]).toEqual({ mode: "data_root", path: null }),
    );
  });

  it("previews the save-time HF_HOME under a pending setup data root", async () => {
    getAppSettings.mockResolvedValue({
      ...APP_SETTINGS_DEFAULT,
      hf_home_mode: "global",
      resolved: {
        ...APP_SETTINGS_DEFAULT.resolved,
        effective_hf_home: "/Users/u/.cache/huggingface",
      },
    });
    renderHf(vi.fn(), { previewDataRoot: "/Volumes/Ext/lookback" });
    await screen.findByDisplayValue("/Users/u/.cache/huggingface");

    fireEvent.click(screen.getByText("App data dir 配下"));

    await screen.findByDisplayValue("/Volumes/Ext/lookback/models");
    expect(screen.getByText("保存後の実効パス")).toBeInTheDocument();
  });
});

describe("DataRootCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getAppSettings.mockReset();
    setDataRoot.mockReset();
    validateDataRoot.mockReset();
    createDataRoot.mockReset();
    openDialog.mockReset();
    setDataRoot.mockResolvedValue(undefined);
    createDataRoot.mockResolvedValue(undefined);
  });

  function renderCard() {
    return renderUi(
      <DataRootCard
        sqlitePath="/Users/u/Library/Application Support/lookback/db/jobworkerp.sqlite3"
        lancedbPath="/Users/u/Library/Application Support/lookback/lancedb"
        pluginsPath="/Users/u/Library/Application Support/lookback/plugins"
        logPath="/Users/u/Library/Application Support/lookback/log"
      />,
    );
  }

  it("uses the persisted override as the default input value", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_WITH_OVERRIDE);
    renderCard();
    await screen.findByDisplayValue("/Volumes/Ext/lookback");
  });

  it("debounces validation and surfaces the message", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    const validation: DataRootValidation = {
      ok: false,
      writable: false,
      is_existing_lookback_root: false,
      creatable: false,
      message: "settings.dataRoot.validation.notExist",
    };
    validateDataRoot.mockResolvedValue(validation);
    renderCard();
    // The editable App data dir input is the only one with a placeholder
    // (the read-only sub-paths use SettingRow which sets `value`, never
    // `placeholder`). Wait for the seed effect to set the placeholder
    // from `resolved.default_data_root`, then operate on that input.
    const input = (await screen.findByPlaceholderText(
      "/Users/u/Library/Application Support/lookback",
    )) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "/tmp/does-not-exist" } });
    await screen.findByText("ディレクトリが存在しません", undefined, { timeout: 2000 });
  });

  it("disables Save while the validation flagged the path as invalid", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    validateDataRoot.mockResolvedValue({
      ok: false,
      writable: false,
      is_existing_lookback_root: false,
      creatable: false,
      message: "settings.dataRoot.validation.notWritable",
    });
    renderCard();
    const input = (await screen.findByPlaceholderText(
      "/Users/u/Library/Application Support/lookback",
    )) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "/protected" } });
    await screen.findByText("書込権限がありません");
    const save = screen.getByText("保存") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
  });

  it("sends null when the user clears the override (reset to default)", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_WITH_OVERRIDE);
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
    try {
      renderCard();
      const input = (await screen.findByDisplayValue("/Volumes/Ext/lookback")) as HTMLInputElement;
      fireEvent.change(input, { target: { value: "" } });
      // Empty input means "use default" — Save sends null. validateDataRoot
      // must NOT be called for an empty string (would surface a confusing
      // "パスが空です" while the intent is the opposite).
      fireEvent.click(screen.getByText("保存"));
      await waitFor(() => expect(setDataRoot).toHaveBeenCalled());
      expect(setDataRoot.mock.lastCall?.[0]).toBeNull();
    } finally {
      confirmSpy.mockRestore();
    }
  });

  it("offers a create button only when validation reports creatable", async () => {
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    validateDataRoot.mockResolvedValue({
      ok: false,
      writable: false,
      is_existing_lookback_root: false,
      creatable: true,
      message: "settings.dataRoot.validation.notExist",
    });
    renderCard();
    const input = (await screen.findByPlaceholderText(
      "/Users/u/Library/Application Support/lookback",
    )) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "/tmp/lookback-new" } });
    // The button is gated on `creatable`, not on the bare "missing path"
    // outcome — pins the contract the backend's parent-dir guard relies on.
    await screen.findByText("このディレクトリを作成");
  });

  it("calls createDataRoot and unblocks Save without re-invoking validateDataRoot", async () => {
    // The pre-create debounce produces a `creatable=true` result. After
    // `createDataRoot` succeeds the UI knows the directory is fresh and
    // writable, so it sets `ok=true` directly instead of paying for
    // another Tauri IPC round trip + write probe. This pins the
    // optimisation: a second `validateDataRoot` call here would be wasted
    // work.
    getAppSettings.mockResolvedValue(APP_SETTINGS_DEFAULT);
    validateDataRoot.mockResolvedValue({
      ok: false,
      writable: false,
      is_existing_lookback_root: false,
      creatable: true,
      message: "settings.dataRoot.validation.notExist",
    });
    renderCard();
    const input = (await screen.findByPlaceholderText(
      "/Users/u/Library/Application Support/lookback",
    )) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "/tmp/lookback-new" } });
    fireEvent.click(await screen.findByText("このディレクトリを作成"));
    await waitFor(() => expect(createDataRoot).toHaveBeenCalledWith("/tmp/lookback-new"));
    // Create button disappears (validation now `ok=true`) and Save unblocks.
    await waitFor(() => expect(screen.queryByText("このディレクトリを作成")).toBeNull());
    const save = screen.getByText("保存") as HTMLButtonElement;
    expect(save.disabled).toBe(false);
    // Exactly one validate invocation — the pre-create debounce. The
    // post-create state is computed locally, NOT via a second IPC call.
    expect(validateDataRoot).toHaveBeenCalledTimes(1);
  });
});
