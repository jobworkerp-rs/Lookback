import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { LOCAL_CONNECTION_CONFIG } from "@/lib/connectionConfig";
import { makeNativeSetupResponse, remoteConnectionConfig } from "@/test-utils";
import type { ConnectionConfig } from "@/types/api";
import { type EmbeddingFocus, Settings } from "./Settings";

// Settings renders every card, so the whole @/api surface is mocked. The
// view-switch tests only care about which cards are visible and the
// in-component leave-guard, so most mocks return innocuous defaults.
const applySettings = vi.fn();
const setConnectionConfig = vi.fn();
const getLlmSettings = vi.fn();
const listLlmPresets = vi.fn();
const getEmbeddingSettings = vi.fn();
const listEmbeddingPresets = vi.fn();
const getAppSettings = vi.fn();
const getConnectionConfig = vi.fn();
const getMemoryEmbeddingStats = vi.fn();
const getBackgroundJobQueueStatus = vi.fn();
const getEmbeddingSpace = vi.fn();
const inspectEmbeddingMigration = vi.fn();
const recoverEmbeddingMigration = vi.fn();
const startEmbeddingRebuild = vi.fn();
const startEmbeddingReconcile = vi.fn();
const planEmbeddingChange = vi.fn();
const switchEmbeddingSpace = vi.fn();
const getModelStatus = vi.fn();
const getSettings = vi.fn();
const codingAgentGetSettings = vi.fn();
const codingAgentUpdateSettings = vi.fn();
const codingAgentGetBackendStatus = vi.fn();
const codingAgentGetNativeSetup = vi.fn();
const codingAgentApproveNativeBackend = vi.fn();
const getMcpSettings = vi.fn();
const getIssueTrackerSettings = vi.fn();
const getSearchIndexMaintenanceStatus = vi.fn();
const startSearchIndexMaintenance = vi.fn();
const setSearchIndexMaintenanceSchedule = vi.fn();

