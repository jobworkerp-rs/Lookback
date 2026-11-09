import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
import { makeNativeSetupResponse, renderWithProviders } from "@/test-utils";
import type { CodingAgentProfile, CodingAgentSettings } from "@/types/api";
import { CodingAgentSettingsCard } from "./CodingAgentSettingsCard";
import {
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
  ProfileSettingsTerminalPanel: ({
    profileId,
    revision,
    dirty,
    credentialReference,
  }: {
    profileId: string;
    revision: number;
    dirty: boolean;
    credentialReference: string | null;
  }) => (
    <div data-testid="profile-terminal-settings">
      {`${profileId}:${revision}:${dirty}`}
      {credentialReference && <span data-testid="credential-reference">{credentialReference}</span>}
    </div>
  ),
}));

function ExternalCodingAgentSettingsCard() {
  const editor = useCodingAgentSettingsEditor();
  return (
    <CodingAgentSettingsCard
      editor={editor}
      showIssuePolicies={false}
      showLocalSaveControls={false}
      showInlineSetupGuide={false}
    />
  );
}

function renderExternalCodingAgentSettingsCard() {
  return renderWithProviders(
    <CodingAgentSettingsEditorProvider>
      <ExternalCodingAgentSettingsCard />
    </CodingAgentSettingsEditorProvider>,
  );
}

function SharedDraftProbe() {
  const editor = useCodingAgentSettingsEditor();
  return <span data-testid="shared-draft-dirty">{String(editor.dirty)}</span>;
}

function renderExternalCodingAgentSettingsCardWithProbe() {
  return renderWithProviders(
    <CodingAgentSettingsEditorProvider>
      <SharedDraftProbe />
      <ExternalCodingAgentSettingsCard />
    </CodingAgentSettingsEditorProvider>,
  );
}

const baseSettings: CodingAgentSettings = {
  formatVersion: 1,
  revision: 6,
  activeProfileId: "profile-active",
  semanticJudgeEnabled: true,
  autoDoneEnabled: false,
  profiles: [
    {
      profileId: "profile-active",
      version: 3,
      displayName: "Existing profile",
      imageRef: "registry.example/agent@sha256:abc",
      allowedImageRefs: ["registry.example/agent@sha256:abc"],
      command: ["agent", "--interactive"],
      containerHome: "/workspace",
      resources: {
        cpuMillis: 2000,
        memoryMib: 2048,
        rootDiskMib: 4096,
        maxParallelRuns: 1,
        useStatic: false,
      },
      mountAllowedRoots: ["/home/user/coding"],
      persistentMounts: [
        {
          hostPath: "/home/user/coding/project",
          guestPath: "/workspace/project",
          readOnly: false,
          executable: false,
        },
      ],
      allowedRepositories: [{ host: "forge.example", organization: "team", repository: "project" }],
      credentialReference: "keyring:agent-login",
    },
  ],
  repositoryHosts: [
    {
      labelHost: "forge.example",
      forge: "github",
      apiBaseUrl: "https://forge.example/api/v3",
      cloneTransport: { kind: "https", baseUrl: "https://forge.example" },
      readCredentialRef: "keyring:repo-read",
      publicationCredentialRef: null,
    },
  ],
};

