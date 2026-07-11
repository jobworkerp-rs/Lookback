import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactElement } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type {
  EmbeddingPreset,
  EmbeddingSettingsResponse,
  SetEmbeddingSettingsRequest,
} from "@/types/api";
import { EmbeddingProviderCard } from "./Settings";

const getEmbeddingSettings = vi.fn();
const listEmbeddingPresets = vi.fn();

vi.mock("@/api", () => ({
  getEmbeddingSettings: () => getEmbeddingSettings(),
  listEmbeddingPresets: () => listEmbeddingPresets(),
  // EmbeddingProviderCard does not call these, but Settings.tsx re-exports
  // them in the same module — stub so the mock module is complete.
  applySettings: () => Promise.resolve({ restarted: true, backup_path: null }),
  createDataRoot: () => Promise.resolve(),
  getAppSettings: () => Promise.resolve(null),
  getConnectionConfig: () => Promise.resolve(null),
  getLlmSettings: () => Promise.resolve(null),
  getMemoryEmbeddingStats: () => Promise.resolve(null),
  getModelStatus: () => Promise.resolve({ llm: null, embedding: null }),
  getSettings: () => Promise.resolve(null),
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

const PRESETS: EmbeddingPreset[] = [
  {
    id: "qwen3-embedding-0-6b",
    display_name: "Qwen3-Embedding 0.6B (text-only)",
    hf_repo: "Qwen/Qwen3-Embedding-0.6B",
    tokenizer_hf_repo: null,
    vector_size: 1024,
    dtype: "F16",
    max_sequence_length: 32_768,
    is_multimodal: false,
    estimated_ram_gb: 2,
    description: "default",
  },
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
    description: "multimodal",
  },
  {
    id: "ruri-v3-310m-onnx-int8",
    display_name: "Ruri v3 310M (Japanese, text-only, 768 dim, INT8)",
    hf_repo: "sirasagi62/ruri-v3-310m-ONNX",
    tokenizer_hf_repo: null,
    vector_size: 768,
    dtype: "F32",
    max_sequence_length: 8192,
    onnx_model_file: "onnx/model_int8.onnx",
    onnx_pooling: "ONNX_POOLING_MEAN",
    document_prefix: "検索文書: ",
    query_prefix: "検索クエリ: ",
    recommended_languages: ["ja"],
    is_multimodal: false,
    estimated_ram_gb: 2,
    description: "ruri",
  },
];

const DEFAULT_LOCAL_SETTINGS: EmbeddingSettingsResponse = {
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
    max_sequence_length: 32_768,
    is_multimodal: false,
  },
  connection_remote: false,
};

// The card now lifts its payload up via onDirtyChange instead of saving
// itself; the parent (Settings) owns the unified save + restart.
function renderCard(
  onDirtyChange = vi.fn(),
  onResetsVectordbChange = vi.fn(),
  props: Partial<Parameters<typeof EmbeddingProviderCard>[0]> = {},
) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const ui: ReactElement = (
    <EmbeddingProviderCard
      retrying={false}
      status={undefined}
      onRetry={() => {}}
      onDirtyChange={onDirtyChange}
      onResetsVectordbChange={onResetsVectordbChange}
      resetSignal={0}
      {...props}
    />
  );
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
  return { onDirtyChange, onResetsVectordbChange };
}

/** Latest value the card reported via onResetsVectordbChange. */
function lastResets(onResets: ReturnType<typeof vi.fn>): boolean {
  return onResets.mock.lastCall?.[0] ?? false;
}

/** Latest payload the card reported (last onDirtyChange arg). */
function lastPayload(onDirty: ReturnType<typeof vi.fn>): SetEmbeddingSettingsRequest | null {
  return onDirty.mock.lastCall?.[0] ?? null;
}

