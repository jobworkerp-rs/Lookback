import { useQuery, useQueryClient } from "@tanstack/react-query";
import { open as openPathDialog } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import {
  codingAgentApproveNativeBackend,
  codingAgentGetBackendStatus,
  codingAgentGetNativeSetup,
  codingAgentGetSettings,
  codingAgentPrepareProfileHome,
  codingAgentUpdateSettings,
} from "@/api";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Modal } from "@/components/Modal";
import { SettingsHint } from "@/components/SettingsHint";
import type {
  CodingAgentNativeRepositoryApproval,
  CodingAgentNativeSetupResponse,
  CodingAgentNativeSetupState,
  CodingAgentPersistentMount,
  CodingAgentPreset,
  CodingAgentProfile,
  CodingAgentRepositoryHost,
  CodingAgentRepositoryIdentity,
  CodingAgentSettings,
} from "@/types/api";
import type { CodingAgentSettingsEditor } from "./CodingAgentSettingsEditorContext";
import { ProfileSettingsTerminalPanel } from "./ProfileSettingsTerminalPanel";
import { profileHomeReady } from "./profileHome";
import { RepositoryHostCheckPanel } from "./RepositoryHostCheckPanel";
import { repositoryIdentityLabel, sameRepositoryIdentity } from "./repositoryIdentity";
import {
  nativeRepositoryApprovalIsValid,
  nativeRepositoryApprovalsAreValid,
  normalizeCodingAgentSettings,
  validateCodingAgentSettings,
} from "./settingsNormalization";

const SETTINGS_QUERY_KEY = ["coding-agent-settings"] as const;
const BACKEND_QUERY_KEY = ["coding-agent-backend-status"] as const;
const NATIVE_SETUP_QUERY_PREFIX = ["coding-agent-native-setup"] as const;
const NATIVE_SETUP_APPROVABLE_STATES: ReadonlySet<CodingAgentNativeSetupState> = new Set([
  "not_approved",
  "reapproval_required",
]);

type Translate = ReturnType<typeof useTranslation>["t"];

function nativeSetupUnavailableText(setup: CodingAgentNativeSetupResponse, t: Translate): string {
  return setup.unavailableReason
    ? t(`codingAgent.settings.nativeSetupUnavailableReason.${setup.unavailableReason}`)
    : t("codingAgent.settings.nativeSetupApprovalUnavailableReason");
}

/** Why the approve action is blocked by the backend-reported state alone. */
function nativeSetupStateBlockedReason(
  setup: CodingAgentNativeSetupResponse,
  t: Translate,
): string | undefined {
  switch (setup.state) {
    case "package_mismatch":
      return t("codingAgent.settings.nativeSetupApprovalPackageMismatchReason");
    case "unavailable":
      return nativeSetupUnavailableText(setup, t);
    case "approved":
      return t("codingAgent.settings.nativeSetupApprovalAlreadyApprovedReason");
    default:
      return setup.packageFingerprint
        ? undefined
        : t("codingAgent.settings.nativeSetupApprovalUnavailableReason");
  }
}

/**
 * Repositories a profile may use that the native approval list does not cover
 * yet. The two lists are independent, so this is the gap the user has to close
 * before an Issue can run natively.
 */
function pendingNativeApprovals(
  profiles: CodingAgentProfile[],
  approvals: CodingAgentNativeRepositoryApproval[],
): CodingAgentRepositoryIdentity[] {
  const pending = new Map<string, CodingAgentRepositoryIdentity>();
  for (const profile of profiles) {
    for (const repository of profile.allowedRepositories) {
      if (
        repositoryIdentityIsValid(repository) &&
        !approvals.some((approval) => sameRepositoryIdentity(approval.repository, repository))
      ) {
        pending.set(repositoryIdentityLabel(repository), repository);
      }
    }
  }
  return [...pending.values()];
}
const DEFAULT_SEMANTIC_POLICY = { maxRetries: 0, deadlineSeconds: 125 };

function semanticPolicyIsValid(settings: CodingAgentSettings): boolean {
  const policy = settings.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY;
  return (
    Number.isSafeInteger(policy.maxRetries) &&
    policy.maxRetries >= 0 &&
    policy.maxRetries <= 5 &&
    Number.isSafeInteger(policy.deadlineSeconds) &&
    policy.deadlineSeconds >= 10 &&
    policy.deadlineSeconds <= 600
  );
}

function multiline(values: readonly string[]): string {
  return values.join("\n");
}

function parseLines(value: string): string[] {
  return value.split("\n");
}

function repositoryLines(profile: CodingAgentProfile): string {
  return profile.allowedRepositories
    .map(({ host, organization, repository }) =>
      !host && !organization && !repository ? "" : `${host}/${organization}/${repository}`,
    )
    .join("\n");
}

function parseRepositories(value: string): CodingAgentProfile["allowedRepositories"] {
  return parseLines(value).map((line) => {
    if (!line.trim()) return { host: "", organization: "", repository: "" };
    const [host = "", organization = "", ...repositoryParts] = line.split("/");
    return { host, organization, repository: repositoryParts.join("/") };
  });
}

function repositoryIdentityIsValid(repository: CodingAgentRepositoryIdentity): boolean {
  return [repository.host, repository.organization, repository.repository].every(
    (part) => Boolean(part.trim()) && !part.includes("/"),
  );
}

function profileRepositoriesAreValid(profile: CodingAgentProfile): boolean {
  return profile.allowedRepositories.every((repository) => {
    const empty = !repository.host && !repository.organization && !repository.repository;
    return empty || repositoryIdentityIsValid(repository);
  });
}

function repositoriesAreValid(settings: CodingAgentSettings): boolean {
  return settings.profiles.every(profileRepositoriesAreValid);
}

function profileHasUnregisteredRepository(
  profile: CodingAgentProfile,
  settings: CodingAgentSettings,
): boolean {
  const configuredHosts = new Set(
    settings.repositoryHosts.map((host) => host.labelHost.trim()).filter(Boolean),
  );
  return profile.allowedRepositories.some(
    (repository) =>
      repositoryIdentityIsValid(repository) && !configuredHosts.has(repository.host.trim()),
  );
}

function hasUnregisteredRepository(settings: CodingAgentSettings): boolean {
  return settings.profiles.some((profile) => profileHasUnregisteredRepository(profile, settings));
}

function isHttpsUrl(value: string): boolean {
  try {
    const parsed = new URL(value.trim());
    return parsed.protocol === "https:" && Boolean(parsed.hostname);
  } catch {
    return false;
  }
}

function repositoryHostIsValid(host: CodingAgentRepositoryHost): boolean {
  if (!host.labelHost.trim() || !isHttpsUrl(host.apiBaseUrl)) return false;
  if (host.cloneTransport.kind === "https") {
    return isHttpsUrl(host.cloneTransport.baseUrl);
  }
  return Boolean(
    host.cloneTransport.host.trim() &&
      host.cloneTransport.user.trim() &&
      (host.cloneTransport.port === null ||
        (Number.isSafeInteger(host.cloneTransport.port) &&
          host.cloneTransport.port >= 1 &&
          host.cloneTransport.port <= 65535)) &&
      (!host.cloneTransport.sshDirectory || isAbsoluteHostPath(host.cloneTransport.sshDirectory)),
  );
}

function isAbsoluteHostPath(value: string | null | undefined): boolean {
  return value === undefined || value === null || (value.startsWith("/") && !value.includes("\0"));
}

function repositoryCandidatesForHost(
  settings: CodingAgentSettings,
  labelHost: string,
): CodingAgentRepositoryIdentity[] {
  const candidates = new Map<string, CodingAgentRepositoryIdentity>();
  for (const profile of settings.profiles) {
    for (const repository of profile.allowedRepositories) {
      if (repository.host === labelHost && repositoryIdentityIsValid(repository)) {
        const key = `${repository.host}/${repository.organization}/${repository.repository}`;
        candidates.set(key, repository);
      }
    }
  }
  return [...candidates.values()];
}

const MAX_U32 = 0xffff_ffff;
const MAX_SAFE_U64 = Number.MAX_SAFE_INTEGER;

function resourcesAreValid(settings: CodingAgentSettings): boolean {
  return settings.profiles.every(profileResourcesAreValid);
}

function profileResourcesAreValid(profile: CodingAgentProfile): boolean {
  const { cpuMillis, memoryMib, rootDiskMib, maxParallelRuns } = profile.resources;
  return (
    Number.isSafeInteger(cpuMillis) &&
    cpuMillis > 0 &&
    cpuMillis % 1000 === 0 &&
    cpuMillis <= MAX_U32 &&
    Number.isSafeInteger(memoryMib) &&
    memoryMib > 0 &&
    memoryMib <= MAX_SAFE_U64 &&
    Number.isSafeInteger(rootDiskMib) &&
    rootDiskMib > 0 &&
    rootDiskMib <= MAX_SAFE_U64 &&
    Number.isSafeInteger(maxParallelRuns) &&
    maxParallelRuns > 0 &&
    maxParallelRuns <= MAX_U32
  );
}

function profileRequiredFieldsAreValid(profile: CodingAgentProfile): boolean {
  return Boolean(
    profile.imageRef.trim() &&
      profile.containerHome.trim() &&
      profile.command.some((argument) => argument.trim()),
  );
}

function profileUsesOpenCode(profile: CodingAgentProfile): boolean {
  const executable = profile.command
    .find((argument) => argument.trim())
    ?.trim()
    .split(/[\\/]/)
    .pop()
    ?.toLowerCase();
  return executable === "opencode";
}

function profileHostConfigurationIsValid(profile: CodingAgentProfile): boolean {
  const configuration = profile.hostConfiguration;
  if (configuration === undefined || configuration === null) return true;
  if (profileUsesOpenCode(profile)) return false;
  return (
    Boolean(configuration.codexDirectory || configuration.claudeDirectory) &&
    [
      configuration.codexDirectory,
      configuration.claudeDirectory,
      configuration.claudeSettingsFile,
    ].every(isAbsoluteHostPath)
  );
}

function resourceNumber(value: string, maximum: number): number {
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed > 0 && parsed <= maximum ? parsed : 0;
}

function isRevisionConflict(error: unknown): boolean {
  return /settings changed in another editor|revision conflict/i.test(errorText(error));
}

interface SettingsDraft {
  settings: CodingAgentSettings;
  baseSettings: CodingAgentSettings;
  baseRevision: number;
}

function newProfile(preset?: CodingAgentPreset): CodingAgentProfile {
  return {
    profileId: `profile-${crypto.randomUUID()}`,
    version: 0,
    displayName: preset?.displayName ?? "",
    imageRef: preset?.imageRef ?? "",
    allowedImageRefs: preset ? [preset.imageRef] : [],
    command: preset?.command ?? [],
    containerHome: preset?.containerHome ?? "/home/codespace",
    runtimeEnvironment: { ...(preset?.runtimeEnvironment ?? {}) },
    networkAccess: "public",
    resources: {
      cpuMillis: 1000,
      memoryMib: 2048,
      rootDiskMib: 8192,
      maxParallelRuns: 1,
      useStatic: true,
    },
    mountAllowedRoots: [],
    persistentMounts: [],
    allowedRepositories: [],
    credentialReference: null,
  };
}

