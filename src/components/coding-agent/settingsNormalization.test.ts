import { describe, expect, it } from "vitest";
import type { CodingAgentSettings } from "@/types/api";
import { normalizeCodingAgentSettings, validateCodingAgentSettings } from "./settingsNormalization";

const baseSettings = (): CodingAgentSettings => ({
  formatVersion: 1,
  revision: 4,
  activeProfileId: "profile",
  semanticJudgeEnabled: true,
  autoDoneEnabled: false,
  profiles: [
    {
      profileId: "profile",
      version: 1,
      displayName: "agent",
      imageRef: "image",
      allowedImageRefs: ["image"],
      command: ["agent"],
      containerHome: "/workspace",
      runtimeEnvironment: { CUSTOM_UNKNOWN: " keep " },
      resources: {
        cpuMillis: 1000,
        memoryMib: 512,
        rootDiskMib: 512,
        maxParallelRuns: 1,
        useStatic: true,
      },
      mountAllowedRoots: [],
      persistentMounts: [],
      allowedRepositories: [],
      credentialReference: "token-id",
    },
  ],
  repositoryHosts: [
    {
      labelHost: "forge.example",
      forge: "github",
      apiBaseUrl: "https://forge.example/api",
      cloneTransport: { kind: "ssh", host: "git.example", user: "git", port: 22 },
      readCredentialRef: "read-id",
      publicationCredentialRef: "publish-id",
    },
  ],
  nativeRepositoryApprovals: [],
});

describe("coding-agent settings normalization", () => {
  it("retains unknown environment entries, token IDs, and omitted legacy fields", () => {
    const normalized = normalizeCodingAgentSettings(baseSettings());
    expect(normalized.profiles[0]?.runtimeEnvironment).toEqual({ CUSTOM_UNKNOWN: "keep" });
    expect(normalized.profiles[0]?.credentialReference).toBe("token-id");
    expect(normalized.profiles[0]).not.toHaveProperty("hostConfiguration");
    expect(normalized.repositoryHosts[0]?.cloneTransport).not.toHaveProperty("sshDirectory");
  });

  it("preserves the backend package baseline while normalizing an ordinary settings save", () => {
    const settings = baseSettings();
    Object.assign(settings, { nativeBackendPackageFingerprint: "a".repeat(64) });
    const normalized = normalizeCodingAgentSettings(settings);
    expect(normalized.nativeBackendPackageFingerprint).toBe("a".repeat(64));
  });

  it("normalizes explicit SSH and host-sharing fields without interpreting tilde paths", () => {
    const settings = baseSettings();
    const host = settings.repositoryHosts[0];
    const profile = settings.profiles[0];
    if (!host || !profile) throw new Error("fixture is incomplete");
    host.cloneTransport = {
      kind: "ssh",
      host: " git.example ",
      user: " git ",
      port: 22,
      sshDirectory: " /home/alice/.ssh ",
    };
    profile.hostConfiguration = {
      codexDirectory: " /home/alice/.codex ",
      claudeDirectory: null,
      claudeSettingsFile: " /home/alice/.claude.json ",
    };
    const normalized = normalizeCodingAgentSettings(settings);
    expect(normalized.repositoryHosts[0]?.cloneTransport).toMatchObject({
      host: "git.example",
      user: "git",
      sshDirectory: "/home/alice/.ssh",
    });
    expect(normalized.profiles[0]?.hostConfiguration).toEqual({
      codexDirectory: "/home/alice/.codex",
      claudeDirectory: null,
      claudeSettingsFile: "/home/alice/.claude.json",
    });
  });

  it("rejects invalid native repository branches and host-sharing paths", () => {
    const settings = baseSettings();
    const host = settings.repositoryHosts[0];
    const profile = settings.profiles[0];
    if (!host || !profile) throw new Error("fixture is incomplete");
    settings.nativeRepositoryApprovals = [
      {
        repository: { host: "forge.example", organization: "team", repository: "repo" },
        allowedBaseBranches: ["bad branch"],
      },
    ];
    profile.hostConfiguration = {
      codexDirectory: "~/.codex",
      claudeDirectory: null,
      claudeSettingsFile: null,
    };
    expect(validateCodingAgentSettings(settings)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ code: "native_repository_approval_invalid" }),
        expect.objectContaining({ code: "host_configuration_invalid" }),
      ]),
    );
  });

  it("rejects native approvals for another host and duplicate repository branches", () => {
    const settings = baseSettings();
    settings.nativeRepositoryApprovals = [
      {
        repository: { host: "other.example", organization: "team", repository: "repo" },
        allowedBaseBranches: ["main"],
      },
      {
        repository: { host: "forge.example", organization: "team", repository: "repo" },
        allowedBaseBranches: ["main"],
      },
      {
        repository: { host: "forge.example", organization: "team", repository: "repo" },
        allowedBaseBranches: ["release"],
      },
    ];
    expect(validateCodingAgentSettings(settings)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          code: "native_repository_approval_invalid",
        }),
      ]),
    );
  });

  it("rejects approvals when no repository host is configured", () => {
    const settings = baseSettings();
    settings.repositoryHosts = [];
    settings.nativeRepositoryApprovals = [
      {
        repository: { host: "forge.example", organization: "team", repository: "repo" },
        allowedBaseBranches: ["main"],
      },
    ];
    expect(validateCodingAgentSettings(settings)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ code: "native_repository_approval_invalid" }),
      ]),
    );
  });
});
