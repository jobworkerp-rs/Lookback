import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  profileTerminalCancel,
  profileTerminalFinishInput,
  profileTerminalList,
  profileTerminalOutput,
  profileTerminalSession,
  profileTerminalStart,
} from "@/api/profileTerminal";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { ProfileSettingsTerminalPanel } from "./ProfileSettingsTerminalPanel";

vi.mock("@/api/profileTerminal", () => ({
  profileTerminalStart: vi.fn(),
  profileTerminalList: vi.fn(),
  profileTerminalSession: vi.fn(),
  profileTerminalOutput: vi.fn(),
  profileTerminalSendInput: vi.fn(),
  profileTerminalFinishInput: vi.fn(),
  profileTerminalCancel: vi.fn(),
}));
vi.mock("./AgentTerminal", () => ({
  AgentTerminal: ({
    session,
    onCancelRequested,
  }: {
    session: { runId: string; purpose: string };
    onCancelRequested: (id: string) => void;
  }) => (
    <div>
      <span>{session.purpose}</span>
      <button type="button" onClick={() => onCancelRequested(session.runId)}>
        cancel-shell
      </button>
    </div>
  ),
}));
const session = {
  sessionId: "setup-session",
  profileId: "profile",
  profileVersion: 3,
  settingsRevision: 4,
  imageRef: "registry.example/coder@sha256:aaa",
  state: "running" as const,
  jobId: "42",
  exitCode: null,
  inputClosed: false,
};
describe("ProfileSettingsTerminalPanel", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.clearAllMocks();
    vi.mocked(profileTerminalList).mockResolvedValue([]);
    vi.mocked(profileTerminalStart).mockResolvedValue(session);
    vi.mocked(profileTerminalSession).mockResolvedValue(session);
    vi.mocked(profileTerminalOutput).mockResolvedValue({ events: [], nextSequence: "0" });
  });
  it("starts a settings terminal from the saved profile and routes finish and cancel separately", async () => {
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty={false} />,
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "認証・設定用シェルを開く" }));
    await screen.findByText("authentication");
    expect(screen.getByRole("heading", { name: "認証・設定用シェル" })).toBeInTheDocument();
    expect(screen.getByText(/同じimage.*共有home.*通常の実行でも再利用/)).toBeInTheDocument();
    expect(profileTerminalStart).toHaveBeenCalledWith({
      expectedRevision: 4,
      profileId: "profile",
    });
    expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "入力を終了する" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "入力を終了する" }));
    await waitFor(() => expect(profileTerminalFinishInput).toHaveBeenCalledWith("setup-session"));
    fireEvent.click(screen.getByRole("button", { name: "cancel-shell" }));
    await waitFor(() => expect(profileTerminalCancel).toHaveBeenCalledWith("setup-session"));
  });
  it("refuses an unsaved profile and recovers an unknown accepted session without replaying start", async () => {
    vi.mocked(profileTerminalList).mockResolvedValue([{ ...session, state: "unknown" }]);
    vi.mocked(profileTerminalSession).mockResolvedValue({ ...session, state: "unknown" });
    renderWithProviders(<ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty />);
    expect(await screen.findByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();
    expect(screen.getByText(/保存してください/)).toBeInTheDocument();
    await screen.findByText("authentication");
    expect(profileTerminalStart).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "入力を終了する" })).toBeDisabled();
  });
  it("recovers the list after a start error and does not retry acceptance", async () => {
    vi.mocked(profileTerminalStart).mockRejectedValue(new Error("unknown acceptance"));
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty={false} />,
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "認証・設定用シェルを開く" }));
    await screen.findByText(
      "terminalを開始できませんでした。設定・mount・実行環境とsession一覧を確認してください。",
    );
    expect(profileTerminalStart).toHaveBeenCalledTimes(1);
    expect(profileTerminalList).toHaveBeenCalledTimes(2);
  });

  it("explains that a dedicated home must be prepared before opening", async () => {
    renderWithProviders(
      <ProfileSettingsTerminalPanel
        profileId="profile"
        revision={4}
        dirty={false}
        homeReady={false}
      />,
    );
    const open = await screen.findByRole("button", { name: "認証・設定用シェルを開く" });
    expect(open).toBeDisabled();
    expect(screen.getByText(/先に「専用homeを用意する」/)).toBeInTheDocument();
    fireEvent.click(open);
    expect(profileTerminalStart).not.toHaveBeenCalled();
  });

  it("does not start a settings terminal for a profile with a credential reference", async () => {
    renderWithProviders(
      <ProfileSettingsTerminalPanel
        profileId="profile"
        revision={4}
        dirty={false}
        credentialReference="keyring:agent-login"
      />,
    );

    const button = await screen.findByRole("button", { name: "認証・設定用シェルを開く" });
    expect(button).toBeDisabled();
    expect(screen.getByText(/credential reference.*認証・設定用シェル/)).toBeInTheDocument();
    expect(profileTerminalStart).not.toHaveBeenCalled();
  });

  it("explains why the session list being pending blocks opening", async () => {
    let resolveList!: (sessions: (typeof session)[]) => void;
    vi.mocked(profileTerminalList).mockReturnValue(
      new Promise((resolve) => {
        resolveList = resolve;
      }),
    );
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty={false} />,
    );

    expect(await screen.findByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();
    expect(screen.getByText(/session一覧の取得が終わるまで/)).toBeInTheDocument();
    resolveList([]);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeEnabled(),
    );
  });

  it("explains why an active session blocks opening", async () => {
    cleanup();
    vi.clearAllMocks();
    vi.mocked(profileTerminalList).mockResolvedValue([{ ...session, state: "running" }]);
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty={false} />,
    );

    expect(await screen.findByText(/既存の認証・設定用シェル.*終了/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();
  });

  it("explains saving and starting states near the settings terminal control", async () => {
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty saving />,
    );
    expect(await screen.findByText(/保存中です/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();

    cleanup();
    vi.clearAllMocks();
    let resolveStart!: (value: typeof session) => void;
    vi.mocked(profileTerminalStart).mockReturnValue(
      new Promise((resolve) => {
        resolveStart = resolve;
      }),
    );
    vi.mocked(profileTerminalList).mockResolvedValue([]);
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty={false} />,
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "認証・設定用シェルを開く" }));
    expect(await screen.findByText(/開始中です/)).toBeInTheDocument();
    resolveStart(session);
  });

  it("explains why an unavailable session list blocks opening and keeps retry available", async () => {
    cleanup();
    vi.clearAllMocks();
    vi.mocked(profileTerminalList).mockRejectedValue(new Error("session list unavailable"));
    renderWithProviders(
      <ProfileSettingsTerminalPanel profileId="profile" revision={4} dirty={false} />,
    );
    expect(await screen.findByText(/session一覧を取得してください/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "再試行" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();
  });

  it.each([
    ["Codex", ["codex", "exec"], /codex login.*CODEX_HOME.*自動で作成されます/],
    ["Claude", ["claude", "-p"], /claude auth --help/],
    ["OpenCode", ["opencode", "run"], /opencode providers --help/],
    ["unknown CLI", ["custom-agent"], /選択したcommand.*help/],
  ])("shows the manual settings guide for %s", (_name, command, expected) => {
    renderWithProviders(
      <ProfileSettingsTerminalPanel
        profileId="profile"
        revision={4}
        dirty={false}
        command={command}
      />,
    );
    expect(screen.getByText(expected)).toBeInTheDocument();
  });
});
