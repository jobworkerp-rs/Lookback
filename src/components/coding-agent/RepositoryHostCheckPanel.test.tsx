import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { codingAgentCheckRepositoryHost } from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type {
  CodingAgentRepositoryHost,
  CodingAgentRepositoryHostCheckResponse,
  CodingAgentRepositoryIdentity,
} from "@/types/api";
import { RepositoryHostCheckPanel } from "./RepositoryHostCheckPanel";

vi.mock("@/api", () => ({
  codingAgentCheckRepositoryHost: vi.fn(),
}));

const host: CodingAgentRepositoryHost = {
  labelHost: "forge.example",
  forge: "github",
  apiBaseUrl: "https://forge.example/api/v3",
  cloneTransport: { kind: "https", baseUrl: "https://forge.example" },
  readCredentialRef: "keyring:read-token",
  publicationCredentialRef: "keyring:publish-token",
};
const repository: CodingAgentRepositoryIdentity = {
  host: "forge.example",
  organization: "team",
  repository: "project",
};
const result = {
  settingsRevision: 9,
  repository,
  stages: [
    { stage: "configuration" as const, status: "passed" as const, code: "settings_valid" },
    { stage: "trusted_policy" as const, status: "passed" as const, code: "policy_matched" },
    { stage: "git_read" as const, status: "passed" as const, code: "git_readable" },
    { stage: "forge_api" as const, status: "failed" as const, code: "authentication_failed" },
  ],
};