function newRepositoryHost(): CodingAgentRepositoryHost {
  return {
    labelHost: "",
    forge: "github",
    apiBaseUrl: "",
    cloneTransport: { kind: "https", baseUrl: "" },
    readCredentialRef: null,
    publicationCredentialRef: null,
  };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export interface CodingAgentSettingsCardProps {
  /**
   * When supplied by Settings, this is the shared draft used by the common
   * save footer. Keeping this optional preserves the standalone card harness
   * used by older callers while the migration is rolled out.
   */
  editor?: CodingAgentSettingsEditor;
  /** Hide the Issue completion controls when Settings renders them elsewhere. */
  showIssuePolicies?: boolean;
  /** Settings owns persistence in the redesigned page; keep local controls for legacy callers. */
  showLocalSaveControls?: boolean;
  /** Show the legacy inline setup guide; redesigned Settings uses the row action dialog. */
  showInlineSetupGuide?: boolean;
}

export function CodingAgentSettingsCard({
  editor: externalEditor,
  showIssuePolicies = true,
  showLocalSaveControls = true,
  showInlineSetupGuide = true,
}: CodingAgentSettingsCardProps = {}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const settingsQuery = useQuery({
    queryKey: SETTINGS_QUERY_KEY,
    queryFn: codingAgentGetSettings,
    retry: false,
  });
  const backendQuery = useQuery({
    queryKey: BACKEND_QUERY_KEY,
    queryFn: codingAgentGetBackendStatus,
    retry: false,
  });
  const nativeSetupQueryRevision =
    externalEditor?.revision ?? settingsQuery.data?.settings.revision ?? 0;
  const nativeSetupQuery = useQuery({
    queryKey: [...NATIVE_SETUP_QUERY_PREFIX, nativeSetupQueryRevision],
    queryFn: codingAgentGetNativeSetup,
    enabled: Boolean(externalEditor),
    retry: false,
  });
  const loaded = externalEditor?.response?.settings ?? settingsQuery.data?.settings;
  const [localEditor, setEditor] = useState<SettingsDraft | null>(null);
  const [selectedProfileId, setSelectedProfileId] = useState<string | null>(null);
  const [repositoryDraftText, setRepositoryDraftText] = useState<string | null>(null);
  const [selectedPresetId, setSelectedPresetId] = useState("");
  const [profileDialogOpen, setProfileDialogOpen] = useState(() => !externalEditor);
  const [authDialogProfileId, setAuthDialogProfileId] = useState<string | null>(null);
  const [hostDialogIndex, setHostDialogIndex] = useState<number | null>(null);
  const [hostCheckIndex, setHostCheckIndex] = useState<number | null>(null);
  const [backendDiagnosticOpen, setBackendDiagnosticOpen] = useState(false);
  const [profileDialogSnapshot, setProfileDialogSnapshot] = useState<CodingAgentProfile | null>(
    null,
  );
  const [profileDialogDraft, setProfileDialogDraft] = useState<CodingAgentProfile | null>(null);
  const [profileDialogHostConfigurationDraft, setProfileDialogHostConfigurationDraft] =
    useState<NonNullable<CodingAgentProfile["hostConfiguration"]> | null>(null);
  const [hostDialogSnapshot, setHostDialogSnapshot] = useState<CodingAgentRepositoryHost | null>(
    null,
  );
  const [hostDialogDraft, setHostDialogDraft] = useState<CodingAgentRepositoryHost | null>(null);
  const [hostDialogApprovalsSnapshot, setHostDialogApprovalsSnapshot] = useState<
    CodingAgentNativeRepositoryApproval[] | undefined
  >();
  const [hostDialogApprovalsDraft, setHostDialogApprovalsDraft] = useState<
    CodingAgentNativeRepositoryApproval[] | undefined
  >();
  const [hostDialogApprovalSource, setHostDialogApprovalSource] = useState<
    CodingAgentNativeRepositoryApproval[] | undefined
  >();
  const [profileCloseConfirm, setProfileCloseConfirm] = useState(false);
  const [hostCloseConfirm, setHostCloseConfirm] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [nativeSetupSaving, setNativeSetupSaving] = useState(false);
  const profileModalCloseRef = useRef<HTMLButtonElement>(null);
  const hadExternalEditor = useRef(Boolean(externalEditor));

  const initializeHostDialogApprovals = useCallback(
    (hostLabel: string, effectiveApprovals: CodingAgentNativeRepositoryApproval[] | undefined) => {
      const approvals = (effectiveApprovals ?? []).filter(
        (approval) => approval.repository.host === hostLabel,
      );
      setHostDialogApprovalSource(
        effectiveApprovals ? structuredClone(effectiveApprovals) : undefined,
      );
      setHostDialogApprovalsSnapshot(effectiveApprovals ? structuredClone(approvals) : undefined);
      setHostDialogApprovalsDraft(effectiveApprovals ? structuredClone(approvals) : undefined);
    },
    [],
  );

  useEffect(() => {
    if (externalEditor || !loaded) return;
    if (!localEditor) {
      setEditor({ settings: loaded, baseSettings: loaded, baseRevision: loaded.revision });
      setSelectedProfileId(loaded.activeProfileId ?? loaded.profiles[0]?.profileId ?? null);
      setRepositoryDraftText(null);
      return;
    }
    const editorIsClean =
      JSON.stringify(localEditor.settings) === JSON.stringify(localEditor.baseSettings);
    if (editorIsClean && localEditor.baseRevision !== loaded.revision) {
      setEditor({ settings: loaded, baseSettings: loaded, baseRevision: loaded.revision });
      setRepositoryDraftText(null);
      setSelectedProfileId((profileId) =>
        loaded.profiles.some((profile) => profile.profileId === profileId)
          ? profileId
          : (loaded.activeProfileId ?? loaded.profiles[0]?.profileId ?? null),
      );
    }
  }, [externalEditor, loaded, localEditor]);

  const editor = externalEditor ?? localEditor;
  const current = editor?.settings ?? loaded ?? null;
  const baseRevision =
    externalEditor?.revision ?? localEditor?.baseRevision ?? loaded?.revision ?? 0;
  const selectedProfileBase = current?.profiles.find(
    (profile) => profile.profileId === selectedProfileId,
  );
  const selectedProfile =
    externalEditor && profileDialogOpen && profileDialogDraft
      ? profileDialogDraft
      : selectedProfileBase;
  const selectedProfileUsesOpenCode = Boolean(
    selectedProfile && profileUsesOpenCode(selectedProfile),
  );
  const selectedProfileAvailability = settingsQuery.data?.profileMountAvailability.find(
    (entry) => entry.profileId === selectedProfileId,
  );
  const localDirty = useMemo(
    () =>
      Boolean(
        localEditor &&
          JSON.stringify(localEditor.settings) !== JSON.stringify(localEditor.baseSettings),
      ),
    [localEditor],
  );
  const dirty = externalEditor?.dirty ?? localDirty;
  const hasInvalidResources = Boolean(current && !resourcesAreValid(current));

  const hasInvalidSemanticPolicy = Boolean(current && !semanticPolicyIsValid(current));
  const hasInvalidRepositorySyntax = Boolean(current && !repositoriesAreValid(current));
  const hasUnregisteredRepositories = Boolean(current && hasUnregisteredRepository(current));
  const hasInvalidRepositories = hasInvalidRepositorySyntax || hasUnregisteredRepositories;
  const hasInvalidRepositoryHosts = Boolean(
    current?.repositoryHosts.some((host) => !repositoryHostIsValid(host)),
  );
  const hasInvalidNativeRepositoryApprovals = Boolean(
    current &&
      !nativeRepositoryApprovalsAreValid(
        current.nativeRepositoryApprovals ?? [],
        current.repositoryHosts,
      ),
  );
  const hasInvalidProfileRequiredFields = Boolean(
    current?.profiles.some((profile) => !profileRequiredFieldsAreValid(profile)),
  );
  const repositoryValidationIssues = current ? validateCodingAgentSettings(current) : [];
  const hasInvalidSettings =
    hasInvalidResources ||
    hasInvalidSemanticPolicy ||
    hasInvalidRepositories ||
    hasInvalidRepositoryHosts ||
    hasInvalidNativeRepositoryApprovals ||
    hasInvalidProfileRequiredFields ||
    Boolean(
      repositoryValidationIssues?.some(
        (issue) =>
          issue.code === "host_configuration_invalid" ||
          issue.code === "native_repository_approval_invalid",
      ),
    );
  const nativeSetup = nativeSetupQuery.data;
  const nativeApprovalSource =
    current?.nativeRepositoryApprovals ?? nativeSetup?.repositoryApprovals;
  const nativeApprovalSourceAvailable = Boolean(
    current && (Object.hasOwn(current, "nativeRepositoryApprovals") || nativeSetup !== undefined),
  );
  const nativeApprovalEditingUnavailable = !nativeApprovalSourceAvailable;

  useEffect(() => {
    if (
      hostDialogIndex === null ||
      !hostDialogDraft ||
      hostDialogApprovalsDraft !== undefined ||
      !nativeApprovalSourceAvailable
    ) {
      return;
    }
    // A dialog opened while the diagnostic query was pending must receive the
    // resolved global policy, without replacing edits made in the meantime.
    initializeHostDialogApprovals(
      hostDialogSnapshot?.labelHost ?? hostDialogDraft.labelHost,
      nativeApprovalSource,
    );
  }, [
    hostDialogApprovalsDraft,
    hostDialogDraft,
    hostDialogIndex,
    hostDialogSnapshot,
    initializeHostDialogApprovals,
    nativeApprovalSource,
    nativeApprovalSourceAvailable,
  ]);
  const nativeSetupRevision = externalEditor?.revision ?? current?.revision ?? 0;
  const nativeSetupInvalidated = Boolean(
    nativeSetup && nativeSetup.settingsRevision !== nativeSetupRevision,
  );
  const externalValid = externalEditor?.valid ?? true;
  const isSaving = externalEditor?.saving ?? saving;
  const hostCheckDisabled = dirty || isSaving;
  const nativeSetupApprovalBlockedReason = nativeSetup
    ? nativeSetupInvalidated
      ? t("codingAgent.settings.nativeSetupApprovalInvalidatedReason")
      : dirty
        ? t("codingAgent.settings.nativeSetupApprovalDirtyReason")
        : nativeSetupSaving
          ? t("codingAgent.settings.nativeSetupApprovalInProgressReason")
          : isSaving
            ? t("codingAgent.settings.nativeSetupApprovalSavingReason")
            : nativeSetupStateBlockedReason(nativeSetup, t)
    : undefined;
  const saveDisabled = !dirty || isSaving || hasInvalidSettings || !externalValid;

  useEffect(() => {
    externalEditor?.setValid(!hasInvalidSettings);
  }, [externalEditor, hasInvalidSettings]);

  useEffect(() => {
    // Settings replaces the standalone card editor after the provider loads.
    // Close the compatibility modal so the shared-draft view starts collapsed.
    if (externalEditor && !hadExternalEditor.current) setProfileDialogOpen(false);
    hadExternalEditor.current = Boolean(externalEditor);
  }, [externalEditor]);

  const updateSettingsDraft = (update: (settings: CodingAgentSettings) => CodingAgentSettings) => {
    if (externalEditor) {
      externalEditor.update(update);
      return;
    }
    setEditor((previous) => {
      const base =
        previous ??
        (loaded ? { settings: loaded, baseSettings: loaded, baseRevision: loaded.revision } : null);
      if (!base) return previous;
      return { ...base, settings: update(base.settings) };
    });
  };

  const updateProfile = (
    profileId: string,
    update: (profile: CodingAgentProfile) => CodingAgentProfile,
  ) => {
    if (externalEditor && profileDialogOpen && profileDialogDraft?.profileId === profileId) {
      setProfileDialogDraft((previous) => (previous ? update(previous) : previous));
      setError(null);
      return;
    }
    updateSettingsDraft((settings) => ({
      ...settings,
      profiles: settings.profiles.map((profile) =>
        profile.profileId === profileId ? update(profile) : profile,
      ),
    }));
    setError(null);
  };

  const updateRepositoryHost = (
    hostIndex: number,
    update: (host: CodingAgentRepositoryHost) => CodingAgentRepositoryHost,
  ) => {
    if (externalEditor && hostDialogIndex !== null && hostDialogDraft) {
      setHostDialogDraft((previous) => (previous ? update(previous) : previous));
      setError(null);
      return;
    }
    updateSettingsDraft((settings) => ({
      ...settings,
      repositoryHosts: settings.repositoryHosts.map((host, index) =>
        index === hostIndex ? update(host) : host,
      ),
    }));
    setError(null);
  };

  const updateNativeRepositoryApprovals = (
    hostLabel: string,
    approvals: CodingAgentNativeRepositoryApproval[],
  ) => {
    updateSettingsDraft((settings) => ({
      ...settings,
      nativeRepositoryApprovals: [
        ...(settings.nativeRepositoryApprovals ?? []).filter(
          (approval) => approval.repository.host !== hostLabel,
        ),
        ...approvals,
      ],
    }));
    setError(null);
  };

  // Seed from the effective list (GUI draft or legacy policy) so the first GUI
  // save does not silently drop repositories the legacy policy still approves.
  const addNativeRepositoryApproval = (repository: CodingAgentRepositoryIdentity) => {
    const kept = (nativeApprovalSource ?? []).filter(
      (approval) => !sameRepositoryIdentity(approval.repository, repository),
    );
    updateSettingsDraft((settings) => ({
      ...settings,
      nativeRepositoryApprovals: [
        ...kept,
        { repository: { ...repository }, allowedBaseBranches: ["main"] },
      ],
    }));
    setError(null);
  };

  const openHostDialogForLabel = (labelHost: string) => {
    const index = current?.repositoryHosts.findIndex((host) => host.labelHost === labelHost) ?? -1;
    if (index >= 0) openHostDialog(index);
  };

  const save = async () => {
    if (externalEditor) {
      setError(null);
      try {
        await externalEditor.save();
      } catch (saveError) {
        setError(
          isRevisionConflict(saveError)
            ? t("codingAgent.settings.revisionConflict")
            : t("codingAgent.settings.saveError"),
        );
      }
      return;
    }
    if (!editor || !current || !dirty || saving || hasInvalidSettings) return;
    setSaving(true);
    setError(null);
    try {
      const response = await codingAgentUpdateSettings({
        expectedRevision: localEditor?.baseRevision ?? baseRevision,
        settings: normalizeCodingAgentSettings(localEditor?.settings ?? current),
      });
      queryClient.setQueryData(SETTINGS_QUERY_KEY, response);
      setEditor({
        settings: response.settings,
        baseSettings: response.settings,
        baseRevision: response.settings.revision,
      });
      setRepositoryDraftText(null);
      setSelectedProfileId((profileId) =>
        response.settings.profiles.some((profile) => profile.profileId === profileId)
          ? profileId
          : (response.settings.activeProfileId ?? response.settings.profiles[0]?.profileId ?? null),
      );
    } catch (saveError) {
      setError(
        isRevisionConflict(saveError)
          ? t("codingAgent.settings.revisionConflict")
          : t("codingAgent.settings.saveError"),
      );
      if (isRevisionConflict(saveError)) await settingsQuery.refetch();
    } finally {
      setSaving(false);
    }
  };

  const reloadAndDiscard = async () => {
    if (externalEditor) {
      externalEditor.discard();
      setError(null);
      return;
    }
    setError(null);
    const refreshed = await settingsQuery.refetch();
    if (!refreshed.data) return;
    setEditor({
      settings: refreshed.data.settings,
      baseSettings: refreshed.data.settings,
      baseRevision: refreshed.data.settings.revision,
    });
    setRepositoryDraftText(null);
    setSelectedProfileId(
      refreshed.data.settings.activeProfileId ??
        refreshed.data.settings.profiles[0]?.profileId ??
        null,
    );
  };

  const prepareProfileHome = async () => {
    if (externalEditor) {
      if (!selectedProfile || dirty || saving || selectedProfile.hostConfiguration) return;
      setError(null);
      try {
        await externalEditor.prepareHome(selectedProfile.profileId);
      } catch (prepareError) {
        setError(
          isRevisionConflict(prepareError)
            ? t("codingAgent.settings.revisionConflict")
            : t("codingAgent.settings.prepareHomeError"),
        );
      }
      return;
    }
    if (!editor || !selectedProfile || dirty || saving || selectedProfile.hostConfiguration) return;
    setSaving(true);
    setError(null);
    try {
      const response = await codingAgentPrepareProfileHome({
        expectedRevision: localEditor?.baseRevision ?? baseRevision,
        profileId: selectedProfile.profileId,
      });
      queryClient.setQueryData(SETTINGS_QUERY_KEY, response);
      setEditor({
        settings: response.settings,
        baseSettings: response.settings,
        baseRevision: response.settings.revision,
      });
    } catch (prepareError) {
      setError(
        isRevisionConflict(prepareError)
          ? t("codingAgent.settings.revisionConflict")
          : t("codingAgent.settings.prepareHomeError"),
      );
      if (isRevisionConflict(prepareError)) await settingsQuery.refetch();
    } finally {
      setSaving(false);
    }
  };

  const approveNativeBackend = async () => {
    if (
      !externalEditor ||
      !nativeSetup ||
      nativeSetupInvalidated ||
      !NATIVE_SETUP_APPROVABLE_STATES.has(nativeSetup.state) ||
      dirty ||
      isSaving
    ) {
      return;
    }
    setNativeSetupSaving(true);
    externalEditor.setExternalSaving(true);
    setError(null);
    try {
      const next = await codingAgentApproveNativeBackend({
        expectedRevision: nativeSetupRevision,
        expectedPackageFingerprint: nativeSetup.packageFingerprint,
      });
      const refreshedSettings = await codingAgentGetSettings();
      queryClient.setQueryData(SETTINGS_QUERY_KEY, refreshedSettings);
      externalEditor.replace(refreshedSettings);
      queryClient.setQueryData(
        [...NATIVE_SETUP_QUERY_PREFIX, refreshedSettings.settings.revision],
        next,
      );
      const refreshedNativeSetup = await codingAgentGetNativeSetup();
      queryClient.setQueryData(
        [...NATIVE_SETUP_QUERY_PREFIX, refreshedSettings.settings.revision],
        refreshedNativeSetup,
      );
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : t("codingAgent.settings.nativeSetupError"));
      await nativeSetupQuery.refetch();
    } finally {
      externalEditor.setExternalSaving(false);
      setNativeSetupSaving(false);
    }
  };

  const addProfile = (preset?: CodingAgentPreset) => {
    if (!current || current.profiles.length >= 64) return;
    const profile = newProfile(preset);
    if (externalEditor) {
      setSelectedProfileId(profile.profileId);
      setProfileDialogSnapshot(null);
      setProfileDialogDraft(profile);
      setProfileDialogHostConfigurationDraft(null);
      setProfileDialogOpen(true);
      setRepositoryDraftText(null);
      setError(null);
      externalEditor.setEditing(true);
      return;
    }
    updateSettingsDraft((settings) => ({ ...settings, profiles: [...settings.profiles, profile] }));
    setSelectedProfileId(profile.profileId);
    setRepositoryDraftText(null);
    setError(null);
  };

  const removeProfile = (profileId: string) => {
    if (!current || profileId === current.activeProfileId) return;
    const profiles = current.profiles.filter((profile) => profile.profileId !== profileId);
    updateSettingsDraft((settings) => ({ ...settings, profiles }));
    setSelectedProfileId(profiles[0]?.profileId ?? current.activeProfileId ?? null);
    setRepositoryDraftText(null);
    setError(null);
  };

  const removeRepositoryHost = (hostIndex: number) => {
    if (!current) return;
    const host = current.repositoryHosts[hostIndex];
    if (
      host &&
      current.profiles.some((profile) =>
        profile.allowedRepositories.some((repository) => repository.host === host.labelHost),
      )
    ) {
      setError(t("codingAgent.settings.hostInUse"));
      return;
    }
    updateSettingsDraft((settings) => {
      const nextSettings = {
        ...settings,
        repositoryHosts: settings.repositoryHosts.filter((_entry, index) => index !== hostIndex),
      };
      if (Object.hasOwn(settings, "nativeRepositoryApprovals")) {
        nextSettings.nativeRepositoryApprovals = settings.nativeRepositoryApprovals?.filter(
          (approval) => approval.repository.host !== host?.labelHost,
        );
      }
      return nextSettings;
    });
    setError(null);
  };

  const openProfileDialog = (profileId: string) => {
    const profile = current?.profiles.find((entry) => entry.profileId === profileId);
    if (!profile) return;
    setSelectedProfileId(profileId);
    setProfileDialogSnapshot(structuredClone(profile));
    setProfileDialogDraft(structuredClone(profile));
    setProfileDialogHostConfigurationDraft(
      profile.hostConfiguration ? structuredClone(profile.hostConfiguration) : null,
    );
    setRepositoryDraftText(null);
    setProfileCloseConfirm(false);
    setProfileDialogOpen(true);
    externalEditor?.setEditing(true);
  };

  const closeProfileDialog = () => {
    const changed = profileDialogDraft
      ? profileDialogSnapshot
        ? JSON.stringify(profileDialogSnapshot) !== JSON.stringify(profileDialogDraft)
        : true
      : false;
    if (changed) {
      setProfileCloseConfirm(true);
      return;
    }
    setProfileCloseConfirm(false);
    setRepositoryDraftText(null);
    setProfileDialogOpen(false);
    setProfileDialogSnapshot(null);
    setProfileDialogDraft(null);
    setProfileDialogHostConfigurationDraft(null);
    externalEditor?.setEditing(false);
  };

  const discardProfileDialog = () => {
    setProfileCloseConfirm(false);
    setRepositoryDraftText(null);
    setProfileDialogOpen(false);
    setProfileDialogSnapshot(null);
    setProfileDialogDraft(null);
    setProfileDialogHostConfigurationDraft(null);
    externalEditor?.setEditing(false);
  };

  const confirmProfileDialog = () => {
    if (
      !profileDialogDraft ||
      !profileRepositoriesAreValid(profileDialogDraft) ||
      !profileResourcesAreValid(profileDialogDraft) ||
      !profileRequiredFieldsAreValid(profileDialogDraft) ||
      !profileHostConfigurationIsValid(profileDialogDraft)
    ) {
      return;
    }
    if (externalEditor && profileDialogDraft) {
      updateSettingsDraft((settings) => ({
        ...settings,
        profiles: settings.profiles.some(
          (entry) => entry.profileId === profileDialogDraft.profileId,
        )
          ? settings.profiles.map((entry) =>
              entry.profileId === profileDialogDraft.profileId ? profileDialogDraft : entry,
            )
          : [...settings.profiles, profileDialogDraft],
      }));
    }
    setProfileDialogOpen(false);
    setProfileCloseConfirm(false);
    setRepositoryDraftText(null);
    setProfileDialogSnapshot(null);
    setProfileDialogDraft(null);
    setProfileDialogHostConfigurationDraft(null);
    externalEditor?.setEditing(false);
  };

  const openHostDialog = (index: number) => {
    const host = current?.repositoryHosts[index];
    if (!host) return;
    setHostDialogSnapshot(structuredClone(host));
    setHostDialogDraft(structuredClone(host));
    initializeHostDialogApprovals(host.labelHost, nativeApprovalSource);
    setHostCloseConfirm(false);
    setHostDialogIndex(index);
    externalEditor?.setEditing(true);
  };

  const closeHostDialog = () => {
    const hostChanged = hostDialogDraft
      ? hostDialogSnapshot
        ? JSON.stringify(hostDialogSnapshot) !== JSON.stringify(hostDialogDraft)
        : true
      : false;
    const approvalsChanged =
      JSON.stringify(hostDialogApprovalsSnapshot) !== JSON.stringify(hostDialogApprovalsDraft);
    const changed = hostChanged || approvalsChanged;
    if (changed) {
      setHostCloseConfirm(true);
      return;
    }
    setHostCloseConfirm(false);
    setHostDialogIndex(null);
    setHostDialogSnapshot(null);
    setHostDialogDraft(null);
    setHostDialogApprovalsSnapshot(undefined);
    setHostDialogApprovalsDraft(undefined);
    setHostDialogApprovalSource(undefined);
    externalEditor?.setEditing(false);
  };

  const discardHostDialog = () => {
    setHostCloseConfirm(false);
    setHostDialogIndex(null);
    setHostDialogSnapshot(null);
    setHostDialogDraft(null);
    setHostDialogApprovalsSnapshot(undefined);
    setHostDialogApprovalsDraft(undefined);
    setHostDialogApprovalSource(undefined);
    externalEditor?.setEditing(false);
  };

  const confirmHostDialog = () => {
    if (externalEditor && hostDialogDraft && hostDialogIndex !== null) {
      const previousLabel = hostDialogSnapshot?.labelHost ?? hostDialogDraft.labelHost;
      const approvalsChanged =
        JSON.stringify(hostDialogApprovalsSnapshot) !== JSON.stringify(hostDialogApprovalsDraft);
      updateSettingsDraft((settings) => {
        const nextSettings = {
          ...settings,
          repositoryHosts:
            hostDialogIndex < settings.repositoryHosts.length
              ? settings.repositoryHosts.map((entry, index) =>
                  index === hostDialogIndex ? hostDialogDraft : entry,
                )
              : [...settings.repositoryHosts, hostDialogDraft],
        };
        if (approvalsChanged) {
          nextSettings.nativeRepositoryApprovals = [
            ...(hostDialogApprovalSource ?? []).filter(
              (approval) =>
                approval.repository.host !== previousLabel &&
                approval.repository.host !== hostDialogDraft.labelHost,
            ),
            ...(hostDialogApprovalsDraft ?? []),
          ];
        }
        return nextSettings;
      });
    }
    setHostDialogIndex(null);
    setHostCloseConfirm(false);
    setHostDialogSnapshot(null);
    setHostDialogDraft(null);
    setHostDialogApprovalsSnapshot(undefined);
    setHostDialogApprovalsDraft(undefined);
    setHostDialogApprovalSource(undefined);
    externalEditor?.setEditing(false);
  };

  const authProfile = current?.profiles.find(
    (profile) => profile.profileId === authDialogProfileId,
  );
  const profileCanConfirm = Boolean(
    profileDialogDraft &&
      profileRepositoriesAreValid(profileDialogDraft) &&
      profileResourcesAreValid(profileDialogDraft) &&
      profileRequiredFieldsAreValid(profileDialogDraft) &&
      profileHostConfigurationIsValid(profileDialogDraft),
  );
  const hostForDialog =
    hostDialogDraft ??
    (hostDialogIndex === null ? undefined : current?.repositoryHosts[hostDialogIndex]);
  const hostValidationHosts =
    current && hostForDialog && hostDialogIndex !== null
      ? hostDialogIndex < current.repositoryHosts.length
        ? current.repositoryHosts.map((host, index) =>
            index === hostDialogIndex ? hostForDialog : host,
          )
        : [...current.repositoryHosts, hostForDialog]
      : (current?.repositoryHosts ?? []);
  const hostCanConfirm = Boolean(
    hostDialogDraft &&
      repositoryHostIsValid(hostDialogDraft) &&
      nativeRepositoryApprovalsAreValid(
        hostDialogApprovalsDraft ?? [],
        hostValidationHosts,
        hostDialogDraft.labelHost,
      ),
  );

  if (!loaded) {
    return (
      <section className="settings-card" aria-label={t("codingAgent.settings.title")}>
        <div className="settings-card-title">{t("codingAgent.settings.title")}</div>
        {settingsQuery.isError ? (
          <div role="alert" className="settings-card-desc">
            {t("codingAgent.settings.loadError")}
            <button type="button" className="btn" onClick={() => void settingsQuery.refetch()}>
              {t("codingAgent.settings.retry")}
            </button>
          </div>
        ) : (
          <div role="status" className="settings-card-desc">
            {t("common.loading")}
          </div>
        )}
      </section>
    );
  }

  return (
    <section className="settings-card" aria-label={t("codingAgent.settings.title")}>
      <div className="coding-agent-settings-card-header">
        <div className="settings-card-title">{t("codingAgent.settings.title")}</div>
        {externalEditor && (
          <div className="coding-agent-settings-card-header-actions">
            {backendQuery.data?.blockers.length ? (
              <button
                type="button"
                className="settings-status-icon settings-status-icon--button"
                aria-label={t("codingAgent.settings.backendBlockers")}
                onClick={() => setBackendDiagnosticOpen(true)}
              >
                !
              </button>
            ) : (
              <span className="settings-status-icon" aria-hidden="true">
                i
              </span>
            )}
            <SettingsHint
              label={t("codingAgent.settings.settingsHelpTitle")}
              content={t("codingAgent.settings.settingsHelpDescription")}
              tone={backendQuery.data?.blockers.length ? "warning" : "info"}
            />
          </div>
        )}
      </div>
      {!externalEditor && (
        <div className="settings-card-desc">{t("codingAgent.settings.description")}</div>
      )}
      {!externalEditor && (
        <>
          <div role="status" className="settings-card-desc">
            {backendQuery.data?.enabled
              ? t("codingAgent.settings.backendEnabled")
              : t("codingAgent.settings.backendDisabled")}
          </div>
          {backendQuery.data && backendQuery.data.blockers.length > 0 && (
            <ul
              aria-label={t("codingAgent.settings.backendBlockers")}
              className="settings-card-desc"
            >
              {backendQuery.data.blockers.map((blocker) => (
                <li key={blocker}>
                  {t(`codingAgent.run.blockerExplanations.${blocker}`, {
                    defaultValue: t("codingAgent.run.blockerExplanations.default"),
                  })}{" "}
                  <code>{blocker}</code>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
      {externalEditor && backendDiagnosticOpen && backendQuery.data?.blockers.length
        ? createPortal(
            <Modal
              focusManagement
              onClose={() => setBackendDiagnosticOpen(false)}
              ariaLabel={t("codingAgent.settings.backendBlockers")}
            >
              <section className="coding-agent-settings-diagnostic">
                <div className="coding-agent-settings-modal-header">
                  <div>
                    <p className="coding-agent-settings-modal-context">
                      {t("codingAgent.settings.backendStatusHelpLabel")}
                    </p>
                    <h2>{t("codingAgent.settings.backendBlockers")}</h2>
                  </div>
                  <button
                    type="button"
                    className="icon-btn coding-agent-settings-modal-close"
                    aria-label={t("codingAgent.settings.closeDialog")}
                    onClick={() => setBackendDiagnosticOpen(false)}
                  >
                    ×
                  </button>
                </div>
                <div className="coding-agent-settings-modal-body">
                  <p className="settings-card-desc">
                    {t("codingAgent.settings.backendDiagnosticDescription")}
                  </p>
                  <ul aria-label={t("codingAgent.settings.backendBlockers")}>
                    {backendQuery.data.blockers.map((blocker) => (
                      <li key={blocker}>
                        <span>{t("codingAgent.settings.backendDiagnosticBlocker")}: </span>
                        <code>{blocker}</code>
                      </li>
                    ))}
                  </ul>
                  <div className="coding-agent-settings-dialog-actions">
                    <button
                      type="button"
                      className="btn"
                      onClick={() => setBackendDiagnosticOpen(false)}
                    >
                      {t("common.cancel")}
                    </button>
                  </div>
                </div>
              </section>
            </Modal>,
            document.body,
          )
        : null}
      {backendQuery.isError && !externalEditor && (
        <p role="status" className="settings-card-desc">
          {t("codingAgent.settings.backendStatusUnavailable")}
        </p>
      )}
      {!externalEditor && (
        <div className="settings-card-desc">
          {t("codingAgent.settings.revision", { revision: loaded.revision })}
          {" · "}
          {t("codingAgent.settings.activeProfile", {
            profileId: loaded.activeProfileId ?? t("codingAgent.settings.none"),
          })}
        </div>
      )}
      {editor && loaded.revision !== baseRevision && dirty && (
        <p role="status" className="settings-card-desc">
          {t("codingAgent.settings.remoteRevisionChanged", {
            baseRevision,
            serverRevision: loaded.revision,
          })}
        </p>
      )}
      {externalEditor && (
        <NativeSetupDiagnostic
          setup={nativeSetup}
          loading={nativeSetupQuery.isPending}
          error={nativeSetupQuery.isError}
          invalidated={nativeSetupInvalidated}
          saving={nativeSetupSaving}
          blockedReason={nativeSetupApprovalBlockedReason}
          approvals={nativeApprovalSource}
          approvalsEditable={nativeApprovalSourceAvailable}
          approvalListUnset={
            current?.nativeRepositoryApprovals == null &&
            nativeSetup?.approvalSource === "legacy_policy"
          }
          profiles={current?.profiles ?? []}
          repositoryHosts={current?.repositoryHosts ?? []}
          dirty={dirty}
          onApprove={() => void approveNativeBackend()}
          onAddApproval={addNativeRepositoryApproval}
          onOpenHost={openHostDialogForLabel}
        />
      )}
      <fieldset disabled={isSaving} style={{ border: 0, margin: 0, padding: 0, minWidth: 0 }}>
        {showIssuePolicies && (
          <>
            <div className="settings-row">
              <label>
                <input
                  type="checkbox"
                  checked={current?.semanticJudgeEnabled ?? true}
                  onChange={(event) => {
                    const enabled = event.target.checked;
                    updateSettingsDraft((settings) => ({
                      ...settings,
                      semanticJudgeEnabled: enabled,
                      autoDoneEnabled: enabled && settings.autoDoneEnabled,
                    }));
                  }}
                />
                {t("codingAgent.settings.semanticJudgeEnabled")}
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={current?.autoDoneEnabled ?? false}
                  disabled={!current?.semanticJudgeEnabled}
                  onChange={(event) => {
                    const enabled = event.target.checked;
                    updateSettingsDraft((settings) => ({ ...settings, autoDoneEnabled: enabled }));
                  }}
                />
                {t("codingAgent.settings.autoDoneEnabled")}
              </label>
            </div>
            <div className="settings-row">
              <label>
                {t("codingAgent.settings.semanticRetryCount")}
                <input
                  type="number"
                  min={0}
                  max={5}
                  step={1}
                  disabled={!current?.semanticJudgeEnabled}
                  value={
                    Number.isFinite(
                      (current?.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY).maxRetries,
                    )
                      ? (current?.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY).maxRetries
                      : ""
                  }
                  onChange={(event) => {
                    const value = event.currentTarget.value;
                    updateSettingsDraft((settings) => ({
                      ...settings,
                      semanticJudgePolicy: {
                        ...(settings.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY),
                        maxRetries: value === "" ? Number.NaN : Number(value),
                      },
                    }));
                  }}
                />
              </label>
              <label>
                {t("codingAgent.settings.semanticDeadline")}
                <input
                  type="number"
                  min={10}
                  max={600}
                  step={1}
                  disabled={!current?.semanticJudgeEnabled}
                  value={
                    Number.isFinite(
                      (current?.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY).deadlineSeconds,
                    )
                      ? (current?.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY).deadlineSeconds
                      : ""
                  }
                  onChange={(event) => {
                    const value = event.currentTarget.value;
                    updateSettingsDraft((settings) => ({
                      ...settings,
                      semanticJudgePolicy: {
                        ...(settings.semanticJudgePolicy ?? DEFAULT_SEMANTIC_POLICY),
                        deadlineSeconds: value === "" ? Number.NaN : Number(value),
                      },
                    }));
                  }}
                />
              </label>
            </div>
          </>
        )}
        <section
          className="coding-agent-settings-summary"
          aria-labelledby="coding-agent-profile-list-title"
        >
          <div className="coding-agent-settings-section-toolbar">
            <h3 id="coding-agent-profile-list-title">
              {t("codingAgent.settings.profileListTitle")}
            </h3>
            <div className="coding-agent-settings-section-meta coding-agent-settings-toolbar-actions">
              <span className="settings-count-chip">
                {t("codingAgent.settings.profileCount", { count: current?.profiles.length ?? 0 })}
              </span>
              <SettingsHint
                label={t("codingAgent.settings.profileListHelpLabel")}
                content={t("codingAgent.settings.profileListHelp")}
              />
              <select
                id="coding-agent-preset-select"
                aria-label={t("codingAgent.settings.addPresetSelect")}
                value={selectedPresetId}
                onChange={(event) => setSelectedPresetId(event.currentTarget.value)}
              >
                <option value="">{t("codingAgent.settings.selectPreset")}</option>
                {(settingsQuery.data?.availableAgentPresets ?? []).map((preset) => (
                  <option key={preset.id} value={preset.id}>
                    {preset.displayName}
                  </option>
                ))}
              </select>
              <button
                type="button"
                className="btn"
                onClick={() => {
                  const preset = settingsQuery.data?.availableAgentPresets.find(
                    (entry) => entry.id === selectedPresetId,
                  );
                  addProfile(preset);
                }}
                disabled={!current || current.profiles.length >= 64}
              >
                {t("codingAgent.settings.addProfile")}
              </button>
            </div>
          </div>
          {(current?.profiles.length ?? 0) === 0 && (
            <p role="status">{t("codingAgent.settings.profileListEmpty")}</p>
          )}
          <div className="coding-agent-settings-summary-list">
            {current?.profiles.map((profile) => (
              <div className="coding-agent-settings-summary-row" key={profile.profileId}>
                <div>
                  <div className="coding-agent-settings-identity">
                    <strong>{profile.displayName || profile.profileId}</strong>
                    {profile.profileId === current.activeProfileId && (
                      <span className="settings-status-chip">
                        {t("codingAgent.settings.defaultProfile")}
                      </span>
                    )}
                  </div>
                  <span className="settings-card-desc coding-agent-settings-metadata">
                    {profile.command[0] || t("codingAgent.settings.unknownCommand")} ·{" "}
                    {t("codingAgent.settings.allowedRepositoryCount", {
                      count: profile.allowedRepositories.length,
                    })}
                  </span>
                  {profileHasUnregisteredRepository(profile, current) && (
                    <div
                      role="alert"
                      className="settings-error coding-agent-settings-row-warning"
                      data-issue-code={
                        repositoryValidationIssues.find(
                          (issue) => issue.profileId === profile.profileId,
                        )?.code
                      }
                    >
                      <span className="visually-hidden">
                        {t("codingAgent.settings.unregisteredRepositoryHost")}
                      </span>
                      <SettingsHint
                        label={t("codingAgent.settings.unregisteredRepositoryHost")}
                        content={t("codingAgent.settings.unregisteredRepositoryHostHelp")}
                        tone="warning"
                        onActivate={() => openProfileDialog(profile.profileId)}
                      />
                    </div>
                  )}
                </div>
                <div className="coding-agent-settings-summary-actions">
                  <button
                    type="button"
                    className="btn"
                    onClick={() => openProfileDialog(profile.profileId)}
                  >
                    {t("codingAgent.settings.editProfile")}
                  </button>
                  <button
                    type="button"
                    className="btn"
                    disabled={profile.profileId === current.activeProfileId}
                    onClick={() => removeProfile(profile.profileId)}
                  >
                    {t("codingAgent.settings.removeProfile")}
                  </button>
                  <button
                    type="button"
                    className="btn"
                    onClick={() => {
                      setSelectedProfileId(profile.profileId);
                      setRepositoryDraftText(null);
                      setError(null);
                      setAuthDialogProfileId(profile.profileId);
                    }}
                  >
                    {t("codingAgent.settings.openAuthentication")}
                  </button>
                </div>
              </div>
            ))}
          </div>
          {!externalEditor && selectedProfile && (
            <label className="settings-row">
              <span className="settings-row-label">
                {t("codingAgent.settings.selectedProfile")}
              </span>
              <select
                id="coding-agent-profile-select"
                value={selectedProfileId ?? ""}
                onChange={(event) => {
                  setSelectedProfileId(event.currentTarget.value || null);
                  setRepositoryDraftText(null);
                }}
              >
                {current?.profiles.map((profile) => (
                  <option key={profile.profileId} value={profile.profileId}>
                    {profile.displayName || profile.profileId}
                  </option>
                ))}
              </select>
            </label>
          )}
        </section>

        {showInlineSetupGuide && (
          <section
            className="coding-agent-settings-setup"
            aria-labelledby="coding-agent-settings-setup-title"
          >
            <h3 id="coding-agent-settings-setup-title">{t("codingAgent.settings.setupTitle")}</h3>
            <p>{t("codingAgent.settings.setupDescription")}</p>
            <p>
              {t("codingAgent.settings.setupTargetProfile", {
                profileName:
                  selectedProfile?.displayName ||
                  selectedProfile?.profileId ||
                  t("codingAgent.settings.none"),
              })}
            </p>
            <ol aria-label={t("codingAgent.settings.setupStepsLabel")}>
              <li>
                <strong>{t("codingAgent.settings.setupStepSave")}</strong>
                <p>{t("codingAgent.settings.setupSaveDescription")}</p>
                {showLocalSaveControls && (
                  <button
                    type="button"
                    className="btn"
                    aria-label={t("codingAgent.settings.setupSaveButton")}
                    disabled={saveDisabled}
                    onClick={() => void save()}
                  >
                    {isSaving ? t("codingAgent.settings.saving") : t("codingAgent.settings.save")}
                  </button>
                )}
              </li>
              <li>
                <strong>{t("codingAgent.settings.setupStepHome")}</strong>
                <p>{t("codingAgent.settings.setupHomeDescription")}</p>
              </li>
              <li>
                <strong>{t("codingAgent.settings.setupStepTerminal")}</strong>
                <p>{t("codingAgent.settings.setupTerminalDescription")}</p>
              </li>
            </ol>
            {selectedProfile ? (
              <>
                <fieldset className="coding-agent-settings-mounts">
                  <legend>{t("codingAgent.settings.persistentMounts")}</legend>
                  <p>{t("codingAgent.settings.prepareHomeDescription")}</p>
                  {(dirty || isSaving) && (
                    <p role="status">
                      {isSaving
                        ? t("codingAgent.settings.prepareHomeSaving")
                        : t("codingAgent.settings.prepareHomeDirty")}
                    </p>
                  )}
                  <button
                    type="button"
                    className="btn"
                    disabled={dirty || isSaving || Boolean(selectedProfile.hostConfiguration)}
                    onClick={() => void prepareProfileHome()}
                  >
                    {t("codingAgent.settings.prepareHome")}
                  </button>
                  {selectedProfile.hostConfiguration && (
                    <SettingsHint
                      label={t("codingAgent.settings.hostConfigurationPrepareHomeHelpLabel")}
                      content={t("codingAgent.settings.hostConfigurationPrepareHomeHelp")}
                      tone="info"
                    />
                  )}
                  {selectedProfile.persistentMounts.map((mount, index) => (
                    <PersistentMountEditor
                      // biome-ignore lint/suspicious/noArrayIndexKey: Mount DTOs have no stable ID; edits preserve their array position.
                      key={index}
                      mount={mount}
                      index={index}
                      onChange={(nextMount) =>
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          persistentMounts: profile.persistentMounts.map((entry, entryIndex) =>
                            entryIndex === index ? nextMount : entry,
                          ),
                        }))
                      }
                      onRemove={() =>
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          persistentMounts: profile.persistentMounts.filter(
                            (_entry, entryIndex) => entryIndex !== index,
                          ),
                        }))
                      }
                    />
                  ))}
                  <button
                    type="button"
                    className="btn"
                    onClick={() =>
                      updateProfile(selectedProfile.profileId, (profile) => ({
                        ...profile,
                        persistentMounts: [
                          ...profile.persistentMounts,
                          {
                            hostPath: "",
                            guestPath: "/workspace",
                            readOnly: true,
                            executable: false,
                          },
                        ],
                      }))
                    }
                  >
                    {t("codingAgent.settings.addMount")}
                  </button>
                </fieldset>
                {editor && (
                  <ProfileSettingsTerminalPanel
                    profileId={selectedProfile.profileId}
                    revision={baseRevision}
                    dirty={dirty}
                    saving={isSaving}
                    credentialReference={selectedProfile.credentialReference}
                    homeReady={profileHomeReady(selectedProfile)}
                    command={selectedProfile.command}
                  />
                )}
              </>
            ) : (
              <p role="status">{t("codingAgent.settings.setupNoProfile")}</p>
            )}
          </section>
        )}

        {authProfile && (
          <Modal
            wide
            focusManagement
            onClose={() => {
              setAuthDialogProfileId(null);
              setError(null);
            }}
            ariaLabel={t("codingAgent.settings.authenticationDialogTitle", {
              profileName: authProfile.displayName || authProfile.profileId,
            })}
          >
            <section className="coding-agent-settings-setup">
              <h3>
                {t("codingAgent.settings.authenticationDialogTitle", {
                  profileName: authProfile.displayName || authProfile.profileId,
                })}
              </h3>
              <p>{t("codingAgent.settings.authenticationDialogDescription")}</p>
              <p role="status" className="settings-card-desc">
                {t(
                  profileHomeReady(authProfile)
                    ? "codingAgent.settings.setupHomeReady"
                    : "codingAgent.settings.setupHomeMissing",
                )}
              </p>
              <button
                type="button"
                className="btn"
                disabled={dirty || isSaving || Boolean(authProfile.hostConfiguration)}
                onClick={() => void prepareProfileHome()}
              >
                {t("codingAgent.settings.prepareHome")}
              </button>
              {authProfile.hostConfiguration && (
                <SettingsHint
                  label={t("codingAgent.settings.hostConfigurationPrepareHomeHelpLabel")}
                  content={t("codingAgent.settings.hostConfigurationPrepareHomeHelp")}
                  tone="info"
                />
              )}
              {dirty && (
                <p role="status" className="settings-card-desc">
                  {t("codingAgent.settings.prepareHomeDirty")}
                </p>
              )}
              {error && (
                <p role="alert" className="settings-error">
                  {error}
                </p>
              )}
              {editor && (
                <ProfileSettingsTerminalPanel
                  profileId={authProfile.profileId}
                  revision={baseRevision}
                  dirty={dirty}
                  saving={isSaving}
                  credentialReference={authProfile.credentialReference}
                  homeReady={profileHomeReady(authProfile)}
                  command={authProfile.command}
                />
              )}
              <div className="coding-agent-settings-dialog-actions">
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    setAuthDialogProfileId(null);
                    setError(null);
                  }}
                >
                  {t("common.cancel")}
                </button>
              </div>
            </section>
          </Modal>
        )}

        {selectedProfile && profileDialogOpen && (
          <Modal
            wide
            focusManagement
            initialFocusRef={profileModalCloseRef}
            onClose={closeProfileDialog}
            ariaLabel={t("codingAgent.settings.profileDialogTitle", {
              profileName: selectedProfile.displayName || selectedProfile.profileId,
            })}
          >
            <div className="coding-agent-settings-modal-header">
              <div>
                <div className="coding-agent-settings-modal-context coding-agent-settings-modal-meta">
                  <span>
                    {t("codingAgent.settings.profileDialogContext", {
                      profileId: selectedProfile.profileId,
                    })}
                  </span>
                  <span>
                    {t("codingAgent.settings.profileVersion", {
                      version: selectedProfile.version,
                    })}
                  </span>
                  <SettingsHint
                    label={t("codingAgent.settings.profileIdentityHelpLabel")}
                    content={t("codingAgent.settings.profileIdentityHelp")}
                    tone="info"
                  />
                  {externalEditor &&
                    selectedProfileAvailability &&
                    !selectedProfileAvailability.available && (
                      <SettingsHint
                        label={t("codingAgent.settings.mountsUnavailable")}
                        content={t("codingAgent.settings.profileMountAvailabilityHelp")}
                        tone="warning"
                      />
                    )}
                </div>
                <h2>
                  {t("codingAgent.settings.profileDialogTitle", {
                    profileName: selectedProfile.displayName || selectedProfile.profileId,
                  })}
                </h2>
              </div>
              <button
                ref={profileModalCloseRef}
                type="button"
                className="icon-btn coding-agent-settings-modal-close"
                aria-label={t("codingAgent.settings.closeDialog")}
                onClick={closeProfileDialog}
              >
                ×
              </button>
            </div>
            <div className="coding-agent-settings-modal-body">
              <div className="coding-agent-settings-form">
                {selectedProfileAvailability && !externalEditor && (
                  <div role="status" className="settings-card-desc">
                    {selectedProfileAvailability.available
                      ? t("codingAgent.settings.mountsAvailable")
                      : t("codingAgent.settings.mountsUnavailable")}
                    {selectedProfileAvailability.blockers.map((blocker) => (
                      <span key={blocker}>
                        {t(`codingAgent.run.blockerExplanations.${blocker}`, {
                          defaultValue: t("codingAgent.run.blockerExplanations.default"),
                        })}{" "}
                        {!externalEditor && <code>{blocker}</code>}
                      </span>
                    ))}
                  </div>
                )}
                <label className="settings-row">
                  <span className="settings-row-label">
                    {t("codingAgent.settings.displayName")}
                  </span>
                  <input
                    aria-label={t("codingAgent.settings.displayName")}
                    value={selectedProfile.displayName}
                    onChange={(event) => {
                      const displayName = event.currentTarget.value;
                      updateProfile(selectedProfile.profileId, (profile) => ({
                        ...profile,
                        displayName,
                      }));
                    }}
                  />
                </label>
                <div className="settings-row">
                  <SettingsFieldLabel
                    fieldId="coding-agent-profile-image-ref"
                    label={t("codingAgent.settings.imageRef")}
                    hintLabel={t("codingAgent.settings.imageRefHelpLabel")}
                    hintContent={t("codingAgent.settings.imageRefHelp")}
                  />
                  <input
                    id="coding-agent-profile-image-ref"
                    className="coding-agent-settings-code-value"
                    aria-label={t("codingAgent.settings.imageRef")}
                    aria-invalid={!selectedProfile.imageRef.trim()}
                    value={selectedProfile.imageRef}
                    onChange={(event) => {
                      const imageRef = event.currentTarget.value;
                      updateProfile(selectedProfile.profileId, (profile) => ({
                        ...profile,
                        imageRef,
                      }));
                    }}
                  />
                </div>
                <div className="settings-row coding-agent-settings-field--full">
                  <SettingsFieldLabel
                    fieldId="coding-agent-profile-command-args"
                    label={t("codingAgent.settings.commandArgs")}
                    hintLabel={t("codingAgent.settings.commandArgsHelpLabel")}
                    hintContent={t("codingAgent.settings.commandArgsHelp")}
                  />
                  <textarea
                    id="coding-agent-profile-command-args"
                    className="coding-agent-settings-code-value"
                    aria-label={t("codingAgent.settings.commandArgs")}
                    aria-invalid={!selectedProfile.command.some((argument) => argument.trim())}
                    rows={3}
                    value={multiline(selectedProfile.command)}
                    onChange={(event) => {
                      const command = parseLines(event.currentTarget.value);
                      updateProfile(selectedProfile.profileId, (profile) => ({
                        ...profile,
                        command,
                      }));
                    }}
                  />
                </div>
                <div className="settings-row coding-agent-settings-field--full">
                  <SettingsFieldLabel
                    fieldId="coding-agent-profile-allowed-repositories"
                    label={t("codingAgent.settings.allowedRepositories")}
                    hintLabel={t("codingAgent.settings.allowedRepositoriesHelpLabel")}
                    hintContent={t("codingAgent.settings.allowedRepositoriesHelp")}
                  />
                  <textarea
                    id="coding-agent-profile-allowed-repositories"
                    className="coding-agent-settings-code-value"
                    aria-label={t("codingAgent.settings.allowedRepositories")}
                    aria-invalid={!profileRepositoriesAreValid(selectedProfile)}
                    rows={3}
                    value={repositoryDraftText ?? repositoryLines(selectedProfile)}
                    onChange={(event) => {
                      const value = event.currentTarget.value;
                      setRepositoryDraftText(value);
                      const allowedRepositories = parseRepositories(value);
                      updateProfile(selectedProfile.profileId, (profile) => ({
                        ...profile,
                        allowedRepositories,
                      }));
                    }}
                  />
                </div>
                {!profileRepositoriesAreValid(selectedProfile) && (
                  <p
                    role="alert"
                    className="settings-error"
                    data-issue-code={
                      repositoryValidationIssues.find(
                        (issue) => issue.profileId === selectedProfile.profileId,
                      )?.code
                    }
                  >
                    {t("codingAgent.settings.invalidRepositories")}
                  </p>
                )}
                {profileRepositoriesAreValid(selectedProfile) &&
                  current &&
                  profileHasUnregisteredRepository(selectedProfile, current) && (
                    <p
                      role="alert"
                      className="settings-error"
                      data-issue-code={
                        repositoryValidationIssues.find(
                          (issue) => issue.profileId === selectedProfile.profileId,
                        )?.code
                      }
                    >
                      {t("codingAgent.settings.unregisteredRepositoryHost")}
                    </p>
                  )}
                {!profileRequiredFieldsAreValid(selectedProfile) && (
                  <p role="alert" className="settings-error">
                    {t("codingAgent.settings.invalidProfileRequired")}
                  </p>
                )}
                <details
                  className="coding-agent-settings-details"
                  open={!profileRequiredFieldsAreValid(selectedProfile)}
                >
                  <summary>
                    <span className="coding-agent-settings-summary-content">
                      {t("codingAgent.settings.profileRuntimeGroup")}
                      <SettingsHint
                        label={t("codingAgent.settings.profileRuntimeGroupHelpLabel")}
                        content={t("codingAgent.settings.profileRuntimeGroupHelp")}
                      />
                    </span>
                  </summary>
                  <div className="settings-row">
                    <SettingsFieldLabel
                      fieldId="coding-agent-profile-container-home"
                      label={t("codingAgent.settings.containerHome")}
                      hintLabel={t("codingAgent.settings.containerHomeHelpLabel")}
                      hintContent={t("codingAgent.settings.containerHomeHelp")}
                    />
                    <input
                      id="coding-agent-profile-container-home"
                      className="coding-agent-settings-code-value"
                      aria-label={t("codingAgent.settings.containerHome")}
                      aria-invalid={!selectedProfile.containerHome.trim()}
                      value={selectedProfile.containerHome}
                      onChange={(event) => {
                        const containerHome = event.currentTarget.value;
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          containerHome,
                        }));
                      }}
                    />
                  </div>
                  <details className="coding-agent-settings-environment">
                    <summary>
                      <span className="coding-agent-settings-summary-content">
                        {t("codingAgent.settings.runtimeEnvironment")}
                        <SettingsHint
                          label={t("codingAgent.settings.runtimeEnvironmentHelpLabel")}
                          content={t("codingAgent.settings.runtimeEnvironmentHelp")}
                        />
                      </span>
                    </summary>
                    {(["CODEX_HOME", "CARGO_HOME", "RUSTUP_HOME"] as const).map((key) => (
                      <label className="settings-row" key={key}>
                        <span className="settings-row-label">{key}</span>
                        <input
                          className="coding-agent-settings-code-value"
                          aria-label={t("codingAgent.settings.runtimeEnvironmentKey", { key })}
                          value={selectedProfile.runtimeEnvironment?.[key] ?? ""}
                          onChange={(event) => {
                            const value = event.currentTarget.value;
                            updateProfile(selectedProfile.profileId, (profile) => {
                              const runtimeEnvironment = { ...(profile.runtimeEnvironment ?? {}) };
                              if (value) runtimeEnvironment[key] = value;
                              else delete runtimeEnvironment[key];
                              return { ...profile, runtimeEnvironment };
                            });
                          }}
                        />
                      </label>
                    ))}
                  </details>
                  <label className="settings-row">
                    <span className="settings-row-label">
                      {t("codingAgent.settings.networkAccess")}
                    </span>
                    <select
                      aria-label={t("codingAgent.settings.networkAccess")}
                      value={selectedProfile.networkAccess ?? "disabled"}
                      onChange={(event) => {
                        const networkAccess = event.currentTarget.value as NonNullable<
                          typeof selectedProfile.networkAccess
                        >;
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          networkAccess,
                        }));
                      }}
                    >
                      {["disabled", "public", "public_and_private", "public_private_and_host"].map(
                        (value) => (
                          <option key={value} value={value}>
                            {t(`codingAgent.settings.network.${value}`)}
                          </option>
                        ),
                      )}
                    </select>
                  </label>
                  <fieldset className="coding-agent-settings-host-configuration">
                    <legend>
                      {t("codingAgent.settings.hostConfigurationMode")}
                      <SettingsHint
                        label={t("codingAgent.settings.hostConfigurationRiskLabel")}
                        content={t("codingAgent.settings.hostConfigurationRisk")}
                        tone="warning"
                      />
                    </legend>
                    <label className="coding-agent-settings-checkbox-row">
                      <input
                        type="radio"
                        name={`host-configuration-${selectedProfile.profileId}`}
                        checked={!selectedProfile.hostConfiguration}
                        onChange={() => {
                          if (selectedProfile.hostConfiguration) {
                            setProfileDialogHostConfigurationDraft(
                              structuredClone(selectedProfile.hostConfiguration),
                            );
                          }
                          updateProfile(selectedProfile.profileId, (profile) => ({
                            ...profile,
                            hostConfiguration: null,
                          }));
                        }}
                      />
                      {t("codingAgent.settings.hostConfigurationPrivate")}
                    </label>
                    <label className="coding-agent-settings-checkbox-row">
                      <input
                        type="radio"
                        name={`host-configuration-${selectedProfile.profileId}`}
                        checked={Boolean(selectedProfile.hostConfiguration)}
                        disabled={selectedProfileUsesOpenCode}
                        onChange={() => {
                          updateProfile(selectedProfile.profileId, (profile) => ({
                            ...profile,
                            hostConfiguration: profile.hostConfiguration ??
                              profileDialogHostConfigurationDraft ?? {
                                codexDirectory: null,
                                claudeDirectory: null,
                                claudeSettingsFile: null,
                              },
                          }));
                        }}
                      />
                      {t("codingAgent.settings.hostConfigurationShared")}
                    </label>
                    {selectedProfileUsesOpenCode && (
                      <SettingsHint
                        label={t("codingAgent.settings.hostConfigurationOpenCodeHelpLabel")}
                        content={t("codingAgent.settings.hostConfigurationOpenCodeHelp")}
                        tone="warning"
                      />
                    )}
                    {selectedProfile.hostConfiguration && (
                      <HostConfigurationEditor
                        configuration={selectedProfile.hostConfiguration}
                        onChange={(hostConfiguration) => {
                          setProfileDialogHostConfigurationDraft(
                            structuredClone(hostConfiguration),
                          );
                          updateProfile(selectedProfile.profileId, (profile) => ({
                            ...profile,
                            hostConfiguration,
                          }));
                        }}
                      />
                    )}
                    {selectedProfile.hostConfiguration &&
                      !profileHostConfigurationIsValid(selectedProfile) && (
                        <p role="alert" className="settings-error">
                          {t("codingAgent.settings.hostConfigurationInvalid")}
                        </p>
                      )}
                  </fieldset>
                </details>
                <details className="coding-agent-settings-details">
                  <summary>
                    <span className="coding-agent-settings-summary-content">
                      {t("codingAgent.settings.profileFileSharingGroup")}
                      <SettingsHint
                        label={t("codingAgent.settings.profileFileSharingGroupHelpLabel")}
                        content={t("codingAgent.settings.profileFileSharingGroupHelp")}
                      />
                    </span>
                  </summary>
                  <div className="settings-row">
                    <SettingsFieldLabel
                      fieldId="coding-agent-profile-mount-roots"
                      label={t("codingAgent.settings.mountRoots")}
                      hintLabel={t("codingAgent.settings.mountRootsHelpLabel")}
                      hintContent={t("codingAgent.settings.mountRootsHelp")}
                    />
                    <textarea
                      id="coding-agent-profile-mount-roots"
                      className="coding-agent-settings-code-value"
                      aria-label={t("codingAgent.settings.mountRoots")}
                      rows={3}
                      value={multiline(selectedProfile.mountAllowedRoots)}
                      onChange={(event) => {
                        const mountAllowedRoots = parseLines(event.currentTarget.value);
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          mountAllowedRoots,
                        }));
                      }}
                    />
                  </div>
                  {externalEditor && (
                    <details className="coding-agent-settings-mounts">
                      <summary>{t("codingAgent.settings.persistentMounts")}</summary>
                      {selectedProfile.persistentMounts.map((mount, index) => (
                        <PersistentMountEditor
                          // biome-ignore lint/suspicious/noArrayIndexKey: Mount DTOs have no stable ID; edits preserve their array position.
                          key={index}
                          mount={mount}
                          index={index}
                          onChange={(nextMount) =>
                            updateProfile(selectedProfile.profileId, (profile) => ({
                              ...profile,
                              persistentMounts: profile.persistentMounts.map((entry, entryIndex) =>
                                entryIndex === index ? nextMount : entry,
                              ),
                            }))
                          }
                          onRemove={() =>
                            updateProfile(selectedProfile.profileId, (profile) => ({
                              ...profile,
                              persistentMounts: profile.persistentMounts.filter(
                                (_entry, entryIndex) => entryIndex !== index,
                              ),
                            }))
                          }
                        />
                      ))}
                      <button
                        type="button"
                        className="btn"
                        onClick={() =>
                          updateProfile(selectedProfile.profileId, (profile) => ({
                            ...profile,
                            persistentMounts: [
                              ...profile.persistentMounts,
                              {
                                hostPath: "",
                                guestPath: "/workspace",
                                readOnly: true,
                                executable: false,
                              },
                            ],
                          }))
                        }
                      >
                        {t("codingAgent.settings.addMount")}
                      </button>
                    </details>
                  )}
                </details>
                <details
                  className="coding-agent-settings-details"
                  open={!selectedProfile.imageRef.trim()}
                >
                  <summary>
                    <span className="coding-agent-settings-summary-content">
                      {t("codingAgent.settings.profileAuthGroup")}
                      <SettingsHint
                        label={t("codingAgent.settings.profileAuthGroupHelpLabel")}
                        content={t("codingAgent.settings.profileAuthGroupHelp")}
                      />
                    </span>
                  </summary>
                  <label className="settings-row">
                    <span className="settings-row-label">
                      {t("codingAgent.settings.allowedImages")}
                    </span>
                    <textarea
                      aria-label={t("codingAgent.settings.allowedImages")}
                      rows={3}
                      value={multiline(selectedProfile.allowedImageRefs)}
                      onChange={(event) => {
                        const allowedImageRefs = parseLines(event.currentTarget.value);
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          allowedImageRefs,
                        }));
                      }}
                    />
                  </label>
                  <div className="settings-row">
                    <SettingsFieldLabel
                      fieldId="coding-agent-profile-credential-reference"
                      label={t("codingAgent.settings.credentialReference")}
                      hintLabel={t("codingAgent.settings.credentialReferenceHelpLabel")}
                      hintContent={t("codingAgent.settings.credentialReferenceHelp")}
                    />
                    <input
                      id="coding-agent-profile-credential-reference"
                      className="coding-agent-settings-code-value"
                      aria-label={t("codingAgent.settings.credentialReference")}
                      autoComplete="off"
                      value={selectedProfile.credentialReference ?? ""}
                      onChange={(event) => {
                        const credentialReference = event.currentTarget.value || null;
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          credentialReference,
                        }));
                      }}
                    />
                  </div>
                </details>
                <details
                  className="coding-agent-settings-details"
                  open={!profileResourcesAreValid(selectedProfile)}
                >
                  <summary>
                    <span className="coding-agent-settings-summary-content">
                      {t("codingAgent.settings.profileResourcesGroup")}
                      <SettingsHint
                        label={t("codingAgent.settings.profileResourcesGroupHelpLabel")}
                        content={t("codingAgent.settings.profileResourcesGroupHelp")}
                      />
                    </span>
                  </summary>
                  <fieldset className="coding-agent-settings-resources">
                    <legend>{t("codingAgent.settings.resourceLimits")}</legend>
                    <ResourceNumberEditor
                      label={t("codingAgent.settings.cpuMillis")}
                      value={selectedProfile.resources.cpuMillis}
                      maximum={MAX_U32}
                      onChange={(value) =>
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          resources: { ...profile.resources, cpuMillis: value },
                        }))
                      }
                    />
                    <ResourceNumberEditor
                      label={t("codingAgent.settings.memoryMib")}
                      value={selectedProfile.resources.memoryMib}
                      maximum={MAX_SAFE_U64}
                      onChange={(value) =>
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          resources: { ...profile.resources, memoryMib: value },
                        }))
                      }
                    />
                    <ResourceNumberEditor
                      label={t("codingAgent.settings.rootDiskMib")}
                      value={selectedProfile.resources.rootDiskMib}
                      maximum={MAX_SAFE_U64}
                      onChange={(value) =>
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          resources: { ...profile.resources, rootDiskMib: value },
                        }))
                      }
                    />
                    <ResourceNumberEditor
                      label={t("codingAgent.settings.maxParallelRuns")}
                      value={selectedProfile.resources.maxParallelRuns}
                      maximum={MAX_U32}
                      onChange={(value) =>
                        updateProfile(selectedProfile.profileId, (profile) => ({
                          ...profile,
                          resources: { ...profile.resources, maxParallelRuns: value },
                        }))
                      }
                    />
                    <label className="settings-row coding-agent-settings-checkbox-row">
                      <input
                        type="checkbox"
                        aria-label={t("codingAgent.settings.useStatic")}
                        checked={selectedProfile.resources.useStatic}
                        onChange={(event) =>
                          updateProfile(selectedProfile.profileId, (profile) => ({
                            ...profile,
                            resources: {
                              ...profile.resources,
                              useStatic: event.currentTarget.checked,
                            },
                          }))
                        }
                      />
                      {t("codingAgent.settings.useStatic")}
                    </label>
                  </fieldset>
                  {!profileResourcesAreValid(selectedProfile) && (
                    <p role="alert" className="settings-error">
                      {t("codingAgent.settings.invalidResources")}
                    </p>
                  )}
                  <div className="settings-card-desc">
                    {t("codingAgent.settings.resources", {
                      cpu: selectedProfile.resources.cpuMillis,
                      memory: selectedProfile.resources.memoryMib,
                      disk: selectedProfile.resources.rootDiskMib,
                    })}
                  </div>
                </details>
                {!externalEditor && selectedProfile.profileId !== current?.activeProfileId && (
                  <button
                    type="button"
                    className="btn"
                    onClick={() => removeProfile(selectedProfile.profileId)}
                  >
                    {t("codingAgent.settings.removeProfile")}
                  </button>
                )}
                <div className="coding-agent-settings-modal-footer">
                  <div className="coding-agent-settings-dialog-actions">
                    <button type="button" className="btn" onClick={closeProfileDialog}>
                      {t("common.cancel")}
                    </button>
                    <button
                      type="button"
                      className="btn primary"
                      disabled={
                        !profileRepositoriesAreValid(selectedProfile) ||
                        !profileResourcesAreValid(selectedProfile) ||
                        !profileRequiredFieldsAreValid(selectedProfile) ||
                        !profileHostConfigurationIsValid(selectedProfile)
                      }
                      onClick={confirmProfileDialog}
                    >
                      {t(
                        externalEditor
                          ? "codingAgent.settings.applyChanges"
                          : profileDialogSnapshot
                            ? "codingAgent.settings.confirmChanges"
                            : "codingAgent.settings.addProfileConfirm",
                      )}
                    </button>
                  </div>
                </div>
              </div>
            </div>
          </Modal>
        )}
        {profileCloseConfirm && (
          <ConfirmDialog
            title={t("codingAgent.settings.confirmDiscardTitle")}
            message={
              <p className="coding-agent-settings-confirm-message">
                {t("codingAgent.settings.confirmDiscardDialog")}
              </p>
            }
            confirmLabel={t("codingAgent.settings.confirmClose")}
            confirmVariant="primary"
            confirmDisabled={!profileCanConfirm}
            secondaryAction={{
              label: t("codingAgent.settings.discardChanges"),
              onClick: discardProfileDialog,
            }}
            cancelLabel={t("codingAgent.settings.continueEditing")}
            focusManagement
            onConfirm={confirmProfileDialog}
            onCancel={() => setProfileCloseConfirm(false)}
          />
        )}

        <fieldset
          className="coding-agent-settings-hosts"
          aria-labelledby="coding-agent-repository-hosts-title"
        >
          <legend className="visually-hidden">{t("codingAgent.settings.repositoryHosts")}</legend>
          <div className="coding-agent-settings-section-toolbar">
            <h3 id="coding-agent-repository-hosts-title">
              {t("codingAgent.settings.repositoryHosts")}
            </h3>
            <div className="coding-agent-settings-section-meta coding-agent-settings-toolbar-actions">
              <span className="settings-count-chip">
                {t("codingAgent.settings.repositoryHostCount", {
                  count: current?.repositoryHosts.length ?? 0,
                })}
              </span>
              <SettingsHint
                label={t("codingAgent.settings.repositoryHostsHelpLabel")}
                content={t("codingAgent.settings.repositoryHostsHelp")}
              />
              <button
                type="button"
                className="btn"
                onClick={() => {
                  if (!current || current.repositoryHosts.length >= 128) return;
                  const index = current.repositoryHosts.length;
                  const host = newRepositoryHost();
                  if (!externalEditor) {
                    updateSettingsDraft((settings) => ({
                      ...settings,
                      repositoryHosts: [...settings.repositoryHosts, host],
                    }));
                    return;
                  }
                  setHostDialogSnapshot(null);
                  setHostDialogDraft(structuredClone(host));
                  initializeHostDialogApprovals(host.labelHost, nativeApprovalSource);
                  setHostCloseConfirm(false);
                  setHostDialogIndex(index);
                  externalEditor.setEditing(true);
                }}
                disabled={!current || current.repositoryHosts.length >= 128}
              >
                {t("codingAgent.settings.addRepositoryHost")}
              </button>
            </div>
          </div>
          <div className="coding-agent-settings-summary-list">
            {current?.repositoryHosts.map((host, index) =>
              externalEditor ? (
                <div
                  className="coding-agent-settings-summary-row"
                  key={`${host.labelHost}:${host.apiBaseUrl}`}
                >
                  <div>
                    <strong>{host.labelHost || t("codingAgent.settings.unlabeledHost")}</strong>
                    <span className="settings-card-desc">
                      {host.forge} · {host.cloneTransport.kind.toUpperCase()}
                    </span>
                  </div>
                  <div className="coding-agent-settings-summary-actions">
                    <button type="button" className="btn" onClick={() => openHostDialog(index)}>
                      {t("codingAgent.settings.editRepositoryHost")}
                    </button>
                    <button
                      type="button"
                      className="btn"
                      disabled={hostCheckDisabled}
                      onClick={() => setHostCheckIndex(index)}
                    >
                      {t("codingAgent.settings.checkRepositoryHost")}
                    </button>
                    {hostCheckDisabled && (
                      <SettingsHint
                        label={t("codingAgent.settings.hostCheckHelpLabel")}
                        content={t(
                          isSaving
                            ? "codingAgent.settings.repositoryHostSavingHint"
                            : "codingAgent.settings.repositoryHostDraftHint",
                        )}
                        tone="info"
                      />
                    )}
                    <button
                      type="button"
                      className="btn"
                      disabled={Boolean(
                        current.profiles.some((profile) =>
                          profile.allowedRepositories.some(
                            (repository) => repository.host === host.labelHost,
                          ),
                        ),
                      )}
                      onClick={() => removeRepositoryHost(index)}
                    >
                      {t("codingAgent.settings.removeRepositoryHost")}
                    </button>
                  </div>
                </div>
              ) : (
                <RepositoryHostEditor
                  // Keep the editor node stable while switching transport fields.
                  // This preserves focus and lets validation messages stay attached to the row.
                  // biome-ignore lint/suspicious/noArrayIndexKey: Host DTOs have no stable ID; edits preserve their array position.
                  key={`host-${index}`}
                  host={host}
                  index={index}
                  referenced={Boolean(
                    current.profiles.some((profile) =>
                      profile.allowedRepositories.some(
                        (repository) => repository.host === host.labelHost,
                      ),
                    ),
                  )}
                  expectedRevision={baseRevision}
                  observedRevision={loaded.revision}
                  repositories={repositoryCandidatesForHost(current, host.labelHost)}
                  onChange={(nextHost) => updateRepositoryHost(index, () => nextHost)}
                  approvals={(nativeApprovalSource ?? []).filter(
                    (approval) => approval.repository.host === host.labelHost,
                  )}
                  approvalsDisabled={nativeApprovalEditingUnavailable}
                  onApprovalsChange={(approvals) =>
                    updateNativeRepositoryApprovals(host.labelHost, approvals)
                  }
                  onRemove={() => removeRepositoryHost(index)}
                />
              ),
            )}
          </div>
        </fieldset>
        {hostDialogIndex !== null && current && hostForDialog && (
          <Modal
            wide
            focusManagement
            onClose={closeHostDialog}
            ariaLabel={t("codingAgent.settings.repositoryHostDialogTitle", {
              index: hostDialogIndex + 1,
            })}
          >
            <div className="coding-agent-settings-modal-header">
              <div>
                <p className="coding-agent-settings-modal-context">
                  {t("codingAgent.settings.repositoryHostDialogContext", {
                    index: hostDialogIndex + 1,
                  })}
                </p>
                <h2>
                  {t("codingAgent.settings.repositoryHostDialogTitle", {
                    index: hostDialogIndex + 1,
                  })}
                </h2>
              </div>
              <button
                type="button"
                className="icon-btn coding-agent-settings-modal-close"
                aria-label={t("codingAgent.settings.closeDialog")}
                onClick={closeHostDialog}
              >
                ×
              </button>
            </div>
            <div className="coding-agent-settings-modal-body">
              <RepositoryHostEditor
                key={hostDialogIndex}
                host={hostForDialog}
                index={hostDialogIndex}
                referenced={Boolean(
                  current.profiles.some((profile) =>
                    profile.allowedRepositories.some(
                      (repository) => repository.host === hostForDialog.labelHost,
                    ),
                  ),
                )}
                expectedRevision={baseRevision}
                observedRevision={loaded.revision}
                repositories={repositoryCandidatesForHost(current, hostForDialog.labelHost)}
                onChange={(nextHost) => updateRepositoryHost(hostDialogIndex, () => nextHost)}
                approvals={hostDialogApprovalsDraft ?? []}
                approvalsDisabled={nativeApprovalEditingUnavailable}
                onApprovalsChange={setHostDialogApprovalsDraft}
                showCheck={false}
              />
              {(!repositoryHostIsValid(hostForDialog) ||
                !nativeRepositoryApprovalsAreValid(
                  hostDialogApprovalsDraft ?? [],
                  hostValidationHosts,
                  hostForDialog.labelHost,
                )) && (
                <p role="alert" className="settings-error">
                  {t("codingAgent.settings.invalidRepositoryHost")}
                </p>
              )}
              <div className="coding-agent-settings-modal-footer">
                <div className="coding-agent-settings-dialog-actions">
                  <button type="button" className="btn" onClick={closeHostDialog}>
                    {t("common.cancel")}
                  </button>
                  <button
                    type="button"
                    className="btn primary"
                    disabled={
                      !repositoryHostIsValid(hostForDialog) ||
                      !nativeRepositoryApprovalsAreValid(
                        hostDialogApprovalsDraft ?? [],
                        hostValidationHosts,
                        hostForDialog.labelHost,
                      )
                    }
                    onClick={confirmHostDialog}
                  >
                    {t("codingAgent.settings.applyChanges")}
                  </button>
                </div>
              </div>
            </div>
          </Modal>
        )}
        {hostCloseConfirm && (
          <ConfirmDialog
            title={t("codingAgent.settings.confirmDiscardTitle")}
            message={
              <p className="coding-agent-settings-confirm-message">
                {t("codingAgent.settings.confirmDiscardDialog")}
              </p>
            }
            confirmLabel={t("codingAgent.settings.confirmClose")}
            confirmVariant="primary"
            confirmDisabled={!hostCanConfirm}
            secondaryAction={{
              label: t("codingAgent.settings.discardChanges"),
              onClick: discardHostDialog,
            }}
            cancelLabel={t("codingAgent.settings.continueEditing")}
            focusManagement
            onConfirm={confirmHostDialog}
            onCancel={() => setHostCloseConfirm(false)}
          />
        )}
        {hostCheckIndex !== null && current?.repositoryHosts[hostCheckIndex] && (
          <Modal
            focusManagement
            onClose={() => setHostCheckIndex(null)}
            ariaLabel={t("codingAgent.settings.checkRepositoryHost")}
          >
            <RepositoryHostCheckPanel
              host={current.repositoryHosts[hostCheckIndex]}
              expectedRevision={baseRevision}
              observedRevision={loaded.revision}
              disabled={hostCheckDisabled}
              repositories={repositoryCandidatesForHost(
                current,
                current.repositoryHosts[hostCheckIndex].labelHost,
              )}
            />
            <div className="coding-agent-settings-dialog-actions">
              <button type="button" className="btn" onClick={() => setHostCheckIndex(null)}>
                {t("common.cancel")}
              </button>
            </div>
          </Modal>
        )}
      </fieldset>
      {hasInvalidSemanticPolicy && (
        <p role="alert" className="settings-error">
          {t("codingAgent.settings.invalidSemanticPolicy")}
        </p>
      )}
      {error && (
        <p role="alert" className="settings-error">
          {error}
        </p>
      )}
      {showLocalSaveControls && (
        <div className="settings-row">
          <span className="settings-row-label" />
          <button
            type="button"
            className="btn primary"
            disabled={saveDisabled}
            onClick={() => void save()}
          >
            {isSaving ? t("codingAgent.settings.saving") : t("codingAgent.settings.save")}
          </button>
          {dirty && (
            <button
              type="button"
              className="btn"
              disabled={isSaving}
              onClick={() => void reloadAndDiscard()}
            >
              {t("codingAgent.settings.reloadDiscard")}
            </button>
          )}
        </div>
      )}
    </section>
  );
}

