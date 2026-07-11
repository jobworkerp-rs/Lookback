import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type {
  BackgroundJobQueueStatus,
  ConnectionConfig,
  EmbeddingSettingsResponse,
  LlmSettingsResponse,
  MemoryEmbeddingStats,
  RedispatchEmbeddingsResult,
  SetEmbeddingSettingsRequest,
  SetLlmSettingsRequest,
} from "@/types/api";
import {
  BACKGROUND_JOB_QUEUE_IDLE_REFETCH_INTERVAL_MS,
  BACKGROUND_JOB_QUEUE_REFETCH_INTERVAL_MS,
  BackgroundJobQueueCard,
  backgroundJobQueueRefetchInterval,
  ConnectionCard,
  LlmProviderCard,
  MemoryEmbeddingCard,
} from "./Settings";

const getConnectionConfig = vi.fn();
const setConnectionConfig = vi.fn();
const testConnectionConfig = vi.fn();
const getLlmSettings = vi.fn();
const getEmbeddingSettings = vi.fn();
const getMemoryEmbeddingStats = vi.fn();
const getBackgroundJobQueueStatus = vi.fn();
const redispatchMemoryEmbeddings = vi.fn();
const listLlmPresets = vi.fn();
const listEmbeddingPresets = vi.fn();
vi.mock("@/api", () => ({
  getConnectionConfig: () => getConnectionConfig(),
  setConnectionConfig: (cfg: unknown) => setConnectionConfig(cfg),
  testConnectionConfig: (cfg: unknown) => testConnectionConfig(cfg),
  getLlmSettings: () => getLlmSettings(),
  getEmbeddingSettings: () => getEmbeddingSettings(),
  getMemoryEmbeddingStats: () => getMemoryEmbeddingStats(),
  getBackgroundJobQueueStatus: () => getBackgroundJobQueueStatus(),
  redispatchMemoryEmbeddings: (req: unknown) => redispatchMemoryEmbeddings(req),
  listLlmPresets: () => listLlmPresets(),
  listEmbeddingPresets: () => listEmbeddingPresets(),
  // LlmProviderCard reads model status via these as part of its render but
  // never branches on the result in the code paths we exercise here; stub
  // them so the component mounts cleanly. The card no longer saves itself
  // (it reports its payload via onDirtyChange), so setLlmSettings is gone.
  getModelStatus: () => Promise.resolve({ llm: null, embedding: null }),
  retryModelSetup: () => Promise.resolve(),
  // HfHomeCard / TimezoneCard read app-settings; TimezoneCard also lists the
  // host zones. Stub both so the Basic view mounts cleanly (these tests
  // exercise the LLM card, not the timezone card).
  getAppSettings: () =>
    Promise.resolve({
      hf_home_mode: "data_root",
      hf_home_path: null,
      data_root_override: null,
      timezone: null,
      effective_timezone: "Asia/Tokyo",
      resolved: {
        current_data_root: "/tmp/lookback",
        default_data_root: "/tmp/lookback",
        effective_hf_home: "/tmp/lookback/models",
        pending_data_root: "/tmp/lookback",
      },
    }),
  listTimezones: () => Promise.resolve(["Asia/Tokyo", "America/New_York", "UTC"]),
}));

