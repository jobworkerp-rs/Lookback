import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { SetupWizard } from "./SetupWizard";

const applySetupMock = vi.fn();
const resumeSetupMock = vi.fn();
const restartForSetupMock = vi.fn();
const validateDataRootMock = vi.fn();
const openDialogMock = vi.fn();

vi.mock("@/api", () => ({
  applySetup: (...args: unknown[]) => applySetupMock(...args),
  resumeSetup: (...args: unknown[]) => resumeSetupMock(...args),
  restartForSetup: (...args: unknown[]) => restartForSetupMock(...args),
  validateDataRoot: (...args: unknown[]) => validateDataRootMock(...args),
  createDataRoot: vi.fn(),
}));

vi.mock("@/pages/Settings", () => ({
  HfHomeCard: ({ onDirtyChange }: { onDirtyChange: (payload: unknown) => void }) => (
    <button type="button" onClick={() => onDirtyChange({ mode: "data_root", path: null })}>
      HF_HOME card
    </button>
  ),
  EmbeddingProviderCard: ({ onDirtyChange }: { onDirtyChange: (payload: unknown) => void }) => (
    <div>
      Embedding card
      <button
        type="button"
        onClick={() =>
          onDirtyChange({
            preset_id: "qwen3-embedding-0-6b",
            custom_model_id: null,
            custom_tokenizer_id: null,
            custom_vector_size: null,
            custom_dtype: null,
            custom_max_sequence_length: null,
            custom_is_multimodal: null,
            evacuate_vectordb: true,
          })
        }
      >
        Pick light embedding
      </button>
    </div>
  ),
  LlmProviderCard: ({
    onDirtyChange,
    pendingEmbeddingSettings,
  }: {
    onDirtyChange: (payload: unknown) => void;
    pendingEmbeddingSettings?: { preset_id?: string | null } | null;
  }) => (
    <button
      type="button"
      onClick={() =>
        onDirtyChange({
          mode: "local",
          provider_model: null,
          api_key: null,
          base_url: null,
          max_tokens: null,
          temperature: null,
          local_preset_id: "qwen3-5-9b-ud-q4-k-xl",
          local_model_file: null,
          local_hf_repo: null,
          local_ctx_size: null,
          local_kv_cache_type: null,
        })
      }
    >
      LLM card {pendingEmbeddingSettings?.preset_id ?? "no pending embedding"}
    </button>
  ),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (opts: unknown) => openDialogMock(opts),
}));

function renderWizard(resumeApply = false) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onComplete = vi.fn();
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <SetupWizard
          resumeApply={resumeApply}
          currentDataRoot="/current"
          defaultDataRoot="/default"
          onComplete={onComplete}
        />
      </QueryClientProvider>
    </I18nextProvider>,
  );
  return { onComplete };
}

