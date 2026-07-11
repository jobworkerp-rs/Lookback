import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { type EmbeddingFocus, Settings } from "./Settings";

// Settings renders every card, so the whole @/api surface is mocked. The
// view-switch tests only care about which cards are visible and the
// in-component leave-guard, so most mocks return innocuous defaults.
const applySettings = vi.fn();
const getLlmSettings = vi.fn();
const listLlmPresets = vi.fn();
const getEmbeddingSettings = vi.fn();
const listEmbeddingPresets = vi.fn();
const getAppSettings = vi.fn();
const getConnectionConfig = vi.fn();
const getMemoryEmbeddingStats = vi.fn();
const getBackgroundJobQueueStatus = vi.fn();
const getModelStatus = vi.fn();
const getSettings = vi.fn();
const getMcpSettings = vi.fn();

vi.mock("@/api", () => ({
  applySettings: (req: unknown) => applySettings(req),
  getLlmSettings: () => getLlmSettings(),
  listLlmPresets: () => listLlmPresets(),
  getEmbeddingSettings: () => getEmbeddingSettings(),
  listEmbeddingPresets: () => listEmbeddingPresets(),
  getMcpSettings: () => getMcpSettings(),
  getAppSettings: () => getAppSettings(),
  getConnectionConfig: () => getConnectionConfig(),
  getMemoryEmbeddingStats: () => getMemoryEmbeddingStats(),
  getBackgroundJobQueueStatus: () => getBackgroundJobQueueStatus(),
  getModelStatus: () => getModelStatus(),
  getSettings: () => getSettings(),
  setConnectionConfig: () => Promise.resolve(),
  setDataRoot: () => Promise.resolve(),
  createDataRoot: () => Promise.resolve(),
  validateDataRoot: () => Promise.resolve(null),
  redispatchMemoryEmbeddings: () => Promise.resolve(null),
  retryModelSetup: () => Promise.resolve(),
  listTimezones: () => Promise.resolve(["Asia/Tokyo", "America/New_York", "UTC"]),
  purgeAllData: () => Promise.resolve({ warnings: [] }),
  readSidecarLog: () =>
    Promise.resolve({ file_name: "", content: "", truncated: false, file_size: 0 }),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: () => Promise.resolve(null) }));

const LLM_SETTINGS = {
  mode: "local" as const,
  provider_model: null,
  api_key_set: false,
  base_url: null,
  max_tokens: null,
  temperature: null,
  local_preset_id: null,
  local_model_file: null,
  local_hf_repo: null,
  local_ctx_size: null,
  local_kv_cache_type: null,
};

const LLM_PRESETS = [
  {
    id: "gemma-4-e2b-it-qat-ud-q4-k-xl",
    display_name: "Gemma 4 E2B IT QAT",
    hf_repo: "unsloth/gemma-4-E2B-it-qat-GGUF",
    gguf_file: "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf",
    recommended_ctx_size: 131072,
    min_ctx_size: 2048,
    estimated_model_ram_gb: 3.5,
    estimated_ram_gb: 5,
    kv_layers: 35,
    kv_embd_k_gqa: 256,
    kv_embd_v_gqa: 256,
    description: "デフォルト",
    thinking_kwarg: "enable" as const,
    mtp_enabled: false,
    mtp_draft_model: null,
  },
  {
    id: "qwen3-5-9b-ud-q4-k-xl",
    display_name: "Qwen3.5 9B",
    hf_repo: "unsloth/Qwen3.5-9B-GGUF",
    gguf_file: "Qwen3.5-9B-UD-Q4_K_XL.gguf",
    recommended_ctx_size: 262144,
    min_ctx_size: 2048,
    estimated_model_ram_gb: 6,
    estimated_ram_gb: 10,
    kv_layers: 36,
    kv_embd_k_gqa: 1024,
    kv_embd_v_gqa: 1024,
    description: "軽量",
    thinking_kwarg: "disable" as const,
    mtp_enabled: false,
    mtp_draft_model: null,
  },
];

const EMB_SETTINGS = {
  preset_id: null,
  custom_model_id: null,
  custom_tokenizer_id: null,
  custom_vector_size: null,
  custom_dtype: null,
  custom_max_sequence_length: null,
  custom_is_multimodal: null,
  effective: {
    model_id: "Qwen/Qwen3-VL-Embedding-2B",
    tokenizer_id: null,
    vector_size: 2048,
    dtype: "F16",
    max_sequence_length: 8192,
    is_multimodal: true,
  },
  connection_remote: false,
};