vi.mock("@/api", () => ({
  applySettings: (req: unknown) => applySettings(req),
  inspectRemoteJobworkerp: () => new Promise(() => {}),
  getLlmSettings: () => getLlmSettings(),
  listLlmPresets: () => listLlmPresets(),
  getEmbeddingSettings: () => getEmbeddingSettings(),
  listEmbeddingPresets: () => listEmbeddingPresets(),
  getMcpSettings: () => getMcpSettings(),
  getIssueTrackerSettings: () => getIssueTrackerSettings(),
  getAppSettings: () => getAppSettings(),
  getConnectionConfig: () => getConnectionConfig(),
  getEmbeddingSpace: () => getEmbeddingSpace(),
  inspectEmbeddingMigration: (...args: unknown[]) => inspectEmbeddingMigration(...args),
  recoverEmbeddingMigration: (...args: unknown[]) => recoverEmbeddingMigration(...args),
  startEmbeddingRebuild: (...args: unknown[]) => startEmbeddingRebuild(...args),
  startEmbeddingReconcile: (...args: unknown[]) => startEmbeddingReconcile(...args),
  planEmbeddingChange: (req: unknown) => planEmbeddingChange(req),
  switchEmbeddingSpace: (req: unknown) => switchEmbeddingSpace(req),
  getMemoryEmbeddingStats: () => getMemoryEmbeddingStats(),
  getBackgroundJobQueueStatus: () => getBackgroundJobQueueStatus(),
  getModelStatus: () => getModelStatus(),
  getSettings: () => getSettings(),
  codingAgentGetSettings: () => codingAgentGetSettings(),
  codingAgentUpdateSettings: (request: unknown) => codingAgentUpdateSettings(request),
  codingAgentGetBackendStatus: () => codingAgentGetBackendStatus(),
  codingAgentGetNativeSetup: () => codingAgentGetNativeSetup(),
  codingAgentApproveNativeBackend: (request: unknown) => codingAgentApproveNativeBackend(request),
  getSearchIndexMaintenanceStatus: () => getSearchIndexMaintenanceStatus(),
  getWorkEligibility: () => Promise.resolve({ work_eligibility: "REVIEWED_ONLY" }),
  setWorkEligibility: () => Promise.resolve({ work_eligibility: "REVIEWED_ONLY" }),
  getIssueReviewPolicy: () => Promise.resolve({ require_high_priority_reason: false }),
  setIssueReviewPolicy: () => Promise.resolve({ require_high_priority_reason: false }),
  getAutoIssuePolicy: () =>
    Promise.resolve({
      version: 1,
      changed_at_ms: 0,
      policy: {
        extraction_enabled: true,
        auto_register_min_confidence: 0.9,
        auto_register_priority_range: { lower: "P0", upper: "P2" },
        explicit_only: true,
        allowed_kinds: [],
        extract_cap: 5,
      },
    }),
  setAutoIssuePolicy: () => Promise.resolve(),
  startSearchIndexMaintenance: () => startSearchIndexMaintenance(),
  setSearchIndexMaintenanceSchedule: (req: unknown) => setSearchIndexMaintenanceSchedule(req),
  setConnectionConfig: (cfg: unknown) => setConnectionConfig(cfg),
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
  model_max_output_tokens: null,
  summary_output_guidance: {
    small: {
      daily: { item_chars: 200, total_chars: 8000 },
      weekly: { item_chars: 350, total_chars: 18000 },
      monthly: { item_chars: 500, total_chars: 40000 },
    },
    standard: {
      daily: { item_chars: 300, total_chars: 12000 },
      weekly: { item_chars: 500, total_chars: 30000 },
      monthly: { item_chars: 800, total_chars: 80000 },
    },
    large: {
      daily: { item_chars: 500, total_chars: 20000 },
      weekly: { item_chars: 800, total_chars: 50000 },
      monthly: { item_chars: 1200, total_chars: 120000 },
    },
    limits: { daily: 10, weekly: 20, monthly: 50, max_source_memory_ids: 10 },
  },
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
  migration_backup: { enabled: true, keep: 1 },
  resolved: {
    current_data_root: "/root",
    default_data_root: "/root",
    effective_hf_home: "/hf",
    pending_data_root: "/root",
  },
};

const CODING_AGENT_PROFILE = {
  profileId: "profile-one",
  version: 1,
  displayName: "Primary agent",
  imageRef: "registry.example/agent:latest",
  allowedImageRefs: ["registry.example/agent:latest"],
  command: ["agent"],
  containerHome: "/workspace",
  runtimeEnvironment: {},
  networkAccess: "public" as const,
  resources: {
    cpuMillis: 1000,
    memoryMib: 1024,
    rootDiskMib: 4096,
    maxParallelRuns: 1,
    useStatic: true,
  },
  mountAllowedRoots: [],
  persistentMounts: [],
  allowedRepositories: [{ host: "forge.example", organization: "team", repository: "repo" }],
  credentialReference: null,
};

const CODING_AGENT_HOST = {
  labelHost: "forge.example",
  forge: "github" as const,
  apiBaseUrl: "https://forge.example/api/v3",
  cloneTransport: { kind: "https" as const, baseUrl: "https://forge.example" },
  readCredentialRef: null,
  publicationCredentialRef: null,
};

function codingAgentResponse(displayName = CODING_AGENT_PROFILE.displayName, revision = 1) {
  return {
    settings: {
      formatVersion: 1,
      revision,
      activeProfileId: CODING_AGENT_PROFILE.profileId,
      semanticJudgeEnabled: true,
      autoDoneEnabled: false,
      profiles: [{ ...CODING_AGENT_PROFILE, displayName }],
      repositoryHosts: [CODING_AGENT_HOST],
    },
    profileMountAvailability: [],
    availableAgentPresets: [],
  };
}

const SETTINGS_SNAPSHOT = {
  data_root: "/root",
  sqlite_path: "/root/db.sqlite",
  plugins_path: "/root/plugins",
  models_path: "/root/models",
  log_path: "/root/log",
  jobworkerp_url: "http://127.0.0.1:9000",
  memories_url: "http://127.0.0.1:9010",
};

function renderSettings(props?: Parameters<typeof Settings>[0]) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidateSpy = vi.spyOn(client, "invalidateQueries");
  const result = render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <Settings {...props} />
      </QueryClientProvider>
    </I18nextProvider>,
  );
  return { ...result, invalidateSpy };
}

async function findReadyIssueTrackerToggle() {
  const toggle = await screen.findByRole("checkbox", {
    name: /Issue Tracker を有効にする/,
  });
  await waitFor(() => expect(toggle).toBeEnabled());
  return toggle;
}