describe("SetupWizard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    applySetupMock.mockReset().mockResolvedValue({ restart_required: false });
    resumeSetupMock.mockReset().mockResolvedValue(undefined);
    restartForSetupMock.mockReset().mockResolvedValue(undefined);
    openDialogMock.mockReset().mockResolvedValue(null);
    validateDataRootMock.mockReset().mockResolvedValue({
      ok: true,
      writable: true,
      is_existing_lookback_root: false,
      creatable: false,
      message: null,
    });
  });

  it("runs the default setup when the welcome screen is skipped", async () => {
    const { onComplete } = renderWizard();
    fireEvent.click(screen.getByRole("button", { name: "スキップしてはじめる" }));
    await waitFor(() => expect(applySetupMock).toHaveBeenCalledTimes(1));
    expect(applySetupMock).toHaveBeenCalledWith({
      data_root: null,
      preferred_language: "ja",
      settings: { llm: null, embedding: null, hf_home: null, mcp: null, timezone: null },
    });
    await waitFor(() => expect(onComplete).toHaveBeenCalled());
  });

  it("resumes model preparation after a data-root restart", async () => {
    const { onComplete } = renderWizard(true);
    await waitFor(() => expect(resumeSetupMock).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(onComplete).toHaveBeenCalled());
  });

  it("describes setup progress without mentioning a service restart", async () => {
    applySetupMock.mockReturnValue(new Promise(() => {}));
    renderWizard();

    fireEvent.click(screen.getByRole("button", { name: "スキップしてはじめる" }));

    expect(await screen.findByText("設定を保存し、モデルを準備しています。")).toBeInTheDocument();
    expect(screen.queryByText(/サービス再起動/)).not.toBeInTheDocument();
  });

  it("keeps next disabled while a data root is invalid", async () => {
    validateDataRootMock.mockResolvedValue({
      ok: false,
      writable: false,
      is_existing_lookback_root: false,
      creatable: false,
      message: "絶対パスを指定してください",
    });
    renderWizard();
    fireEvent.click(screen.getByRole("button", { name: "設定をはじめる" }));
    fireEvent.change(screen.getByPlaceholderText("/default"), { target: { value: "relative" } });
    expect(screen.getByRole("button", { name: "次へ" })).toBeDisabled();
    await waitFor(() => expect(validateDataRootMock).toHaveBeenCalledWith("relative"));
    expect(screen.getByRole("button", { name: "次へ" })).toBeDisabled();
  });

  it("fills the data root from the directory picker", async () => {
    openDialogMock.mockResolvedValue("/home/me/lookback");
    renderWizard();

    fireEvent.click(screen.getByRole("button", { name: "設定をはじめる" }));
    fireEvent.click(screen.getByRole("button", { name: "選択…" }));

    await waitFor(() =>
      expect(screen.getByPlaceholderText("/default")).toHaveValue("/home/me/lookback"),
    );
    expect(openDialogMock).toHaveBeenCalledWith({ directory: true, multiple: false });
  });

  it("keeps manual path entry usable when the directory picker fails", async () => {
    openDialogMock.mockRejectedValue(new Error("portal unavailable"));
    renderWizard();

    fireEvent.click(screen.getByRole("button", { name: "設定をはじめる" }));
    fireEvent.click(screen.getByRole("button", { name: "選択…" }));

    expect(
      await screen.findByText(/ディレクトリ選択ダイアログを開けませんでした/),
    ).toBeInTheDocument();
    fireEvent.change(screen.getByPlaceholderText("/default"), {
      target: { value: "/home/me/lookback" },
    });

    await waitFor(() => expect(validateDataRootMock).toHaveBeenCalledWith("/home/me/lookback"));
    expect(screen.getByRole("button", { name: "次へ" })).not.toBeDisabled();
  });

  it("applies selected HF_HOME and Qwen3.5 preset from the wizard", async () => {
    renderWizard();
    fireEvent.click(screen.getByRole("button", { name: "設定をはじめる" }));
    fireEvent.change(screen.getByPlaceholderText("/default"), {
      target: { value: "/Volumes/Ext/lookback" },
    });
    await waitFor(() => expect(validateDataRootMock).toHaveBeenCalledWith("/Volumes/Ext/lookback"));
    fireEvent.click(screen.getByRole("button", { name: "HF_HOME card" }));
    fireEvent.click(screen.getByRole("button", { name: "次へ" }));
    fireEvent.click(screen.getByRole("button", { name: "スキップ" }));
    fireEvent.click(screen.getByRole("button", { name: /LLM card/ }));
    fireEvent.click(screen.getByRole("button", { name: "次へ" }));

    await waitFor(() => expect(applySetupMock).toHaveBeenCalledTimes(1));
    expect(applySetupMock).toHaveBeenCalledWith({
      data_root: "/Volumes/Ext/lookback",
      preferred_language: "ja",
      settings: {
        embedding: null,
        hf_home: { mode: "data_root", path: null },
        llm: expect.objectContaining({
          mode: "local",
          local_preset_id: "qwen3-5-9b-ud-q4-k-xl",
        }),
        mcp: null,
        timezone: null,
      },
    });
  });

  it("shows the embedding card in the embedding step", () => {
    renderWizard();
    fireEvent.click(screen.getByRole("button", { name: "設定をはじめる" }));
    fireEvent.click(screen.getByRole("button", { name: "次へ" }));

    expect(screen.getByText("Embedding card")).toBeInTheDocument();
  });

  it("passes the pending embedding selection to the LLM step", () => {
    renderWizard();
    fireEvent.click(screen.getByRole("button", { name: "設定をはじめる" }));
    fireEvent.click(screen.getByRole("button", { name: "次へ" }));
    fireEvent.click(screen.getByRole("button", { name: "Pick light embedding" }));
    fireEvent.click(screen.getByRole("button", { name: "次へ" }));

    expect(screen.getByText(/LLM card qwen3-embedding-0-6b/)).toBeInTheDocument();
  });

  it("does not close on backdrop click", () => {
    renderWizard();
    const overlay = screen.getByText("Lookback セットアップ").closest(".modal-overlay");
    expect(overlay).not.toBeNull();
    if (overlay) fireEvent.click(overlay);
    expect(screen.getByText("Lookback セットアップ")).toBeInTheDocument();
  });
});