function SettingsFieldLabel({
  fieldId,
  label,
  hintLabel,
  hintContent,
}: {
  fieldId: string;
  label: string;
  hintLabel: string;
  hintContent: string;
}) {
  return (
    <span className="coding-agent-settings-field-label">
      <label className="settings-row-label" htmlFor={fieldId}>
        {label}
      </label>
      <SettingsHint label={hintLabel} content={hintContent} />
    </span>
  );
}

function NativeSetupDiagnostic({
  setup,
  loading,
  error,
  invalidated,
  saving,
  blockedReason,
  approvals,
  approvalsEditable,
  approvalListUnset,
  profiles,
  repositoryHosts,
  dirty,
  onApprove,
  onAddApproval,
  onOpenHost,
}: {
  setup: CodingAgentNativeSetupResponse | undefined;
  loading: boolean;
  error: boolean;
  invalidated: boolean;
  saving: boolean;
  blockedReason?: string;
  /** Effective approval list shown to the user: the draft, else the backend's. */
  approvals: CodingAgentNativeRepositoryApproval[] | undefined;
  approvalsEditable: boolean;
  /** No GUI list saved yet and the legacy policy is the authority. */
  approvalListUnset: boolean;
  profiles: CodingAgentProfile[];
  repositoryHosts: CodingAgentRepositoryHost[];
  dirty: boolean;
  onApprove: () => void;
  onAddApproval: (repository: CodingAgentRepositoryIdentity) => void;
  onOpenHost: (labelHost: string) => void;
}) {
  const { t } = useTranslation();
  const [addedRepository, setAddedRepository] = useState<string | null>(null);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const state = setup?.state;
  const packageIdentity = {
    version: setup?.packageVersion ?? "?",
    fingerprint: setup?.packageFingerprint?.slice(0, 12) ?? "?",
  };
  const canApprove = Boolean(
    setup &&
      !invalidated &&
      !saving &&
      !blockedReason &&
      NATIVE_SETUP_APPROVABLE_STATES.has(setup.state) &&
      setup.packageFingerprint,
  );
  const effectiveApprovals = approvals ?? setup?.repositoryApprovals ?? [];
  const pending = pendingNativeApprovals(profiles, effectiveApprovals);
  const registeredHosts = new Set(repositoryHosts.map((host) => host.labelHost));
  return (
    <section
      className="coding-agent-settings-native-setup"
      aria-label={t("codingAgent.settings.nativeSetupTitle")}
    >
      <div className="coding-agent-settings-panel-toolbar">
        <strong>{t("codingAgent.settings.nativeSetupTitle")}</strong>
        <SettingsHint
          label={t("codingAgent.settings.nativeSetupHelpLabel")}
          content={t("codingAgent.settings.nativeSetupHelp")}
        />
      </div>
      {loading && <p role="status">{t("common.loading")}</p>}
      {error && <p role="status">{t("codingAgent.settings.nativeSetupUnavailable")}</p>}
      {setup && (
        <ol className="coding-agent-settings-native-setup-steps">
          <li>
            <strong>{t("codingAgent.settings.nativeSetupStepPackage")}</strong>
            <p role="status">
              {state === "unavailable"
                ? nativeSetupUnavailableText(setup, t)
                : t("codingAgent.settings.nativeSetupPackageReady")}
            </p>
            {setup.packageProvenance === "development" && (
              <p className="settings-card-desc">
                {t("codingAgent.settings.nativeSetupPackageDevelopment")}
              </p>
            )}
          </li>
          <li>
            <strong>{t("codingAgent.settings.nativeSetupStepApproval")}</strong>
            <p className="settings-card-desc">
              {t("codingAgent.settings.nativeSetupApprovalMeaning")}
            </p>
            <p role="status">
              {state === "approved"
                ? t("codingAgent.settings.nativeSetupApprovedDetail", packageIdentity)
                : t(`codingAgent.settings.nativeSetupState.${state}`, {
                    defaultValue: t("codingAgent.settings.nativeSetupUnavailable"),
                  })}
            </p>
            {invalidated && (
              <p role="alert" className="settings-error">
                {t("codingAgent.settings.nativeSetupInvalidated")}
              </p>
            )}
            {state !== "approved" && (
              <div className="coding-agent-settings-native-setup-action">
                <button
                  type="button"
                  className="btn"
                  disabled={!canApprove}
                  onClick={() => setConfirmOpen(true)}
                >
                  {saving
                    ? t("codingAgent.settings.nativeSetupApproving")
                    : t("codingAgent.settings.nativeSetupApprove")}
                </button>
                {blockedReason && (
                  <SettingsHint
                    label={t("codingAgent.settings.nativeSetupApprovalReasonLabel")}
                    content={blockedReason}
                    // Only a package problem is a warning; a dirty draft or a
                    // save in progress is ordinary sequencing.
                    tone={
                      state === "package_mismatch" || state === "unavailable" ? "warning" : "info"
                    }
                  />
                )}
              </div>
            )}
          </li>
          <li>
            <strong>{t("codingAgent.settings.nativeSetupStepRepositories")}</strong>
            <p className="settings-card-desc">
              {approvalListUnset
                ? t("codingAgent.settings.nativeSetupApprovalListUnset")
                : t("codingAgent.settings.nativeSetupApprovalCount", {
                    count: effectiveApprovals.length,
                  })}
            </p>
            {!approvalListUnset && effectiveApprovals.length === 0 && (
              <p role="status" className="settings-card-desc">
                {t("codingAgent.settings.nativeSetupApprovalListEmpty")}
              </p>
            )}
            {pending.length > 0 && (
              <div className="coding-agent-settings-native-setup-pending">
                <p className="settings-card-desc">
                  {t("codingAgent.settings.nativeSetupPendingRepositories")}
                </p>
                <ul>
                  {pending.map((repository) => {
                    const label = repositoryIdentityLabel(repository);
                    return (
                      <li key={label}>
                        <span>{label}</span>
                        {registeredHosts.has(repository.host) ? (
                          <button
                            type="button"
                            className="btn"
                            disabled={!approvalsEditable}
                            onClick={() => {
                              onAddApproval(repository);
                              setAddedRepository(label);
                            }}
                          >
                            {t("codingAgent.settings.nativeSetupAddApproval", {
                              repository: label,
                            })}
                          </button>
                        ) : (
                          <span className="settings-card-desc">
                            {t("codingAgent.settings.nativeSetupHostMissing", {
                              host: repository.host,
                            })}
                          </span>
                        )}
                      </li>
                    );
                  })}
                </ul>
              </div>
            )}
            {dirty && addedRepository !== null && (
              <p role="status" className="settings-card-desc">
                {t("codingAgent.settings.nativeSetupAddedPending")}
              </p>
            )}
            {repositoryHosts.length > 0 && (
              <div className="coding-agent-settings-native-setup-action">
                {repositoryHosts.map((host) => (
                  <button
                    key={host.labelHost}
                    type="button"
                    className="btn"
                    onClick={() => onOpenHost(host.labelHost)}
                  >
                    {t("codingAgent.settings.nativeSetupOpenHost", { host: host.labelHost })}
                  </button>
                ))}
              </div>
            )}
          </li>
        </ol>
      )}
      {confirmOpen && setup && (
        <ConfirmDialog
          title={t("codingAgent.settings.nativeSetupApprovalConfirmTitle")}
          message={
            <div className="coding-agent-settings-confirm-message">
              <p>{t("codingAgent.settings.nativeSetupApprovalConfirmTarget", packageIdentity)}</p>
              {setup.packageProvenance === "development" && (
                <p>{t("codingAgent.settings.nativeSetupPackageDevelopment")}</p>
              )}
              <p>
                <strong>{t("codingAgent.settings.nativeSetupApprovalConfirmRecordsTitle")}</strong>{" "}
                {t("codingAgent.settings.nativeSetupApprovalConfirmRecordsBody")}
              </p>
              <p>
                <strong>{t("codingAgent.settings.nativeSetupApprovalConfirmEnablesTitle")}</strong>
              </p>
              <ul>
                <li>{t("codingAgent.settings.nativeSetupApprovalConfirmEnablesWorkspace")}</li>
                <li>{t("codingAgent.settings.nativeSetupApprovalConfirmEnablesPublish")}</li>
                <li>{t("codingAgent.settings.nativeSetupApprovalConfirmEnablesCheck")}</li>
              </ul>
              <p>
                <strong>{t("codingAgent.settings.nativeSetupApprovalConfirmLimitsTitle")}</strong>
              </p>
              <ul>
                <li>{t("codingAgent.settings.nativeSetupApprovalConfirmLimitsNoAutostart")}</li>
                <li>{t("codingAgent.settings.nativeSetupApprovalConfirmLimitsApprovalList")}</li>
                <li>{t("codingAgent.settings.nativeSetupApprovalConfirmLimitsShell")}</li>
              </ul>
              <p>{t("codingAgent.settings.nativeSetupApprovalConfirmRevoke")}</p>
            </div>
          }
          confirmLabel={t("codingAgent.settings.nativeSetupApprove")}
          confirmVariant="primary"
          focusManagement
          onConfirm={() => {
            setConfirmOpen(false);
            onApprove();
          }}
          onCancel={() => setConfirmOpen(false)}
        />
      )}
    </section>
  );
}

