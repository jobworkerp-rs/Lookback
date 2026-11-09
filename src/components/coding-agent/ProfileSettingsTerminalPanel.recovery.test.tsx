import { fireEvent, screen, waitFor } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import {
  profileTerminalCancel,
  profileTerminalList,
  profileTerminalOutput,
  profileTerminalSession,
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

it("allows cancelling an unknown job through the actual settings terminal panel", async () => {
  i18n.changeLanguage("ja");
  const session = {
    sessionId: "unknown-shell",
    profileId: "profile",
    profileVersion: 1,
    settingsRevision: 2,
    imageRef: "image",
    state: "unknown" as const,
    jobId: "42",
    exitCode: null,
    inputClosed: false,
  };
  vi.mocked(profileTerminalList).mockResolvedValue([session]);
  vi.mocked(profileTerminalSession).mockResolvedValue(session);
  vi.mocked(profileTerminalOutput).mockResolvedValue({ events: [], nextSequence: "0" });
  vi.mocked(profileTerminalCancel).mockResolvedValue(undefined);
  renderWithProviders(
    <ProfileSettingsTerminalPanel
      profileId="profile"
      revision={2}
      dirty={false}
      rendererFactory={async () => ({
        open: () => {},
        write: () => {},
        setInputEnabled: () => {},
        onData: () => ({ dispose: () => {} }),
        onBinary: () => ({ dispose: () => {} }),
        dispose: () => {},
      })}
    />,
  );
  fireEvent.click(await screen.findByRole("button", { name: "未確定sessionの取消を要求する" }));
  await waitFor(() => expect(profileTerminalCancel).toHaveBeenCalledWith("unknown-shell"));
  expect(screen.getByRole("button", { name: "認証・設定用シェルを開く" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "入力を終了する" })).toBeDisabled();
});
