import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  codingAgentCheckRepositoryHost,
  codingAgentGetBackendStatus,
  codingAgentGetNativeSetup,
  codingAgentGetSettings,
  codingAgentPrepareProfileHome,
  codingAgentUpdateSettings,
} from "@/api";
import i18n from "@/i18n";
import { makeNativeSetupResponse } from "@/test-utils";
import type {
  CodingAgentProfile,
  CodingAgentRepositoryHost,
  CodingAgentSettingsResponse,
} from "@/types/api";
import { CodingAgentSettingsCard } from "./CodingAgentSettingsCard";
import {
  type CodingAgentSettingsEditor,
  CodingAgentSettingsEditorProvider,
  useCodingAgentSettingsEditor,
} from "./CodingAgentSettingsEditorContext";

vi.mock("@/api", () => ({
  codingAgentApproveNativeBackend: vi.fn(),
  codingAgentCheckRepositoryHost: vi.fn(),
  codingAgentGetBackendStatus: vi.fn(),
  codingAgentGetNativeSetup: vi.fn(),
  codingAgentGetSettings: vi.fn(),
  codingAgentPrepareProfileHome: vi.fn(),
  codingAgentUpdateSettings: vi.fn(),
}));

vi.mock("./ProfileSettingsTerminalPanel", () => ({
  ProfileSettingsTerminalPanel: () => <div data-testid="profile-settings-terminal" />,
}));

const profile: CodingAgentProfile = {
  profileId: "profile-one",
  version: 1,
  displayName: "Primary agent",
  imageRef: "registry.example/agent:latest",
  allowedImageRefs: ["registry.example/agent:latest"],
  command: ["agent", "--interactive"],
  containerHome: "/workspace",
  runtimeEnvironment: {
    CODEX_HOME: "/home/codex",
    CUSTOM_AGENT_SETTING: "keep-this-value",
  },
  networkAccess: "public",
  resources: {
    cpuMillis: 1000,
    memoryMib: 1024,
    rootDiskMib: 4096,
    maxParallelRuns: 1,
    useStatic: true,
  },
  mountAllowedRoots: ["/home/user/coding"],
  persistentMounts: [],
  allowedRepositories: [{ host: "forge.example", organization: "team", repository: "project" }],
  credentialReference: null,
};

const repositoryHost: CodingAgentRepositoryHost = {
  labelHost: "forge.example",
  forge: "github",
  apiBaseUrl: "https://forge.example/api/v3",
  cloneTransport: { kind: "https" as const, baseUrl: "https://forge.example" },
  readCredentialRef: null,
  publicationCredentialRef: null,
};

const response: CodingAgentSettingsResponse = {
  settings: {
    formatVersion: 1,
    revision: 8,
    activeProfileId: "profile-one",
    semanticJudgeEnabled: true,
    semanticJudgePolicy: { maxRetries: 1, deadlineSeconds: 60 },
    autoDoneEnabled: false,
    profiles: [profile],
    repositoryHosts: [repositoryHost],
  },
  profileMountAvailability: [],
  availableAgentPresets: [],
};

function EditorProbe({ onEditor }: { onEditor?: (editor: CodingAgentSettingsEditor) => void }) {
  const editor = useCodingAgentSettingsEditor();
  onEditor?.(editor);
  return (
    <div data-testid="editor-probe">
      <span data-testid="editor-dirty">{String(editor.dirty)}</span>
      <span data-testid="editor-valid">{String(editor.valid)}</span>
      <button
        type="button"
        disabled={!editor.dirty || !editor.valid || editor.saving}
        onClick={() => void editor.save()}
      >
        Save draft
      </button>
    </div>
  );
}

function DirtyDraftProbe() {
  const editor = useCodingAgentSettingsEditor();
  return (
    <button
      type="button"
      onClick={() =>
        editor.update((settings) => ({
          ...settings,
          semanticJudgeEnabled: !settings.semanticJudgeEnabled,
        }))
      }
    >
      Make draft dirty
    </button>
  );
}

function AcceptanceHarness() {
  const [editor, setEditor] = useState<CodingAgentSettingsEditor | null>(null);
  return (
    <CodingAgentSettingsEditorProvider onChange={setEditor}>
      {editor && (
        <>
          <EditorProbe />
          <CodingAgentSettingsCard
            editor={editor}
            showIssuePolicies={false}
            showLocalSaveControls={false}
            showInlineSetupGuide={false}
          />
        </>
      )}
    </CodingAgentSettingsEditorProvider>
  );
}