describe("EmbeddingProviderCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    getEmbeddingSettings.mockReset();
    listEmbeddingPresets.mockReset();
    listEmbeddingPresets.mockResolvedValue(PRESETS);
  });

  it("keeps embedding editable without reset warning when connection is remote", async () => {
    getEmbeddingSettings.mockResolvedValue({
      ...DEFAULT_LOCAL_SETTINGS,
      connection_remote: true,
    });
    const { onDirtyChange: onDirty, onResetsVectordbChange } = renderCard();
    await waitFor(() =>
      expect(
        screen.getByText(/リモート接続中はローカルで各記事の embedding は生成されません/),
      ).toBeInTheDocument(),
    );
    expect(screen.getByText(/各記事の embedding 再生成もリクエストしません/)).toBeInTheDocument();
    const select = screen.getByRole("combobox") as HTMLSelectElement;
    expect(select.disabled).toBe(false);
    fireEvent.change(select, { target: { value: "qwen3-vl-embedding-2b" } });
    await waitFor(() =>
      expect(lastPayload(onDirty)).toMatchObject({ preset_id: "qwen3-vl-embedding-2b" }),
    );
    await waitFor(() => expect(lastResets(onResetsVectordbChange)).toBe(false));
    expect(screen.queryByText(/既存の全 embedding が/)).toBeNull();
    expect(screen.queryByText(/再生成が必要/)).toBeNull();
    expect(screen.queryByText(/退避する \(チェックを外すと削除\)/)).toBeNull();
  });

  it("loads presets and shows the current effective model", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard();
    await waitFor(() => expect(getEmbeddingSettings).toHaveBeenCalled());
    await waitFor(() => expect(listEmbeddingPresets).toHaveBeenCalled());
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    const effectiveInput = screen.getByDisplayValue(/Qwen\/Qwen3-Embedding-0.6B \(1024 dim, F16\)/);
    expect(effectiveInput).toBeInTheDocument();
  });

  it("shows RAM estimates for embedding presets", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard();

    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());

    expect(screen.getByRole("option", { name: /Qwen3-Embedding 0\.6B.*2\.0GB/ })).toBeDefined();
    expect(screen.getByText("Embedding RAM: 2.0GB")).toBeInTheDocument();

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "qwen3-vl-embedding-2b" } });

    expect(screen.getByRole("option", { name: /Qwen3-VL-Embedding 2B.*6\.0GB/ })).toBeDefined();
    expect(screen.getByText("Embedding RAM: 6.0GB")).toBeInTheDocument();
  });

  it("does not show a preset RAM estimate for Custom", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard();

    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "custom" } });

    expect(screen.queryByText(/Embedding RAM:/)).not.toBeInTheDocument();
  });

  it("selecting Custom reveals custom input fields", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    const presetSelect = screen.getByRole("combobox") as HTMLSelectElement;
    fireEvent.change(presetSelect, { target: { value: "custom" } });
    expect(screen.getByPlaceholderText("org/name")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("空欄なら HF Repo と同じ")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("e.g. 1024")).toBeInTheDocument();
  });

  it("reports null while a custom model_id is empty", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "custom" } });
    // No model_id yet → incomplete custom → null payload (can't apply).
    await waitFor(() => expect(lastPayload(onDirty)).toBeNull());
  });

  it("seeds the default preset when settings resolve before the preset list", async () => {
    // Regression: when getEmbeddingSettings (preset_id: null) resolved
    // BEFORE listEmbeddingPresets, the initial reseed left presetId as
    // "" and never recovered. A late-arriving preset list must promote
    // the first preset into the dropdown if the user has not yet picked.
    let resolvePresets: (presets: EmbeddingPreset[]) => void = () => {};
    listEmbeddingPresets.mockReturnValueOnce(
      new Promise<EmbeddingPreset[]>((res) => {
        resolvePresets = res;
      }),
    );
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    resolvePresets(PRESETS);
    await waitFor(() => {
      const select = screen.getByRole("combobox") as HTMLSelectElement;
      expect(select.value).toBe("qwen3-embedding-0-6b");
    });
    expect(screen.queryByText(/text-only モデルです。画像検索は無効化されます/)).toBeNull();
  });

  it("uses the Japanese recommendation only when first-run requests it", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard(vi.fn(), vi.fn(), { preferredDefaultLanguage: "ja" });
    await waitFor(() => {
      const select = screen.getByRole("combobox") as HTMLSelectElement;
      expect(select.value).toBe("ruri-v3-310m-onnx-int8");
    });
  });

  it("reports null when custom vector size is empty even with a model id", async () => {
    // Regression: the validator requires `custom_vector_size` because an
    // empty input previously fell back to the active preset dim, mismatching
    // memories' MEMORY_VECTOR_SIZE against the actual output.
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("org/name"), {
      target: { value: "Qwen/Qwen3-Embedding-0.6B" },
    });
    // Still no vector size → null.
    await waitFor(() => expect(lastPayload(onDirty)).toBeNull());
    // Fill in vector size → a payload is reported.
    fireEvent.change(screen.getByPlaceholderText("e.g. 1024"), { target: { value: "1024" } });
    await waitFor(() => expect(lastPayload(onDirty)).not.toBeNull());
    expect(lastPayload(onDirty)).toMatchObject({
      preset_id: "custom",
      custom_model_id: "Qwen/Qwen3-Embedding-0.6B",
      custom_vector_size: 1024,
    });
  });

  it("reports the selected preset as a payload", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "qwen3-vl-embedding-2b" } });
    await waitFor(() =>
      expect(lastPayload(onDirty)).toMatchObject({
        preset_id: "qwen3-vl-embedding-2b",
        custom_model_id: null,
        evacuate_vectordb: true,
      }),
    );
  });

  it("shows the destructive vectordb-reset warning + reports resets=true on a model/dim change", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onResetsVectordbChange } = renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    // Clean state → no banner, resets reported false.
    expect(screen.queryByText(/既存の全 embedding が/)).toBeNull();
    await waitFor(() => expect(lastResets(onResetsVectordbChange)).toBe(false));
    // Switch to a different model (and dim) → reset banner + resets=true.
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "qwen3-vl-embedding-2b" } });
    await waitFor(() => expect(screen.getByText(/既存の全 embedding が/)).toBeInTheDocument());
    await waitFor(() => expect(lastResets(onResetsVectordbChange)).toBe(true));
  });

  it("suppresses the destructive vectordb warning during first-run setup", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onResetsVectordbChange } = renderCard(undefined, undefined, {
      suppressResetWarning: true,
    });
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "qwen3-vl-embedding-2b" } });

    await waitFor(() => expect(lastResets(onResetsVectordbChange)).toBe(true));
    expect(screen.queryByText(/既存の全 embedding が/)).toBeNull();
    expect(screen.queryByText(/退避する \(チェックを外すと削除\)/)).toBeNull();
  });

  it("does NOT warn (resets=false) for a dtype-only custom change at the same model/dim", async () => {
    // Regression: a dtype / max-seq / tokenizer / multimodal-only edit is
    // saveable but leaves the index intact (backend `needs_vectordb_reset`
    // is false). The destructive banner + evacuate checkbox must NOT appear,
    // and the card must report resets=false so the parent's save-bar warning
    // and blocker-modal copy stay non-destructive.
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onDirtyChange: onDirty, onResetsVectordbChange } = renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    // Custom row, same model id + same vector size as the effective runtime,
    // but a different dtype (effective is F16 → pick BF16).
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("org/name"), {
      target: { value: "Qwen/Qwen3-Embedding-0.6B" },
    });
    fireEvent.change(screen.getByPlaceholderText("e.g. 1024"), { target: { value: "1024" } });
    // dtype select is the only <select> besides the preset combobox.
    const selects = screen.getAllByRole("combobox") as HTMLSelectElement[];
    const dtypeSelect = selects.find((s) => Array.from(s.options).some((o) => o.value === "BF16"));
    expect(dtypeSelect).toBeDefined();
    fireEvent.change(dtypeSelect as HTMLSelectElement, { target: { value: "BF16" } });
    // A payload IS reported (the change is saveable)…
    await waitFor(() => expect(lastPayload(onDirty)).not.toBeNull());
    // …but it does not reset the vectordb.
    await waitFor(() => expect(lastResets(onResetsVectordbChange)).toBe(false));
    // The destructive banner is absent; the milder "index preserved" note is
    // shown instead, and the evacuate checkbox is hidden.
    expect(screen.queryByText(/既存の全 embedding が/)).toBeNull();
    expect(screen.getByText(/既存の vectordb は保持されます/)).toBeInTheDocument();
    expect(screen.queryByText(/退避する \(チェックを外すと削除\)/)).toBeNull();
  });

  it("does not show a separate text-only warning chip", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "qwen3-embedding-0-6b" } });
    expect(screen.queryByText(/text-only モデルです。画像検索は無効化されます/)).toBeNull();
  });

  it("reports evacuate_vectordb=false when the user opts out", async () => {
    getEmbeddingSettings.mockResolvedValue(DEFAULT_LOCAL_SETTINGS);
    const { onDirtyChange: onDirty } = renderCard();
    await waitFor(() => expect(screen.queryByText(/現在の設定/)).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "qwen3-vl-embedding-2b" } });
    const cb = screen.getAllByRole("checkbox").at(-1) as HTMLInputElement;
    fireEvent.click(cb);
    await waitFor(() => expect(lastPayload(onDirty)).toMatchObject({ evacuate_vectordb: false }));
  });
});