// Default preset list used by the api_key tests below — they don't care
// about preset semantics, only that the dropdown has something to seed
// from so `localPresetId` isn't blank when the user clicks Save.
const DEFAULT_PRESETS_FIXTURE = [
  {
    id: "gemma-4-e2b-it-qat-ud-q4-k-xl",
    display_name: "Gemma 4 E2B IT QAT (Q4_K_XL / Unsloth)",
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
    display_name: "Qwen3.5 9B (Q4_K_XL / Unsloth)",
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

const EMBEDDING_PRESETS_FIXTURE = [
  {
    id: "qwen3-embedding-0-6b",
    display_name: "Qwen3 Embedding 0.6B",
    hf_repo: "Qwen/Qwen3-Embedding-0.6B",
    tokenizer_hf_repo: null,
    vector_size: 1024,
    dtype: "F16",
    max_sequence_length: 32768,
    is_multimodal: false,
    estimated_ram_gb: 2,
    description: "embedding default",
  },
  {
    id: "qwen3-vl-embedding-2b",
    display_name: "Qwen3 VL Embedding 2B",
    hf_repo: "Qwen/Qwen3-VL-Embedding-2B",
    tokenizer_hf_repo: null,
    vector_size: 2048,
    dtype: "F16",
    max_sequence_length: 32768,
    is_multimodal: true,
    estimated_ram_gb: 6,
    description: "embedding multimodal",
  },
];

const LOCAL_CONFIG: ConnectionConfig = {
  mode: "local",
  remote_jobworkerp_url: null,
  remote_memories_url: null,
};

function renderCard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidateSpy = vi.spyOn(client, "invalidateQueries");
  const ui: ReactElement = (
    <ConnectionCard
      localJobworkerpUrl="http://127.0.0.1:9000"
      localMemoriesUrl="http://127.0.0.1:9010"
    />
  );
  const result = render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
  return { ...result, invalidateSpy };
}

async function switchToRemoteAndFill() {
  fireEvent.click(screen.getByText("Remote server"));
  fireEvent.change(screen.getByPlaceholderText("http://host:9000"), {
    target: { value: "http://10.0.0.2:9000" },
  });
  fireEvent.change(screen.getByPlaceholderText("http://host:9010"), {
    target: { value: "http://10.0.0.2:9010" },
  });
}

describe("ConnectionCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getConnectionConfig.mockReset();
    setConnectionConfig.mockReset();
    testConnectionConfig.mockReset();
    getConnectionConfig.mockResolvedValue(LOCAL_CONFIG);
  });

  it("invalidates every query after a successful remote save", async () => {
    setConnectionConfig.mockResolvedValue(undefined);
    const { invalidateSpy } = renderCard();

    await switchToRemoteAndFill();
    fireEvent.click(screen.getByText("接続先を保存"));

    await screen.findByText("保存しました");
    expect(setConnectionConfig).toHaveBeenCalledWith({
      mode: "remote",
      remote_jobworkerp_url: "http://10.0.0.2:9000",
      remote_memories_url: "http://10.0.0.2:9010",
    });
    // The whole cache must be dropped (no queryKey filter): listing one
    // instance's threads while detail/link queries hit another is exactly the
    // half-broken state this guards against.
    expect(invalidateSpy).toHaveBeenCalledTimes(1);
    expect(invalidateSpy).toHaveBeenCalledWith();
  });

  it("does not touch the cache when the save fails", async () => {
    setConnectionConfig.mockRejectedValue(new Error("connect refused"));
    const { invalidateSpy } = renderCard();

    await switchToRemoteAndFill();
    fireEvent.click(screen.getByText("接続先を保存"));

    await screen.findByText("connect refused");
    // A failed save must keep the existing (working) data on screen, not blank
    // it by refetching against an unreachable target.
    expect(invalidateSpy).not.toHaveBeenCalled();
  });

  it("tests the edited remote target without saving it", async () => {
    testConnectionConfig.mockResolvedValue({
      jobworkerp_url: "http://10.0.0.2:9000",
      memories_url: "http://10.0.0.2:9010",
    });
    const { invalidateSpy } = renderCard();

    await switchToRemoteAndFill();
    fireEvent.click(screen.getByText("接続をテスト"));

    await screen.findByText("接続できました");
    expect(testConnectionConfig).toHaveBeenCalledWith({
      mode: "remote",
      remote_jobworkerp_url: "http://10.0.0.2:9000",
      remote_memories_url: "http://10.0.0.2:9010",
    });
    expect(setConnectionConfig).not.toHaveBeenCalled();
    expect(invalidateSpy).not.toHaveBeenCalled();
  });

  it("shows remote test failures without touching the query cache", async () => {
    testConnectionConfig.mockRejectedValue(
      new Error("remote memories connection failed (http://10.0.0.2:9010): refused"),
    );
    const { invalidateSpy } = renderCard();

    await switchToRemoteAndFill();
    fireEvent.click(screen.getByText("接続をテスト"));

    await screen.findByText("remote memories connection failed (http://10.0.0.2:9010): refused");
    expect(invalidateSpy).not.toHaveBeenCalled();
  });

  it("shows local URLs read-only and offers no editable fields in local mode", async () => {
    renderCard();
    // Seed effect runs from the mocked config (local).
    await waitFor(() => expect(getConnectionConfig).toHaveBeenCalled());
    expect(screen.queryByPlaceholderText("http://host:9000")).toBeNull();
    expect(screen.getByDisplayValue("http://127.0.0.1:9010")).toBeInTheDocument();
  });
});

