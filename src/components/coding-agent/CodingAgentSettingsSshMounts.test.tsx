import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  codingAgentApproveNativeBackend,
  codingAgentGetBackendStatus,
  codingAgentGetNativeSetup,
  codingAgentGetSettings,
  codingAgentPrepareProfileHome,
  codingAgentUpdateSettings,
} from "@/api";
import i18n from "@/i18n";
import { makeNativeSetupResponse } from "@/test-utils";
import type {
  CodingAgentNativeSetupResponse,
  CodingAgentProfile,
  CodingAgentSettingsResponse,
} from "@/types/api";
import { CodingAgentSettingsCard } from "./CodingAgentSettingsCard";
import {
  type CodingAgentSettingsEditor,
  CodingAgentSettingsEditorProvider,
} from "./CodingAgentSettingsEditorContext";

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("@/api", () => ({
  codingAgentApproveNativeBackend: vi.fn(),
  codingAgentGetBackendStatus: vi.fn(),
  codingAgentGetNativeSetup: vi.fn(),
  codingAgentGetSettings: vi.fn(),
  codingAgentPrepareProfileHome: vi.fn(),
  codingAgentUpdateSettings: vi.fn(),
}));
vi.mock("./ProfileSettingsTerminalPanel", () => ({
  ProfileSettingsTerminalPanel: ({ homeReady }: { homeReady?: boolean }) => (
    <div data-testid="profile-settings-terminal">{`homeReady:${homeReady}`}</div>
  ),
}));

const profile: CodingAgentProfile = {
  profileId: "profile-one",
  version: 1,
  displayName: "Primary agent",
  imageRef: "registry.example/agent:latest",
  allowedImageRefs: ["registry.example/agent:latest"],
  command: ["agent"],
  containerHome: "/workspace",
  networkAccess: "public",
  resources: {
    cpuMillis: 1000,
    memoryMib: 1024,
    rootDiskMib: 4096,
    maxParallelRuns: 1,
    useStatic: true,
  },
  mountAllowedRoots: [],
  persistentMounts: [
    { hostPath: "/host/work", guestPath: "/workspace/work", readOnly: true, executable: false },
  ],
  allowedRepositories: [],
  credentialReference: "credential-id",
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
    nativeRepositoryApprovals: [],
    repositoryHosts: [
      {
        labelHost: "git.example",
        forge: "github",
        apiBaseUrl: "https://git.example/api",
        cloneTransport: {
          kind: "ssh",
          host: "git.example",
          user: "git",
          port: 22,
          sshDirectory: null,
        },
        readCredentialRef: null,
        publicationCredentialRef: null,
      },
    ],
  },
  profileMountAvailability: [],
  availableAgentPresets: [],
};

function Harness() {
  const [editor, setEditor] = useState<CodingAgentSettingsEditor | null>(null);
  return (
    <CodingAgentSettingsEditorProvider onChange={setEditor}>
      {editor && (
        <>
          <span data-testid="dirty">{String(editor.dirty)}</span>
          <span data-testid="saving">{String(editor.saving)}</span>
          <button type="button" onClick={() => void editor.save()}>
            Save draft
          </button>
          <CodingAgentSettingsCard
            editor={editor}
            showIssuePolicies={false}
            showLocalSaveControls={false}
            showInlineSetupGuide
          />
        </>
      )}
    </CodingAgentSettingsEditorProvider>
  );
}

function renderHarness() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <Harness />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

/** Approval is a two-step action: the step button opens an explanatory dialog. */
async function approveViaDialog(setup: HTMLElement) {
  fireEvent.click(within(setup).getByRole("button", { name: "実行環境を承認" }));
  const dialog = await screen.findByRole("dialog", { name: "実行環境の承認" });
  fireEvent.click(within(dialog).getByRole("button", { name: "実行環境を承認" }));
}