const EMB_PRESETS = [
  {
    id: "qwen3-vl-embedding-2b",
    display_name: "Qwen3-VL-Embedding 2B",
    hf_repo: "Qwen/Qwen3-VL-Embedding-2B",
    tokenizer_hf_repo: null,
    vector_size: 2048,
    dtype: "F16",
    max_sequence_length: 8192,
    is_multimodal: true,
    estimated_ram_gb: 6,
    description: "default",
  },
];

const APP_SETTINGS = {
  hf_home_mode: "global" as const,
  hf_home_path: null,
  data_root_override: null,
  timezone: null,
  effective_timezone: "Asia/Tokyo",
  resolved: {
    current_data_root: "/root",
    default_data_root: "/root",
    effective_hf_home: "/hf",
    pending_data_root: "/root",
  },
};

const SETTINGS_SNAPSHOT = {
  data_root: "/root",
  sqlite_path: "/root/db.sqlite",
  lancedb_path: "/root/lancedb",
  plugins_path: "/root/plugins",
  models_path: "/root/models",
  log_path: "/root/log",
  jobworkerp_url: "http://127.0.0.1:9000",
  memories_url: "http://127.0.0.1:9010",
};

function renderSettings(props?: Parameters<typeof Settings>[0]) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <Settings {...props} />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