// LlmProviderCard tests cover the api_key tri-state contract with the
// backend (`null` = no change, `""` = delete, `"x"` = update). Save NEVER
// produces `""` — that would let "edit temperature without re-typing the
// key" silently wipe the credential. Deletion is an explicit operation
// behind its own button + confirm.

// Spread base so each fixture only spells the fields it actually
// exercises — every new `LlmSettingsResponse` field becomes a one-line
// edit here instead of N places below.
const LLM_SETTINGS_BASE: LlmSettingsResponse = {
  mode: "local",
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

const EXTERNAL_NO_KEY: LlmSettingsResponse = {
  ...LLM_SETTINGS_BASE,
  mode: "external",
  provider_model: "gpt-4o",
};

const EXTERNAL_WITH_KEY: LlmSettingsResponse = {
  ...EXTERNAL_NO_KEY,
  api_key_set: true,
};

const LOCAL_WITH_OVERRIDES: LlmSettingsResponse = {
  ...LLM_SETTINGS_BASE,
  max_tokens: 8000,
  temperature: 0.7,
};

// The card now reports its save payload via onDirtyChange rather than
// saving itself; the parent (Settings) owns the unified save + restart.
// Tests assert on the LAST payload the card reported.
function renderLlmCard(
  onDirtyChange = vi.fn(),
  props: Partial<Parameters<typeof LlmProviderCard>[0]> = {},
) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const ui: ReactElement = (
    <LlmProviderCard
      retrying={false}
      status={undefined}
      onRetry={() => {}}
      onDirtyChange={onDirtyChange}
      resetSignal={0}
      {...props}
    />
  );
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
  return onDirtyChange;
}

/** The latest payload the card reported (last onDirtyChange arg). */
function llmPayload(onDirty: ReturnType<typeof vi.fn>): SetLlmSettingsRequest | null {
  return onDirty.mock.lastCall?.[0] ?? null;
}

describe("LlmProviderCard api_key payload", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getLlmSettings.mockReset();
    getEmbeddingSettings.mockReset();
    listLlmPresets.mockReset();
    listEmbeddingPresets.mockReset();
    listLlmPresets.mockResolvedValue(DEFAULT_PRESETS_FIXTURE);
    getEmbeddingSettings.mockResolvedValue(EMBEDDING_SETTINGS_BASE);
    listEmbeddingPresets.mockResolvedValue(EMBEDDING_PRESETS_FIXTURE);
  });

  it("reports the typed key verbatim when the input has a value", async () => {
    getLlmSettings.mockResolvedValue(EXTERNAL_NO_KEY);
    const onDirty = renderLlmCard();
    const apiKeyInput = await screen.findByPlaceholderText("sk-...");
    fireEvent.change(apiKeyInput, { target: { value: "sk-new" } });
    await waitFor(() => expect(llmPayload(onDirty)).toMatchObject({ api_key: "sk-new" }));
  });

  it("reports null (clean) when a key is stored and the input is left blank", async () => {
    getLlmSettings.mockResolvedValue(EXTERNAL_WITH_KEY);
    const onDirty = renderLlmCard();
    // The "(Keychain に保存済み)" placeholder confirms the seed effect ran.
    await screen.findByPlaceholderText("(Keychain に保存済み)");
    // Untouched form mirroring "edit nothing": api_key must NOT be reported
    // as "" — that would wipe the stored credential on save. A clean card
    // reports null.
    await waitFor(() => expect(llmPayload(onDirty)).toBeNull());
  });

  it("preserves temperature=0 instead of falling back to the chat default", async () => {
    // Regression: `Number("0") || null` collapsed legitimate `0` into null.
    const LOCAL_WITH_TEMP: LlmSettingsResponse = { ...LLM_SETTINGS_BASE, temperature: 0.5 };
    getLlmSettings.mockResolvedValue(LOCAL_WITH_TEMP);
    const onDirty = renderLlmCard();
    const temperatureInput = await screen.findByDisplayValue("0.5");
    fireEvent.change(temperatureInput, { target: { value: "0" } });
    expect((temperatureInput as HTMLInputElement).value).toBe("0");
    await waitFor(() => expect(llmPayload(onDirty)).toMatchObject({ temperature: 0 }));
  });

  it("reports api_key='' via the explicit Delete button (no confirm; batched into save)", async () => {
    getLlmSettings.mockResolvedValue(EXTERNAL_WITH_KEY);
    const onDirty = renderLlmCard();
    const deleteBtn = await screen.findByText("キーを削除");
    fireEvent.click(deleteBtn);
    // The destructive confirm now lives on the unified save bar, not here.
    await waitFor(() => expect(llmPayload(onDirty)).toMatchObject({ api_key: "" }));
    expect(screen.getByText("保存すると Keychain の API キーが削除されます。")).toBeInTheDocument();
  });

  it("cancels a pending key delete via 削除を取消", async () => {
    getLlmSettings.mockResolvedValue(EXTERNAL_WITH_KEY);
    const onDirty = renderLlmCard();
    fireEvent.click(await screen.findByText("キーを削除"));
    await waitFor(() => expect(llmPayload(onDirty)).toMatchObject({ api_key: "" }));
    fireEvent.click(screen.getByText("削除を取消"));
    // Back to clean (no other edits) → null payload.
    await waitFor(() => expect(llmPayload(onDirty)).toBeNull());
  });

  it("keeps Max tokens / Temperature editable in Local mode (chat overrides apply to both)", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_WITH_OVERRIDES);
    const onDirty = renderLlmCard();
    const maxTokensInput = await screen.findByDisplayValue("8000");
    const temperatureInput = screen.getByDisplayValue("0.7");
    fireEvent.change(maxTokensInput, { target: { value: "2000" } });
    fireEvent.change(temperatureInput, { target: { value: "0.1" } });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "local",
        max_tokens: 2000,
        temperature: 0.1,
      }),
    );
  });

  it("clears persisted Max tokens / Temperature when the inputs are blanked", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_WITH_OVERRIDES);
    const onDirty = renderLlmCard();
    const maxTokensInput = await screen.findByDisplayValue("8000");
    const temperatureInput = screen.getByDisplayValue("0.7");
    fireEvent.change(maxTokensInput, { target: { value: "" } });
    fireEvent.change(temperatureInput, { target: { value: "" } });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({ max_tokens: null, temperature: null }),
    );
  });
});