function HostConfigurationEditor({
  configuration,
  onChange,
}: {
  configuration: NonNullable<CodingAgentProfile["hostConfiguration"]>;
  onChange: (configuration: NonNullable<CodingAgentProfile["hostConfiguration"]>) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="coding-agent-settings-host-configuration-fields">
      <PathChooser
        value={configuration.codexDirectory ?? ""}
        label={t("codingAgent.settings.codexDirectory")}
        directory
        onChange={(codexDirectory) => onChange({ ...configuration, codexDirectory })}
      />
      <PathChooser
        value={configuration.claudeDirectory ?? ""}
        label={t("codingAgent.settings.claudeDirectory")}
        directory
        onChange={(claudeDirectory) => onChange({ ...configuration, claudeDirectory })}
      />
      <PathChooser
        value={configuration.claudeSettingsFile ?? ""}
        label={t("codingAgent.settings.claudeSettingsFile")}
        hintLabel={t("codingAgent.settings.claudeSettingsFileHelpLabel")}
        hintContent={t("codingAgent.settings.claudeSettingsFileHelp")}
        onChange={(claudeSettingsFile) => onChange({ ...configuration, claudeSettingsFile })}
      />
      <SettingsHint
        label={t("codingAgent.settings.hostConfigurationHelpLabel")}
        content={t("codingAgent.settings.hostConfigurationHelp")}
      />
    </div>
  );
}

