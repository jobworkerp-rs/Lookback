import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  codingAgentGetSettings,
  codingAgentPrepareProfileHome,
  codingAgentUpdateSettings,
} from "@/api";
import i18n from "@/i18n";
import type { CodingAgentProfile, CodingAgentSettingsResponse } from "@/types/api";
import {
  CodingAgentSettingsEditorProvider,
  useCodingAgentSettingsEditor,
} from "./CodingAgentSettingsEditorContext";

vi.mock("@/api", () => ({
  codingAgentGetSettings: vi.fn(),
  codingAgentPrepareProfileHome: vi.fn(),
  codingAgentUpdateSettings: vi.fn(),
}));

const response: CodingAgentSettingsResponse = {
  settings: {
    formatVersion: 1,
    revision: 3,
    activeProfileId: "profile-1",
    semanticJudgeEnabled: true,
    semanticJudgePolicy: { maxRetries: 1, deadlineSeconds: 60 },
    autoDoneEnabled: false,
    profiles: [],
    repositoryHosts: [],
  },
  profileMountAvailability: [],
  availableAgentPresets: [],
};

function wrapper({ children }: { children: React.ReactNode }) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return (
    <QueryClientProvider client={queryClient}>
      <I18nextProvider i18n={i18n}>
        <CodingAgentSettingsEditorProvider>{children}</CodingAgentSettingsEditorProvider>
      </I18nextProvider>
    </QueryClientProvider>
  );
}

describe("CodingAgentSettingsEditorProvider", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(codingAgentGetSettings).mockResolvedValue(response);
    vi.mocked(codingAgentUpdateSettings).mockResolvedValue({
      ...response,
      settings: { ...response.settings, revision: 4 },
    });
    vi.mocked(codingAgentPrepareProfileHome).mockResolvedValue({
      ...response,
      settings: { ...response.settings, revision: 4 },
    });
  });

  it("keeps edits local until save and updates the revision from the response", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.settings?.revision).toBe(3));

    act(() => {
      result.current.update((settings) => ({ ...settings, autoDoneEnabled: true }));
    });
    expect(result.current.dirty).toBe(true);
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();

    await act(async () => {
      await result.current.save();
    });
    expect(codingAgentUpdateSettings).toHaveBeenCalledWith({
      expectedRevision: 3,
      settings: expect.objectContaining({ autoDoneEnabled: true }),
    });
    expect(result.current.dirty).toBe(false);
    expect(result.current.revision).toBe(4);
  });

  it("normalizes optional line fields before sending the complete snapshot", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.settings?.revision).toBe(3));
    act(() =>
      result.current.update((settings) => ({
        ...settings,
        repositoryHosts: [
          {
            labelHost: "forge",
            forge: "github",
            apiBaseUrl: "https://forge.example/api",
            cloneTransport: { kind: "https", baseUrl: "https://forge.example" },
            readCredentialRef: null,
            publicationCredentialRef: null,
          },
        ],
        profiles: [
          {
            profileId: "profile-1",
            version: 1,
            displayName: "agent",
            imageRef: " image ",
            allowedImageRefs: [" image ", "  "],
            command: [" agent ", ""],
            containerHome: "/workspace",
            runtimeEnvironment: { CODEX_HOME: " /home/codex ", EMPTY: " " },
            networkAccess: "public",
            resources: {
              cpuMillis: 1000,
              memoryMib: 1024,
              rootDiskMib: 1024,
              maxParallelRuns: 1,
              useStatic: true,
            },
            mountAllowedRoots: [" /home/user ", ""],
            persistentMounts: [],
            allowedRepositories: [
              { host: " forge ", organization: " team ", repository: " repo " },
              { host: "", organization: "", repository: "" },
            ],
            credentialReference: null,
          },
        ],
      })),
    );
    await act(async () => {
      await result.current.save();
    });
    expect(
      vi.mocked(codingAgentUpdateSettings).mock.calls.at(-1)?.[0].settings.profiles[0],
    ).toEqual(
      expect.objectContaining({
        imageRef: " image ",
        allowedImageRefs: ["image"],
        command: ["agent"],
        mountAllowedRoots: ["/home/user"],
        runtimeEnvironment: { CODEX_HOME: "/home/codex" },
        allowedRepositories: [{ host: "forge", organization: "team", repository: "repo" }],
      }),
    );
  });

  it("discards only the local draft", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.settings?.revision).toBe(3));
    act(() => result.current.update((settings) => ({ ...settings, autoDoneEnabled: true })));
    act(() => result.current.discard());
    expect(result.current.settings?.autoDoneEnabled).toBe(false);
    expect(result.current.dirty).toBe(false);
  });

  it("blocks a draft that references a repository host not in the host list", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.settings?.revision).toBe(3));
    const profile: CodingAgentProfile = {
      profileId: "profile-unknown-host",
      version: 1,
      displayName: "agent",
      imageRef: "registry.example/agent:latest",
      allowedImageRefs: ["registry.example/agent:latest"],
      command: ["agent"],
      containerHome: "/workspace",
      networkAccess: "public",
      resources: {
        cpuMillis: 1000,
        memoryMib: 1024,
        rootDiskMib: 1024,
        maxParallelRuns: 1,
        useStatic: true,
      },
      mountAllowedRoots: [],
      persistentMounts: [],
      allowedRepositories: [
        { host: "missing.example", organization: "team", repository: "project" },
      ],
      credentialReference: null,
    };

    act(() => {
      result.current.update((settings) => ({ ...settings, profiles: [profile] }));
    });

    expect(result.current.valid).toBe(false);
    expect(result.current.validationIssues).toEqual([
      expect.objectContaining({
        code: "repository_host_not_found",
        profileId: profile.profileId,
        repositoryIndex: 0,
      }),
    ]);
    await act(async () => {
      await result.current.save();
    });
    expect(codingAgentUpdateSettings).not.toHaveBeenCalled();
  });

  it("synchronizes the response revision after preparing a clean profile home", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.revision).toBe(3));

    await act(async () => {
      await result.current.prepareHome("profile-1");
    });

    expect(codingAgentPrepareProfileHome).toHaveBeenCalledWith({
      expectedRevision: 3,
      profileId: "profile-1",
    });
    expect(result.current.revision).toBe(4);
    expect(result.current.dirty).toBe(false);
  });

  it("does not prepare a profile home while another draft is dirty", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.revision).toBe(3));
    act(() => {
      result.current.update((settings) => ({ ...settings, autoDoneEnabled: true }));
    });

    await act(async () => {
      await result.current.prepareHome("profile-1");
    });

    expect(codingAgentPrepareProfileHome).not.toHaveBeenCalled();
    expect(result.current.dirty).toBe(true);
  });

  it("clears dialog editing state when the shared draft is discarded", async () => {
    const { result } = renderHook(() => useCodingAgentSettingsEditor(), { wrapper });
    await waitFor(() => expect(result.current.revision).toBe(3));
    act(() => result.current.setEditing(true));
    expect(result.current.editing).toBe(true);

    act(() => result.current.discard());

    expect(result.current.editing).toBe(false);
  });
});