// LlmProviderCard local-LLM model selection. Pin the preset dropdown
// rendering, custom-mode reveal, advanced section toggle, the
// confirm-before-restart gate, and the payload shape (`local_*` flow
// through to `setLlmSettings`).

const LOCAL_PRE_FEATURE: LlmSettingsResponse = { ...LLM_SETTINGS_BASE };

const LOCAL_NON_DEFAULT_PRESET: LlmSettingsResponse = {
  ...LLM_SETTINGS_BASE,
  local_preset_id: "qwen3-5-9b-ud-q4-k-xl",
};

const EMBEDDING_SETTINGS_BASE: EmbeddingSettingsResponse = {
  preset_id: null,
  custom_model_id: null,
  custom_tokenizer_id: null,
  custom_vector_size: null,
  custom_dtype: null,
  custom_max_sequence_length: null,
  custom_is_multimodal: null,
  effective: {
    model_id: "Qwen/Qwen3-Embedding-0.6B",
    tokenizer_id: null,
    vector_size: 1024,
    dtype: "F16",
    max_sequence_length: 32768,
    is_multimodal: false,
  },
  connection_remote: false,
};

describe("LlmProviderCard local LLM presets", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getLlmSettings.mockReset();
    getEmbeddingSettings.mockReset();
    listLlmPresets.mockReset();
    listEmbeddingPresets.mockReset();
    listLlmPresets.mockResolvedValue(DEFAULT_PRESETS_FIXTURE);
    getEmbeddingSettings.mockResolvedValue(EMBEDDING_SETTINGS_BASE);
    listEmbeddingPresets.mockResolvedValue(EMBEDDING_PRESETS_FIXTURE);
  });

  // Wait until the preset dropdown is populated with curated options
  // (presets useQuery resolved) AND the local seed effect has run
  // (localPresetId initialised). All preset-aware tests start with this.
  async function waitForPresetDropdownReady(): Promise<HTMLSelectElement> {
    const label = await screen.findByText("プリセット");
    const select = label.closest(".settings-row")?.querySelector("select") as HTMLSelectElement;
    await waitFor(() => {
      // 1 = only "カスタム…" rendered; we need the curated options too.
      expect(select.options.length).toBeGreaterThan(1);
    });
    return select;
  }

  it("renders the preset dropdown with curated + カスタム options in local mode", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    renderLlmCard();
    await waitForPresetDropdownReady();
    for (const p of DEFAULT_PRESETS_FIXTURE) {
      expect(
        screen.getByText(
          new RegExp(
            `${p.display_name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")} \\(≈ \\d+\\.\\dGB\\)`,
          ),
          {
            selector: "option",
          },
        ),
      ).toBeInTheDocument();
    }
    expect(screen.getByText("カスタム…", { selector: "option" })).toBeInTheDocument();
  });

  it("hides custom inputs when a curated preset is selected", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    renderLlmCard();
    await waitForPresetDropdownReady();
    expect(screen.queryByPlaceholderText("unsloth/Qwen3.5-9B-GGUF")).not.toBeInTheDocument();
    expect(screen.queryByPlaceholderText("Qwen3.5-9B-UD-Q4_K_XL.gguf")).not.toBeInTheDocument();
  });

  it("reveals hf_repo / gguf inputs when the user picks カスタム…", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    renderLlmCard();
    const dropdown = await waitForPresetDropdownReady();
    fireEvent.change(dropdown, { target: { value: "custom" } });
    expect(screen.getByPlaceholderText("unsloth/Qwen3.5-9B-GGUF")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Qwen3.5-9B-UD-Q4_K_XL.gguf")).toBeInTheDocument();
    expect(
      screen.getByText(/サポート対象は Qwen3\.5\/3\.6 系と Gemma 4 系のみです/),
    ).toBeInTheDocument();
  });

  it("keeps the advanced section collapsed by default and shows ctx_size when expanded", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    renderLlmCard();
    await waitForPresetDropdownReady();
    expect(screen.queryByPlaceholderText("131072")).not.toBeInTheDocument();
    fireEvent.click(screen.getByText("▶ 上級設定"));
    expect(screen.getByPlaceholderText("131072")).toBeInTheDocument();
    expect(screen.getByDisplayValue("Q4_0")).toBeInTheDocument();
  });

  it("reports the selected KV cache type in the local payload", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    const onDirty = renderLlmCard();
    await waitForPresetDropdownReady();
    fireEvent.click(screen.getByText("▶ 上級設定"));
    const kvSelect = screen.getByDisplayValue("Q4_0");
    fireEvent.change(kvSelect, { target: { value: "Q8_0" } });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "local",
        local_kv_cache_type: "Q8_0",
      }),
    );
  });

  it("updates the RAM estimate when ctx_size shrinks", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    renderLlmCard();
    await waitForPresetDropdownReady();
    expect(screen.getByText(/KV cache: .*131072 ctx/)).toBeInTheDocument();
    fireEvent.click(screen.getByText("▶ 上級設定"));
    fireEvent.change(screen.getByPlaceholderText("131072"), { target: { value: "32768" } });
    await waitFor(() => expect(screen.getByText(/KV cache: .*32768 ctx/)).toBeInTheDocument());
  });

  it("adds the selected embedding preset RAM to the local RAM estimate", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    renderLlmCard();
    await waitForPresetDropdownReady();
    await waitFor(() =>
      expect(
        screen.getByText(/Embedding: 2\.0GB \/ LLM \+ Embedding total: \d+\.\dGB/),
      ).toBeInTheDocument(),
    );
    expect(screen.getByText(/Model: 3\.5GB/).closest(".settings-ram-estimate")).not.toBeNull();
  });

  it("uses a pending embedding preset when calculating combined RAM", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    const pendingEmbedding: SetEmbeddingSettingsRequest = {
      preset_id: "qwen3-embedding-0-6b",
      custom_model_id: null,
      custom_tokenizer_id: null,
      custom_vector_size: null,
      custom_dtype: null,
      custom_max_sequence_length: null,
      custom_is_multimodal: null,
      evacuate_vectordb: true,
    };
    renderLlmCard(undefined, { pendingEmbeddingSettings: pendingEmbedding });

    await waitForPresetDropdownReady();

    expect(screen.getByText(/Embedding: 2\.0GB \/ LLM \+ Embedding total:/)).toBeInTheDocument();
    expect(screen.queryByText(/Embedding: 6\.0GB \/ LLM \+ Embedding total:/)).toBeNull();
  });

  it("explains when embedding RAM is unavailable for a custom embedding model", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    getEmbeddingSettings.mockResolvedValue({
      ...EMBEDDING_SETTINGS_BASE,
      preset_id: "custom",
      custom_model_id: "org/custom-embedding",
    });
    renderLlmCard();
    await waitForPresetDropdownReady();
    expect(screen.getByText(/Embedding RAM: custom embedding model/)).toBeInTheDocument();
  });

  it("reports null while the custom hf_repo has an invalid shape", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    const onDirty = renderLlmCard();
    const dropdown = await waitForPresetDropdownReady();
    fireEvent.change(dropdown, { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("unsloth/Qwen3.5-9B-GGUF"), {
      target: { value: "no-slash-repo" },
    });
    fireEvent.change(screen.getByPlaceholderText("Qwen3.5-9B-UD-Q4_K_XL.gguf"), {
      target: { value: "model.gguf" },
    });
    // Invalid repo shape → card cannot be applied → null payload.
    await waitFor(() => expect(llmPayload(onDirty)).toBeNull());
  });

  it("reports a payload when the custom hf_repo and gguf inputs are both valid", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_PRE_FEATURE);
    const onDirty = renderLlmCard();
    const dropdown = await waitForPresetDropdownReady();
    fireEvent.change(dropdown, { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("unsloth/Qwen3.5-9B-GGUF"), {
      target: { value: "unsloth/Qwen3.5-9B-GGUF" },
    });
    fireEvent.change(screen.getByPlaceholderText("Qwen3.5-9B-UD-Q4_K_XL.gguf"), {
      target: { value: "Qwen3.5-9B-UD-Q4_K_XL.gguf" },
    });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "local",
        local_preset_id: "custom",
        local_hf_repo: "unsloth/Qwen3.5-9B-GGUF",
        local_model_file: "Qwen3.5-9B-UD-Q4_K_XL.gguf",
      }),
    );
  });

  it("reports null while only temperature changes match the persisted value", async () => {
    // A temperature edit that ends up equal to the persisted value leaves
    // the card clean. (The download-cost confirm now lives on the save bar.)
    getLlmSettings.mockResolvedValue({ ...LOCAL_PRE_FEATURE, temperature: 0.5 });
    const onDirty = renderLlmCard();
    const dropdown = await waitForPresetDropdownReady();
    await waitFor(() => expect(dropdown.value).toBe("gemma-4-e2b-it-qat-ud-q4-k-xl"));
    const tempInput = screen.getByDisplayValue("0.5") as HTMLInputElement;
    fireEvent.change(tempInput, { target: { value: "0.7" } });
    await waitFor(() => expect(llmPayload(onDirty)).toMatchObject({ temperature: 0.7 }));
    // Revert to the persisted value → clean again.
    fireEvent.change(tempInput, { target: { value: "0.5" } });
    await waitFor(() => expect(llmPayload(onDirty)).toBeNull());
  });

  it("reports the new preset id when the preset changes", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_NON_DEFAULT_PRESET);
    const onDirty = renderLlmCard();
    const dropdown = await waitForPresetDropdownReady();
    await waitFor(() => expect(dropdown.value).toBe("qwen3-5-9b-ud-q4-k-xl"));
    fireEvent.change(dropdown, { target: { value: "custom" } });
    const repoInput = await screen.findByPlaceholderText("unsloth/Qwen3.5-9B-GGUF");
    fireEvent.change(repoInput, { target: { value: "me/my-fork" } });
    fireEvent.change(screen.getByPlaceholderText("Qwen3.5-9B-UD-Q4_K_XL.gguf"), {
      target: { value: "fork.gguf" },
    });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "local",
        local_preset_id: "custom",
        local_model_file: "fork.gguf",
        local_hf_repo: "me/my-fork",
      }),
    );
  });

  it("reports local_preset_id=null for an external-mode change (local fields gated by mode)", async () => {
    // External mode dispatches via genai; the persisted `local_*` remain in
    // the file (preserved across a flip back), but the External payload MUST
    // drop them so a partial form state can't bleed in.
    getLlmSettings.mockResolvedValue(EXTERNAL_NO_KEY);
    const onDirty = renderLlmCard();
    await screen.findByPlaceholderText("sk-...");
    // Change the model so the card is dirty.
    fireEvent.change(screen.getByPlaceholderText(/gpt-4o/), { target: { value: "gpt-4.1" } });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "external",
        provider_model: "gpt-4.1",
        local_preset_id: null,
        local_model_file: null,
        local_hf_repo: null,
        local_ctx_size: null,
      }),
    );
  });
});