function PathChooser({
  value,
  label,
  hintLabel,
  hintContent,
  directory = false,
  onChange,
}: {
  value: string;
  label: string;
  hintLabel?: string;
  hintContent?: string;
  directory?: boolean;
  onChange: (value: string | null) => void;
}) {
  const { t } = useTranslation();
  const choose = async () => {
    const selected = await openPathDialog({ directory, multiple: false });
    if (typeof selected === "string") onChange(selected);
  };
  return (
    <div className="settings-row coding-agent-settings-path-picker">
      <label>
        <span className="coding-agent-settings-path-picker-label">
          <span className="settings-row-label">{label}</span>
          {hintLabel && hintContent && <SettingsHint label={hintLabel} content={hintContent} />}
        </span>
        <input
          aria-label={label}
          value={value}
          placeholder={t("codingAgent.settings.hostPathAbsolutePlaceholder")}
          onChange={(event) => onChange(event.currentTarget.value || null)}
        />
      </label>
      <div className="coding-agent-settings-path-picker-actions">
        <button
          type="button"
          className="btn"
          aria-label={t("codingAgent.settings.choosePathFor", { path: label })}
          onClick={() => void choose()}
        >
          {t("codingAgent.settings.choosePath")}
        </button>
        {value && (
          <button
            type="button"
            className="btn"
            onClick={() => onChange(null)}
            aria-label={t("codingAgent.settings.clearPath", { path: label })}
          >
            {t("codingAgent.settings.clearPathButton")}
          </button>
        )}
      </div>
    </div>
  );
}