describe("Settings view switching", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    applySettings.mockReset();
    applySettings.mockResolvedValue({ restarted: true, backup_path: null });
    getLlmSettings.mockResolvedValue(LLM_SETTINGS);
    listLlmPresets.mockResolvedValue(LLM_PRESETS);
    getEmbeddingSettings.mockResolvedValue(EMB_SETTINGS);
    listEmbeddingPresets.mockResolvedValue(EMB_PRESETS);
    getMcpSettings.mockResolvedValue({
      enabled: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      set_name: "lookback-mcp-rag",
      active_port: null,
    });
    getAppSettings.mockResolvedValue(APP_SETTINGS);
    getConnectionConfig.mockResolvedValue({
      mode: "local",
      remote_jobworkerp_url: null,
      remote_memories_url: null,
    });
    getMemoryEmbeddingStats.mockResolvedValue({
      total_records: 0,
      records_with_embedding: 0,
      records_without_embedding: 0,
      vector_dimension: 1024,
    });
    getBackgroundJobQueueStatus.mockResolvedValue({ active: false, rows: [] });
    // Both models report a concrete state; the production refetchInterval
    // reads `d.llm.state` / `d.embedding.state` and would NPE on null sides.
    getModelStatus.mockResolvedValue({
      llm: { name: "llm", repo: "org/llm", state: "ready", error: null },
      embedding: { name: "emb", repo: "org/emb", state: "ready", error: null },
    });
    getSettings.mockResolvedValue(SETTINGS_SNAPSHOT);
  });

  it("shows only the basic cards by default", async () => {
    renderSettings();
    // Basic-view cards.
    await screen.findByText("LLM Provider");
    expect(screen.getByText("Embedding index")).toBeInTheDocument();
    expect(screen.getByText("Hugging Face cache (HF_HOME)")).toBeInTheDocument();
    expect(screen.getByText("Logs")).toBeInTheDocument();
    // Advanced-view cards hidden.
    expect(screen.queryByText("Connection")).toBeNull();
    expect(screen.queryByText("Embedding model")).toBeNull();
    expect(screen.queryByText("Data location")).toBeNull();
    expect(screen.queryByText("Destructive")).toBeNull();
    // Entry button present.
    expect(screen.getByText("高度な設定を開く…")).toBeInTheDocument();
  });

  it("disables timezone edits when the saved connection mode is remote", async () => {
    getConnectionConfig.mockResolvedValue({
      mode: "remote",
      remote_jobworkerp_url: "http://10.0.0.2:9000",
      remote_memories_url: "http://10.0.0.2:9010",
    });

    renderSettings();

    const help = await screen.findByText(i18n.t("settings.timezone.remoteDisabled"));
    const card = help.closest(".settings-card");
    const select = card?.querySelector("select");
    expect(select).toBeDisabled();
  });

  it("switches to the advanced view and back", async () => {
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    // Advanced cards now visible, LLM Provider gone.
    await screen.findByText("Connection");
    expect(screen.getByText("Embedding model")).toBeInTheDocument();
    expect(screen.getByText("Data location")).toBeInTheDocument();
    expect(screen.getByText("Destructive")).toBeInTheDocument();
    expect(screen.queryByText("LLM Provider")).toBeNull();
    // Back button returns to basic.
    fireEvent.click(screen.getByText("← 設定に戻る"));
    await screen.findByText("LLM Provider");
    expect(screen.queryByText("Connection")).toBeNull();
  });

  it("intercepts the view switch when the basic view has unsaved changes", async () => {
    renderSettings();
    // Dirty the HF_HOME card (basic view): switch mode to data_root.
    await screen.findByText("LLM Provider");
    // Wait for HF_HOME settings to load (effective path field) before
    // editing — otherwise the seed effect re-runs after the click and
    // reverts the mode.
    await screen.findByDisplayValue("/hf");
    fireEvent.click(screen.getByText("App data dir 配下"));
    // The basic save bar appears once dirty.
    await screen.findByText(/未保存の変更があります/);
    // Attempt to open advanced → confirm dialog, still on basic.
    fireEvent.click(screen.getByText("高度な設定を開く…"));
    expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();
    expect(screen.getByText("LLM Provider")).toBeInTheDocument();
    // キャンセル keeps us on basic with the edit intact.
    fireEvent.click(screen.getByText("キャンセル"));
    expect(screen.queryByText("保存していない変更があります")).toBeNull();
    expect(screen.getByText("LLM Provider")).toBeInTheDocument();
    expect(screen.getByText(/未保存の変更があります/)).toBeInTheDocument();
  });

  it("renders the save bar OUTSIDE the scroll container so it can't scroll off-screen", async () => {
    // Regression: the save bar used to live at the tail of `.content` (the
    // scroll area) with `position: sticky; bottom: 0`, so editing a card at
    // the top of a long page hid the save action off-screen at the bottom
    // until the user scrolled all the way down. It must instead be a sibling
    // of `.content` (a flex child of `.main`) so it is pinned to the foot of
    // the viewport regardless of scroll position. jsdom can't observe sticky
    // layout, but it CAN assert the bar is not inside the scroll subtree.
    const { container } = renderSettings();
    await screen.findByText("LLM Provider");
    await screen.findByDisplayValue("/hf");
    // Dirty a basic-view card so the save bar renders.
    fireEvent.click(screen.getByText("App data dir 配下"));
    const saveBar = await screen.findByText(/未保存の変更があります/);
    const bar = saveBar.closest(".settings-save-bar");
    expect(bar).not.toBeNull();
    const scrollArea = container.querySelector(".content");
    expect(scrollArea).not.toBeNull();
    expect(scrollArea?.contains(bar)).toBe(false);
  });

  it("discards and switches on 破棄して移動", async () => {
    renderSettings();
    await screen.findByText("LLM Provider");
    // Wait for HF_HOME settings to load (effective path field) before
    // editing — otherwise the seed effect re-runs after the click and
    // reverts the mode.
    await screen.findByDisplayValue("/hf");
    fireEvent.click(screen.getByText("App data dir 配下"));
    await screen.findByText(/未保存の変更があります/);
    fireEvent.click(screen.getByText("高度な設定を開く…"));
    fireEvent.click(screen.getByText("破棄して移動"));
    // Now on advanced view.
    await screen.findByText("Connection");
    // Going back to basic, the HF_HOME edit was discarded (save bar gone).
    fireEvent.click(screen.getByText("← 設定に戻る"));
    await screen.findByText("LLM Provider");
    expect(screen.queryByText(/未保存の変更があります/)).toBeNull();
  });

  it("guards the view switch when an edited form is invalid (no save bar)", async () => {
    // Finding 3: an edited-but-invalid form (External with an empty model
    // name) reports edited=true / payload=null. The save bar stays hidden
    // (nothing saveable) but the leave-guard MUST still intercept the
    // switch so the in-progress input isn't dropped silently.
    getLlmSettings.mockResolvedValue({
      ...LLM_SETTINGS,
      mode: "external",
      provider_model: "gpt-4o",
    });
    renderSettings();
    const modelInput = await screen.findByDisplayValue("gpt-4o");
    // Clear the model name → invalid (externalIncomplete) but edited.
    fireEvent.change(modelInput, { target: { value: "" } });
    // No save bar (not saveable)…
    expect(screen.queryByText(/未保存の変更があります/)).toBeNull();
    // …but switching views is still intercepted.
    fireEvent.click(screen.getByText("高度な設定を開く…"));
    expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();
    expect(screen.getByText("LLM Provider")).toBeInTheDocument();
  });

  it("applies only the advanced scope (embedding) on the advanced save bar", async () => {
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Embedding model");
    // Wait for the embedding card to seed (effective row) before editing.
    await screen.findByText(/現在の設定/);
    // Change the embedding preset to dirty the advanced view.
    const select = screen
      .getByText("Embedding model")
      .closest(".settings-card")
      ?.querySelector("select") as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("org/name"), {
      target: { value: "org/model" },
    });
    fireEvent.change(screen.getByPlaceholderText("e.g. 1024"), { target: { value: "1024" } });
    // The advanced save bar appears; click it.
    await screen.findByText(/未保存の変更があります/);
    fireEvent.click(screen.getByText("保存して適用 (再起動)"));
    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    const req = applySettings.mock.lastCall?.[0];
    // Only the embedding scope is sent; llm / hf_home are null.
    expect(req.llm).toBeNull();
    expect(req.hf_home).toBeNull();
    expect(req.embedding).toMatchObject({ preset_id: "custom", custom_model_id: "org/model" });
  });

  it("guards the view switch when the Connection card (advanced) is edited", async () => {
    // Regression: Connection / Data location are self-contained cards (own
    // Save button, not in the unified save bar). Their in-progress edits must
    // still arm the leave-guard, or switching back to basic drops the input
    // silently. Here: switch Connection to Remote, then try to leave.
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Connection");
    // Switch the connection mode → the form now differs from the persisted
    // (local) config, so the card is edited. The seed effect re-runs when the
    // config resolves and reverts the mode, so click inside waitFor until the
    // Remote URL inputs appear (mode stuck on remote past the last seed).
    await waitFor(() => {
      fireEvent.click(screen.getByText("Remote server"));
      expect(screen.getByPlaceholderText("http://host:9000")).toBeInTheDocument();
    });
    // No save bar (self-contained card), but the leave-guard must fire.
    expect(screen.queryByText(/未保存の変更があります/)).toBeNull();
    fireEvent.click(screen.getByText("← 設定に戻る"));
    expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();
    // Cancel keeps us on advanced with the edit intact.
    fireEvent.click(screen.getByText("キャンセル"));
    expect(screen.getByText("Connection")).toBeInTheDocument();
  });

  it("guards the view switch when the Data location card (advanced) is edited", async () => {
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Data location");
    const input = screen
      .getByText("Data location")
      .closest(".settings-card")
      ?.querySelector("input[type='text']") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "/custom/data/dir" } });
    // No save bar (self-contained card), but the leave-guard must fire.
    expect(screen.queryByText(/未保存の変更があります/)).toBeNull();
    fireEvent.click(screen.getByText("← 設定に戻る"));
    expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();
    fireEvent.click(screen.getByText("キャンセル"));
    expect(screen.getByText("Data location")).toBeInTheDocument();
  });

  it("releases the guard after the edited advanced card unmounts on discard", async () => {
    // After "破棄して移動" switches away, the self-contained card unmounts and
    // its cleanup must clear the parent's edited flag — otherwise the basic
    // view would stay falsely guarded.
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Connection");
    await waitFor(() => {
      fireEvent.click(screen.getByText("Remote server"));
      expect(screen.getByPlaceholderText("http://host:9000")).toBeInTheDocument();
    });
    fireEvent.click(screen.getByText("← 設定に戻る"));
    fireEvent.click(screen.getByText("破棄して移動"));
    // Back on basic. The discarded Connection edit must not keep the guard
    // armed: a subsequent (clean) switch to advanced is immediate, no dialog.
    await screen.findByText("LLM Provider");
    fireEvent.click(screen.getByText("高度な設定を開く…"));
    expect(screen.queryByText("保存していない変更があります")).toBeNull();
    await screen.findByText("Connection");
  });

  it("re-syncs the LLM preset dropdown after a save and stays clean", async () => {
    // Finding 4: changing the Local LLM preset then saving must leave the
    // dropdown on the NEW persisted value after the refetch — otherwise the
    // resetSignal (fired with stale data) reverts it and the card re-reports
    // dirty right after a successful save.
    renderSettings();
    // Wait for the preset dropdown to seed to the persisted default.
    const presetLabel = await screen.findByText("プリセット");
    const select = presetLabel
      .closest(".settings-row")
      ?.querySelector("select") as HTMLSelectElement;
    await waitFor(() => expect(select.options.length).toBeGreaterThan(1));
    await waitFor(() => expect(select.value).toBe("gemma-4-e2b-it-qat-ud-q4-k-xl"));
    // Switch to the second preset → dirty → basic save bar appears.
    fireEvent.change(select, { target: { value: "qwen3-5-9b-ud-q4-k-xl" } });
    await screen.findByText(/未保存の変更があります/);
    // After save, the server reports the new preset (simulating persistence).
    getLlmSettings.mockResolvedValue({ ...LLM_SETTINGS, local_preset_id: "qwen3-5-9b-ud-q4-k-xl" });
    // A Local model change restarts the sidecar (the GGUF loads in a fresh
    // child), so the bar shows the restart copy.
    fireEvent.click(screen.getByText("保存して適用 (再起動)"));
    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    // The save bar must disappear (no lingering dirty) and the dropdown
    // must reflect the saved preset.
    await waitFor(() => expect(screen.queryByText(/未保存の変更があります/)).toBeNull());
    expect(select.value).toBe("qwen3-5-9b-ud-q4-k-xl");
  });

  it("shows the restart copy when switching to External (not hot-reload)", async () => {
    // Regression for the P1 review finding: an External-side change is NOT
    // hot-reload safe (the API key / provider env reaches the running child
    // only at spawn). The save bar must therefore promise a restart — the
    // `basicHotReload` gate excludes any non-local mode.
    renderSettings();
    // Switch the LLM card to External API and give it a model name so the
    // payload is saveable (and thus the save bar renders). The card's seed
    // effect re-runs when the settings query resolves and reverts the mode,
    // so click inside waitFor until the External model input appears.
    await screen.findByText("External API");
    let modelInput!: HTMLElement;
    await waitFor(() => {
      fireEvent.click(screen.getByText("External API"));
      modelInput = screen.getByPlaceholderText(
        "gpt-4o / claude-sonnet-4-20250514 / gemini-2.5-flash",
      );
    });
    fireEvent.change(modelInput, { target: { value: "gpt-4o" } });
    await screen.findByText(/未保存の変更があります/);
    // Restart copy, NOT the hot-reload copy.
    expect(screen.getByText("保存して適用 (再起動)")).toBeInTheDocument();
    expect(screen.queryByText("保存して適用")).toBeNull();
    expect(
      screen.queryByText("保存すると新しい LLM モデルを読み込みます (sidecar の再起動は不要)。"),
    ).toBeNull();
  });

  it("shows the restart copy when switching External→Local", async () => {
    // A switch INTO Local restarts the sidecar so the GGUF loads in a fresh
    // child (an in-process Release→Load of the static Metal worker crashed
    // macOS). The bar therefore shows the restart copy.
    getLlmSettings.mockResolvedValue({
      ...LLM_SETTINGS,
      mode: "external",
      provider_model: "gpt-4o",
    });
    renderSettings();
    // Switch the LLM card to Local LLM. The seed effect re-runs when the
    // External settings resolve and reverts the mode, so click inside waitFor
    // until the Local preset dropdown appears.
    await screen.findByText("Local LLM");
    let select!: HTMLSelectElement;
    await waitFor(() => {
      fireEvent.click(screen.getByText("Local LLM"));
      select = screen
        .getByText("プリセット")
        .closest(".settings-row")
        ?.querySelector("select") as HTMLSelectElement;
      expect(select.options.length).toBeGreaterThan(1);
    });
    // Pick a preset so the Local payload is saveable (mode differs from the
    // persisted External, so the bar renders regardless).
    fireEvent.change(select, { target: { value: "qwen3-5-9b-ud-q4-k-xl" } });
    await screen.findByText(/未保存の変更があります/);
    // New mode is Local ⇒ restart copy.
    expect(screen.getByText("保存して適用 (再起動)")).toBeInTheDocument();
    expect(screen.queryByText("保存して適用")).toBeNull();
  });

  it("opens the advanced view and scrolls the embedding card into view on focus", async () => {
    // The banner CTA deep-links here via an `embeddingFocus` seed: the page
    // must flip to the advanced view (where the embedding model card lives)
    // and scroll it into view, then consume the seed exactly once.
    const scrollSpy = vi.fn();
    Element.prototype.scrollIntoView = scrollSpy;
    const onConsumed = vi.fn();
    renderSettings({ embeddingFocus: { nonce: 1 }, onEmbeddingFocusConsumed: onConsumed });

    // Advanced-only "Embedding model" card becomes visible.
    await screen.findByText("Embedding model");
    await waitFor(() => expect(scrollSpy).toHaveBeenCalled());
    expect(onConsumed).toHaveBeenCalledTimes(1);
  });

  it("keeps the scheduled embedding-card scroll after consuming the focus seed", async () => {
    const scrollSpy = vi.fn();
    Element.prototype.scrollIntoView = scrollSpy;
    let nextFrameId = 1;
    const frames = new Map<number, FrameRequestCallback>();
    const rafSpy = vi.spyOn(globalThis, "requestAnimationFrame").mockImplementation((cb) => {
      const id = nextFrameId++;
      frames.set(id, cb);
      return id;
    });
    const cancelSpy = vi.spyOn(globalThis, "cancelAnimationFrame").mockImplementation((id) => {
      frames.delete(id);
    });

    function FocusHost() {
      const [focus, setFocus] = useState<EmbeddingFocus | null>({
        nonce: 1,
      });
      return <Settings embeddingFocus={focus} onEmbeddingFocusConsumed={() => setFocus(null)} />;
    }

    try {
      const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      render(
        <I18nextProvider i18n={i18n}>
          <QueryClientProvider client={client}>
            <FocusHost />
          </QueryClientProvider>
        </I18nextProvider>,
      );
      await screen.findByText("Embedding model");
      expect(frames.size).toBe(1);

      act(() => {
        for (const cb of Array.from(frames.values())) cb(performance.now());
        frames.clear();
      });

      expect(scrollSpy).toHaveBeenCalledTimes(1);
    } finally {
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
    }
  });

  it("keeps the embedding-card focus seed while the dirty-view confirmation is pending", async () => {
    const scrollSpy = vi.fn();
    Element.prototype.scrollIntoView = scrollSpy;
    let nextFrameId = 1;
    const frames = new Map<number, FrameRequestCallback>();
    const rafSpy = vi.spyOn(globalThis, "requestAnimationFrame").mockImplementation((cb) => {
      const id = nextFrameId++;
      frames.set(id, cb);
      return id;
    });
    const cancelSpy = vi.spyOn(globalThis, "cancelAnimationFrame").mockImplementation((id) => {
      frames.delete(id);
    });

    function FocusHost() {
      const [focus, setFocus] = useState<EmbeddingFocus | null>(null);
      return (
        <>
          <button type="button" onClick={() => setFocus({ nonce: 1 })}>
            focus embedding
          </button>
          <Settings embeddingFocus={focus} onEmbeddingFocusConsumed={() => setFocus(null)} />
        </>
      );
    }

    try {
      const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      render(
        <I18nextProvider i18n={i18n}>
          <QueryClientProvider client={client}>
            <FocusHost />
          </QueryClientProvider>
        </I18nextProvider>,
      );

      await screen.findByText("LLM Provider");
      await screen.findByDisplayValue("/hf");
      fireEvent.click(screen.getByText("App data dir 配下"));
      await screen.findByText(/未保存の変更があります/);

      fireEvent.click(screen.getByText("focus embedding"));
      expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();

      act(() => {
        for (const cb of Array.from(frames.values())) cb(performance.now());
        frames.clear();
      });
      expect(scrollSpy).not.toHaveBeenCalled();

      fireEvent.click(screen.getByText("破棄して移動"));
      await screen.findByText("Embedding model");
      expect(frames.size).toBe(1);

      act(() => {
        for (const cb of Array.from(frames.values())) cb(performance.now());
        frames.clear();
      });
      expect(scrollSpy).toHaveBeenCalledTimes(1);
    } finally {
      rafSpy.mockRestore();
      cancelSpy.mockRestore();
    }
  });
});