describe("Settings view switching", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    setConnectionConfig.mockReset().mockResolvedValue(undefined);
    applySettings.mockReset();
    applySettings.mockResolvedValue({
      restarted: true,
      embedding_runtime: null,
      warnings: [],
      mcp_auth_token: null,
    });
    getLlmSettings.mockResolvedValue(LLM_SETTINGS);
    listLlmPresets.mockResolvedValue(LLM_PRESETS);
    getEmbeddingSettings.mockResolvedValue(EMB_SETTINGS);
    listEmbeddingPresets.mockResolvedValue(EMB_PRESETS);
    getMcpSettings.mockResolvedValue({
      enabled: false,
      allow_unauthenticated_requests: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      rag_default_period_scope: "overall_only",
      set_name: "lookback-mcp",
      active_port: null,
      auth_token_configured: false,
      runtime_status: "disabled",
      unavailable_reason: null,
    });
    getIssueTrackerSettings.mockResolvedValue({
      enabled: false,
      effective_enabled: false,
    });
    getAppSettings.mockResolvedValue(APP_SETTINGS);
    getConnectionConfig.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    getEmbeddingSpace.mockResolvedValue({
      registration_state: "registered",
      current_space: null,
      mm_embedding_worker_name: "worker",
      tables: [],
      rpc_migration_supported: true,
      write_rejections: {},
    });
    inspectEmbeddingMigration.mockReset().mockResolvedValue({
      kind: "inspect",
      data: {
        state: "consistent",
        space: { kind: "none" },
        required: 0,
        complete: 0,
        missing: 0,
        stale: 0,
        unverified: 0,
        failed_transient: 0,
        failed_permanent: 0,
        orphan: 0,
        backup_supported: true,
        attempt: null,
        backup: null,
        unavailable_reason: null,
        next_action: "none",
      },
    });
    recoverEmbeddingMigration.mockReset().mockResolvedValue({
      result: {
        kind: "restored",
        data: {
          outcome: "restored",
          attempt: "attempt-1",
          space: { kind: "none" },
          next_action: "reconcile",
        },
      },
      restarted: true,
    });
    startEmbeddingRebuild
      .mockReset()
      .mockResolvedValue({ attempt_id: "attempt-1", task_id: "rebuild-task" });
    startEmbeddingReconcile.mockReset().mockResolvedValue({ task_id: "reconcile-task" });
    planEmbeddingChange.mockReset().mockResolvedValue({
      kind: "plan",
      data: {
        decision: "no_change",
        executable: true,
        reason: null,
        current_space: { kind: "id", value: "source-space" },
        target_space: "target-space",
        memory_text: 10,
        memory_media: 2,
        thread: 3,
        reflection_intent: 1,
        failed_transient: 0,
        failed_permanent: 0,
      },
    });
    switchEmbeddingSpace.mockReset().mockResolvedValue({
      result: {
        kind: "switched",
        data: { outcome: "switched", attempt: "attempt-1", next_action: "start_rebuild" },
      },
      restarted: false,
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
    codingAgentGetSettings.mockResolvedValue({
      settings: {
        formatVersion: 1,
        revision: 1,
        activeProfileId: null,
        semanticJudgeEnabled: true,
        autoDoneEnabled: false,
        profiles: [],
        repositoryHosts: [],
      },
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    codingAgentUpdateSettings.mockReset();
    codingAgentGetBackendStatus.mockResolvedValue({
      enabled: false,
      executorVersion: null,
      executorProtocolVersion: null,
      executorFeatures: [],
      appManagedJobworkerpReady: false,
      blockers: ["coding_agent_executor_not_bundled"],
    });
    codingAgentGetNativeSetup.mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: 1,
        state: "unavailable",
        packageFingerprint: null,
      }),
    );
    codingAgentApproveNativeBackend.mockReset();
    startSearchIndexMaintenance.mockReset();
    startSearchIndexMaintenance.mockResolvedValue(undefined);
    setSearchIndexMaintenanceSchedule.mockReset();
    setSearchIndexMaintenanceSchedule.mockResolvedValue(undefined);
    getSearchIndexMaintenanceStatus.mockResolvedValue({
      memory: { write_generation: 0, dirty: false, ready_runtime_secs: 0, last_result: null },
      thread: { write_generation: 0, dirty: false, ready_runtime_secs: 0, last_result: null },
      reconcile_pending: false,
      writing: false,
      optimizing: false,
      active_optimizations: [],
      memory_eligible: false,
      thread_eligible: false,
      recovery_required: null,
      schedule: { enabled: false, start: null, end: null },
    });
  });

  it("hides the Issue Tracker toggle and experimental policy cards when tracking is not configurable", async () => {
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: false });

    expect(
      screen.queryByRole("checkbox", { name: /Issue Tracker を有効にする/ }),
    ).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Issues 設定 (experimental)")).not.toBeInTheDocument();
    expect(
      await screen.findByText("Issue機能が無効なため、現在のIssue処理では使用されません。"),
    ).toBeInTheDocument();
  });

  it("shows the Issue Tracker toggle when configurable even if the effective capability is disabled", async () => {
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: true });

    expect(
      await screen.findByRole("checkbox", { name: /Issue Tracker を有効にする/ }),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText("Issues 設定 (experimental)")).not.toBeInTheDocument();
  });

  it("sends the Issue Tracker toggle in the basic apply payload", async () => {
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: true });
    const toggle = await findReadyIssueTrackerToggle();

    fireEvent.click(toggle);
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    expect(applySettings).toHaveBeenCalledWith(
      expect.objectContaining({
        issue_tracker: { enabled: true },
        embedding: null,
        mcp: null,
      }),
    );
  });

  it("sends an ON-to-OFF Issue Tracker toggle in the basic apply payload", async () => {
    getIssueTrackerSettings.mockResolvedValue({ enabled: true, effective_enabled: true });
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: true });
    const toggle = await findReadyIssueTrackerToggle();

    expect(toggle).toBeChecked();
    fireEvent.click(toggle);
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    expect(applySettings).toHaveBeenCalledWith(
      expect.objectContaining({
        issue_tracker: { enabled: false },
        embedding: null,
        mcp: null,
      }),
    );
  });

  it("invalidates capabilities and Issue Tracker settings after applying the toggle", async () => {
    const { invalidateSpy } = renderSettings({
      experimentalIssuesEnabled: false,
      issueTrackingConfigurable: true,
    });
    const toggle = await findReadyIssueTrackerToggle();
    fireEvent.click(toggle);
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    await waitFor(() => {
      expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: ["app-capabilities"] });
      expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: ["issue-tracker-settings"] });
    });
  });

  it("reflects the refreshed effective Issue Tracker state after applying", async () => {
    getIssueTrackerSettings
      .mockResolvedValueOnce({ enabled: false, effective_enabled: false })
      .mockResolvedValueOnce({ enabled: true, effective_enabled: true });
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: true });
    const toggle = await findReadyIssueTrackerToggle();

    fireEvent.click(toggle);
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    await waitFor(() => expect(toggle).toBeChecked());
    expect(screen.queryByText(/未保存の変更があります/)).not.toBeInTheDocument();
  });

  it("discards a pending Issue Tracker toggle", async () => {
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: true });
    const toggle = await findReadyIssueTrackerToggle();

    fireEvent.click(toggle);
    expect(toggle).toBeChecked();
    fireEvent.click(await screen.findByText("破棄"));

    await waitFor(() => expect(toggle).not.toBeChecked());
    expect(applySettings).not.toHaveBeenCalled();
  });

  it("keeps the Issue Tracker edit dirty when applying fails", async () => {
    applySettings.mockRejectedValue(new Error("Issue Tracker apply failed"));
    renderSettings({ experimentalIssuesEnabled: false, issueTrackingConfigurable: true });
    const toggle = await findReadyIssueTrackerToggle();

    fireEvent.click(toggle);
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    expect(await screen.findByText("Issue Tracker apply failed")).toBeInTheDocument();
    expect(toggle).toBeChecked();
    expect(screen.getByText(/未保存の変更があります/)).toBeInTheDocument();
  });

  it("shows the experimental Issues settings only when the capability is enabled", async () => {
    renderSettings({ experimentalIssuesEnabled: true, issueTrackingConfigurable: true });

    expect(
      await screen.findByRole("checkbox", { name: /Issue Tracker を有効にする/ }),
    ).toBeInTheDocument();
    expect(await screen.findByLabelText("Issues 設定 (experimental)")).toBeInTheDocument();
  });

  it("localizes the Issues section heading and description", async () => {
    i18n.changeLanguage("en");
    renderSettings({ experimentalIssuesEnabled: true, issueTrackingConfigurable: true });

    expect(await screen.findByLabelText("Issues settings (experimental)")).toBeInTheDocument();
    expect(
      screen.getByText(
        "Configure Issue requirement assessment, completion decisions, and related automation.",
      ),
    ).toBeInTheDocument();
  });

  it("shows only the basic cards by default", async () => {
    renderSettings();
    // Basic-view cards.
    await screen.findByText("LLM Provider");
    expect(screen.getByText("Embedding index")).toBeInTheDocument();
    expect(screen.getByText("Hugging Face cache (HF_HOME)")).toBeInTheDocument();
    expect(screen.queryByText("Logs")).toBeNull();
    // Advanced-view cards hidden.
    expect(screen.queryByText("Connection")).toBeNull();
    expect(screen.queryByText("Embedding model")).toBeNull();
    expect(screen.queryByText("Data location")).toBeNull();
    expect(screen.queryByText("Destructive")).toBeNull();
    // Entry button present.
    expect(screen.getByText("高度な設定を開く…")).toBeInTheDocument();
    fireEvent.click(screen.getByText("高度な設定を開く…"));
    expect((await screen.findAllByText("Logs")).length).toBeGreaterThan(0);
  });

  it("connects the coding-agent Settings card and keeps an unconfirmed backend state neutral", async () => {
    renderSettings();

    const card = await screen.findByRole("region", { name: i18n.t("codingAgent.settings.title") });
    const help = await within(card).findByRole("button", {
      name: i18n.t("codingAgent.settings.settingsHelpTitle"),
    });
    expect(
      within(card).queryByText(i18n.t("codingAgent.settings.backendStatusNeutral")),
    ).toBeNull();

    fireEvent.click(help);
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      i18n.t("codingAgent.settings.settingsHelpDescription"),
    );

    const blockers = within(card).getByRole("button", {
      name: i18n.t("codingAgent.settings.backendBlockers"),
    });
    fireEvent.click(blockers);
    expect(within(card).queryByText("coding_agent_executor_not_bundled")).toBeNull();
    const diagnostic = await screen.findByRole("dialog", {
      name: i18n.t("codingAgent.settings.backendBlockers"),
    });
    expect(within(diagnostic).getByText("coding_agent_executor_not_bundled")).toBeVisible();
    expect(codingAgentGetSettings).toHaveBeenCalled();
    expect(codingAgentGetBackendStatus).toHaveBeenCalled();
  });

  it("saves coding-agent-only edits through the common footer", async () => {
    const response = codingAgentResponse();
    codingAgentGetSettings.mockResolvedValue(response);
    codingAgentUpdateSettings.mockResolvedValue({
      ...response,
      settings: {
        ...response.settings,
        revision: 2,
        profiles: [{ ...CODING_AGENT_PROFILE, displayName: "Edited agent" }],
      },
    });

    renderSettings();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Edited agent" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    const save = await screen.findByRole("button", { name: "保存して適用" });
    fireEvent.click(save);
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 1,
      settings: expect.objectContaining({
        profiles: [expect.objectContaining({ displayName: "Edited agent" })],
      }),
    });
    expect(applySettings).not.toHaveBeenCalled();
    expect(screen.queryByText(/未保存の変更があります/)).not.toBeInTheDocument();
  });

  it("retries a Coding Agent CAS after an initial conflict", async () => {
    const response = codingAgentResponse();
    codingAgentGetSettings.mockResolvedValue(response);
    codingAgentUpdateSettings
      .mockRejectedValueOnce(new Error("CAS failed"))
      .mockResolvedValueOnce(codingAgentResponse("Retried agent", 2));

    renderSettings();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Retried agent" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    const save = await screen.findByRole("button", { name: "保存して適用" });
    fireEvent.click(save);
    expect(await screen.findByText("CAS failed")).toBeInTheDocument();
    expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(screen.getByRole("button", { name: "保存して適用" })).toBeEnabled());

    fireEvent.click(screen.getByRole("button", { name: "保存して適用" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(2));
    expect(applySettings).not.toHaveBeenCalled();
    expect(screen.queryByText(/未保存の変更があります/)).not.toBeInTheDocument();
  });

  it("clears Coding Agent dialog state when discarding a view move", async () => {
    codingAgentGetSettings.mockResolvedValue(codingAgentResponse());
    renderSettings();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.click(screen.getByText("高度な設定を開く…"));
    expect(await screen.findByText("保存していない変更があります")).toBeInTheDocument();

    fireEvent.click(screen.getByText("破棄して移動"));
    await screen.findByText("Connection");
    fireEvent.click(screen.getByText("← 設定に戻る"));
    await screen.findByText("LLM Provider");

    fireEvent.click(screen.getByText("高度な設定を開く…"));
    await screen.findByText("Connection");
    expect(screen.queryByText("保存していない変更があります")).not.toBeInTheDocument();
  });

  it("does not resend a successful CAS when a later sidecar save fails", async () => {
    const response = codingAgentResponse();
    codingAgentGetSettings.mockResolvedValue(response);
    codingAgentUpdateSettings.mockResolvedValue(codingAgentResponse("Edited agent", 2));
    applySettings.mockRejectedValueOnce(new Error("sidecar save failed"));

    renderSettings();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Edited agent" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    const presetLabel = await screen.findByText("プリセット");
    const preset = presetLabel.closest(".settings-row")?.querySelector("select");
    if (!(preset instanceof HTMLSelectElement)) throw new Error("LLM preset selector is missing");
    await waitFor(() => expect(preset.options.length).toBeGreaterThan(1));
    fireEvent.change(preset, { target: { value: "qwen3-5-9b-ud-q4-k-xl" } });
    await screen.findByText(/未保存の変更があります/);

    fireEvent.click(screen.getByRole("button", { name: "保存して適用 (再起動)" }));
    expect(await screen.findByText("sidecar save failed")).toBeInTheDocument();
    expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1);
    expect(applySettings).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "保存して適用 (再起動)" })).toBeEnabled();

    fireEvent.click(screen.getByRole("button", { name: "保存して適用 (再起動)" }));
    await waitFor(() => expect(applySettings).toHaveBeenCalledTimes(2));
    expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1);
  });

  it("states each card's restart targets and job interruption risk", async () => {
    renderSettings();
    await screen.findByText("LLM Provider");
    expect(
      screen.getByText(/安全に hot reload できない変更では jobworkerp を再起動/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/保存時に jobworkerp を再起動します。実行中の生成/),
    ).toBeInTheDocument();
    expect(screen.getByText(/保存時に jobworkerp・conductor を再起動/)).toBeInTheDocument();

    fireEvent.click(screen.getByText("高度な設定を開く…"));
    await screen.findByText("MCP Server");
    expect(screen.getByText(/jobworkerp・memories を再起動/)).toBeInTheDocument();
    expect(screen.getByText(/変わったサービスに応じて sidecar を再構成/)).toBeInTheDocument();
    expect(screen.getAllByText(/保存時に jobworkerp を再起動します。実行中の生成/)).toHaveLength(2);
  });

  it("states each card's restart targets and job interruption risk", async () => {
    renderSettings();
    await screen.findByText("LLM Provider");
    expect(
      screen.getByText(/安全に hot reload できない変更では jobworkerp を再起動/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/保存時に jobworkerp を再起動します。実行中の生成/),
    ).toBeInTheDocument();
    expect(screen.getByText(/保存時に jobworkerp・conductor を再起動/)).toBeInTheDocument();

    fireEvent.click(screen.getByText("高度な設定を開く…"));
    await screen.findByText("MCP Server");
    expect(screen.getByText(/jobworkerp・memories を再起動/)).toBeInTheDocument();
    expect(screen.getByText(/変わったサービスに応じて sidecar を再構成/)).toBeInTheDocument();
    expect(screen.getAllByText(/保存時に jobworkerp を再起動します。実行中の生成/)).toHaveLength(2);
  });

  it("shows active index optimization and prevents another manual start", async () => {
    getSearchIndexMaintenanceStatus.mockResolvedValue({
      memory: {
        write_generation: 3,
        dirty: true,
        ready_runtime_secs: 3600,
        last_result: "accepted",
      },
      thread: { write_generation: 2, dirty: true, ready_runtime_secs: 1800, last_result: null },
      reconcile_pending: false,
      writing: false,
      optimizing: true,
      active_optimizations: [{ table: "memory", task_id: "maintenance-1", state: "accepted" }],
      memory_eligible: false,
      thread_eligible: false,
      recovery_required: null,
      schedule: { enabled: false, start: null, end: null },
    });

    renderSettings();

    const button = await screen.findByRole("button", { name: "最適化中…" });
    expect(button).toBeDisabled();
    expect(screen.getByText("最適化中です。完了までお待ちください。")).toBeInTheDocument();
    expect(screen.getByText("memory: 最適化中 / 未反映 / 稼働 1h")).toBeInTheDocument();
    expect(screen.getByText("thread: 待機中 / 未反映 / 稼働 0h")).toBeInTheDocument();
    fireEvent.click(button);
    expect(startSearchIndexMaintenance).not.toHaveBeenCalled();
  });

  it("enables maintenance-window saving only after scheduled optimization is enabled", async () => {
    renderSettings();

    await screen.findByText("memory: 待機中 / 最適化済み / 稼働 0h");
    const save = await screen.findByRole("button", { name: "保守時間帯を保存" });
    expect(save).toBeDisabled();

    fireEvent.click(screen.getByRole("checkbox", { name: "未使用時間帯に定期最適化" }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "保守時間帯を保存" })).toBeEnabled(),
    );

    fireEvent.click(screen.getByRole("button", { name: "保守時間帯を保存" }));
    await waitFor(() =>
      expect(setSearchIndexMaintenanceSchedule).toHaveBeenCalledWith({
        enabled: true,
        start: "23:00",
        end: "02:00",
      }),
    );
  });

  it("disables timezone edits when the saved connection mode is remote", async () => {
    getConnectionConfig.mockResolvedValue(remoteConnectionConfig());

    renderSettings();

    const help = await screen.findByText(i18n.t("settings.timezone.remoteDisabled"));
    const card = help.closest(".settings-card");
    const select = card?.querySelector("select");
    expect(select).toBeDisabled();
  });

  it("keeps timezone edits available while only memories is remote", async () => {
    getConnectionConfig.mockResolvedValue({
      ...LOCAL_CONNECTION_CONFIG,
      memories: remoteConnectionConfig().memories,
    });

    renderSettings();

    await screen.findByText("LLM Provider");
    await waitFor(() => expect(getConnectionConfig).toHaveBeenCalled());
    expect(screen.queryByText(i18n.t("settings.timezone.remoteDisabled"))).toBeNull();
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
    expect(req.issue_tracker).toBeNull();
    expect(req.embedding).toMatchObject({ preset_id: "custom", custom_model_id: "org/model" });
  });

  it("reviews a local migration plan before applying an embedding change", async () => {
    planEmbeddingChange.mockResolvedValue({
      kind: "plan",
      data: {
        decision: "reembed_required",
        executable: true,
        reason: null,
        current_space: { kind: "id", value: "source-space" },
        target_space: "target-space",
        memory_text: 10,
        memory_media: 2,
        thread: 3,
        reflection_intent: 1,
        failed_transient: 0,
        failed_permanent: 0,
      },
    });
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Embedding model");
    await screen.findByText(/現在の設定/);

    const select = screen
      .getByText("Embedding model")
      .closest(".settings-card")
      ?.querySelector("select") as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("org/name"), {
      target: { value: "org/model" },
    });
    fireEvent.change(screen.getByPlaceholderText("e.g. 1024"), { target: { value: "1024" } });
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    await waitFor(() =>
      expect(planEmbeddingChange).toHaveBeenCalledWith(
        expect.objectContaining({ custom_model_id: "org/model", custom_vector_size: 1024 }),
      ),
    );
    expect(await screen.findByText(/再生成対象: Memory text 10/)).toBeInTheDocument();
    expect(applySettings).not.toHaveBeenCalled();
    expect(switchEmbeddingSpace).not.toHaveBeenCalled();
  });

  it("does not apply local embedding settings when migration planning fails", async () => {
    planEmbeddingChange.mockResolvedValue({
      kind: "failed",
      data: {
        command: "plan_embedding_change",
        stage: "plan",
        error_code: "resource_unavailable",
        resolution: "check_environment",
        attempt: null,
        backup: null,
        cancel_operation: null,
        verification_counts: null,
      },
    });
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Embedding model");
    await screen.findByText(/現在の設定/);

    const select = screen
      .getByText("Embedding model")
      .closest(".settings-card")
      ?.querySelector("select") as HTMLSelectElement;
    await waitFor(() => expect(select).toBeEnabled());
    fireEvent.change(select, { target: { value: "custom" } });
    fireEvent.change(screen.getByPlaceholderText("org/name"), {
      target: { value: "org/model" },
    });
    fireEvent.change(screen.getByPlaceholderText("e.g. 1024"), { target: { value: "1024" } });
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    expect(
      await screen.findByText(/plan_embedding_change.*resource_unavailable/),
    ).toBeInTheDocument();
    expect(applySettings).not.toHaveBeenCalled();
    expect(screen.getByText(/未保存の変更があります/)).toBeInTheDocument();
  });

  it.each([
    ["jobworkerp and memories", remoteConnectionConfig()],
    ["memories only", { ...LOCAL_CONNECTION_CONFIG, memories: remoteConnectionConfig().memories }],
  ])(
    "keeps remote embedding settings saveable without local migration operations (%s remote)",
    async (_, config) => {
      getConnectionConfig.mockResolvedValue(config);
      getEmbeddingSettings.mockResolvedValue({ ...EMB_SETTINGS, connection_remote: true });
      renderSettings();
      fireEvent.click(await screen.findByText("高度な設定を開く…"));
      await screen.findByText("Embedding model");

      fireEvent.change(screen.getByLabelText("Model revision"), {
        target: { value: "remote-revision" },
      });
      fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

      await waitFor(() =>
        expect(applySettings).toHaveBeenCalledWith(
          expect.objectContaining({
            embedding: expect.objectContaining({ model_revision: "remote-revision" }),
          }),
        ),
      );
      expect(planEmbeddingChange).not.toHaveBeenCalled();
      expect(switchEmbeddingSpace).not.toHaveBeenCalled();
      expect(inspectEmbeddingMigration).not.toHaveBeenCalled();
    },
  );

  it("waits for the connection mode before planning or saving local embedding edits", async () => {
    let resolveConnection: ((value: ConnectionConfig) => void) | null = null;
    getConnectionConfig.mockReturnValueOnce(
      new Promise<ConnectionConfig>((resolve) => {
        resolveConnection = resolve;
      }),
    );
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Embedding model");
    fireEvent.change(await screen.findByLabelText("Model revision"), {
      target: { value: "revision-waiting" },
    });

    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));

    expect(await screen.findByText(/接続モードの確認後にEmbedding設定を保存/)).toBeInTheDocument();
    expect(planEmbeddingChange).not.toHaveBeenCalled();
    expect(applySettings).not.toHaveBeenCalled();
    expect(switchEmbeddingSpace).not.toHaveBeenCalled();
    expect(inspectEmbeddingMigration).not.toHaveBeenCalled();
    expect(await screen.findByLabelText("Model revision")).toHaveValue("revision-waiting");
    expect(screen.getByText(/未保存の変更があります/)).toBeInTheDocument();

    await act(async () => {
      resolveConnection?.(LOCAL_CONNECTION_CONFIG);
    });
    fireEvent.click(await screen.findByText("保存して適用 (再起動)"));
    await waitFor(() => expect(planEmbeddingChange).toHaveBeenCalled());
  });

  it("sends the migration backup choice in the advanced apply payload", async () => {
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    const toggle = await screen.findByRole("checkbox", { name: /移行前にバックアップを作成/ });
    await waitFor(() => expect(toggle).toBeChecked());

    fireEvent.click(toggle);
    await screen.findByText(/未保存の変更があります/);
    fireEvent.click(screen.getByText("保存して適用 (再起動)"));

    await waitFor(() => expect(applySettings).toHaveBeenCalled());
    const req = applySettings.mock.lastCall?.[0];
    expect(req.migration_backup).toEqual({ enabled: false, keep: 1 });
    expect(req.embedding).toBeNull();
    expect(req.llm).toBeNull();
  });

  it("applies a pending MCP token rotation once and displays the returned token after save", async () => {
    getMcpSettings.mockResolvedValue({
      enabled: true,
      allow_unauthenticated_requests: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      rag_default_period_scope: "overall_only",
      set_name: "lookback-mcp",
      active_port: 39010,
      auth_token_configured: true,
      runtime_status: "ready",
      unavailable_reason: null,
    });
    applySettings.mockResolvedValue({
      restarted: true,
      embedding_runtime: null,
      warnings: [],
      mcp_auth_token: "saved-rotated-token",
    });
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("MCP Server");
    fireEvent.click(screen.getByRole("button", { name: "Regenerate auth token" }));
    await screen.findByText(/保存時に jobworkerp を再起動して新しい token を反映します/);

    fireEvent.click(screen.getByText("保存して適用 (再起動)"));
    await waitFor(() =>
      expect(applySettings).toHaveBeenCalledWith(
        expect.objectContaining({ mcp: expect.objectContaining({ rotate_auth_token: true }) }),
      ),
    );
    await screen.findByText("saved-rotated-token");
    expect(screen.queryByText(/未保存の変更があります/)).toBeNull();
  });

  it("discards a pending MCP token rotation without saving it", async () => {
    getMcpSettings.mockResolvedValue({
      enabled: true,
      allow_unauthenticated_requests: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      rag_default_period_scope: "overall_only",
      set_name: "lookback-mcp",
      active_port: 39010,
      auth_token_configured: true,
      runtime_status: "ready",
      unavailable_reason: null,
    });
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("MCP Server");
    fireEvent.click(screen.getByRole("button", { name: "Regenerate auth token" }));
    await screen.findByText(/保存時に jobworkerp を再起動して新しい token を反映します/);

    fireEvent.click(screen.getByText("破棄"));
    await waitFor(() => expect(screen.queryByText(/未保存の変更があります/)).toBeNull());
    expect(applySettings).not.toHaveBeenCalled();
  });

  it("keeps a pending MCP token rotation and shows the save failure without revealing a token", async () => {
    getMcpSettings.mockResolvedValue({
      enabled: true,
      allow_unauthenticated_requests: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      rag_default_period_scope: "overall_only",
      set_name: "lookback-mcp",
      active_port: 39010,
      auth_token_configured: true,
      runtime_status: "ready",
      unavailable_reason: null,
    });
    applySettings.mockRejectedValue(new Error("MCP restart failed"));
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("MCP Server");
    fireEvent.click(screen.getByRole("button", { name: "Regenerate auth token" }));
    fireEvent.click(screen.getByText("保存して適用 (再起動)"));

    await screen.findAllByText("MCP restart failed");
    expect(
      screen.getByText(/保存時に jobworkerp を再起動して新しい token を反映します/),
    ).toBeInTheDocument();
    expect(screen.queryByText("saved-rotated-token")).toBeNull();
    expect(screen.getByText(/未保存の変更があります/)).toBeInTheDocument();
  });

  it("guards the view switch when the Connection card (advanced) is edited", async () => {
    // An in-progress Connection edit must arm the leave-guard, or switching
    // back to basic drops the input silently. Here: switch memories to
    // Remote without its URL, then try to leave.
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    await screen.findByText("Connection");
    // Switch memories to remote → the form now differs from the persisted
    // (local) config, so the card is edited.
    fireEvent.click(
      within(await screen.findByRole("group", { name: "memories" })).getByText("Remote server"),
    );
    expect(screen.getByPlaceholderText("http://host:9010")).toBeInTheDocument();
    // The save bar shows the edit but cannot apply it without the URL.
    expect(screen.getByText("保存して適用 (再起動)")).toBeDisabled();
    expect(setConnectionConfig).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("← 設定に戻る"));
    expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();
    // Cancel keeps us on advanced with the edit intact.
    fireEvent.click(screen.getByText("キャンセル"));
    expect(screen.getByText("Connection")).toBeInTheDocument();
  });

  it("applies a Connection change only from the advanced save bar", async () => {
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    const memories = within(await screen.findByRole("group", { name: "memories" }));
    fireEvent.click(memories.getByText("Remote server"));
    fireEvent.change(memories.getByRole("textbox", { name: "memories URL" }), {
      target: { value: "http://10.0.0.2:9010" },
    });
    expect(setConnectionConfig).not.toHaveBeenCalled();

    fireEvent.click(screen.getByText("保存して適用 (再起動)"));

    await waitFor(() =>
      expect(setConnectionConfig).toHaveBeenCalledWith({
        ...LOCAL_CONNECTION_CONFIG,
        memories: remoteConnectionConfig().memories,
      }),
    );
    // Nothing else was edited, so no other settings are applied.
    await waitFor(() => expect(screen.queryByText("保存して適用 (再起動)")).toBeNull());
    expect(applySettings).not.toHaveBeenCalled();
  });

  it("keeps a Connection edit whose apply failed", async () => {
    setConnectionConfig.mockRejectedValue("a remote jobworkerp needs a remote memories");
    renderSettings();
    fireEvent.click(await screen.findByText("高度な設定を開く…"));
    const memories = within(await screen.findByRole("group", { name: "memories" }));
    fireEvent.click(memories.getByText("Remote server"));
    fireEvent.change(memories.getByRole("textbox", { name: "memories URL" }), {
      target: { value: "http://10.0.0.2:9010" },
    });

    fireEvent.click(screen.getByText("保存して適用 (再起動)"));

    expect(
      await screen.findByText("a remote jobworkerp needs a remote memories"),
    ).toBeInTheDocument();
    expect(screen.getByDisplayValue("http://10.0.0.2:9010")).toBeInTheDocument();
    expect(screen.getByText("保存して適用 (再起動)")).toBeEnabled();
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
    fireEvent.click(
      within(await screen.findByRole("group", { name: "memories" })).getByText("Remote server"),
    );
    expect(screen.getByPlaceholderText("http://host:9010")).toBeInTheDocument();
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