function ResourceNumberEditor({
  label,
  value,
  maximum,
  onChange,
}: {
  label: string;
  value: number;
  maximum: number;
  onChange: (value: number) => void;
}) {
  return (
    <label className="settings-row">
      <span className="settings-row-label">{label}</span>
      <input
        type="number"
        min={1}
        max={maximum}
        step={1}
        value={value}
        onChange={(event) => onChange(resourceNumber(event.currentTarget.value, maximum))}
      />
    </label>
  );
}

function PersistentMountEditor({
  mount,
  index,
  onChange,
  onRemove,
}: {
  mount: CodingAgentPersistentMount;
  index: number;
  onChange: (mount: CodingAgentPersistentMount) => void;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="coding-agent-settings-mount">
      <label>
        {t("codingAgent.settings.hostPath")}
        <input
          className="coding-agent-settings-code-value"
          aria-label={t("codingAgent.settings.hostPathIndex", { index: index + 1 })}
          value={mount.hostPath}
          onChange={(event) => onChange({ ...mount, hostPath: event.currentTarget.value })}
        />
      </label>
      <label>
        {t("codingAgent.settings.guestPath")}
        <input
          className="coding-agent-settings-code-value"
          aria-label={t("codingAgent.settings.guestPathIndex", { index: index + 1 })}
          value={mount.guestPath}
          onChange={(event) => onChange({ ...mount, guestPath: event.currentTarget.value })}
        />
      </label>
      <label className="coding-agent-settings-checkbox-row">
        <input
          type="checkbox"
          checked={mount.readOnly}
          onChange={(event) => onChange({ ...mount, readOnly: event.currentTarget.checked })}
        />
        {t("codingAgent.settings.readOnly")}
      </label>
      <label className="coding-agent-settings-checkbox-row">
        <input
          type="checkbox"
          checked={mount.executable}
          onChange={(event) => onChange({ ...mount, executable: event.currentTarget.checked })}
        />
        {t("codingAgent.settings.executable")}
      </label>
      <button type="button" className="btn" onClick={onRemove}>
        {t("codingAgent.settings.removeMount")}
      </button>
    </div>
  );
}