// MemoryEmbeddingCard: the regression that pulled the old "自省インデックス"
// card was a permanently-disabled action + zero counters. These tests pin
// the disabled-only-when-table-missing contract and the confirm-gated
// re-dispatch path so the card cannot silently regress to the old shape.

const STATS_HEALTHY: MemoryEmbeddingStats = {
  total_records: 1234,
  records_with_embedding: 1200,
  records_without_embedding: 34,
  vector_dimension: 1024,
};

const STATS_TABLE_MISSING: MemoryEmbeddingStats = {
  total_records: 0,
  records_with_embedding: 0,
  records_without_embedding: 0,
  vector_dimension: 0,
};

const REDISPATCH_RESULT: RedispatchEmbeddingsResult = {
  dispatched_count: 34,
  skipped_count: 0,
  failed_count: 0,
  duration_ms: 1500,
};

function renderEmbeddingCard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <MemoryEmbeddingCard />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

const BACKGROUND_QUEUE: BackgroundJobQueueStatus = {
  active: true,
  rows: [
    { kind: "embedding", pending: 3, running: 1, wait_result: 0, cancelling: 0 },
    { kind: "summary", pending: 2, running: 0, wait_result: 1, cancelling: 0 },
    { kind: "personality", pending: 0, running: 0, wait_result: 0, cancelling: 0 },
    { kind: "reflection", pending: 0, running: 0, wait_result: 0, cancelling: 0 },
    { kind: "llm_other", pending: 1, running: 0, wait_result: 0, cancelling: 0 },
  ],
};

