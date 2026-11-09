import type {
  CodingAgentNativeRepositoryApproval,
  CodingAgentRepositoryHost,
  CodingAgentSettings,
} from "@/types/api";

type NativeRepositoryApproval = CodingAgentNativeRepositoryApproval;

export interface CodingAgentSettingsValidationIssue {
  code:
    | "repository_identity_invalid"
    | "repository_host_not_found"
    | "native_repository_approval_invalid"
    | "host_configuration_invalid";
  profileId?: string;
  repositoryIndex?: number;
  host?: string;
}

function trimmedLines(values: readonly string[]): string[] {
  return values.map((value) => value.trim()).filter(Boolean);
}

function isAbsoluteHostPath(value: string | null | undefined): boolean {
  return value === undefined || value === null || (value.startsWith("/") && !value.includes("\0"));
}

function isBranchNameValid(value: string): boolean {
  return (
    Boolean(value) &&
    value.length <= 256 &&
    !value.startsWith("-") &&
    !value.endsWith("/") &&
    !value.includes("..") &&
    !/[\s~^:?*[\\]/.test(value)
  );
}

export function nativeRepositoryApprovalIsValid(
  approval: NativeRepositoryApproval,
  expectedHost?: string,
): boolean {
  const { repository, allowedBaseBranches } = approval;
  return (
    [repository.host, repository.organization, repository.repository].every(
      (part) => Boolean(part.trim()) && !part.includes("/"),
    ) &&
    (expectedHost === undefined || repository.host.trim() === expectedHost.trim()) &&
    allowedBaseBranches.length > 0 &&
    new Set(allowedBaseBranches.map((branch) => branch.trim())).size ===
      allowedBaseBranches.length &&
    allowedBaseBranches.every((branch) => isBranchNameValid(branch.trim()))
  );
}

export function nativeRepositoryApprovalsAreValid(
  approvals: readonly NativeRepositoryApproval[],
  repositoryHosts: readonly CodingAgentRepositoryHost[],
  expectedHost?: string,
  allowUnconfiguredHost = false,
): boolean {
  const configuredHosts = new Set(
    repositoryHosts.map((host) => host.labelHost.trim()).filter(Boolean),
  );
  const seenRepositories = new Set<string>();
  return approvals.every((approval) => {
    if (
      !nativeRepositoryApprovalIsValid(approval, expectedHost) ||
      (!allowUnconfiguredHost && !configuredHosts.has(approval.repository.host.trim()))
    )
      return false;
    const repositoryKey = `${approval.repository.host.trim()}/${approval.repository.organization.trim()}/${approval.repository.repository.trim()}`;
    if (seenRepositories.has(repositoryKey)) return false;
    seenRepositories.add(repositoryKey);
    return true;
  });
}

/** Normalize editable text fields before the Coding Agent CAS request. */
export function normalizeCodingAgentSettings(settings: CodingAgentSettings): CodingAgentSettings {
  return {
    ...settings,
    ...(settings.nativeRepositoryApprovals
      ? {
          nativeRepositoryApprovals: settings.nativeRepositoryApprovals.map((approval) => ({
            repository: {
              host: approval.repository.host.trim(),
              organization: approval.repository.organization.trim(),
              repository: approval.repository.repository.trim(),
            },
            allowedBaseBranches: trimmedLines(approval.allowedBaseBranches),
          })),
        }
      : {}),
    repositoryHosts: settings.repositoryHosts.map((host) => ({
      ...host,
      cloneTransport: normalizeCloneTransport(host.cloneTransport),
    })),
    profiles: settings.profiles.map((profile) => {
      const runtimeEnvironment = Object.fromEntries(
        Object.entries(profile.runtimeEnvironment ?? {})
          .map(([key, value]) => [key, value.trim()] as const)
          .filter(([, value]) => value),
      );
      const normalizedProfile = {
        ...profile,
        allowedImageRefs: trimmedLines(profile.allowedImageRefs),
        command: trimmedLines(profile.command),
        mountAllowedRoots: trimmedLines(profile.mountAllowedRoots),
        allowedRepositories: profile.allowedRepositories
          .map(({ host, organization, repository }) => ({
            host: host.trim(),
            organization: organization.trim(),
            repository: repository.trim(),
          }))
          .filter(({ host, organization, repository }) => host || organization || repository),
      };
      if (Object.keys(runtimeEnvironment).length > 0) {
        normalizedProfile.runtimeEnvironment = runtimeEnvironment;
      } else {
        delete normalizedProfile.runtimeEnvironment;
      }
      if (Object.hasOwn(profile, "hostConfiguration")) {
        normalizedProfile.hostConfiguration =
          profile.hostConfiguration === null || profile.hostConfiguration === undefined
            ? profile.hostConfiguration
            : {
                codexDirectory: profile.hostConfiguration.codexDirectory?.trim() || null,
                claudeDirectory: profile.hostConfiguration.claudeDirectory?.trim() || null,
                claudeSettingsFile: profile.hostConfiguration.claudeSettingsFile?.trim() || null,
              };
      }
      return normalizedProfile;
    }),
  };
}

function normalizeCloneTransport(
  transport: CodingAgentSettings["repositoryHosts"][number]["cloneTransport"],
): CodingAgentSettings["repositoryHosts"][number]["cloneTransport"] {
  if (transport.kind !== "ssh") return transport;
  const normalized = {
    ...transport,
    host: transport.host.trim(),
    user: transport.user.trim(),
  };
  if (Object.hasOwn(transport, "sshDirectory")) {
    normalized.sshDirectory = transport.sshDirectory?.trim() || null;
  } else {
    delete normalized.sshDirectory;
  }
  return normalized;
}

/** Return row-addressable validation issues that must block a CAS save. */
export function validateCodingAgentSettings(
  settings: CodingAgentSettings,
): CodingAgentSettingsValidationIssue[] {
  const repositoryHosts = new Set(
    settings.repositoryHosts.map((host) => host.labelHost.trim()).filter(Boolean),
  );
  const issues: CodingAgentSettingsValidationIssue[] = [];
  for (const profile of settings.profiles) {
    profile.allowedRepositories.forEach((repository, repositoryIndex) => {
      const empty = !repository.host && !repository.organization && !repository.repository;
      if (empty) return;
      const parts = [repository.host, repository.organization, repository.repository];
      if (parts.some((part) => !part.trim()) || parts.some((part) => part.includes("/"))) {
        issues.push({
          code: "repository_identity_invalid",
          profileId: profile.profileId,
          repositoryIndex,
          host: repository.host,
        });
        return;
      }
      if (!repositoryHosts.has(repository.host.trim())) {
        issues.push({
          code: "repository_host_not_found",
          profileId: profile.profileId,
          repositoryIndex,
          host: repository.host,
        });
      }
    });
    if (profile.hostConfiguration) {
      const configuration = profile.hostConfiguration;
      const hasCliDirectory = Boolean(
        configuration.codexDirectory || configuration.claudeDirectory,
      );
      if (
        !hasCliDirectory ||
        !isAbsoluteHostPath(configuration.codexDirectory) ||
        !isAbsoluteHostPath(configuration.claudeDirectory) ||
        !isAbsoluteHostPath(configuration.claudeSettingsFile)
      ) {
        issues.push({ code: "host_configuration_invalid", profileId: profile.profileId });
      }
    }
  }
  settings.repositoryHosts.forEach((host) => {
    if (
      host.cloneTransport.kind === "ssh" &&
      !isAbsoluteHostPath(host.cloneTransport.sshDirectory)
    ) {
      issues.push({ code: "native_repository_approval_invalid", host: host.labelHost });
    }
  });
  if (
    !nativeRepositoryApprovalsAreValid(
      settings.nativeRepositoryApprovals ?? [],
      settings.repositoryHosts,
    )
  ) {
    issues.push({ code: "native_repository_approval_invalid" });
  }
  return issues;
}