function RepositoryHostEditor({
  host,
  index,
  referenced,
  expectedRevision,
  observedRevision,
  repositories,
  approvals,
  approvalsDisabled,
  onChange,
  onApprovalsChange,
  onRemove,
  showCheck = true,
}: {
  host: CodingAgentRepositoryHost;
  index: number;
  referenced: boolean;
  expectedRevision: number;
  observedRevision: number;
  repositories: readonly CodingAgentRepositoryIdentity[];
  approvals: CodingAgentNativeRepositoryApproval[];
  approvalsDisabled: boolean;
  onChange: (host: CodingAgentRepositoryHost) => void;
  onApprovalsChange: (approvals: CodingAgentNativeRepositoryApproval[]) => void;
  onRemove?: () => void;
  showCheck?: boolean;
}) {
  const { t } = useTranslation();
  const transport = host.cloneTransport;
  const updateTransport = (kind: "https" | "ssh") => {
    onChange(
      kind === "https"
        ? { ...host, cloneTransport: { kind, baseUrl: "" } }
        : { ...host, cloneTransport: { kind, host: "", user: "", port: null } },
    );
  };
  const hostIndex = index + 1;
  const indexedLabel = (key: string) => t(`${key}Index`, { index: hostIndex });
  const sshPortLabel = indexedLabel("codingAgent.settings.sshPort");
  const apiBaseUrlInvalid = !isHttpsUrl(host.apiBaseUrl);
  const cloneBaseUrlInvalid =
    transport.kind === "https"
      ? !isHttpsUrl(transport.baseUrl)
      : !transport.host.trim() ||
        !transport.user.trim() ||
        (transport.port !== null &&
          (!Number.isSafeInteger(transport.port) || transport.port < 1 || transport.port > 65535));
  return (
    <fieldset className="coding-agent-settings-host">
      <legend>{t("codingAgent.settings.repositoryHostLegend", { index: hostIndex })}</legend>
      <label>
        {indexedLabel("codingAgent.settings.labelHost")}
        <input
          aria-label={indexedLabel("codingAgent.settings.labelHost")}
          value={host.labelHost}
          onChange={(event) => onChange({ ...host, labelHost: event.currentTarget.value })}
        />
      </label>
      <label>
        {indexedLabel("codingAgent.settings.forge")}
        <select
          aria-label={indexedLabel("codingAgent.settings.forge")}
          value={host.forge}
          onChange={(event) =>
            onChange({ ...host, forge: event.currentTarget.value as "github" | "gitea" })
          }
        >
          <option value="github">GitHub</option>
          <option value="gitea">Gitea</option>
        </select>
      </label>
      <label className="coding-agent-settings-field--full">
        {indexedLabel("codingAgent.settings.apiBaseUrl")}
        <input
          aria-label={indexedLabel("codingAgent.settings.apiBaseUrl")}
          aria-invalid={apiBaseUrlInvalid}
          value={host.apiBaseUrl}
          onChange={(event) => onChange({ ...host, apiBaseUrl: event.currentTarget.value })}
        />
      </label>
      <label>
        {indexedLabel("codingAgent.settings.cloneTransport")}
        <select
          aria-label={indexedLabel("codingAgent.settings.cloneTransport")}
          value={transport.kind}
          onChange={(event) => updateTransport(event.currentTarget.value as "https" | "ssh")}
        >
          <option value="https">HTTPS</option>
          <option value="ssh">SSH</option>
        </select>
      </label>
      {transport.kind === "https" ? (
        <label className="coding-agent-settings-field--full">
          {indexedLabel("codingAgent.settings.cloneBaseUrl")}
          <input
            aria-label={indexedLabel("codingAgent.settings.cloneBaseUrl")}
            aria-invalid={cloneBaseUrlInvalid}
            value={transport.baseUrl}
            onChange={(event) =>
              onChange({
                ...host,
                cloneTransport: { kind: "https", baseUrl: event.currentTarget.value },
              })
            }
          />
        </label>
      ) : (
        <>
          <label>
            {indexedLabel("codingAgent.settings.sshHost")}
            <input
              aria-label={indexedLabel("codingAgent.settings.sshHost")}
              aria-invalid={cloneBaseUrlInvalid}
              value={transport.host}
              onChange={(event) =>
                onChange({
                  ...host,
                  cloneTransport: {
                    ...transport,
                    host: event.currentTarget.value,
                  },
                })
              }
            />
          </label>
          <label>
            {indexedLabel("codingAgent.settings.sshUser")}
            <input
              aria-label={indexedLabel("codingAgent.settings.sshUser")}
              aria-invalid={cloneBaseUrlInvalid}
              value={transport.user}
              onChange={(event) =>
                onChange({
                  ...host,
                  cloneTransport: {
                    ...transport,
                    host: transport.host,
                    user: event.currentTarget.value,
                  },
                })
              }
            />
          </label>
          <label>
            {sshPortLabel}
            <input
              aria-label={sshPortLabel}
              aria-invalid={cloneBaseUrlInvalid}
              type="number"
              min={1}
              max={65535}
              value={transport.port ?? ""}
              onChange={(event) => {
                const rawPort = event.currentTarget.value;
                const port = Number.parseInt(rawPort, 10);
                onChange({
                  ...host,
                  cloneTransport: {
                    ...transport,
                    port: rawPort === "" || !Number.isInteger(port) ? null : port,
                  },
                });
              }}
            />
          </label>
          <PathChooser
            value={transport.sshDirectory ?? ""}
            label={indexedLabel("codingAgent.settings.sshDirectory")}
            hintLabel={t("codingAgent.settings.sshDirectoryHelpLabel")}
            hintContent={t("codingAgent.settings.sshCredentialsHelp")}
            directory
            onChange={(sshDirectory) =>
              onChange({
                ...host,
                cloneTransport: { ...transport, sshDirectory },
              })
            }
          />
        </>
      )}
      <div className="coding-agent-settings-panel-toolbar">
        <span>{t("codingAgent.settings.repositoryHostCredentials")}</span>
        <SettingsHint
          label={t("codingAgent.settings.repositoryHostCredentialsHelpLabel")}
          content={t("codingAgent.settings.repositoryHostCredentialsHelp")}
        />
      </div>
      {transport.kind === "ssh" && (
        <p className="settings-card-desc">
          {t("codingAgent.settings.sshCredentialsShort")}{" "}
          <SettingsHint
            label={t("codingAgent.settings.sshCredentialsHelpLabel")}
            content={t("codingAgent.settings.sshCredentialsHelp")}
          />
        </p>
      )}
      {transport.kind === "https" && (
        <label>
          {indexedLabel("codingAgent.settings.readCredentialReference")}
          <input
            aria-label={indexedLabel("codingAgent.settings.readCredentialReference")}
            autoComplete="off"
            value={host.readCredentialRef ?? ""}
            onChange={(event) =>
              onChange({ ...host, readCredentialRef: event.currentTarget.value || null })
            }
          />
        </label>
      )}
      <NativeRepositoryApprovalsEditor
        hostLabel={host.labelHost}
        approvals={approvals}
        disabled={approvalsDisabled}
        disabledReason={t("codingAgent.settings.nativeRepositoryApprovalsUnavailable")}
        onChange={onApprovalsChange}
      />
      <label>
        {indexedLabel("codingAgent.settings.publicationCredentialReference")}
        <input
          aria-label={indexedLabel("codingAgent.settings.publicationCredentialReference")}
          autoComplete="off"
          value={host.publicationCredentialRef ?? ""}
          onChange={(event) =>
            onChange({ ...host, publicationCredentialRef: event.currentTarget.value || null })
          }
        />
      </label>
      {showCheck && (
        <RepositoryHostCheckPanel
          host={host}
          expectedRevision={expectedRevision}
          observedRevision={observedRevision}
          repositories={repositories}
        />
      )}
      {onRemove && (
        <button type="button" className="btn" disabled={referenced} onClick={onRemove}>
          {t("codingAgent.settings.removeRepositoryHost")}
        </button>
      )}
    </fieldset>
  );
}