describe("BackgroundJobQueueCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getBackgroundJobQueueStatus.mockReset();
  });

  it("renders task-specific active queue counts", async () => {
    getBackgroundJobQueueStatus.mockResolvedValue(BACKGROUND_QUEUE);
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={client}>
          <BackgroundJobQueueCard />
        </QueryClientProvider>
      </I18nextProvider>,
    );

    await screen.findByText("Embedding");
    expect(screen.getByText("要約")).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
    expect(screen.getAllByText("1")).toHaveLength(3);
  });

  it("uses a slower interval while queues are idle", () => {
    expect(BACKGROUND_JOB_QUEUE_REFETCH_INTERVAL_MS).toBe(10_000);
    expect(BACKGROUND_JOB_QUEUE_IDLE_REFETCH_INTERVAL_MS).toBe(60_000);
    expect(backgroundJobQueueRefetchInterval(true)).toBe(10_000);
    expect(backgroundJobQueueRefetchInterval(false)).toBe(60_000);
  });
});

describe("MemoryEmbeddingCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getMemoryEmbeddingStats.mockReset();
    redispatchMemoryEmbeddings.mockReset();
  });

  it("disables 再生成 when the memory_vector table is missing", async () => {
    // vector_dimension === 0 signals the LanceDB table never came up
    // (sidecar down or MEMORY_VECTOR_ENABLED unset). Dispatching against a
    // missing table would fail at upsert, so the button must be inert and
    // surface a state hint instead of a misleading clickable action.
    getMemoryEmbeddingStats.mockResolvedValue(STATS_TABLE_MISSING);
    renderEmbeddingCard();

    // Wait for the state hint first — its rendering implies the useQuery
    // has resolved (stats.isLoading=false), so the disabled assertion below
    // sees the post-load button state rather than the loading-state default.
    // The state hint is the only signal the user has for "why is this
    // disabled?" — keep it asserted so a copy edit doesn't silently strip it.
    await screen.findByText(
      "memory_vector テーブルが未作成です (sidecar 未起動、または埋め込みが無効)。",
    );
    expect(screen.getByRole("button", { name: "再生成する" })).toBeDisabled();
  });

  it("calls redispatchMemoryEmbeddings({}) after confirm and renders the result line", async () => {
    getMemoryEmbeddingStats.mockResolvedValue(STATS_HEALTHY);
    redispatchMemoryEmbeddings.mockResolvedValue(REDISPATCH_RESULT);
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
    try {
      renderEmbeddingCard();
      // Wait until the stat row paints; before this the useQuery is still
      // loading and the button is `tableMissing`-disabled, so a click would
      // be swallowed and the test would just time out on the spy.
      await screen.findByDisplayValue("1234");
      fireEvent.click(screen.getByRole("button", { name: "再生成する" }));

      await waitFor(() => expect(redispatchMemoryEmbeddings).toHaveBeenCalled());
      expect(confirmSpy).toHaveBeenCalledTimes(1);
      // Empty request: card runs the full RDB rescan, no scope filters.
      expect(redispatchMemoryEmbeddings.mock.lastCall?.[0]).toEqual({});
      // Result line uses the same projection the deleted ReflectionIndexCard
      // had, so users get the dispatched/skipped/failed/duration view.
      await screen.findByText(/投入 34 \/ スキップ 0 \/ 失敗 0/);
    } finally {
      confirmSpy.mockRestore();
    }
  });

  it("does not dispatch when the user cancels the confirm dialog", async () => {
    // Re-dispatch is a long-running, irreversible-ish action (it re-runs
    // every embedding job), so a stray click MUST be recoverable by hitting
    // Cancel on the system prompt without touching the backend.
    getMemoryEmbeddingStats.mockResolvedValue(STATS_HEALTHY);
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);
    try {
      renderEmbeddingCard();
      // Same loading-gate as above: wait for the stats row to materialise so
      // the button is actually clickable (otherwise the test trivially
      // "passes" by clicking a still-disabled button).
      await screen.findByDisplayValue("1234");
      fireEvent.click(screen.getByRole("button", { name: "再生成する" }));

      // Anchor on the observable effect (confirm is the only side effect of
      // the cancel path) instead of a setTimeout-based flush. If a future
      // refactor reordered the dispatch ahead of the confirm, this would
      // fail loudly instead of racing.
      await waitFor(() => expect(confirmSpy).toHaveBeenCalledTimes(1));
      expect(redispatchMemoryEmbeddings).not.toHaveBeenCalled();
    } finally {
      confirmSpy.mockRestore();
    }
  });
});