describe("Coding Agent SSH and host agent settings", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.clearAllMocks();
    vi.mocked(codingAgentGetSettings).mockResolvedValue(response);
    vi.mocked(codingAgentGetBackendStatus).mockResolvedValue({
      enabled: true,
      executorVersion: "test",
      executorProtocolVersion: 1,
      executorFeatures: [],
      appManagedJobworkerpReady: true,
      blockers: [],
    });
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "not_approved",
        packageFingerprint: "pkg-1",
      }),
    );
    vi.mocked(codingAgentApproveNativeBackend).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "approved",
        packageFingerprint: "pkg-1",
      }),
    );
    vi.mocked(codingAgentPrepareProfileHome).mockResolvedValue(response);
    vi.mocked(codingAgentUpdateSettings).mockResolvedValue(response);
    vi.mocked(openDialog).mockResolvedValue(null);
  });

  it("keeps SSH directory unchanged when the native picker is cancelled and saves an explicit path", async () => {
    renderHarness();
    const [hostEditButton] = await screen.findAllByRole("button", { name: "接続先を編集" });
    if (!hostEditButton) throw new Error("host edit button is missing");
    fireEvent.click(hostEditButton);
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "SSH設定ディレクトリの説明" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(/ホストのGit操作/);
    expect(screen.getByRole("tooltip")).toHaveTextContent(/guest/);
    const choose = within(dialog).getByRole("button", {
      name: /SSH設定ディレクトリ .*を選択/,
    });
    fireEvent.click(choose);
    await waitFor(() =>
      expect(openDialog).toHaveBeenCalledWith({ directory: true, multiple: false }),
    );
    expect(within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ })).toHaveValue("");

    vi.mocked(openDialog).mockResolvedValue("/home/alice/.ssh");
    fireEvent.click(choose);
    await waitFor(() =>
      expect(within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ })).toHaveValue(
        "/home/alice/.ssh",
      ),
    );
    const clear = within(dialog).getByRole("button", {
      name: /SSH設定ディレクトリ.*クリア/,
    });
    expect(clear).toHaveTextContent("クリア");
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.repositoryHosts[0]
        ?.cloneTransport,
    ).toEqual({
      kind: "ssh",
      host: "git.example",
      user: "git",
      port: 22,
      sshDirectory: "/home/alice/.ssh",
    });
  });

  it("retains SSH directory when SSH host fields are edited", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    const sshDirectory = within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ });
    fireEvent.change(sshDirectory, { target: { value: "/home/alice/.ssh" } });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /SSHホスト/ }), {
      target: { value: "git.internal.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /SSHユーザー/ }), {
      target: { value: "builder" },
    });
    fireEvent.change(within(dialog).getByRole("spinbutton", { name: /SSHポート/ }), {
      target: { value: "2222" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.repositoryHosts[0]
        ?.cloneTransport,
    ).toEqual({
      kind: "ssh",
      host: "git.internal.example",
      user: "builder",
      port: 2222,
      sshDirectory: "/home/alice/.ssh",
    });
  });

  it("keeps approval row focus while typing repository identity", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    const repositoryInput = within(dialog).getByRole("textbox", {
      name: "リポジトリ名 1",
    });
    repositoryInput.focus();
    fireEvent.change(repositoryInput, { target: { value: "p" } });
    expect(document.activeElement).toBe(repositoryInput);
    fireEvent.change(repositoryInput, { target: { value: "project" } });
    expect(document.activeElement).toBe(repositoryInput);
  });

  it("warns about host-side automatic execution before host settings are shared", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    const shared = within(dialog).getByRole("radio", { name: /ホストの設定を共有/ });
    expect(shared).not.toBeChecked();

    fireEvent.click(
      within(dialog).getByRole("button", {
        name: i18n.t("codingAgent.settings.hostConfigurationRiskLabel"),
      }),
    );

    const tooltip = screen.getByRole("tooltip");
    expect(tooltip).toHaveTextContent(i18n.t("codingAgent.settings.hostConfigurationRisk"));
    expect(tooltip).toHaveTextContent(/ホスト.*自動.*実行/);
  });

  it("supports host-shared profile configuration while retaining user persistent mounts", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.click(within(dialog).getByRole("radio", { name: /ホストの設定を共有/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: /Codex設定ディレクトリ/ }));
    vi.mocked(openDialog).mockResolvedValue("/home/alice/.codex");
    fireEvent.click(within(dialog).getByRole("button", { name: /Codex設定ディレクトリ/ }));
    await waitFor(() =>
      expect(within(dialog).getByRole("textbox", { name: "Codex設定ディレクトリ" })).toHaveValue(
        "/home/alice/.codex",
      ),
    );
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    expect(
      screen.getByRole("button", { name: i18n.t("codingAgent.settings.prepareHome") }),
    ).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const savedProfile =
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.profiles[0];
    expect(savedProfile?.hostConfiguration).toEqual({
      codexDirectory: "/home/alice/.codex",
      claudeDirectory: null,
      claudeSettingsFile: null,
    });
    expect(savedProfile?.persistentMounts).toEqual(profile.persistentMounts);
  });

  it("saves the Claude settings import source and clears only that source", async () => {
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    let dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.click(within(dialog).getByRole("radio", { name: /ホストの設定を共有/ }));
    const codexChoose = within(dialog).getByRole("button", { name: /Codex設定ディレクトリ/ });
    vi.mocked(openDialog).mockResolvedValueOnce("/home/alice/.codex");
    fireEvent.click(codexChoose);
    await waitFor(() =>
      expect(within(dialog).getByRole("textbox", { name: "Codex設定ディレクトリ" })).toHaveValue(
        "/home/alice/.codex",
      ),
    );
    const claudeFileChoose = within(dialog).getByRole("button", {
      name: /Claude共通設定を取り込む/,
    });
    vi.mocked(openDialog).mockResolvedValueOnce("/home/alice/.claude.json");
    fireEvent.click(claudeFileChoose);
    await waitFor(() =>
      expect(openDialog).toHaveBeenLastCalledWith({ directory: false, multiple: false }),
    );
    await waitFor(() =>
      expect(
        within(dialog).getByRole("textbox", { name: "Claude共通設定を取り込む（任意）" }),
      ).toHaveValue("/home/alice/.claude.json"),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Claude共通設定の取込元の説明" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(/原本へ書き戻しません/);
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.profiles[0],
    ).toMatchObject({
      hostConfiguration: {
        codexDirectory: "/home/alice/.codex",
        claudeSettingsFile: "/home/alice/.claude.json",
      },
    });

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    const clearClaudeFile = within(dialog).getByRole("button", {
      name: /Claude共通設定を取り込む（任意）.*クリア/,
    });
    fireEvent.click(clearClaudeFile);
    expect(
      within(dialog).getByRole("textbox", { name: "Claude共通設定を取り込む（任意）" }),
    ).toHaveValue("");
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(2));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[1]?.[0].settings.profiles[0],
    ).toMatchObject({
      hostConfiguration: {
        codexDirectory: "/home/alice/.codex",
        claudeSettingsFile: null,
      },
    });
  });

  it("requires an absolute host directory and disables host sharing for OpenCode", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.click(within(dialog).getByRole("radio", { name: /ホストの設定を共有/ }));
    const codexDirectory = within(dialog).getByRole("textbox", {
      name: "Codex設定ディレクトリ",
    });
    fireEvent.change(codexDirectory, { target: { value: "~/.codex" } });
    expect(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    ).toBeDisabled();
    expect(within(dialog).getByRole("alert")).toHaveTextContent(/絶対directory/);
    fireEvent.change(
      within(dialog).getByRole("textbox", { name: "CLIコマンド引数（1行につき1項目）" }),
      {
        target: { value: "opencode\nrun" },
      },
    );
    expect(within(dialog).getByRole("radio", { name: /ホストの設定を共有/ })).toBeDisabled();
    fireEvent.click(within(dialog).getByRole("button", { name: /OpenCodeのホスト共有/ }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(/対応していません/);
  });

  it("restores shared host paths after a private-mode round trip and keeps discard isolated", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    let dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.click(within(dialog).getByRole("radio", { name: /ホストの設定を共有/ }));
    const choose = within(dialog).getByRole("button", { name: /Codex設定ディレクトリ/ });
    vi.mocked(openDialog).mockResolvedValue("/home/alice/.codex");
    fireEvent.click(choose);
    await waitFor(() =>
      expect(within(dialog).getByRole("textbox", { name: "Codex設定ディレクトリ" })).toHaveValue(
        "/home/alice/.codex",
      ),
    );
    fireEvent.click(within(dialog).getByRole("radio", { name: /アプリ専用設定/ }));
    fireEvent.click(within(dialog).getByRole("radio", { name: /ホストの設定を共有/ }));
    expect(within(dialog).getByRole("textbox", { name: "Codex設定ディレクトリ" })).toHaveValue(
      "/home/alice/.codex",
    );

    fireEvent.click(within(dialog).getByRole("button", { name: i18n.t("common.cancel") }));
    const discard = await screen.findByRole("button", {
      name: i18n.t("codingAgent.settings.discardChanges"),
    });
    fireEvent.click(discard);
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    expect(within(dialog).getByRole("radio", { name: /アプリ専用設定/ })).toBeChecked();
    expect(within(dialog).queryByRole("textbox", { name: "Codex設定ディレクトリ" })).toBeNull();
  });

  it("shows native setup as a diagnostic and approves only with the current CAS facts", async () => {
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText(/未承認/)).toBeInTheDocument();
    await approveViaDialog(setup);
    await waitFor(() =>
      expect(codingAgentApproveNativeBackend).toHaveBeenCalledWith({
        expectedRevision: response.settings.revision,
        expectedPackageFingerprint: "pkg-1",
      }),
    );
  });

  it.each([
    ["package_mismatch", "設定画面からは承認できません"],
    ["unavailable", "実行パッケージが同梱されていません"],
  ] as const)("does not offer native approval for %s and explains why", async (state, reason) => {
    vi.mocked(codingAgentGetNativeSetup).mockImplementation(async () =>
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state,
        packageFingerprint: state === "package_mismatch" ? "pkg-1" : null,
      }),
    );
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    const approve = within(setup).getByRole("button", { name: "実行環境を承認" });
    expect(approve).toBeDisabled();
    fireEvent.click(within(setup).getByRole("button", { name: /実行環境を承認できない理由/ }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(reason);
  });

  it("explains what approval records and enables, and cancelling approves nothing", async () => {
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText(/承認だけでは何も実行されません/)).toBeInTheDocument();
    fireEvent.click(within(setup).getByRole("button", { name: "実行環境を承認" }));
    const dialog = await screen.findByRole("dialog", { name: "実行環境の承認" });
    expect(dialog).toHaveTextContent("対象: coding-agent-executor 0.1.0（fingerprint pkg-1）");
    expect(dialog).toHaveTextContent("承認で記録されること");
    expect(dialog).toHaveTextContent("承認後にアプリができること");
    expect(dialog).toHaveTextContent("承認してもしないこと");
    fireEvent.click(within(dialog).getByRole("button", { name: "キャンセル" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "実行環境の承認" })).toBeNull(),
    );
    expect(codingAgentApproveNativeBackend).not.toHaveBeenCalled();
  });

  it("shows an approved package as completed without a warning or a second approve button", async () => {
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "approved",
        packageFingerprint: "pkg-1",
      }),
    );
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText(/承認済み: coding-agent-executor 0\.1\.0/)).toBeInTheDocument();
    expect(within(setup).queryByRole("button", { name: "実行環境を承認" })).toBeNull();
    expect(within(setup).queryByRole("button", { name: /実行環境を承認できない理由/ })).toBeNull();
  });

  it("re-approves an updated package from the diagnostic", async () => {
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "reapproval_required",
        packageFingerprint: "pkg-2",
      }),
    );
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText(/再承認が必要/)).toBeInTheDocument();
    await approveViaDialog(setup);
    await waitFor(() =>
      expect(codingAgentApproveNativeBackend).toHaveBeenCalledWith({
        expectedRevision: response.settings.revision,
        expectedPackageFingerprint: "pkg-2",
      }),
    );
  });

  it("labels a development package and explains an unset approval list", async () => {
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: { ...response.settings, nativeRepositoryApprovals: undefined },
    });
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "not_approved",
        packageProvenance: "development",
        approvalSource: "legacy_policy",
      }),
    );
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText(/同梱された実行パッケージを確認しました/)).toBeInTheDocument();
    expect(within(setup).getByText(/開発用パッケージです/)).toBeInTheDocument();
    expect(within(setup).getByText(/未設定 — ホスト単位の承認リスト/)).toBeInTheDocument();
  });

  it("adds a profile repository to the approval list draft and opens the host editor from the diagnostic", async () => {
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: {
        ...response.settings,
        profiles: [
          {
            ...profile,
            allowedRepositories: [
              { host: "git.example", organization: "team", repository: "project" },
            ],
          },
        ],
      },
    });
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText("ホスト単位の承認リスト: 0件")).toBeInTheDocument();
    expect(within(setup).getByText(/承認リストが空のため/)).toBeInTheDocument();

    fireEvent.click(
      within(setup).getByRole("button", { name: "git.example/team/project を承認リストに追加" }),
    );
    expect(screen.getByTestId("dirty")).toHaveTextContent("true");
    expect(within(setup).getByText("ホスト単位の承認リスト: 1件")).toBeInTheDocument();
    expect(within(setup).getByText(/承認リストに追加しました/)).toBeInTheDocument();
    expect(
      within(setup).queryByRole("button", { name: "git.example/team/project を承認リストに追加" }),
    ).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([
      {
        repository: { host: "git.example", organization: "team", repository: "project" },
        allowedBaseBranches: ["main"],
      },
    ]);

    fireEvent.click(within(setup).getByRole("button", { name: "接続先 git.example を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    expect(within(dialog).getByRole("textbox", { name: "リポジトリホスト 2" })).toHaveValue(
      "git.example",
    );
  });

  it("explains that a pending repository needs its host registered first", async () => {
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: {
        ...response.settings,
        profiles: [
          {
            ...profile,
            allowedRepositories: [
              { host: "other.example", organization: "team", repository: "elsewhere" },
            ],
          },
        ],
      },
    });
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByText(/接続先 other.example が未登録です/)).toBeInTheDocument();
    expect(within(setup).queryByRole("button", { name: /を承認リストに追加/ })).toBeNull();
  });

  it("shows whether the dedicated home is prepared before offering the settings shell", async () => {
    vi.mocked(codingAgentPrepareProfileHome).mockResolvedValue({
      ...response,
      settings: {
        ...response.settings,
        revision: response.settings.revision + 1,
        profiles: [
          {
            ...profile,
            persistentMounts: [
              {
                hostPath: "/data/home",
                guestPath: "/workspace",
                readOnly: false,
                executable: false,
              },
            ],
          },
        ],
      },
    });
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "認証・設定" }));
    const dialog = await screen.findByRole("dialog", { name: /認証・設定:/ });
    expect(within(dialog).getByText("専用home: 未準備")).toBeInTheDocument();
    expect(within(dialog).getByTestId("profile-settings-terminal")).toHaveTextContent(
      "homeReady:false",
    );

    fireEvent.click(within(dialog).getByRole("button", { name: "専用homeを用意する" }));
    await waitFor(() => expect(within(dialog).getByText(/専用home: 準備済み/)).toBeInTheDocument());
    expect(within(dialog).getByTestId("profile-settings-terminal")).toHaveTextContent(
      "homeReady:true",
    );
  });

  it("keeps an edited draft when native approval loses its CAS race", async () => {
    let rejectApproval: ((cause: Error) => void) | undefined;
    vi.mocked(codingAgentApproveNativeBackend).mockImplementationOnce(
      () =>
        new Promise((_resolve, reject) => {
          rejectApproval = reject;
        }),
    );
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    await approveViaDialog(setup);
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    const displayName = within(dialog).getByLabelText("表示名");
    fireEvent.change(displayName, { target: { value: "draft survives" } });
    rejectApproval?.(new Error("revision conflict"));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("revision conflict"));
    expect(within(dialog).getByLabelText("表示名")).toHaveValue("draft survives");
  });

  it("blocks profile apply while approval is pending and keeps the draft through success", async () => {
    let resolveApproval: ((value: CodingAgentNativeSetupResponse) => void) | undefined;
    vi.mocked(codingAgentApproveNativeBackend).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveApproval = resolve;
        }),
    );
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "draft before approval" },
    });
    await approveViaDialog(await screen.findByRole("region", { name: /実行環境セットアップ診断/ }));
    await waitFor(() => expect(screen.getByTestId("saving")).toHaveTextContent("true"));
    const apply = within(dialog).getByRole("button", {
      name: i18n.t("codingAgent.settings.applyChanges"),
    });
    expect(apply).toBeDisabled();
    fireEvent.click(apply);
    resolveApproval?.(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision + 1,
        state: "approved",
        packageFingerprint: "pkg-1",
      }),
    );
    await waitFor(() => expect(screen.getByTestId("saving")).toHaveTextContent("false"));
    expect(within(dialog).getByLabelText("表示名")).toHaveValue("draft before approval");
  });

  it("blocks native approval while a settings draft is dirty and explains the save requirement", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "dirty profile" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    expect(within(setup).getByRole("button", { name: "実行環境を承認" })).toBeDisabled();
    fireEvent.click(within(setup).getByRole("button", { name: /実行環境を承認できない理由/ }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(/draftを保存または破棄/);
  });

  it("refreshes settings and native diagnostics after approval before the next save", async () => {
    let approved = false;
    vi.mocked(codingAgentGetSettings).mockImplementation(async () => ({
      ...response,
      settings: {
        ...response.settings,
        revision: approved ? response.settings.revision + 1 : response.settings.revision,
      },
    }));
    vi.mocked(codingAgentGetNativeSetup).mockImplementation(async () =>
      makeNativeSetupResponse({
        settingsRevision: approved ? response.settings.revision + 1 : response.settings.revision,
        state: approved ? "approved" : "not_approved",
        packageFingerprint: "pkg-1",
      }),
    );
    vi.mocked(codingAgentApproveNativeBackend).mockImplementation(async () => {
      approved = true;
      return makeNativeSetupResponse({
        settingsRevision: response.settings.revision + 1,
        state: "approved",
        packageFingerprint: "pkg-1",
      });
    });
    renderHarness();
    const setup = await screen.findByRole("region", { name: /実行環境セットアップ診断/ });
    await approveViaDialog(setup);
    await waitFor(() => expect(within(setup).getByText(/承認済み/)).toBeInTheDocument());
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "after approval" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].expectedRevision).toBe(
      response.settings.revision + 1,
    );
    expect(vi.mocked(codingAgentGetNativeSetup).mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it("keeps host-sharing limitations available through English help tooltips", async () => {
    i18n.changeLanguage("en");
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "Edit profile" }));
    const dialog = await screen.findByRole("dialog", { name: /Edit profile/ });
    fireEvent.click(within(dialog).getByRole("radio", { name: "Share host settings" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Host settings sharing help" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(/OS credential stores/);
    expect(screen.getByRole("tooltip")).toHaveTextContent(/SSH agent/);
    expect(screen.getByRole("tooltip")).toHaveTextContent(/does not guarantee authentication/);
  });

  it("refreshes a native diagnostic after a settings CAS advances the revision", async () => {
    let revision = response.settings.revision;
    vi.mocked(codingAgentGetNativeSetup).mockImplementation(async () =>
      makeNativeSetupResponse({
        settingsRevision: revision,
        state: "not_approved",
        packageFingerprint: "pkg-1",
      }),
    );
    vi.mocked(codingAgentUpdateSettings).mockImplementationOnce(async ({ settings }) => {
      revision += 1;
      return { ...response, settings: { ...settings, revision } };
    });
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "プロフィールを編集" }));
    const dialog = await screen.findByRole("dialog", { name: /プロフィールを編集/ });
    fireEvent.change(within(dialog).getByLabelText("表示名"), {
      target: { value: "Changed agent" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentGetNativeSetup).toHaveBeenCalledTimes(2));
    expect(screen.queryByText(/設定revisionが変わった/)).not.toBeInTheDocument();
    expect(
      within(await screen.findByRole("region", { name: /実行環境セットアップ診断/ })).getByRole(
        "button",
        {
          name: "実行環境を承認",
        },
      ),
    ).toBeEnabled();
  });

  it("adds, validates, and removes native repository approvals without changing profile allowlists", async () => {
    let revision = response.settings.revision;
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings: { ...settings, revision: ++revision },
    }));
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    let dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリホスト 1" }), {
      target: { value: "git.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ組織 1" }), {
      target: { value: "team" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ名 1" }), {
      target: { value: "project" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /利用を許可するbase branch 1/ }), {
      target: { value: "bad branch" },
    });
    expect(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    ).toBeDisabled();
    fireEvent.change(within(dialog).getByRole("textbox", { name: /利用を許可するbase branch 1/ }), {
      target: { value: "main\nrelease/native" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([
      {
        repository: { host: "git.example", organization: "team", repository: "project" },
        allowedBaseBranches: ["main", "release/native"],
      },
    ]);

    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを削除" }));
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(2));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[1]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([]);
  });

  it("merges host-dialog approval edits into the global policy and keeps other hosts", async () => {
    const otherHostApproval = {
      repository: { host: "other.example", organization: "team", repository: "kept" },
      allowedBaseBranches: ["main"],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: {
        ...response.settings,
        nativeRepositoryApprovals: [otherHostApproval],
        repositoryHosts: [
          ...response.settings.repositoryHosts,
          {
            labelHost: "other.example",
            forge: "github",
            apiBaseUrl: "https://other.example/api",
            cloneTransport: { kind: "https", baseUrl: "https://other.example/repo" },
            readCredentialRef: null,
            publicationCredentialRef: null,
          },
        ],
      },
    });
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    const [hostEditButton] = await screen.findAllByRole("button", { name: "接続先を編集" });
    if (!hostEditButton) throw new Error("host edit button is missing");
    fireEvent.click(hostEditButton);
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリホスト 1" }), {
      target: { value: "git.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ組織 1" }), {
      target: { value: "team" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ名 1" }), {
      target: { value: "new" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /利用を許可するbase branch 1/ }), {
      target: { value: "release" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([
      otherHostApproval,
      {
        repository: { host: "git.example", organization: "team", repository: "new" },
        allowedBaseBranches: ["release"],
      },
    ]);
  });

  it("discards host-dialog approval drafts without changing global settings", async () => {
    renderHarness();
    const [hostEditButton] = await screen.findAllByRole("button", { name: "接続先を編集" });
    if (!hostEditButton) throw new Error("host edit button is missing");
    fireEvent.click(hostEditButton);
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリホスト 1" }), {
      target: { value: "git.example" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: i18n.t("common.cancel") }));
    fireEvent.click(
      await screen.findByRole("button", { name: i18n.t("codingAgent.settings.discardChanges") }),
    );
    expect(screen.getByTestId("dirty")).toHaveTextContent("false");
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("preserves an omitted approval policy when editing only SSH settings", async () => {
    const { nativeRepositoryApprovals: _legacyApprovals, ...legacySettings } = response.settings;
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: legacySettings,
    });
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "not_approved",
        packageFingerprint: "pkg-1",
        repositoryApprovals: [
          {
            repository: { host: "git.example", organization: "team", repository: "legacy" },
            allowedBaseBranches: ["main"],
          },
          {
            repository: { host: "other.example", organization: "team", repository: "kept" },
            allowedBaseBranches: ["release"],
          },
        ],
      }),
    );
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ }), {
      target: { value: "/home/alice/.ssh" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings).not.toHaveProperty(
      "nativeRepositoryApprovals",
    );
  });

  it("seeds a legacy approval edit from the effective diagnostic and keeps other hosts", async () => {
    const { nativeRepositoryApprovals: _legacyApprovals, ...legacySettings } = response.settings;
    const effectiveApprovals = [
      {
        repository: { host: "git.example", organization: "team", repository: "legacy" },
        allowedBaseBranches: ["main"],
      },
      {
        repository: { host: "other.example", organization: "team", repository: "kept" },
        allowedBaseBranches: ["release"],
      },
    ];
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: {
        ...legacySettings,
        repositoryHosts: [
          ...legacySettings.repositoryHosts,
          {
            labelHost: "other.example",
            forge: "github",
            apiBaseUrl: "https://other.example/api",
            cloneTransport: { kind: "https", baseUrl: "https://other.example/repo" },
            readCredentialRef: null,
            publicationCredentialRef: null,
          },
        ],
      },
    });
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "not_approved",
        packageFingerprint: "pkg-1",
        repositoryApprovals: effectiveApprovals,
      }),
    );
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    const [hostEditButton] = await screen.findAllByRole("button", { name: "接続先を編集" });
    if (!hostEditButton) throw new Error("host edit button is missing");
    fireEvent.click(hostEditButton);
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /利用を許可するbase branch 1/ }), {
      target: { value: "develop" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([
      {
        repository: { host: "other.example", organization: "team", repository: "kept" },
        allowedBaseBranches: ["release"],
      },
      {
        repository: { host: "git.example", organization: "team", repository: "legacy" },
        allowedBaseBranches: ["develop"],
      },
    ]);
  });

  it("blocks legacy approval editing when effective policy is unavailable but leaves auth fields editable", async () => {
    const { nativeRepositoryApprovals: _legacyApprovals, ...legacySettings } = response.settings;
    vi.mocked(codingAgentGetSettings).mockResolvedValue({ ...response, settings: legacySettings });
    vi.mocked(codingAgentGetNativeSetup).mockRejectedValue(new Error("diagnostic unavailable"));
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    const addApproval = within(dialog).getByRole("button", { name: "許可リポジトリを追加" });
    expect(addApproval).toBeDisabled();
    fireEvent.click(within(dialog).getByRole("button", { name: /既存の実行環境許可/ }));
    expect(screen.getByRole("tooltip")).toHaveTextContent(/取得できない/);
    fireEvent.change(within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ }), {
      target: { value: "/home/alice/.ssh" },
    });
    expect(within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ })).toHaveValue(
      "/home/alice/.ssh",
    );
  });

  it("removes approvals for a removed host while keeping other host approvals", async () => {
    const otherHost = {
      labelHost: "other.example",
      forge: "github" as const,
      apiBaseUrl: "https://other.example/api",
      cloneTransport: { kind: "https" as const, baseUrl: "https://other.example/repo" },
      readCredentialRef: null,
      publicationCredentialRef: null,
    };
    const firstApproval = {
      repository: { host: "git.example", organization: "team", repository: "keep" },
      allowedBaseBranches: ["main"],
    };
    const secondApproval = {
      repository: { host: "other.example", organization: "team", repository: "remove" },
      allowedBaseBranches: ["main"],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: {
        ...response.settings,
        repositoryHosts: [...response.settings.repositoryHosts, otherHost],
        nativeRepositoryApprovals: [firstApproval, secondApproval],
      },
    });
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    const removeButtons = await screen.findAllByRole("button", {
      name: i18n.t("codingAgent.settings.removeRepositoryHost"),
    });
    const secondRemoveButton = removeButtons[1];
    if (!secondRemoveButton) throw new Error("second host remove button is missing");
    fireEvent.click(secondRemoveButton);
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([firstApproval]);
  });

  it("keeps existing global approvals when adding a new host approval", async () => {
    const existingApproval = {
      repository: { host: "git.example", organization: "team", repository: "existing" },
      allowedBaseBranches: ["main"],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: { ...response.settings, nativeRepositoryApprovals: [existingApproval] },
    });
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "ホストを追加" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /リポジトリラベルのホスト名/ }), {
      target: { value: "new.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /APIベースURL/ }), {
      target: { value: "https://new.example/api" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /HTTPSクローン用ベースURL/ }), {
      target: { value: "https://new.example/repo" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリホスト 1" }), {
      target: { value: "new.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ組織 1" }), {
      target: { value: "team" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ名 1" }), {
      target: { value: "new" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([
      existingApproval,
      {
        repository: { host: "new.example", organization: "team", repository: "new" },
        allowedBaseBranches: ["main"],
      },
    ]);
  });

  it("keeps effective legacy approvals when adding a new host approval", async () => {
    const { nativeRepositoryApprovals: _legacyApprovals, ...legacySettings } = response.settings;
    const existingApproval = {
      repository: { host: "git.example", organization: "team", repository: "existing" },
      allowedBaseBranches: ["main"],
    };
    vi.mocked(codingAgentGetSettings).mockResolvedValue({ ...response, settings: legacySettings });
    vi.mocked(codingAgentGetNativeSetup).mockResolvedValue(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "not_approved",
        packageFingerprint: "pkg-1",
        repositoryApprovals: [existingApproval],
      }),
    );
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "ホストを追加" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /リポジトリラベルのホスト名/ }), {
      target: { value: "new.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /APIベースURL/ }), {
      target: { value: "https://new.example/api" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: /HTTPSクローン用ベースURL/ }), {
      target: { value: "https://new.example/repo" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリホスト 1" }), {
      target: { value: "new.example" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ組織 1" }), {
      target: { value: "team" },
    });
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリ名 1" }), {
      target: { value: "new" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings.nativeRepositoryApprovals,
    ).toEqual([
      existingApproval,
      {
        repository: { host: "new.example", organization: "team", repository: "new" },
        allowedBaseBranches: ["main"],
      },
    ]);
  });

  it("seeds a host dialog opened while the native diagnostic is loading without replacing host edits", async () => {
    const { nativeRepositoryApprovals: _legacyApprovals, ...legacySettings } = response.settings;
    const existingApprovals = [
      {
        repository: { host: "git.example", organization: "team", repository: "existing" },
        allowedBaseBranches: ["main"],
      },
      {
        repository: { host: "other.example", organization: "team", repository: "kept" },
        allowedBaseBranches: ["release"],
      },
    ];
    let resolveSetup: (value: CodingAgentNativeSetupResponse) => void = () => {};
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      ...response,
      settings: {
        ...legacySettings,
        repositoryHosts: [
          ...legacySettings.repositoryHosts,
          {
            labelHost: "other.example",
            forge: "github",
            apiBaseUrl: "https://other.example/api",
            cloneTransport: { kind: "https", baseUrl: "https://other.example/repo" },
            readCredentialRef: null,
            publicationCredentialRef: null,
          },
        ],
      },
    });
    vi.mocked(codingAgentGetNativeSetup).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveSetup = resolve;
        }),
    );
    vi.mocked(codingAgentUpdateSettings).mockImplementation(async ({ settings }) => ({
      ...response,
      settings,
    }));
    renderHarness();
    const [firstHostEditButton] = await screen.findAllByRole("button", {
      name: "接続先を編集",
    });
    if (!firstHostEditButton) throw new Error("first host edit button is missing");
    fireEvent.click(firstHostEditButton);
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    expect(within(dialog).getByRole("button", { name: "許可リポジトリを追加" })).toBeDisabled();
    fireEvent.change(within(dialog).getByRole("textbox", { name: /SSH設定ディレクトリ/ }), {
      target: { value: "/home/alice/.ssh" },
    });
    resolveSetup(
      makeNativeSetupResponse({
        settingsRevision: response.settings.revision,
        state: "not_approved",
        packageFingerprint: "pkg-1",
        repositoryApprovals: existingApprovals,
      }),
    );
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "許可リポジトリを追加" })).toBeEnabled(),
    );
    fireEvent.change(within(dialog).getByRole("textbox", { name: /利用を許可するbase branch 1/ }), {
      target: { value: "develop" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(codingAgentUpdateSettings).toHaveBeenCalledTimes(1));
    const savedSettings = vi.mocked(codingAgentUpdateSettings).mock.calls[0]?.[0].settings;
    if (!savedSettings) throw new Error("saved settings are missing");
    expect(savedSettings.repositoryHosts[0]?.cloneTransport).toMatchObject({
      sshDirectory: "/home/alice/.ssh",
    });
    expect(savedSettings.nativeRepositoryApprovals).toEqual([
      {
        repository: { host: "other.example", organization: "team", repository: "kept" },
        allowedBaseBranches: ["release"],
      },
      {
        repository: { host: "git.example", organization: "team", repository: "existing" },
        allowedBaseBranches: ["develop"],
      },
    ]);
  });

  it("blocks native approvals with a different host or duplicate repository branch", async () => {
    renderHarness();
    fireEvent.click(await screen.findByRole("button", { name: "接続先を編集" }));
    const dialog = await screen.findByRole("dialog", { name: /リポジトリホストを編集/ });
    const addApproval = () =>
      fireEvent.click(within(dialog).getByRole("button", { name: "許可リポジトリを追加" }));
    const fillApproval = (index: number, host: string, branch = "main") => {
      fireEvent.change(within(dialog).getByRole("textbox", { name: `リポジトリホスト ${index}` }), {
        target: { value: host },
      });
      fireEvent.change(within(dialog).getByRole("textbox", { name: `リポジトリ組織 ${index}` }), {
        target: { value: "team" },
      });
      fireEvent.change(within(dialog).getByRole("textbox", { name: `リポジトリ名 ${index}` }), {
        target: { value: "project" },
      });
      fireEvent.change(
        within(dialog).getByRole("textbox", {
          name: new RegExp(`利用を許可するbase branch ${index}`),
        }),
        {
          target: { value: branch },
        },
      );
    };
    addApproval();
    fillApproval(1, "other.example");
    expect(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    ).toBeDisabled();
    fireEvent.change(within(dialog).getByRole("textbox", { name: "リポジトリホスト 1" }), {
      target: { value: "git.example" },
    });
    expect(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    ).toBeEnabled();
    addApproval();
    fillApproval(2, "git.example", "release");
    expect(
      within(dialog).getByRole("button", { name: i18n.t("codingAgent.settings.applyChanges") }),
    ).toBeDisabled();
  });
});