function DirtyAcceptanceHarness() {
  const [editor, setEditor] = useState<CodingAgentSettingsEditor | null>(null);
  return (
    <CodingAgentSettingsEditorProvider onChange={setEditor}>
      {editor && (
        <>
          <EditorProbe />
          <DirtyDraftProbe />
          <CodingAgentSettingsCard
            editor={editor}
            showIssuePolicies={false}
            showLocalSaveControls={false}
            showInlineSetupGuide={false}
          />
        </>
      )}
    </CodingAgentSettingsEditorProvider>
  );
}

function renderHarness() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <AcceptanceHarness />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

describe("Coding agent settings list and dialogs", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.clearAllMocks();
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "unavailable",
        packageFingerprint: null,
      }),
    );
    vi.mocked(codingAgentGetSettings).mockResolvedValue(response);
    vi.mocked(codingAgentGetBackendStatus).mockResolvedValue({
      enabled: true,
      executorVersion: "test",
      executorProtocolVersion: 1,
      executorFeatures: [],
      appManagedJobworkerpReady: true,
      blockers: [],
    });
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValue({
      settingsRevision: response.settings.revision,
      repository: null,
      stages: [],
    });
    vi.mocked(codingAgentPrepareProfileHome).mockResolvedValue(response);
    vi.mocked(codingAgentUpdateSettings).mockResolvedValue({
      ...response,
      settings: { ...response.settings, revision: response.settings.revision + 1 },
    });
  });

  it("keeps the repository connection check available for pristine settings", async () => {
    renderHarness();

    expect(await screen.findByRole("button", { name: "接続先を確認" })).toBeEnabled();
  });

  it("disables the connection check after applying SSH edits until the shared save succeeds", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByLabelText("クローンの接続方式 1"), {
      target: { value: "ssh" },
    });
    fireEvent.change(within(dialog).getByLabelText("SSHホスト名 1"), {
      target: { value: "forge.example" },
    });
    fireEvent.change(within(dialog).getByLabelText("SSHユーザー名 1"), {
      target: { value: "git" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    expect(screen.getByRole("button", { name: "接続先を確認" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "接続確認の説明" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      i18n.t("codingAgent.settings.repositoryHostDraftHint"),
    );
    expect(screen.getByRole("tooltip")).not.toHaveTextContent(
      "codingAgent.settings.repositoryHostDraftHint",
    );
    fireEvent.click(screen.getByRole("button", { name: "接続確認の説明" }));

    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.getByRole("button", { name: "接続先を確認" })).toBeEnabled());
  });

  it("disables the connection check after applying an approval-only edit", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    fireEvent.change(within(dialog).getByLabelText("リポジトリホスト 1"), {
      target: { value: "forge.example" },
    });
    fireEvent.change(within(dialog).getByLabelText("リポジトリ組織 1"), {
      target: { value: "team" },
    });
    fireEvent.change(within(dialog).getByLabelText("リポジトリ名 1"), {
      target: { value: "project" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    expect(screen.getByRole("button", { name: "接続先を確認" })).toBeDisabled();
  });

  it("keeps the connection check disabled while the shared save is pending", async () => {
    let resolveSave: ((value: CodingAgentSettingsResponse) => void) | undefined;
    vi.mocked(codingAgentUpdateSettings).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveSave = resolve;
        }),
    );
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByLabelText("HTTPSクローン用ベースURL 1"), {
      target: { value: "https://forge.example/pending" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));

    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("button", { name: "接続先を確認" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "接続確認の説明" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      i18n.t("codingAgent.settings.repositoryHostSavingHint"),
    );
    fireEvent.click(screen.getByRole("button", { name: "接続確認の説明" }));

    resolveSave?.({
      ...response,
      settings: { ...response.settings, revision: response.settings.revision + 1 },
    });
    await waitFor(() => expect(screen.getByRole("button", { name: "接続先を確認" })).toBeEnabled());
  });

  it("does not start a host check if settings become dirty after opening its modal", async () => {
    render(
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider
          client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
        >
          <DirtyAcceptanceHarness />
        </QueryClientProvider>
      </I18nextProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "接続先を確認" }));
    const dialog = await screen.findByRole("dialog", { name: "接続先を確認" });
    fireEvent.click(screen.getByRole("button", { name: "Make draft dirty" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "設定・接続を確認" }));

    expect(codingAgentCheckRepositoryHost).not.toHaveBeenCalled();
  });

  it("keeps profile edits local until the dialog is confirmed", async () => {
    renderHarness();
    const editButton = await screen.findByRole("button", { name: "プロフィールを編集" });
    fireEvent.click(editButton);

    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Edited agent" },
    });
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("false");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    expect(screen.getAllByText("Edited agent").length).toBeGreaterThan(0);
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("discards local dialog changes after the close confirmation", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Discarded agent" },
    });

    fireEvent.click(within(dialog).getByRole("button", { name: "キャンセル" }));
    const confirmDialog = await screen.findByRole("dialog", { name: "未保存の編集" });
    expect(confirmDialog).not.toBe(dialog);
    fireEvent.click(within(confirmDialog).getByRole("button", { name: "破棄して閉じる" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: /プロフィールを編集/ })).toBeNull(),
    );
    expect(screen.getAllByText("Primary agent").length).toBeGreaterThan(0);
    expect(screen.queryByText("Discarded agent")).toBeNull();
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("false");
  });

  it("preserves unknown environment entries when the confirmed draft is saved", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Saved agent" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));

    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const saved = vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings;
    expect(saved).toBeDefined();
    if (!saved) return;
    expect(saved.profiles[0]?.displayName).toBe("Saved agent");
    expect(saved.profiles[0]?.runtimeEnvironment?.CUSTOM_AGENT_SETTING).toBe("keep-this-value");
  });

  it("keeps an invalid repository draft visible and blocks the shared save", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText(/許可するrepository/), {
      target: { value: "missing.example/team/project" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    const warning = screen.getByRole("alert");
    expect(
      within(warning).getByRole("button", {
        name: i18n.t("codingAgent.settings.unregisteredRepositoryHost"),
      }),
    ).toBeInTheDocument();
    expect(warning).toHaveAttribute("data-issue-code", "repository_host_not_found");
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");
    expect(screen.getByTestId("editor-valid")).toHaveTextContent("false");
    expect(screen.getByRole("button", { name: "Save draft" })).toBeDisabled();
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("keeps profile and repository host dialog drafts independent", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const profileDialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(profileDialog).getByLabelText("表示名"), {
      target: { value: "Profile draft" },
    });
    fireEvent.click(
      within(profileDialog).getByRole("button", {
        name: i18n.t("codingAgent.settings.applyChanges"),
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: "接続先を編集" }));
    const hostDialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(hostDialog).getByLabelText("HTTPSクローン用ベースURL 1"), {
      target: { value: "https://forge.example/scm" },
    });
    fireEvent.click(
      within(hostDialog).getByRole("button", {
        name: i18n.t("codingAgent.settings.applyChanges"),
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const updateCall = vi.mocked(codingAgentUpdateSettings).mock.calls[0];
    expect(updateCall).toBeDefined();
    if (!updateCall) return;
    const saved = updateCall[0].settings;
    expect(saved).toBeDefined();
    if (!saved) return;
    expect(saved.profiles[0]?.displayName).toBe("Profile draft");
    expect(saved.repositoryHosts[0]?.labelHost).toBe("forge.example");
    expect(saved.repositoryHosts[0]?.apiBaseUrl).toBe("https://forge.example/api/v3");
    expect(saved.repositoryHosts[0]?.readCredentialRef).toBeNull();
    expect(saved.repositoryHosts[0]?.cloneTransport).toEqual({
      kind: "https",
      baseUrl: "https://forge.example/scm",
    });
  });

  it("keeps detailed profile edits local on cancel and includes confirmed resource and mount changes in the save payload", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    let dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Profile draft" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    fireEvent.click(screen.getByRole("button", { name: "プロフィールを編集" }));
    dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("CPU millicores"), {
      target: { value: "2000" },
    });
    fireEvent.click(within(dialog).getByText("永続mount", { selector: "summary" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Mount を追加" }));
    fireEvent.change(within(dialog).getByLabelText("Host path 1"), {
      target: { value: "/home/user/coding" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "キャンセル" }));
    const confirmDialog = await screen.findByRole("dialog", { name: "未保存の編集" });
    expect(confirmDialog).not.toBe(dialog);
    expect(
      within(confirmDialog).getByText(i18n.t("codingAgent.settings.confirmDiscardDialog")),
    ).toBeInTheDocument();
    expect(within(confirmDialog).getByRole("button", { name: "編集を続ける" })).toBeEnabled();
    fireEvent.click(within(confirmDialog).getByRole("button", { name: "破棄して閉じる" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: /プロフィールを編集/ })).toBeNull(),
    );
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");

    fireEvent.click(screen.getByRole("button", { name: "プロフィールを編集" }));
    dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    expect(within(dialog).getByLabelText("CPU millicores")).toHaveValue(1000);
    fireEvent.click(within(dialog).getByText("永続mount", { selector: "summary" }));
    expect(within(dialog).queryByLabelText("Host path 1")).toBeNull();
    fireEvent.change(within(dialog).getByLabelText("CPU millicores"), {
      target: { value: "2000" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "Mount を追加" }));
    fireEvent.change(within(dialog).getByLabelText("Host path 1"), {
      target: { value: "/home/user/coding" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));

    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const updateCall = vi.mocked(codingAgentUpdateSettings).mock.calls[0];
    expect(updateCall).toBeDefined();
    if (!updateCall) return;
    const saved = updateCall[0].settings;
    expect(saved).toBeDefined();
    if (!saved) return;
    expect(saved.profiles[0]?.displayName).toBe("Profile draft");
    expect(saved.profiles[0]?.resources.cpuMillis).toBe(2000);
    expect(saved.profiles[0]?.persistentMounts[0]).toMatchObject({
      hostPath: "/home/user/coding",
      guestPath: "/workspace",
    });
  });

  it("does not add an unconfirmed repository host and saves it only after confirmation", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "ホストを追加" }));
    let dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByLabelText("リポジトリラベルのホスト名 2"), {
      target: { value: "new.example" },
    });
    fireEvent.change(within(dialog).getByLabelText("APIベースURL 2"), {
      target: { value: "https://new.example/api/v3" },
    });
    fireEvent.change(within(dialog).getByLabelText("HTTPSクローン用ベースURL 2"), {
      target: { value: "https://new.example" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "キャンセル" }));
    const confirmDialog = await screen.findByRole("dialog", { name: "未保存の編集" });
    fireEvent.click(within(confirmDialog).getByRole("button", { name: "破棄して閉じる" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: /リポジトリホストを編集/ })).toBeNull(),
    );
    expect(screen.queryByText("new.example")).toBeNull();
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("false");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "ホストを追加" }));
    dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByLabelText("リポジトリラベルのホスト名 2"), {
      target: { value: "new.example" },
    });
    fireEvent.change(within(dialog).getByLabelText("APIベースURL 2"), {
      target: { value: "https://new.example/api/v3" },
    });
    fireEvent.change(within(dialog).getByLabelText("HTTPSクローン用ベースURL 2"), {
      target: { value: "https://new.example" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    expect(screen.getAllByText("new.example").length).toBeGreaterThan(0);
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const updateCall = vi.mocked(codingAgentUpdateSettings).mock.calls[0];
    expect(updateCall).toBeDefined();
    if (!updateCall) return;
    const saved = updateCall[0].settings;
    expect(saved).toBeDefined();
    if (!saved) return;
    expect(saved.repositoryHosts).toContainEqual({
      labelHost: "new.example",
      forge: "github",
      apiBaseUrl: "https://new.example/api/v3",
      cloneTransport: { kind: "https", baseUrl: "https://new.example" },
      readCredentialRef: null,
      publicationCredentialRef: null,
    });
  });

  it("shows a field error and disables host confirmation for an invalid URL", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    const apiBaseUrl = within(dialog).getByLabelText("APIベースURL 1");
    fireEvent.change(apiBaseUrl, { target: { value: "not-a-url" } });

    expect(apiBaseUrl).toHaveAttribute("aria-invalid", "true");
    expect(within(dialog).getByRole("alert")).toBeInTheDocument();
    expect(
      within(dialog).getByRole("button", {
        name: i18n.t("codingAgent.settings.applyChanges"),
      }),
    ).toBeDisabled();
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });
});