// Regression guard: saving in one mode MUST NOT overwrite the other mode's
// persisted fields. The sidecar stores Local and External independently, so
// switching back to External after a Local save must show the previously
// entered provider_model / base_url, not a blank field. Same for the local_*
// fields when External saves. Otherwise the user is forced to re-enter the
// model name (and friends) every time they flip the mode toggle.

describe("LlmProviderCard mode independence on save", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getLlmSettings.mockReset();
    getEmbeddingSettings.mockReset();
    listLlmPresets.mockReset();
    listEmbeddingPresets.mockReset();
    listLlmPresets.mockResolvedValue(DEFAULT_PRESETS_FIXTURE);
    getEmbeddingSettings.mockResolvedValue(EMBEDDING_SETTINGS_BASE);
    listEmbeddingPresets.mockResolvedValue(EMBEDDING_PRESETS_FIXTURE);
  });

  const EXTERNAL_FULL: LlmSettingsResponse = {
    ...LLM_SETTINGS_BASE,
    mode: "external",
    provider_model: "gpt-4o",
    base_url: "https://proxy.example.com/v1",
    api_key_set: true,
  };

  const LOCAL_WITH_CUSTOM_PRESET: LlmSettingsResponse = {
    ...LLM_SETTINGS_BASE,
    mode: "local",
    local_preset_id: "custom",
    local_model_file: "Qwen3.5-9B-UD-Q4_K_XL.gguf",
    local_hf_repo: "unsloth/Qwen3.5-9B-GGUF",
    local_ctx_size: 65536,
  };

  it("preserves External-side fields when changing to Local mode", async () => {
    // Seed external, then user toggles to Local. The reported payload MUST
    // carry the External provider_model / base_url verbatim so flipping back
    // later restores them.
    getLlmSettings.mockResolvedValue(EXTERNAL_FULL);
    const onDirty = renderLlmCard();
    await screen.findByDisplayValue("gpt-4o");
    fireEvent.click(screen.getByText("Local LLM"));
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "local",
        provider_model: "gpt-4o",
        base_url: "https://proxy.example.com/v1",
      }),
    );
  });

  it("preserves local_* fields when changing to External mode", async () => {
    getLlmSettings.mockResolvedValue(LOCAL_WITH_CUSTOM_PRESET);
    const onDirty = renderLlmCard();
    await screen.findByDisplayValue("Qwen3.5-9B-UD-Q4_K_XL.gguf");
    fireEvent.click(screen.getByText("External API"));
    fireEvent.change(screen.getByPlaceholderText(/gpt-4o/), { target: { value: "gpt-4o" } });
    await waitFor(() =>
      expect(llmPayload(onDirty)).toMatchObject({
        mode: "external",
        provider_model: "gpt-4o",
        local_preset_id: "custom",
        local_model_file: "Qwen3.5-9B-UD-Q4_K_XL.gguf",
        local_hf_repo: "unsloth/Qwen3.5-9B-GGUF",
        local_ctx_size: 65536,
      }),
    );
  });
});