describe("CodingAgentSettingsCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.clearAllMocks();
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: baseSettings.revision,
        state: "unavailable",
        packageFingerprint: null,
      }),
    );
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: baseSettings,
      profileMountAvailability: [
        { profileId: "profile-active", available: false, blockers: ["mount_root_unavailable"] },
      ],
      availableAgentPresets: [],
    });
    vi.mocked(codingAgentGetBackendStatus).mockResolvedValue({
      enabled: false,
      executorVersion: null,
      executorProtocolVersion: null,
      executorFeatures: [],
      appManagedJobworkerpReady: false,
      blockers: ["coding_agent_executor_not_bundled", "host_workspace_freeze_unavailable"],
    });
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      settings: { ...settings, revision: 7 },
      profileMountAvailability: [{ profileId: "profile-active", available: true, blockers: [] }],
      availableAgentPresets: [],
    }));
  });

  it("shows setup guidance before profile details and keeps the saved revision for its controls", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);

    const setup = await screen.findByRole("region", { name: "事前設定" });
    expect(within(setup).getByRole("heading", { name: "事前設定" })).toBeInTheDocument();
    expect(
      within(setup).getByText(
        "設定を保存 → 専用homeを準備 → 認証・設定用シェルを開く、の順に進めます。実行 backend の probe とは別の操作です。",
      ),
    ).toBeInTheDocument();
    expect(within(setup).getByText("対象 profile: Existing profile")).toBeInTheDocument();
    expect(within(setup).getByRole("list", { name: "事前設定の手順" })).toBeInTheDocument();
    expect(within(setup).getByText("設定を保存", { selector: "strong" })).toBeInTheDocument();
    expect(within(setup).getByText("専用homeを準備", { selector: "strong" })).toBeInTheDocument();
    expect(
      within(setup).getByText("認証・設定用シェルを開く", { selector: "strong" }),
    ).toBeInTheDocument();
    expect(within(setup).getByText(/全設定を保存/)).toBeInTheDocument();
    expect(within(setup).getByText(/既存の書き込み可能なhome mount/)).toBeInTheDocument();
    expect(within(setup).getByRole("button", { name: "事前設定: 設定を保存" })).toBeDisabled();
    expect(within(setup).getByRole("button", { name: "専用homeを用意する" })).toBeInTheDocument();
    expect(within(setup).getByTestId("profile-terminal-settings")).toHaveTextContent(
      "profile-active:6:false",
    );
    expect(setup.compareDocumentPosition(screen.getByLabelText("表示名"))).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
  });

  it("shows the saving reason before the unsaved reason while a save is pending", async () => {
    vi.mocked(codingAgentUpdateSettings).mockImplementation(() => new Promise(() => {}));
    renderWithProviders(<CodingAgentSettingsCard />);

    fireEvent.change(await screen.findByLabelText("表示名"), {
      target: { value: "Saving profile" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));

    const setup = await screen.findByRole("region", { name: "事前設定" });
    expect(within(setup).getByRole("button", { name: "事前設定: 設定を保存" })).toBeDisabled();
    expect(
      within(setup).getByText(
        "設定を保存または専用homeを準備中です。完了するまで再実行できません。",
      ),
    ).toBeInTheDocument();
    expect(
      within(setup).queryByText(
        "未保存の変更があります。専用homeを用意する前に設定を保存してください。",
      ),
    ).toBeNull();
  });

  it("explains how to create a profile when no profile is selected", async () => {
    const emptySettings: CodingAgentSettings = {
      ...baseSettings,
      revision: 0,
      activeProfileId: null,
      profiles: [],
      repositoryHosts: [],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: emptySettings,
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    renderWithProviders(<CodingAgentSettingsCard />);

    const setup = await screen.findByRole("region", { name: "事前設定" });
    expect(within(setup).getByText(/プリセット.*プロフィール.*追加.*保存/)).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "プロフィールを追加" })).toHaveLength(1);
    expect(screen.getByRole("combobox", { name: "追加するプロフィールのプリセット" })).toHaveValue(
      "",
    );
    expect(
      screen.getByRole("option", { name: "プリセットなし（空のプロフィール）" }),
    ).toBeInTheDocument();
    expect(within(setup).queryByRole("button", { name: "専用homeを用意する" })).toBeNull();
    expect(within(setup).queryByRole("button", { name: "認証・設定用シェルを開く" })).toBeNull();
  });

  it("uses the same save conditions for the setup shortcut and the main save button", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const setup = await screen.findByRole("region", { name: "事前設定" });
    const setupSave = within(setup).getByRole("button", { name: "事前設定: 設定を保存" });
    const mainSave = screen.getByRole("button", { name: "設定を保存" });
    expect(setupSave).toBeDisabled();
    expect(mainSave).toBeDisabled();

    fireEvent.change(screen.getByRole("textbox", { name: "表示名" }), {
      target: { value: "Changed" },
    });
    expect(setupSave).toBeEnabled();
    expect(mainSave).toBeEnabled();
    fireEvent.click(setupSave);
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 6,
      settings: {
        ...baseSettings,
        profiles: baseSettings.profiles.map((profile) => ({
          ...profile,
          displayName: "Changed",
        })),
      },
    });
  });

  it("targets the selected profile when preparing a home among multiple profiles", async () => {
    const firstProfile = baseSettings.profiles[0];
    if (!firstProfile) throw new Error("base settings fixture has no profile");
    const secondProfile = {
      ...structuredClone(firstProfile),
      profileId: "profile-second",
      displayName: "Second profile",
      credentialReference: null,
      persistentMounts: [],
    };
    const multipleProfiles = {
      ...structuredClone(baseSettings),
      profiles: [...structuredClone(baseSettings.profiles), secondProfile],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: multipleProfiles,
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    vi.mocked(codingAgentPrepareProfileHome).mockResolvedValue({
      settings: multipleProfiles,
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    renderWithProviders(<CodingAgentSettingsCard />);

    fireEvent.change(await screen.findByRole("combobox", { name: "認証設定の対象" }), {
      target: { value: "profile-second" },
    });
    expect(screen.getByTestId("profile-terminal-settings")).toHaveTextContent(
      "profile-second:6:false",
    );
    fireEvent.click(screen.getByRole("button", { name: "専用homeを用意する" }));
    await waitFor(() =>
      expect(codingAgentPrepareProfileHome).toHaveBeenCalledWith({
        expectedRevision: 6,
        profileId: "profile-second",
      }),
    );
  });

  it("provisions the saved selected profile home and refreshes its mounts", async () => {
    const prepared = structuredClone(baseSettings);
    prepared.revision = 7;
    prepared.profiles = prepared.profiles.map((profile) => ({
      ...profile,
      persistentMounts: [
        {
          hostPath: "/app/profile-homes/first",
          guestPath: "/home/agent",
          readOnly: false,
          executable: false,
        },
      ],
    }));
    vi.mocked(codingAgentPrepareProfileHome).mockResolvedValue({
      settings: prepared,
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    renderWithProviders(<CodingAgentSettingsCard />);
    fireEvent.click(await screen.findByRole("button", { name: "専用homeを用意する" }));
    await waitFor(() =>
      expect(codingAgentPrepareProfileHome).toHaveBeenCalledWith({
        expectedRevision: 6,
        profileId: "profile-active",
      }),
    );
    await screen.findByDisplayValue("/app/profile-homes/first");
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
  });

  it("does not provision a home from an unsaved draft and shows provisioning errors", async () => {
    vi.mocked(codingAgentPrepareProfileHome).mockRejectedValue(new Error("profile is in use"));
    renderWithProviders(<CodingAgentSettingsCard />);
    const button = await screen.findByRole("button", { name: "専用homeを用意する" });
    fireEvent.change(screen.getByRole("textbox", { name: "表示名" }), {
      target: { value: "Unsaved" },
    });
    expect(button).toBeDisabled();
    expect(codingAgentPrepareProfileHome).not.toHaveBeenCalled();
    fireEvent.change(screen.getByRole("textbox", { name: "表示名" }), {
      target: { value: "Existing profile" },
    });
    fireEvent.click(button);
    await screen.findByText(
      "専用homeを用意できませんでした。設定の競合や利用中のprofileを確認してください。",
    );
    expect(screen.getByDisplayValue("/home/user/coding/project")).toBeInTheDocument();
  });

  it("locks editing while home preparation is pending", async () => {
    let resolve!: (response: Awaited<ReturnType<typeof codingAgentPrepareProfileHome>>) => void;
    vi.mocked(codingAgentPrepareProfileHome).mockReturnValue(
      new Promise((done) => {
        resolve = done;
      }),
    );
    renderWithProviders(<CodingAgentSettingsCard />);
    fireEvent.click(await screen.findByRole("button", { name: "専用homeを用意する" }));
    expect(screen.getByRole("textbox", { name: "表示名" })).toBeDisabled();
    const profileList = screen
      .getByRole("heading", { name: "実行プロフィール" })
      .closest("section");
    if (!profileList) throw new Error("profile list section is missing");
    expect(within(profileList).getByRole("button", { name: "プロフィールを追加" })).toBeDisabled();
    expect(screen.getByRole("checkbox", { name: "意味判定を実施する" })).toBeDisabled();
    resolve({ settings: baseSettings, profileMountAvailability: [], availableAgentPresets: [] });
    await waitFor(() => expect(screen.getByRole("textbox", { name: "表示名" })).toBeEnabled());
  });

  it("binds the settings terminal to the saved profile revision and blocks unsaved edits", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    expect(await screen.findByTestId("profile-terminal-settings")).toHaveTextContent(
      "profile-active:6:false",
    );
    fireEvent.change(screen.getByRole("textbox", { name: "表示名" }), {
      target: { value: "Unsaved" },
    });
    expect(screen.getByTestId("profile-terminal-settings")).toHaveTextContent(
      "profile-active:6:true",
    );
  });

  it("disables automatic completion with semantic judging and does not restore it on reenable", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const judge = await screen.findByRole("checkbox", { name: "意味判定を実施する" });
    const autoDone = screen.getByRole("checkbox", { name: "合格したIssueを自動完了する" });
    fireEvent.click(autoDone);
    expect(autoDone).toBeChecked();
    fireEvent.click(judge);
    expect(autoDone).not.toBeChecked();
    expect(autoDone).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() =>
      expect(codingAgentUpdateSettings).toHaveBeenCalledWith(
        expect.objectContaining({
          settings: expect.objectContaining({
            semanticJudgeEnabled: false,
            autoDoneEnabled: false,
          }),
        }),
      ),
    );
    fireEvent.click(judge);
    expect(autoDone).toBeEnabled();
    expect(autoDone).not.toBeChecked();
  });

  it("saves bounded semantic retry settings and rejects invalid numbers", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const retries = await screen.findByRole("spinbutton", { name: "意味判定の通信再試行回数" });
    const deadline = screen.getByRole("spinbutton", { name: "意味判定の総期限（秒）" });
    fireEvent.change(retries, { target: { value: "2" } });
    await waitFor(() => expect(retries).toHaveValue(2));
    fireEvent.change(deadline, { target: { value: "180" } });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() =>
      expect(codingAgentUpdateSettings).toHaveBeenCalledWith(
        expect.objectContaining({
          settings: expect.objectContaining({
            semanticJudgePolicy: { maxRetries: 2, deadlineSeconds: 180 },
          }),
        }),
      ),
    );
    for (const value of ["-1", "1.5", "6", ""]) {
      fireEvent.change(retries, { target: { value } });
      expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
    }
    fireEvent.change(retries, { target: { value: "0" } });
    fireEvent.change(deadline, { target: { value: "0" } });
    expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
    fireEvent.change(deadline, { target: { value: "120" } });
    fireEvent.click(screen.getByRole("checkbox", { name: "意味判定を実施する" }));
    expect(retries).toBeDisabled();
    expect(deadline).toBeDisabled();
  });

  it("saves explicit provider and private-network access and allows networking to be disabled", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const network = await screen.findByRole("combobox", { name: "Agentのネットワーク接続" });
    fireEvent.change(network, { target: { value: "public_and_private" } });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() =>
      expect(codingAgentUpdateSettings).toHaveBeenCalledWith(
        expect.objectContaining({
          settings: expect.objectContaining({
            profiles: [expect.objectContaining({ networkAccess: "public_and_private" })],
          }),
        }),
      ),
    );
    fireEvent.change(network, { target: { value: "disabled" } });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() =>
      expect(codingAgentUpdateSettings).toHaveBeenLastCalledWith(
        expect.objectContaining({
          settings: expect.objectContaining({
            profiles: [expect.objectContaining({ networkAccess: "disabled" })],
          }),
        }),
      ),
    );
  });

  it("rejects fractional virtual CPU counts and saves an exact whole CPU choice", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const cpu = await screen.findByLabelText("CPU millicores");
    fireEvent.change(cpu, { target: { value: "1500" } });
    expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
    fireEvent.change(cpu, { target: { value: "3000" } });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() =>
      expect(codingAgentUpdateSettings).toHaveBeenCalledWith(
        expect.objectContaining({
          settings: expect.objectContaining({
            profiles: [
              expect.objectContaining({ resources: expect.objectContaining({ cpuMillis: 3000 }) }),
            ],
          }),
        }),
      ),
    );
  });

  it("loads mount diagnostics but keeps profile repair editable when execution is blocked", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);

    expect(await screen.findByText(/実行環境の準備が完了していません/)).toBeInTheDocument();
    expect(screen.getByText(/mount_root_unavailable/)).toBeInTheDocument();
    expect(screen.getByLabelText("共有を許可するホスト側フォルダー")).toBeEnabled();
    expect(screen.getByLabelText("表示名")).toHaveValue("Existing profile");
    expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
    expect(screen.queryByLabelText(/quota authority|sudo|OS quota/i)).not.toBeInTheDocument();
  });

  it("renders translated load and retry labels without exposing raw native errors", async () => {
    vi.mocked(codingAgentGetSettings)
      .mockRejectedValueOnce(new Error("private token material"))
      .mockResolvedValueOnce({
        settings: baseSettings,
        profileMountAvailability: [],
        availableAgentPresets: [],
      });
    renderWithProviders(<CodingAgentSettingsCard />);

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Coding agent 設定を読み込めませんでした。");
    expect(alert).not.toHaveTextContent("private token material");
    fireEvent.click(screen.getByRole("button", { name: "再試行" }));
    expect(await screen.findByLabelText("表示名")).toHaveValue("Existing profile");
  });

  it("saves profile edits with the loaded CAS revision and preserves immutable active-profile and auto-DONE settings", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const name = await screen.findByLabelText("表示名");
    fireEvent.change(name, { target: { value: "Updated profile" } });

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));

    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 6,
      settings: {
        ...baseSettings,
        profiles: baseSettings.profiles.map((profile) => ({
          ...profile,
          displayName: "Updated profile",
        })),
      },
    });
    expect(await screen.findByText(/revision 7/)).toBeInTheDocument();
    expect(screen.getByText(/Active profile: profile-active/)).toBeInTheDocument();
  });

  it("does not disable mount repair when the backend reports an unavailable root", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const roots = await screen.findByLabelText("共有を許可するホスト側フォルダー");
    fireEvent.change(roots, { target: { value: "/home/user/coding\n/home/user/new-workspace" } });

    expect(screen.getByRole("button", { name: "設定を保存" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalled());
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith(
      expect.objectContaining({
        expectedRevision: 6,
        settings: expect.objectContaining({
          activeProfileId: "profile-active",
          profiles: [
            expect.objectContaining({
              mountAllowedRoots: ["/home/user/coding", "/home/user/new-workspace"],
            }),
          ],
        }),
      }),
    );
  });

  it("keeps the draft after a CAS/backend error and requires the user to resolve it", async () => {
    vi.mocked(codingAgentUpdateSettings).mockRejectedValue(new Error("settings revision conflict"));
    renderWithProviders(<CodingAgentSettingsCard />);
    const name = await screen.findByLabelText("表示名");
    fireEvent.change(name, { target: { value: "Draft after conflict" } });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /別の editor で設定が更新されました/,
    );
    expect(screen.getByRole("alert")).not.toHaveTextContent("settings revision conflict");
    expect(screen.getByLabelText("表示名")).toHaveValue("Draft after conflict");
    expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "設定を再読込して編集を破棄" })).toBeEnabled();
  });

  it("keeps the editor's revision-6 CAS base and does not merge a newer revision until explicit reload", async () => {
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const view = render(
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>
          <CodingAgentSettingsCard />
        </QueryClientProvider>
      </I18nextProvider>,
    );
    fireEvent.change(await screen.findByLabelText("表示名"), {
      target: { value: "Revision 6 draft" },
    });

    const serverRevision7 = {
      settings: {
        ...baseSettings,
        revision: 7,
        repositoryHosts: [
          ...baseSettings.repositoryHosts,
          {
            labelHost: "new.example",
            forge: "github" as const,
            apiBaseUrl: "https://new.example/api",
            cloneTransport: { kind: "https" as const, baseUrl: "https://new.example" },
            readCredentialRef: null,
            publicationCredentialRef: null,
          },
        ],
      },
      profileMountAvailability: baseSettings.profiles.map((profile) => ({
        profileId: profile.profileId,
        available: true,
        blockers: [],
      })),
      availableAgentPresets: [],
    };
    queryClient.setQueryData(["coding-agent-settings"], serverRevision7);
    vi.mocked(codingAgentUpdateSettings).mockRejectedValueOnce(
      new Error("settings revision conflict: sensitive backend detail"),
    );
    vi.mocked(codingAgentGetSettings).mockResolvedValue(serverRevision7);

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 6,
      settings: {
        ...baseSettings,
        profiles: baseSettings.profiles.map((profile) => ({
          ...profile,
          displayName: "Revision 6 draft",
        })),
      },
    });
    expect(await screen.findByRole("alert")).toHaveTextContent(/changed|競合|更新/);
    expect(screen.getByRole("alert")).not.toHaveTextContent("sensitive backend detail");
    expect(screen.getByLabelText("表示名")).toHaveValue("Revision 6 draft");
    expect(screen.getByRole("button", { name: "設定を再読込して編集を破棄" })).toBeEnabled();
    expect(screen.queryByDisplayValue("new.example")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "設定を再読込して編集を破棄" }));
    await waitFor(() => expect(screen.getByLabelText("表示名")).toHaveValue("Existing profile"));
    expect(screen.getByText(/revision 7/)).toBeInTheDocument();
    expect(screen.getByDisplayValue("new.example")).toBeInTheDocument();
    view.unmount();
    queryClient.clear();
  });

  it("creates an editable new profile from an empty registry with explicit nonzero native resources", async () => {
    const emptySettings: CodingAgentSettings = {
      ...baseSettings,
      revision: 0,
      activeProfileId: null,
      profiles: [],
      repositoryHosts: [],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: emptySettings,
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    renderWithProviders(<CodingAgentSettingsCard />);

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを追加" }));
    expect(screen.getByLabelText("表示名")).toHaveValue("");
    expect(screen.getByLabelText("コンテナイメージ")).toHaveValue("");
    expect(screen.getByLabelText("CLIコマンド引数（1行につき1項目）")).toHaveValue("");
    fireEvent.change(screen.getByLabelText("表示名"), { target: { value: "New pinned agent" } });
    fireEvent.change(screen.getByLabelText("コンテナイメージ"), {
      target: { value: "registry.example/agent@sha256:abc" },
    });
    fireEvent.change(screen.getByLabelText("許可するコンテナイメージ（1行につき1件）"), {
      target: { value: "registry.example/agent@sha256:abc" },
    });
    const command = screen.getByLabelText("CLIコマンド引数（1行につき1項目）");
    fireEvent.change(command, { target: { value: "  agent  \n--interactive  \n" } });
    expect(command).toHaveValue("  agent  \n--interactive  \n");
    expect(screen.getByLabelText("Static worker を使用")).toBeChecked();

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 0,
      settings: expect.objectContaining({
        activeProfileId: null,
        profiles: [
          expect.objectContaining({
            displayName: "New pinned agent",
            imageRef: "registry.example/agent@sha256:abc",
            allowedImageRefs: ["registry.example/agent@sha256:abc"],
            command: ["agent", "--interactive"],
            version: 0,
            resources: expect.objectContaining({ useStatic: true }),
            mountAllowedRoots: [],
            persistentMounts: [],
            allowedRepositories: [],
            credentialReference: null,
          }),
        ],
        repositoryHosts: [],
      }),
    });
    const call = vi.mocked(codingAgentUpdateSettings).mock.calls[0];
    if (!call) throw new Error("settings update was not called");
    const saved = call[0].settings.profiles[0];
    if (!saved) throw new Error("new profile was not saved");
    expect(saved.resources.cpuMillis % 1000).toBe(0);
    expect(saved.resources.cpuMillis).toBeGreaterThan(0);
    expect(saved.containerHome.startsWith("/workspace")).toBe(false);
  });

  it("creates empty or preset-filled profiles from one selector and keeps the active profile", async () => {
    const presetSettings = structuredClone(baseSettings);
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: presetSettings,
      profileMountAvailability: [{ profileId: "profile-active", available: true, blockers: [] }],
      availableAgentPresets: [
        {
          id: "openai-codex",
          displayName: "OpenAI Codex",
          imageRef: "ghcr.io/sutr-app/openai-codex:20261025_0",
          command: ["codex", "exec"],
          containerHome: "/home/codespace/.agent-home",
          runtimeEnvironment: {
            CODEX_HOME: "/home/codespace/.agent-home/.codex",
            CARGO_HOME: "/home/codespace/.cargo",
            RUSTUP_HOME: "/home/codespace/.rustup",
          },
        },
      ],
    });
    renderWithProviders(<CodingAgentSettingsCard />);

    const presetSelect = await screen.findByLabelText("追加するプロフィールのプリセット");
    expect(presetSelect).toHaveValue("");
    fireEvent.change(presetSelect, {
      target: { value: "openai-codex" },
    });
    const profileList = screen
      .getByRole("heading", { name: "実行プロフィール" })
      .closest("section");
    if (!profileList) throw new Error("profile list section is missing");
    const addProfile = within(profileList).getByRole("button", { name: "プロフィールを追加" });
    expect(within(profileList).getAllByRole("button", { name: "プロフィールを追加" })).toHaveLength(
      1,
    );
    fireEvent.click(addProfile);
    const firstProfileId = screen
      .getByTestId("profile-terminal-settings")
      .textContent?.split(":")[0];
    expect(screen.getByLabelText("表示名")).toHaveValue("OpenAI Codex");
    expect(screen.getByLabelText("コンテナイメージ")).toHaveValue(
      "ghcr.io/sutr-app/openai-codex:20261025_0",
    );
    expect(screen.getByLabelText("許可するコンテナイメージ（1行につき1件）")).toHaveValue(
      "ghcr.io/sutr-app/openai-codex:20261025_0",
    );
    expect(screen.getByLabelText("CLIコマンド引数（1行につき1項目）")).toHaveValue("codex\nexec");
    expect(screen.getByLabelText("コンテナhome")).toHaveValue("/home/codespace/.agent-home");
    expect(screen.getByLabelText("CODEX_HOME")).toHaveValue("/home/codespace/.agent-home/.codex");
    expect(screen.getByLabelText("CARGO_HOME")).toHaveValue("/home/codespace/.cargo");
    expect(screen.getByLabelText("RUSTUP_HOME")).toHaveValue("/home/codespace/.rustup");

    fireEvent.change(screen.getByLabelText("CODEX_HOME"), { target: { value: "" } });
    expect(screen.getByLabelText("CODEX_HOME")).toHaveValue("");

    fireEvent.change(screen.getByLabelText("表示名"), { target: { value: "Edited Codex" } });
    fireEvent.change(presetSelect, { target: { value: "" } });
    fireEvent.click(addProfile);
    const secondProfileId = screen
      .getByTestId("profile-terminal-settings")
      .textContent?.split(":")[0];
    expect(secondProfileId).not.toBe(firstProfileId);
    expect(screen.getByLabelText("表示名")).toHaveValue("");
    expect(screen.getByLabelText("コンテナイメージ")).toHaveValue("");
    expect(screen.getByLabelText("CLIコマンド引数（1行につき1項目）")).toHaveValue("");
    expect(screen.getByText(/Active profile: profile-active/)).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText("表示名"), { target: { value: "Manual profile" } });
    fireEvent.change(screen.getByLabelText("コンテナイメージ"), {
      target: { value: "registry.example/manual:stable" },
    });
    fireEvent.change(screen.getByLabelText("許可するコンテナイメージ（1行につき1件）"), {
      target: { value: "registry.example/manual:stable" },
    });
    fireEvent.change(screen.getByLabelText("CLIコマンド引数（1行につき1項目）"), {
      target: { value: "manual-agent" },
    });

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const saved = vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings;
    expect(saved?.activeProfileId).toBe("profile-active");
    expect(saved?.profiles).toHaveLength(3);
    expect(saved?.profiles.some((profile) => profile.displayName === "Edited Codex")).toBe(true);
    expect(saved?.profiles.some((profile) => profile.displayName === "Manual profile")).toBe(true);
    expect(saved?.profiles.find((profile) => profile.profileId === "profile-active")).toEqual(
      expect.objectContaining({ displayName: "Existing profile" }),
    );
    const presetProfiles = saved?.profiles.filter((profile) =>
      profile.imageRef.includes("openai-codex"),
    );
    expect(presetProfiles).toHaveLength(1);
    expect(
      presetProfiles?.every(
        (profile) =>
          profile.allowedImageRefs.length === 1 && profile.allowedImageRefs[0] === profile.imageRef,
      ),
    ).toBe(true);
    expect(
      saved?.profiles.find((profile) => profile.displayName === "Edited Codex")?.runtimeEnvironment,
    ).toEqual({
      CARGO_HOME: "/home/codespace/.cargo",
      RUSTUP_HOME: "/home/codespace/.rustup",
    });
  });

  it("rejects zero or out-of-native-range resource values before saving", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    expect(await screen.findByLabelText("CPU millicores")).toHaveAttribute("max", "4294967295");
    expect(screen.getByLabelText("Memory MiB")).toHaveAttribute("max", "9007199254740991");
    fireEvent.change(await screen.findByLabelText("CPU millicores"), {
      target: { value: "0" },
    });

    expect(screen.getByLabelText("CPU millicores")).toHaveValue(0);
    const setup = screen.getByRole("region", { name: "事前設定" });
    expect(within(setup).getByRole("button", { name: "事前設定: 設定を保存" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
    expect(screen.getByRole("alert")).toHaveTextContent(/positive|正の整数/);
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("shows opaque credential references without providing a secret value or clipboard action", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);

    expect(await screen.findByDisplayValue("keyring:agent-login")).toBeInTheDocument();
    expect(screen.getByDisplayValue("keyring:repo-read")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /copy.*credential|credential.*copy/i })).toBeNull();
    expect(screen.queryByLabelText(/secret value|password/i)).toBeNull();
  });

  it("offers a host check for the draft repository identity without saving settings", async () => {
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValue({
      settingsRevision: 6,
      repository: null,
      stages: [
        { stage: "configuration", status: "passed", code: "settings_valid" },
        { stage: "trusted_policy", status: "skipped", code: "repository_required" },
        { stage: "git_read", status: "skipped", code: "repository_required" },
        { stage: "forge_api", status: "skipped", code: "repository_required" },
      ],
    });
    renderWithProviders(<CodingAgentSettingsCard />);

    const check = await screen.findByRole("button", { name: "設定・接続を確認" });
    fireEvent.change(screen.getByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(check);
    await waitFor(() => expect(codingAgentCheckRepositoryHost).toHaveBeenCalled());
    expect(codingAgentCheckRepositoryHost).toHaveBeenCalledWith({
      expectedRevision: 6,
      host: baseSettings.repositoryHosts[0],
      repository: {
        host: "forge.example",
        organization: "team",
        repository: "project",
      },
    });
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("does not silently adopt an extra repository path segment as another repository", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const repositories = await screen.findByLabelText(
      "許可するrepository（host/org/repository、1行につき1件）",
    );
    fireEvent.change(repositories, { target: { value: "forge.example/team/project/extra" } });

    expect(screen.getByRole("button", { name: "設定を保存" })).toBeDisabled();
    expect(screen.getByRole("alert")).toHaveTextContent(/host\/org\/repository/);
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("ignores blank repository lines while preserving a valid repository save", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const repositories = await screen.findByLabelText(
      "許可するrepository（host/org/repository、1行につき1件）",
    );
    fireEvent.change(repositories, {
      target: { value: "  \nforge.example/team/next-project\n\t" },
    });

    expect(screen.getByRole("button", { name: "設定を保存" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.profiles[0]).toEqual(
      expect.objectContaining({
        allowedRepositories: [
          { host: "forge.example", organization: "team", repository: "next-project" },
        ],
      }),
    );
  });

  it("keeps the newline while typing a second repository line", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const repositories = await screen.findByLabelText(
      "許可するrepository（host/org/repository、1行につき1件）",
    );
    fireEvent.change(repositories, { target: { value: "" } });
    const textarea = repositories as HTMLTextAreaElement;
    for (const character of "forge.example/team/one\nforge.example/team/two") {
      fireEvent.change(textarea, { target: { value: textarea.value + character } });
    }

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.profiles[0]).toEqual(
      expect.objectContaining({
        allowedRepositories: [
          { host: "forge.example", organization: "team", repository: "one" },
          { host: "forge.example", organization: "team", repository: "two" },
        ],
      }),
    );
  });

  it("invalidates a host-check result when the loaded settings revision changes", async () => {
    const repository = baseSettings.profiles[0]?.allowedRepositories[0];
    if (!repository) throw new Error("test fixture repository is missing");
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValue({
      settingsRevision: 6,
      repository,
      stages: [
        { stage: "configuration", status: "passed", code: "settings_valid" },
        { stage: "trusted_policy", status: "passed", code: "policy_matched" },
        { stage: "git_read", status: "passed", code: "git_readable" },
        { stage: "forge_api", status: "passed", code: "forge_readable" },
      ],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>
          <CodingAgentSettingsCard />
        </QueryClientProvider>
      </I18nextProvider>,
    );
    fireEvent.change(await screen.findByLabelText("表示名"), {
      target: { value: "Draft with remote revision" },
    });
    fireEvent.change(screen.getByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));
    await screen.findByText("有効なリポジトリ許可設定と一致しました。");

    queryClient.setQueryData(["coding-agent-settings"], {
      settings: { ...baseSettings, revision: 7 },
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    await waitFor(() =>
      expect(screen.queryByText("有効なリポジトリ許可設定と一致しました。")).toBeNull(),
    );
  });

  it("names repository host editors and keeps the visible field names aligned with their indexed accessible names", async () => {
    const secondHost = {
      labelHost: "gitea.example",
      forge: "gitea" as const,
      apiBaseUrl: "https://gitea.example/api/v1",
      cloneTransport: { kind: "ssh" as const, host: "gitea.example", user: "git", port: 2222 },
      readCredentialRef: null,
      publicationCredentialRef: "keyring:gitea-publish",
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: { ...baseSettings, repositoryHosts: [...baseSettings.repositoryHosts, secondHost] },
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    renderWithProviders(<CodingAgentSettingsCard />);

    const first = await screen.findByRole("group", { name: "リポジトリホスト 1" });
    const second = screen.getByRole("group", { name: "リポジトリホスト 2" });
    expect(within(first).getByLabelText("リポジトリラベルのホスト名 1")).toHaveValue(
      "forge.example",
    );
    expect(within(first).getByLabelText("Gitサービス 1")).toHaveValue("github");
    expect(within(first).getByLabelText("APIベースURL 1")).toHaveValue(
      "https://forge.example/api/v3",
    );
    expect(within(first).getByLabelText("クローンの接続方式 1")).toHaveValue("https");
    expect(within(first).getByLabelText("HTTPSクローン用ベースURL 1")).toHaveValue(
      "https://forge.example",
    );
    expect(within(second).getByLabelText("SSHホスト名 2")).toHaveValue("gitea.example");
    expect(within(second).getByLabelText("SSHユーザー名 2")).toHaveValue("git");
    expect(within(second).getByLabelText("SSHポート（任意） 2")).toHaveValue(2222);
    expect(
      within(second).getByLabelText("ホストAPI・公開用トークン設定ID（任意・ホスト 2）"),
    ).toHaveValue("keyring:gitea-publish");

    fireEvent.change(within(second).getByLabelText("リポジトリラベルのホスト名 2"), {
      target: { value: "updated-gitea.example" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 6,
      settings: {
        ...baseSettings,
        repositoryHosts: [
          baseSettings.repositoryHosts[0],
          { ...secondHost, labelHost: "updated-gitea.example" },
        ],
      },
    });
  });

  it("blocks a host rename that would leave an allowed repository unregistered", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const first = await screen.findByRole("group", { name: "リポジトリホスト 1" });
    fireEvent.change(within(first).getByLabelText("リポジトリラベルのホスト名 1"), {
      target: { value: "new-forge.example" },
    });

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
    expect(screen.getAllByRole("alert")[0]).toHaveTextContent("未登録のrepository host");
  });

  it("switches a host editor between HTTPS and SSH fields without changing transport behavior", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const first = await screen.findByRole("group", { name: "リポジトリホスト 1" });
    const transport = within(first).getByLabelText("クローンの接続方式 1");

    expect(within(first).getByLabelText("HTTPSクローン用ベースURL 1")).toBeInTheDocument();
    fireEvent.change(transport, { target: { value: "ssh" } });
    expect(transport).toHaveValue("ssh");
    expect(within(first).queryByLabelText("HTTPSクローン用ベースURL 1")).toBeNull();
    expect(within(first).getByLabelText("SSHホスト名 1")).toHaveValue("");
    expect(within(first).getByLabelText("SSHユーザー名 1")).toHaveValue("");
    expect(within(first).getByLabelText("SSHポート（任意） 1")).toHaveValue(null);

    fireEvent.change(transport, { target: { value: "https" } });
    expect(within(first).getByLabelText("HTTPSクローン用ベースURL 1")).toHaveValue("");
    expect(within(first).queryByLabelText("SSHホスト名 1")).toBeNull();
  });

  it("shows credential source IDs in the normal section and hides only SSH read credentials", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const first = await screen.findByRole("group", { name: "リポジトリホスト 1" });
    const readLabel = "HTTPS読み取り用トークン設定ID（任意・ホスト 1）";
    const publicationLabel = "ホストAPI・公開用トークン設定ID（任意・ホスト 1）";
    const readCredential = within(first).getByLabelText(readLabel);
    const publicationCredential = within(first).getByLabelText(publicationLabel);

    expect(readCredential.closest("details")).toBeNull();
    expect(publicationCredential.closest("details")).toBeNull();

    fireEvent.click(within(first).getByRole("button", { name: "認証情報設定IDの説明" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      "trusted-host.json の credential_sources[].id に登録した opaque な設定IDだけを入力します。空欄（未指定）なら、同じrepositoryのlegacy trusted policyに既存の参照IDがある場合はそれを引き継ぎ、既存設定がなければ読み取りは匿名、公開は未設定です。入力欄をクリアしても、引き継いだ認証情報の解除は保証されません。secret本体は入力しません。",
    );
    fireEvent.click(within(first).getByRole("button", { name: "認証情報設定IDの説明" }));

    fireEvent.change(publicationCredential, { target: { value: "api-token" } });
    fireEvent.change(within(first).getByLabelText("クローンの接続方式 1"), {
      target: { value: "ssh" },
    });
    expect(within(first).queryByLabelText(readLabel)).toBeNull();
    expect(within(first).getByLabelText(publicationLabel)).toHaveValue("api-token");
    expect(within(first).getByText("選択したSSH設定をホストのGit操作で使用")).toBeInTheDocument();

    fireEvent.click(within(first).getByRole("button", { name: "SSH認証設定の説明" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      "選択したSSH設定ディレクトリ（未指定時は同じrepositoryのlegacy trusted policyにある既存パス）のconfig・known_hosts・identity filesをホストのGit操作で使い、guestへ自動共有しません。設定画面ではSSH鍵を登録・選択せず、SSH agentや対話的passphraseにも対応しません。パスのクリアは入力値を消すだけで、引き継いだ認証設定の解除を保証しません。",
    );

    fireEvent.change(within(first).getByLabelText("クローンの接続方式 1"), {
      target: { value: "https" },
    });
    expect(within(first).getByLabelText(readLabel)).toHaveValue("keyring:repo-read");
  });

  it("keeps a hidden SSH read credential when saving and clears it only explicitly", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const first = await screen.findByRole("group", { name: "リポジトリホスト 1" });
    fireEvent.change(within(first).getByLabelText("クローンの接続方式 1"), {
      target: { value: "ssh" },
    });
    fireEvent.change(within(first).getByLabelText("SSHホスト名 1"), {
      target: { value: "forge.example" },
    });
    fireEvent.change(within(first).getByLabelText("SSHユーザー名 1"), {
      target: { value: "git" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 6,
      settings: expect.objectContaining({
        repositoryHosts: [
          expect.objectContaining({
            readCredentialRef: "keyring:repo-read",
          }),
        ],
      }),
    });

    vi.mocked(codingAgentUpdateSettings).mockClear();
    fireEvent.change(within(first).getByLabelText("クローンの接続方式 1"), {
      target: { value: "https" },
    });
    fireEvent.change(within(first).getByLabelText("HTTPSクローン用ベースURL 1"), {
      target: { value: "https://forge.example" },
    });
    fireEvent.change(
      within(first).getByLabelText("HTTPS読み取り用トークン設定ID（任意・ホスト 1）"),
      {
        target: { value: "" },
      },
    );
    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 7,
      settings: expect.objectContaining({
        repositoryHosts: [expect.objectContaining({ readCredentialRef: null })],
      }),
    });
  });

  it("adds and removes an unreferenced host while guarding a host referenced by a profile", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const hosts = await screen.findByRole("group", { name: "リポジトリホスト" });
    expect(within(hosts).getByRole("button", { name: "ホストを削除" })).toBeDisabled();

    fireEvent.click(within(hosts).getByRole("button", { name: "ホストを追加" }));
    expect(screen.getByRole("group", { name: "リポジトリホスト 2" })).toBeInTheDocument();
    const second = screen.getByRole("group", { name: "リポジトリホスト 2" });
    fireEvent.click(within(second).getByRole("button", { name: "ホストを削除" }));
    expect(screen.queryByRole("group", { name: "リポジトリホスト 2" })).toBeNull();
  });

  it("converts empty repository credential references to null when saving", async () => {
    renderWithProviders(<CodingAgentSettingsCard />);
    const first = await screen.findByRole("group", { name: "リポジトリホスト 1" });
    fireEvent.change(
      within(first).getByLabelText("HTTPS読み取り用トークン設定ID（任意・ホスト 1）"),
      { target: { value: "" } },
    );
    fireEvent.change(
      within(first).getByLabelText("ホストAPI・公開用トークン設定ID（任意・ホスト 1）"),
      { target: { value: "" } },
    );

    fireEvent.click(screen.getByRole("button", { name: "設定を保存" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 6,
      settings: {
        ...baseSettings,
        repositoryHosts: [
          {
            ...baseSettings.repositoryHosts[0],
            readCredentialRef: null,
            publicationCredentialRef: null,
          },
        ],
      },
    });
  });

  it("uses the existing English repository host wording in the public DOM", async () => {
    i18n.changeLanguage("en");
    renderWithProviders(<CodingAgentSettingsCard />);
    const first = await screen.findByRole("group", { name: "Repository host 1" });
    const fields = [
      ["labelHost", "textbox"],
      ["forge", "combobox"],
      ["apiBaseUrl", "textbox"],
      ["cloneTransport", "combobox"],
      ["cloneBaseUrl", "textbox"],
      ["readCredentialReference", "textbox"],
      ["publicationCredentialReference", "textbox"],
    ] as const;
    for (const [key, role] of fields) {
      const label = i18n.t(`codingAgent.settings.${key}Index`, { index: 1 });
      const field = within(first).getByRole(role, { name: label });
      expect(field.closest("label")).toHaveTextContent(label);
    }
    expect(i18n.t("codingAgent.settings.readCredentialReferenceIndex", { index: 1 })).toBe(
      "HTTPS read token setting ID (optional; host 1)",
    );
    expect(
      i18n.t("codingAgent.settings.publicationCredentialReferenceIndex", {
        index: 1,
      }),
    ).toBe("Host API/publication token setting ID (optional; host 1)");
    fireEvent.change(within(first).getByLabelText("Clone transport 1"), {
      target: { value: "ssh" },
    });
    fireEvent.click(within(first).getByRole("button", { name: "Credential setting ID help" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      "Enter only the opaque setting ID registered in credential_sources[].id in trusted-host.json. When left blank (unspecified), an existing reference for the same repository in the legacy trusted policy is inherited; if none exists, reads are anonymous and publication is unconfigured. Clearing the input does not guarantee removal of an inherited credential. Never enter the secret itself.",
    );
    fireEvent.click(within(first).getByRole("button", { name: "Credential setting ID help" }));
    expect(
      within(first).getByText("Use selected SSH settings for host Git operations"),
    ).toBeInTheDocument();
    fireEvent.click(within(first).getByRole("button", { name: "SSH credential setup help" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      "SSH uses config, known_hosts, and identity files from the selected SSH directory (or an existing path for the same repository in the legacy trusted policy when unspecified) for host Git operations; they are not automatically shared into the guest. This settings screen does not register or select SSH keys and does not support SSH agents or interactive passphrases. Clearing the path only clears the input and does not guarantee removal of inherited authentication settings.",
    );
    i18n.changeLanguage("ja");
  });

  it("keeps the external profile dialog concise and groups advanced fields", async () => {
    renderExternalCodingAgentSettingsCardWithProbe();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });

    expect(within(dialog).getByLabelText("表示名")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("コンテナイメージ")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("CLIコマンド引数（1行につき1項目）")).toBeInTheDocument();
    expect(
      within(dialog).getByLabelText("許可するrepository（host/org/repository、1行につき1件）"),
    ).toBeInTheDocument();

    const runtime = within(dialog).getByText("実行環境（home・環境変数・ネットワーク）");
    const resources = within(dialog).getByText("リソース");
    const fileSharing = within(dialog).getByText("ファイル共有・保存");
    const auth = within(dialog).getByText("認証・イメージ制約");
    expect(runtime.closest("details")).not.toHaveAttribute("open");
    expect(resources.closest("details")).not.toHaveAttribute("open");
    expect(fileSharing.closest("details")).not.toHaveAttribute("open");
    expect(auth.closest("details")).not.toHaveAttribute("open");

    expect(screen.queryByText(i18n.t("codingAgent.settings.settingsHelpDescription"))).toBeNull();
    fireEvent.click(
      within(screen.getByRole("region", { name: i18n.t("codingAgent.settings.title") })).getByRole(
        "button",
        { name: i18n.t("codingAgent.settings.settingsHelpTitle") },
      ),
    );
    expect(screen.getByRole("tooltip")).toHaveTextContent(
      i18n.t("codingAgent.settings.settingsHelpDescription"),
    );
    fireEvent.click(runtime);
    expect(runtime.closest("details")).toHaveAttribute("open");
    expect(within(dialog).getByLabelText("コンテナhome")).toHaveValue("/workspace");
  });

  it("reflects a preset-created profile in the shared draft before common save", async () => {
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: structuredClone(baseSettings),
      profileMountAvailability: [],
      availableAgentPresets: [
        {
          id: "openai-codex",
          displayName: "OpenAI Codex",
          imageRef: "ghcr.io/sutr-app/openai-codex:20261025_0",
          command: ["codex", "exec"],
          containerHome: "/home/codespace/.agent-home",
          runtimeEnvironment: {
            CODEX_HOME: "/home/codespace/.agent-home/.codex",
            CARGO_HOME: "/home/codespace/.cargo",
            RUSTUP_HOME: "/home/codespace/.rustup",
          },
        },
      ],
    });
    renderExternalCodingAgentSettingsCardWithProbe();

    const presetSelect = await screen.findByLabelText("追加するプロフィールのプリセット");
    fireEvent.change(presetSelect, { target: { value: "openai-codex" } });
    const profileList = screen
      .getByRole("heading", { name: "実行プロフィール" })
      .closest("section");
    if (!profileList) throw new Error("profile list section is missing");
    fireEvent.click(within(profileList).getByRole("button", { name: "プロフィールを追加" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    expect(within(dialog).getByLabelText("表示名")).toHaveValue("OpenAI Codex");
    fireEvent.click(within(dialog).getByRole("button", { name: "変更を反映" }));

    expect(screen.getByTestId("shared-draft-dirty")).toHaveTextContent("true");
    expect(
      within(profileList).getByText("OpenAI Codex", { selector: "strong" }),
    ).toBeInTheDocument();
  });

  it("keeps legacy backend blocker details out of the modern settings surface", async () => {
    renderExternalCodingAgentSettingsCard();

    expect(
      await screen.findByRole("button", {
        name: i18n.t("codingAgent.settings.backendBlockers"),
      }),
    ).toBeInTheDocument();
    expect(screen.queryByText("coding_agent_executor_not_bundled")).toBeNull();
    expect(screen.queryByText("host_workspace_freeze_unavailable")).toBeNull();
    expect(screen.queryByText(i18n.t("codingAgent.run.blockerExplanations.default"))).toBeNull();
  });

  it("places concise hints on profile fields and groups long values as full-width fields", async () => {
    renderExternalCodingAgentSettingsCard();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });

    for (const key of [
      "imageRefHelpLabel",
      "commandArgsHelpLabel",
      "allowedRepositoriesHelpLabel",
      "containerHomeHelpLabel",
      "mountRootsHelpLabel",
      "credentialReferenceHelpLabel",
    ]) {
      expect(
        within(dialog).getByRole("button", { name: i18n.t(`codingAgent.settings.${key}`) }),
      ).toBeInTheDocument();
    }
    expect(
      within(dialog).getByLabelText("コンテナイメージ").closest(".settings-row"),
    ).not.toHaveClass("coding-agent-settings-field--full");
    expect(
      within(dialog)
        .getByLabelText("CLIコマンド引数（1行につき1項目）")
        .closest(".coding-agent-settings-field--full"),
    ).toHaveClass("coding-agent-settings-field--full");
    expect(
      within(dialog)
        .getByLabelText("許可するrepository（host/org/repository、1行につき1件）")
        .closest(".coding-agent-settings-field--full"),
    ).toHaveClass("coding-agent-settings-field--full");
  });

  it("uses two columns for host identity and full width for long host URLs", async () => {
    renderExternalCodingAgentSettingsCard();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });

    expect(
      within(dialog).getByLabelText("リポジトリラベルのホスト名 1").closest("label"),
    ).not.toHaveClass("coding-agent-settings-field--full");
    expect(within(dialog).getByLabelText("Gitサービス 1").closest("label")).not.toHaveClass(
      "coding-agent-settings-field--full",
    );
    expect(within(dialog).getByLabelText("APIベースURL 1").closest("label")).toHaveClass(
      "coding-agent-settings-field--full",
    );
    expect(
      within(dialog).getByLabelText("HTTPSクローン用ベースURL 1").closest("label"),
    ).toHaveClass("coding-agent-settings-field--full");
  });

  it("shows credential IDs in the initial external host editor modal", async () => {
    renderExternalCodingAgentSettingsCard();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    const readCredential = within(dialog).getByLabelText(
      "HTTPS読み取り用トークン設定ID（任意・ホスト 1）",
    );
    const publicationCredential = within(dialog).getByLabelText(
      "ホストAPI・公開用トークン設定ID（任意・ホスト 1）",
    );
    expect(readCredential.closest("details")).toBeNull();
    expect(publicationCredential.closest("details")).toBeNull();
  });

  it("uses a sibling confirmation dialog for a dirty profile close", async () => {
    renderExternalCodingAgentSettingsCard();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const profileDialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(profileDialog).getByLabelText("表示名"), {
      target: { value: "Draft profile" },
    });
    fireEvent.click(within(profileDialog).getByRole("button", { name: "キャンセル" }));

    const confirmDialog = await screen.findByRole("dialog", { name: "未保存の編集" });
    expect(confirmDialog).not.toBe(profileDialog);
    expect(
      within(confirmDialog).getByRole("button", { name: "変更を反映して閉じる" }),
    ).toBeEnabled();
    expect(within(confirmDialog).getByRole("button", { name: "破棄して閉じる" })).toBeEnabled();
    expect(within(confirmDialog).getByRole("button", { name: "編集を続ける" })).toHaveFocus();

    fireEvent.click(within(confirmDialog).getByRole("button", { name: "編集を続ける" }));
    expect(screen.queryByRole("dialog", { name: "未保存の編集" })).toBeNull();
    expect(screen.getByRole("dialog", { name: /プロフィールを編集/ })).toBeInTheDocument();

    fireEvent.click(
      within(screen.getByRole("dialog", { name: /プロフィールを編集/ })).getByRole("button", {
        name: "閉じる",
      }),
    );
    const secondConfirm = await screen.findByRole("dialog", { name: "未保存の編集" });
    fireEvent.click(within(secondConfirm).getByRole("button", { name: "破棄して閉じる" }));
    expect(screen.queryByRole("dialog", { name: /プロフィールを編集/ })).toBeNull();
    expect(screen.getByText("Existing profile")).toBeInTheDocument();
  });

  it("keeps the host draft until the dirty-close choice is made", async () => {
    renderExternalCodingAgentSettingsCard();

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const hostDialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(hostDialog).getByLabelText("リポジトリラベルのホスト名 1"), {
      target: { value: "draft-forge.example" },
    });
    fireEvent.click(within(hostDialog).getByRole("button", { name: "キャンセル" }));

    const confirmDialog = await screen.findByRole("dialog", { name: "未保存の編集" });
    fireEvent.click(within(confirmDialog).getByRole("button", { name: "変更を反映して閉じる" }));
    expect(screen.queryByRole("dialog", { name: /リポジトリホストを編集/ })).toBeNull();
    expect(screen.getByText("draft-forge.example")).toBeInTheDocument();
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("blocks profile confirmation for blank required fields and opens the relevant details", async () => {
    renderExternalCodingAgentSettingsCard();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("コンテナイメージ"), {
      target: { value: "" },
    });
    fireEvent.change(within(dialog).getByLabelText("CLIコマンド引数（1行につき1項目）"), {
      target: { value: "" },
    });
    fireEvent.change(within(dialog).getByLabelText("コンテナhome"), {
      target: { value: "" },
    });

    expect(within(dialog).getByRole("alert")).toHaveTextContent("必須項目を入力してください。");
    expect(within(dialog).getByRole("button", { name: "変更を反映" })).toBeDisabled();
    expect(
      within(dialog).getByText("実行環境（home・環境変数・ネットワーク）").closest("details"),
    ).toHaveAttribute("open");
    expect(within(dialog).getByText("認証・イメージ制約").closest("details")).toHaveAttribute(
      "open",
    );
  });

  it("rejects malformed repository host URLs and out-of-range SSH ports", async () => {
    renderExternalCodingAgentSettingsCard();

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    let dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByLabelText("APIベースURL 1"), {
      target: { value: "not-a-url" },
    });
    expect(within(dialog).getByRole("alert")).toHaveTextContent(
      "ホスト名、API URL、クローン接続先を入力してください。",
    );
    expect(within(dialog).getByRole("button", { name: "変更を反映" })).toBeDisabled();
    fireEvent.click(within(dialog).getByRole("button", { name: "キャンセル" }));

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByLabelText("クローンの接続方式 1"), {
      target: { value: "ssh" },
    });
    fireEvent.change(within(dialog).getByLabelText("SSHホスト名 1"), {
      target: { value: "forge.example" },
    });
    fireEvent.change(within(dialog).getByLabelText("SSHユーザー名 1"), {
      target: { value: "git" },
    });
    fireEvent.change(within(dialog).getByLabelText("SSHポート（任意） 1"), {
      target: { value: "65536" },
    });
    expect(within(dialog).getByRole("button", { name: "変更を反映" })).toBeDisabled();
  });

  it("keeps profile deletion in the list and preserves the other profile", async () => {
    const primaryProfile = baseSettings.profiles[0];
    if (!primaryProfile) throw new Error("base profile is missing");
    const secondaryProfile: CodingAgentProfile = {
      ...primaryProfile,
      profileId: "profile-secondary",
      version: 1,
      displayName: "Secondary profile",
      allowedRepositories: [],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: { ...baseSettings, profiles: [...baseSettings.profiles, secondaryProfile] },
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    renderExternalCodingAgentSettingsCard();

    const editButtons = await screen.findAllByRole("button", { name: "プロフィールを編集" });
    const secondaryEditButton = editButtons.at(1);
    if (!secondaryEditButton) throw new Error("secondary edit button is missing");
    fireEvent.click(secondaryEditButton);
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    expect(within(dialog).queryByRole("button", { name: "Profile を削除" })).toBeNull();
    fireEvent.click(within(dialog).getByRole("button", { name: "キャンセル" }));

    const removeButtons = screen.getAllByRole("button", { name: "Profile を削除" });
    const secondaryRemoveButton = removeButtons.at(1);
    if (!secondaryRemoveButton) throw new Error("secondary remove button is missing");
    fireEvent.click(secondaryRemoveButton);
    expect(screen.queryByText("Secondary profile")).toBeNull();
    expect(screen.getByText("Existing profile")).toBeInTheDocument();
  });

  it("provides settings load and retry translations in both supported locales", () => {
    i18n.changeLanguage("en");
    expect(i18n.t("codingAgent.settings.loadError")).toBe(
      "Coding agent settings could not be loaded.",
    );
    expect(i18n.t("codingAgent.settings.retry")).toBe("Retry");
    expect(i18n.t("codingAgent.settings.setupTitle")).toBe("Setup");
    expect(i18n.t("codingAgent.settings.setupDescription")).toBe(
      "Proceed in this order: save settings, prepare the dedicated home, then open the authentication and settings shell. This is separate from the execution backend probe.",
    );
    expect(i18n.t("codingAgent.settings.openTerminal")).toBe(
      "Open authentication and settings shell",
    );
    expect(i18n.t("codingAgent.settings.addPresetSelect")).toBe("Preset for the new profile");
    expect(i18n.t("codingAgent.settings.selectPreset")).toBe("No preset (empty profile)");
    expect(i18n.t("codingAgent.run.blockerExplanations.repository_label_missing")).toBe(
      "The Issue does not identify a repository.",
    );
    i18n.changeLanguage("ja");
    expect(i18n.t("codingAgent.settings.loadError")).toBe(
      "Coding agent 設定を読み込めませんでした。",
    );
    expect(i18n.t("codingAgent.settings.retry")).toBe("再試行");
    expect(i18n.t("codingAgent.settings.setupTitle")).toBe("事前設定");
    expect(i18n.t("codingAgent.settings.setupDescription")).toBe(
      "設定を保存 → 専用homeを準備 → 認証・設定用シェルを開く、の順に進めます。実行 backend の probe とは別の操作です。",
    );
    expect(i18n.t("codingAgent.settings.openTerminal")).toBe("認証・設定用シェルを開く");
    expect(i18n.t("codingAgent.settings.addPresetSelect")).toBe("追加するプロフィールのプリセット");
    expect(i18n.t("codingAgent.settings.selectPreset")).toBe("プリセットなし（空のプロフィール）");
    expect(i18n.t("codingAgent.run.blockerExplanations.repository_label_missing")).toBe(
      "Issue に repository が指定されていません。",
    );
  });
});