function NativeRepositoryApprovalsEditor({
  hostLabel,
  approvals,
  disabled = false,
  disabledReason,
  onChange,
}: {
  hostLabel: string;
  approvals: CodingAgentNativeRepositoryApproval[];
  disabled?: boolean;
  disabledReason?: string;
  onChange: (approvals: CodingAgentNativeRepositoryApproval[]) => void;
}) {
  const { t } = useTranslation();
  const nextApprovalKey = useRef(0);
  const previousApprovalKeys = useRef<
    Array<{
      approval: CodingAgentNativeRepositoryApproval;
      key: string;
    }>
  >([]);
  const approvalKeys = useMemo(() => {
    const previousByObject = new Map(
      previousApprovalKeys.current.map(({ approval, key }) => [approval, key]),
    );
    const usedKeys = new Set<string>();
    const keys: Array<string | undefined> = approvals.map((approval) => {
      const key = previousByObject.get(approval);
      if (!key || usedKeys.has(key)) return undefined;
      usedKeys.add(key);
      return key;
    });
    approvals.forEach((_approval, index) => {
      if (keys[index]) return;
      const positionalKey = previousApprovalKeys.current[index]?.key;
      const key =
        positionalKey && !usedKeys.has(positionalKey)
          ? positionalKey
          : `native-approval-${nextApprovalKey.current++}`;
      usedKeys.add(key);
      keys[index] = key;
    });
    previousApprovalKeys.current = approvals.map((approval, index) => ({
      approval,
      key: keys[index] as string,
    }));
    return keys as string[];
  }, [approvals]);
  const approvalsValid = nativeRepositoryApprovalsAreValid(approvals, [], hostLabel, true);
  return (
    <div>
      {disabled && disabledReason && (
        <SettingsHint
          label={t("codingAgent.settings.nativeRepositoryApprovalsUnavailableLabel")}
          content={disabledReason}
          tone="warning"
        />
      )}
      <fieldset className="coding-agent-settings-native-approvals" disabled={disabled}>
        <legend>
          <span className="coding-agent-settings-summary-content">
            {t("codingAgent.settings.nativeRepositoryApprovals")}
            <SettingsHint
              label={t("codingAgent.settings.nativeRepositoryApprovalsHelpLabel")}
              content={t("codingAgent.settings.nativeRepositoryApprovalsHelp")}
            />
          </span>
        </legend>
        {approvals.map((approval, index) => (
          <div className="coding-agent-settings-native-approval" key={approvalKeys[index]}>
            <label>
              {t("codingAgent.settings.approvalRepositoryHost", { index: index + 1 })}
              <input
                aria-label={t("codingAgent.settings.approvalRepositoryHost", { index: index + 1 })}
                value={approval.repository.host}
                onChange={(event) => {
                  const repository = { ...approval.repository, host: event.currentTarget.value };
                  onChange(
                    approvals.map((entry, entryIndex) =>
                      entryIndex === index ? { ...entry, repository } : entry,
                    ),
                  );
                }}
              />
            </label>
            <label>
              {t("codingAgent.settings.approvalRepositoryOrganization", { index: index + 1 })}
              <input
                aria-label={t("codingAgent.settings.approvalRepositoryOrganization", {
                  index: index + 1,
                })}
                value={approval.repository.organization}
                onChange={(event) => {
                  const repository = {
                    ...approval.repository,
                    organization: event.currentTarget.value,
                  };
                  onChange(
                    approvals.map((entry, entryIndex) =>
                      entryIndex === index ? { ...entry, repository } : entry,
                    ),
                  );
                }}
              />
            </label>
            <label>
              {t("codingAgent.settings.approvalRepository", { index: index + 1 })}
              <input
                aria-label={t("codingAgent.settings.approvalRepository", { index: index + 1 })}
                value={approval.repository.repository}
                onChange={(event) => {
                  const repository = {
                    ...approval.repository,
                    repository: event.currentTarget.value,
                  };
                  onChange(
                    approvals.map((entry, entryIndex) =>
                      entryIndex === index ? { ...entry, repository } : entry,
                    ),
                  );
                }}
              />
            </label>
            <label className="coding-agent-settings-field--full">
              {t("codingAgent.settings.approvalBranches", { index: index + 1 })}
              <textarea
                aria-label={t("codingAgent.settings.approvalBranches", { index: index + 1 })}
                rows={2}
                value={multiline(approval.allowedBaseBranches)}
                onChange={(event) =>
                  onChange(
                    approvals.map((entry, entryIndex) =>
                      entryIndex === index
                        ? { ...entry, allowedBaseBranches: parseLines(event.currentTarget.value) }
                        : entry,
                    ),
                  )
                }
              />
            </label>
            {!nativeRepositoryApprovalIsValid(approval) && (
              <p role="alert" className="settings-error">
                {t("codingAgent.settings.invalidNativeRepositoryApproval")}
              </p>
            )}
            <div className="coding-agent-settings-native-approval-actions">
              <button
                type="button"
                className="btn"
                onClick={() =>
                  onChange(approvals.filter((_entry, entryIndex) => entryIndex !== index))
                }
              >
                {t("codingAgent.settings.removeNativeRepositoryApproval")}
              </button>
            </div>
          </div>
        ))}
        {!approvalsValid && approvals.length > 0 && (
          <p role="alert" className="settings-error">
            {t("codingAgent.settings.invalidNativeRepositoryApproval")}
          </p>
        )}
        <div className="coding-agent-settings-native-approval-actions">
          <button
            type="button"
            className="btn"
            onClick={() =>
              onChange([
                ...approvals,
                {
                  repository: { host: hostLabel, organization: "", repository: "" },
                  allowedBaseBranches: ["main"],
                },
              ])
            }
          >
            {t("codingAgent.settings.addNativeRepositoryApproval")}
          </button>
        </div>
      </fieldset>
    </div>
  );
}