describe("RepositoryHostCheckPanel", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.clearAllMocks();
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValue(result);
  });

  it("checks the selected draft repository without saving and shows independent stages", async () => {
    renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );

    fireEvent.change(await screen.findByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));
    await screen.findByText("Forge APIの認証を確認できませんでした。");

    expect(codingAgentCheckRepositoryHost).toHaveBeenCalledWith({
      expectedRevision: 8,
      host,
      repository,
    });
    expect(screen.getByText("Git 読み取り")).toBeInTheDocument();
    expect(screen.getAllByText("確認成功")).toHaveLength(3);
    expect(screen.getByText("確認失敗")).toBeInTheDocument();
    expect(
      screen.getByText(/Gitの成功からAPI認証や書き込み権限を推測しません/),
    ).toBeInTheDocument();
    expect(screen.queryByText(/利用可能|認証済み|すべてOK/)).toBeNull();
  });

  it("allows structural-only checks without a selected repository", async () => {
    renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[]} />,
    );

    expect(
      screen.getByText("リポジトリを選択しない場合は構造確認のみ実行します。"),
    ).toBeInTheDocument();
    fireEvent.click(await screen.findByRole("button", { name: "設定・接続を確認" }));
    await waitFor(() => expect(codingAgentCheckRepositoryHost).toHaveBeenCalled());
    expect(codingAgentCheckRepositoryHost).toHaveBeenCalledWith({
      expectedRevision: 8,
      host,
      repository: null,
    });
  });

  it("explains authentication, restriction, and not-found outcomes without overclaiming", async () => {
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValueOnce({
      settingsRevision: 9,
      repository,
      stages: [
        { stage: "configuration", status: "passed", code: "settings_valid" },
        { stage: "trusted_policy", status: "failed", code: "access_denied" },
        { stage: "git_read", status: "failed", code: "repository_not_found" },
        { stage: "forge_api", status: "failed", code: "authentication_failed" },
      ],
    });
    renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );
    fireEvent.change(await screen.findByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));

    await screen.findByText("Forge APIの認証を確認できませんでした。");
    expect(screen.getByText("Forge APIが権限または利用制限を返しました。")).toBeInTheDocument();
    expect(screen.getByText("リポジトリが未検出または参照できません。")).toBeInTheDocument();
    expect(screen.queryByText(/token不正|対象repository不存在|認証済み/)).toBeNull();
  });

  it("explains how to register an unapproved execution environment", async () => {
    const unapprovedResult: CodingAgentRepositoryHostCheckResponse = {
      settingsRevision: 9,
      repository,
      stages: [
        { stage: "configuration", status: "passed", code: "settings_valid" },
        {
          stage: "trusted_policy",
          status: "failed",
          code: "native_backend_not_approved",
        },
        { stage: "git_read", status: "skipped", code: "prerequisite_failed" },
        { stage: "forge_api", status: "skipped", code: "prerequisite_failed" },
      ],
    };
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValue(unapprovedResult);
    const view = renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );
    fireEvent.change(await screen.findByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));

    await screen.findByText("設定画面で実行環境を登録してから、接続を確認してください。");
    expect(screen.queryByText(/fingerprint|trusted.?policy|opaque/i)).toBeNull();

    view.unmount();
    i18n.changeLanguage("en");
    renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );
    fireEvent.change(await screen.findByRole("combobox", { name: "Repository to check" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Check settings and connection" }));
    await screen.findByText(
      "Register the execution environment in Settings, then check the connection again.",
    );
    expect(
      screen.getByText(
        "Register the execution environment in Settings, then check the connection again.",
      ),
    ).toBeInTheDocument();
  });

  it("clears a successful result when the selected repository changes", async () => {
    const alternateRepository = { ...repository, repository: "other" };
    renderWithProviders(
      <RepositoryHostCheckPanel
        host={host}
        expectedRevision={8}
        repositories={[repository, alternateRepository]}
      />,
    );
    const repositorySelect = await screen.findByRole("combobox", { name: "確認するリポジトリ" });
    fireEvent.change(repositorySelect, { target: { value: "forge.example/team/project" } });
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));
    await screen.findByText("Forge APIの認証を確認できませんでした。");

    fireEvent.change(repositorySelect, { target: { value: "forge.example/team/other" } });
    expect(screen.queryByText("Forge APIの認証を確認できませんでした。")).toBeNull();
  });

  it("keeps the check busy until completion and ignores an old response after the draft changes", async () => {
    let resolveCheck!: (value: typeof result) => void;
    vi.mocked(codingAgentCheckRepositoryHost).mockReturnValue(
      new Promise((resolve) => {
        resolveCheck = resolve;
      }),
    );
    const view = renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );
    fireEvent.change(await screen.findByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    const check = screen.getByRole("button", { name: "設定・接続を確認" });
    fireEvent.click(check);
    expect(check).toBeDisabled();

    view.rerender(
      <RepositoryHostCheckPanel
        host={{ ...host, labelHost: "new.example" }}
        expectedRevision={9}
        repositories={[{ ...repository, host: "new.example" }]}
      />,
    );
    resolveCheck(result);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(screen.queryByText("Forge APIの認証を確認できませんでした。")).toBeNull();
    expect(screen.getByRole("button", { name: "設定・接続を確認" })).toBeEnabled();
  });

  it("ignores a delayed response after the observed settings revision changes", async () => {
    let resolveCheck!: (value: typeof result) => void;
    vi.mocked(codingAgentCheckRepositoryHost).mockReturnValue(
      new Promise((resolve) => {
        resolveCheck = resolve;
      }),
    );
    const view = renderWithProviders(
      <RepositoryHostCheckPanel
        host={host}
        expectedRevision={8}
        observedRevision={8}
        repositories={[repository]}
      />,
    );
    fireEvent.change(await screen.findByRole("combobox", { name: "確認するリポジトリ" }), {
      target: { value: "forge.example/team/project" },
    });
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));
    view.rerender(
      <RepositoryHostCheckPanel
        host={host}
        expectedRevision={8}
        observedRevision={9}
        repositories={[repository]}
      />,
    );
    resolveCheck(result);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(screen.queryByText("Forge APIの認証を確認できませんでした。")).toBeNull();
  });

  it("does not expose raw errors or unknown stage codes", async () => {
    vi.mocked(codingAgentCheckRepositoryHost).mockResolvedValueOnce({
      settingsRevision: 9,
      repository: null,
      stages: [
        { stage: "configuration", status: "failed", code: "mystery_secret_token" },
        { stage: "trusted_policy", status: "skipped", code: "prerequisite_failed" },
        { stage: "git_read", status: "skipped", code: "credential_unavailable" },
        { stage: "forge_api", status: "skipped", code: "unsupported_capability" },
      ],
    });
    renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[]} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: "設定・接続を確認" }));
    await screen.findByText("この確認段階の詳細は利用できません。");
    expect(screen.queryByText("mystery_secret_token")).toBeNull();
    expect(screen.queryByText(/keyring:read-token|publish-token/)).toBeNull();

    vi.mocked(codingAgentCheckRepositoryHost).mockRejectedValueOnce(
      new Error("raw token and HTTP body"),
    );
    fireEvent.click(screen.getByRole("button", { name: "設定・接続を確認" }));
    await screen.findByRole("alert");
    expect(screen.getByRole("alert")).toHaveTextContent(
      "リポジトリホストの確認に失敗しました。詳細な外部エラーは表示しません。",
    );
    expect(screen.getByRole("alert")).not.toHaveTextContent("raw token");
    fireEvent.click(screen.getByRole("button", { name: "再確認" }));
    await screen.findByText("Forge APIの認証を確認できませんでした。");
  });

  it("uses the repository selector label from the active locale in the public DOM", async () => {
    i18n.changeLanguage("en");
    const view = renderWithProviders(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );
    expect(screen.getByRole("combobox", { name: "Repository to check" })).toBeInTheDocument();

    i18n.changeLanguage("ja");
    view.rerender(
      <RepositoryHostCheckPanel host={host} expectedRevision={8} repositories={[repository]} />,
    );
    expect(screen.getByRole("combobox", { name: "確認するリポジトリ" })).toBeInTheDocument();
  });
});
