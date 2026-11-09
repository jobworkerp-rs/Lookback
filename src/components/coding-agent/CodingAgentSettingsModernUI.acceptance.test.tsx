import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, within } from "@testing-library/react";
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
  cloneTransport: { kind: "https", baseUrl: "https://forge.example" },
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

function EditorProbe() {
  const editor = useCodingAgentSettingsEditor();
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

function renderHarness(settingsResponse: CodingAgentSettingsResponse = response) {
  vi.mocked(codingAgentGetSettings).mockResolvedValue(settingsResponse);
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

function profileDialog() {
  return screen.getByRole("dialog", { name: /プロフィールを編集/ });
}

function dirtyConfirmDialog() {
  return screen.getByRole("dialog", { name: /未保存の編集|Unsaved edits/ });
}

function closeWithEscape() {
  fireEvent.keyDown(window, { key: "Escape" });
}

describe("Coding agent modern settings acceptance", () => {
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

  it("opens a sibling three-action confirm on a dirty profile Escape and keeps the draft local", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = profileDialog();
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Escaped profile" },
    });
    closeWithEscape();

    const confirm = dirtyConfirmDialog();
    expect(confirm).not.toBe(dialog);
    expect(within(confirm).getByRole("button", { name: "変更を反映して閉じる" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "破棄して閉じる" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "編集を続ける" })).toHaveFocus();
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(within(confirm).getByRole("button", { name: "編集を続ける" }));
    expect(screen.queryByRole("dialog", { name: "未保存の編集" })).toBeNull();
    expect(profileDialog()).toBeInTheDocument();
    expect(within(profileDialog()).getByLabelText("表示名")).toHaveValue("Escaped profile");

    closeWithEscape();
    fireEvent.click(
      within(dirtyConfirmDialog()).getByRole("button", { name: "変更を反映して閉じる" }),
    );
    expect(screen.queryByRole("dialog", { name: /プロフィールを編集/ })).toBeNull();
    expect(screen.getAllByText("Escaped profile").length).toBeGreaterThan(0);
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "プロフィールを編集" }));
    const discardedDialog = profileDialog();
    fireEvent.change(within(discardedDialog).getByLabelText("表示名"), {
      target: { value: "Discarded profile" },
    });
    closeWithEscape();
    fireEvent.click(within(dirtyConfirmDialog()).getByRole("button", { name: "破棄して閉じる" }));
    expect(screen.queryByRole("dialog", { name: /プロフィールを編集/ })).toBeNull();
    expect(screen.getAllByText("Escaped profile").length).toBeGreaterThan(0);
    expect(screen.queryByText("Discarded profile")).toBeNull();
  });

  it("discards a dirty host from the independent confirm without losing an earlier shared profile draft", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    let dialog = profileDialog();
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Earlier profile draft" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );

    fireEvent.click(screen.getByRole("button", { name: "接続先を編集" }));
    const hostDialog = screen.getByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(hostDialog).getByLabelText("HTTPSクローン用ベースURL 1"), {
      target: { value: "https://forge.example/draft" },
    });
    closeWithEscape();

    const confirm = dirtyConfirmDialog();
    expect(confirm).not.toBe(hostDialog);
    expect(within(confirm).getByRole("button", { name: "変更を反映して閉じる" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "破棄して閉じる" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "編集を続ける" })).toBeEnabled();
    fireEvent.click(within(confirm).getByRole("button", { name: "編集を続ける" }));
    expect(screen.getByRole("dialog", { name: /リポジトリホストを編集/ })).toBe(hostDialog);
    expect(within(hostDialog).getByLabelText("HTTPSクローン用ベースURL 1")).toHaveValue(
      "https://forge.example/draft",
    );

    closeWithEscape();
    fireEvent.click(
      within(dirtyConfirmDialog()).getByRole("button", { name: "変更を反映して閉じる" }),
    );
    expect(screen.queryByRole("dialog", { name: /リポジトリホストを編集/ })).toBeNull();
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "接続先を編集" }));
    const discardedHostDialog = screen.getByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(discardedHostDialog).getByLabelText("HTTPSクローン用ベースURL 1"), {
      target: { value: "https://forge.example/discard" },
    });
    closeWithEscape();
    fireEvent.click(within(dirtyConfirmDialog()).getByRole("button", { name: "破棄して閉じる" }));
    expect(screen.queryByRole("dialog", { name: /リポジトリホストを編集/ })).toBeNull();
    expect(screen.getAllByText("Earlier profile draft").length).toBeGreaterThan(0);
    expect(screen.getByTestId("editor-dirty")).toHaveTextContent("true");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "接続先を編集" }));
    const retainedHostDialog = screen.getByRole("dialog", { name: /リポジトリホストを編集/ });
    expect(within(retainedHostDialog).getByLabelText("HTTPSクローン用ベースURL 1")).toHaveValue(
      "https://forge.example/draft",
    );
    fireEvent.click(within(retainedHostDialog).getByRole("button", { name: "キャンセル" }));

    fireEvent.click(screen.getByRole("button", { name: "プロフィールを編集" }));
    dialog = profileDialog();
    expect(within(dialog).getByLabelText("表示名")).toHaveValue("Earlier profile draft");
  });

  it("closes clean dialogs directly and leaves invalid drafts usable except for the primary action", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    closeWithEscape();
    expect(screen.queryByRole("dialog", { name: /プロフィールを編集/ })).toBeNull();
    expect(screen.queryByRole("dialog", { name: "未保存の編集" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "プロフィールを編集" }));
    const invalidDialog = profileDialog();
    fireEvent.change(within(invalidDialog).getByLabelText("コンテナイメージ"), {
      target: { value: "" },
    });
    expect(
      within(invalidDialog).getByRole("button", {
        name: i18n.t("codingAgent.settings.applyChanges"),
      }),
    ).toBeDisabled();
    expect(within(invalidDialog).getByRole("button", { name: "キャンセル" })).toBeEnabled();
    expect(within(invalidDialog).getByLabelText("表示名")).toBeEnabled();

    closeWithEscape();
    const confirm = dirtyConfirmDialog();
    expect(within(confirm).getByRole("button", { name: "変更を反映して閉じる" })).toBeDisabled();
    expect(within(confirm).getByRole("button", { name: "破棄して閉じる" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "編集を続ける" })).toBeEnabled();
  });

  it("keeps help interaction inside the profile dialog, including touch and Escape", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = profileDialog();
    const displayName = within(dialog).getByLabelText("表示名");
    fireEvent.change(displayName, { target: { value: "Help draft" } });
    const helpButton = within(dialog).getByRole("button", {
      name: i18n.t("codingAgent.settings.imageRefHelpLabel"),
    });
    fireEvent.focus(helpButton);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("tooltip")).toBeNull();
    expect(screen.getByRole("dialog", { name: /プロフィールを編集/ })).toBe(dialog);
    expect(displayName).toHaveValue("Help draft");
    fireEvent.touchStart(helpButton);
    fireEvent.touchEnd(helpButton);
    fireEvent.click(helpButton);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
  });

  it("keeps the independent close confirmation accessible in English", async () => {
    i18n.changeLanguage("en");
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "Edit profile" }));
    const dialog = screen.getByRole("dialog", { name: /Edit profile/ });
    fireEvent.change(within(dialog).getByLabelText("Display name"), {
      target: { value: "English draft" },
    });
    closeWithEscape();

    const confirm = dirtyConfirmDialog();
    expect(within(confirm).getByRole("button", { name: "Apply changes and close" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "Discard and close" })).toBeEnabled();
    expect(within(confirm).getByRole("button", { name: "Continue editing" })).toHaveFocus();
  });

  it("keeps a row warning readable and opens the profile repair surface with its issue code", async () => {
    const warningResponse: CodingAgentSettingsResponse = {
      ...response,
      settings: {
        ...response.settings,
        profiles: [
          {
            ...profile,
            allowedRepositories: [
              { host: "missing.example", organization: "team", repository: "project" },
            ],
          },
        ],
      },
    };
    renderHarness(warningResponse);

    const warning = await screen.findByRole("button", {
      name: i18n.t("codingAgent.settings.unregisteredRepositoryHost"),
    });
    expect(screen.queryByText("repository_host_not_found")).toBeNull();
    fireEvent.mouseEnter(warning);
    expect(screen.getByRole("tooltip")).toHaveTextContent(/\S/);
    fireEvent.focus(warning);
    expect(screen.getByRole("tooltip")).toBeInTheDocument();
    fireEvent.mouseLeave(warning);
    fireEvent.blur(warning);
    fireEvent.click(warning);
    const profileDialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    expect(within(profileDialog).getByRole("alert")).toHaveAttribute(
      "data-issue-code",
      "repository_host_not_found",
    );
    expect(
      within(profileDialog).getByLabelText(
        "許可するrepository（host/org/repository、1行につき1件）",
      ),
    ).toBeVisible();
  });
});
