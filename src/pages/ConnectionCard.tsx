import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  getConnectionConfig,
  getIssueTrackerSettings,
  inspectRemoteJobworkerp,
  registerRemoteJobworkerpWorkers,
  testConnectionConfig,
} from "@/api";
import { SettingRow } from "@/components/SettingRow";
import {
  type ConnectionService,
  hasRemoteService,
  LOCAL_CONNECTION_CONFIG,
  missingRemoteUrl,
  modeDisabledReason,
  normalizeConnectionConfig,
} from "@/lib/connectionConfig";
import { errorMessage } from "@/lib/errorMessage";
import type {
  ConnectionConfig,
  ConnectionMode,
  EndpointConfig,
  JobworkerpHostPaths,
  SidecarWarning,
  WorkerViewOverrides,
} from "@/types/api";
import type { DirtyReporter } from "./Settings";

const CONNECTION_MODES: { value: ConnectionMode; labelKey: string }[] = [
  { value: "local", labelKey: "settings.connection.modeLocal" },
  { value: "remote", labelKey: "settings.connection.modeRemote" },
];

const HINT_STYLE = { color: "var(--label-tertiary)", fontSize: 11, marginTop: 2 } as const;
const SUCCESS_STYLE = { color: "var(--success, #2ea043)", fontSize: 11, marginLeft: 8 } as const;
const ERROR_STYLE = { color: "var(--danger)", fontSize: 11 } as const;

const WORKER_VIEW_FIELDS: { key: keyof WorkerViewOverrides; label: string }[] = [
  { key: "jobworkerp_url", label: "jobworkerp" },
  { key: "memories_url", label: "memories" },
  { key: "issue_tracker_url", label: "Issue tracker" },
  { key: "issue_tracker_gateway_worker_url", label: "Issue tracker gateway worker" },
];

const HOST_PATH_FIELDS: { key: keyof JobworkerpHostPaths; labelKey: string }[] = [
  { key: "memories_import_bin", labelKey: "settings.connection.hostPaths.memoriesImportBin" },
  { key: "opencode_db_path", labelKey: "settings.connection.hostPaths.opencodeDbPath" },
  { key: "import_working_dir", labelKey: "settings.connection.hostPaths.importWorkingDir" },
];

function TextRow({
  label,
  value,
  placeholder,
  onChange,
}: {
  label: string;
  value: string | null;
  placeholder?: string;
  onChange: (value: string) => void;
}) {
  return (
    <div className="settings-row">
      <div className="settings-row-label">{label}</div>
      <input
        type="text"
        aria-label={label}
        value={value ?? ""}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        style={{ flex: 1 }}
      />
    </div>
  );
}

/** One service's local/remote choice. Local shows the live URL when the app
 *  knows it; remote edits the URL the app dials. */
function ServiceTarget({
  label,
  endpoint,
  localUrl,
  placeholder,
  disabledReason,
  onModeChange,
  onUrlChange,
  children,
}: {
  label: string;
  endpoint: EndpointConfig;
  localUrl?: string | null;
  placeholder: string;
  disabledReason: (mode: ConnectionMode) => string | null;
  onModeChange: (mode: ConnectionMode) => void;
  onUrlChange: (url: string) => void;
  /** Extra remote-only fields. */
  children?: ReactNode;
}) {
  const { t } = useTranslation();
  const reasons = CONNECTION_MODES.map((opt) => disabledReason(opt.value));
  const reason = reasons.find((r) => r != null);
  return (
    <fieldset aria-label={label} style={{ border: "none", margin: 0, padding: 0 }}>
      <div className="settings-row">
        <div className="settings-row-label">{label}</div>
        <div className="segment">
          {CONNECTION_MODES.map((opt, index) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${endpoint.mode === opt.value ? " active" : ""}`}
              disabled={reasons[index] != null}
              title={reasons[index] ? t(reasons[index]) : undefined}
              onClick={() => onModeChange(opt.value)}
            >
              {t(opt.labelKey)}
            </button>
          ))}
        </div>
      </div>
      {reason && <div style={HINT_STYLE}>{t(reason)}</div>}
      {endpoint.mode === "local" ? (
        localUrl !== undefined && (
          <SettingRow
            label={`${label} URL`}
            value={localUrl ?? t("settings.connection.starting")}
          />
        )
      ) : (
        <>
          <TextRow
            label={`${label} URL`}
            value={endpoint.remote_url}
            placeholder={placeholder}
            onChange={onUrlChange}
          />
          {children}
        </>
      )}
    </fieldset>
  );
}

function NameList({ titleKey, names }: { titleKey: string; names: string[] }) {
  const { t } = useTranslation();
  if (names.length === 0) return null;
  return (
    <div style={{ fontSize: 12, marginTop: 4 }}>
      <div>{t(titleKey)}</div>
      <ul style={{ margin: "2px 0 0 16px", padding: 0 }}>
        {names.map((name) => (
          <li key={name}>
            <code>{name}</code>
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * What the saved remote jobworkerp lacks. Workers are registered from here;
 * channels, plugins and environment live on the remote host, so those are
 * shown as settings to copy there.
 */
function RemoteJobworkerpReadinessPanel() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const readiness = useQuery({
    queryKey: ["remote-jobworkerp-readiness"],
    queryFn: inspectRemoteJobworkerp,
    // Each inspection makes several calls to the remote server, so it runs
    // on mount, after a save, and on demand only.
    staleTime: Number.POSITIVE_INFINITY,
    retry: false,
  });
  const [registering, setRegistering] = useState(false);
  const [warnings, setWarnings] = useState<SidecarWarning[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const handleRegister = async () => {
    setRegistering(true);
    setError(null);
    setWarnings([]);
    try {
      const result = await registerRemoteJobworkerpWorkers();
      queryClient.setQueryData(["remote-jobworkerp-readiness"], result.readiness);
      setWarnings(result.warnings);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setRegistering(false);
    }
  };

  const data = readiness.data;
  const environment = data?.environment.join("\n") ?? "";
  return (
    <div style={{ marginTop: 12 }}>
      <div className="settings-card-title" style={{ fontSize: 13 }}>
        {t("settings.connection.readiness.title")}
      </div>
      <div style={HINT_STYLE}>{t("settings.connection.readiness.desc")}</div>
      {readiness.isError && <div style={ERROR_STYLE}>{errorMessage(readiness.error)}</div>}
      {data &&
        (data.ready ? (
          <div style={{ ...SUCCESS_STYLE, marginLeft: 0 }}>
            {t("settings.connection.readiness.ready")}
          </div>
        ) : (
          <>
            {data.registration_outdated && (
              <div style={{ fontSize: 12, marginTop: 4 }}>
                {t("settings.connection.readiness.outdated")}
              </div>
            )}
            <NameList
              titleKey="settings.connection.readiness.missingWorkers"
              names={data.missing_workers}
            />
            <NameList
              titleKey="settings.connection.readiness.missingFunctionSets"
              names={data.missing_function_sets}
            />
            <NameList
              titleKey="settings.connection.readiness.missingChannels"
              names={data.missing_channels}
            />
            <NameList
              titleKey="settings.connection.readiness.missingRunners"
              names={data.missing_runners}
            />
          </>
        ))}
      {data && data.environment.length > 0 && (
        <div style={{ marginTop: 8 }}>
          <div style={HINT_STYLE}>{t("settings.connection.readiness.environment")}</div>
          <pre
            style={{
              fontFamily: "var(--font-mono)",
              fontSize: 11,
              padding: 8,
              background: "var(--secondary-bg)",
              borderRadius: 6,
              whiteSpace: "pre-wrap",
            }}
          >
            {environment}
          </pre>
          <button
            type="button"
            className="btn"
            onClick={() =>
              void navigator.clipboard?.writeText(environment).then(() => setCopied(true))
            }
          >
            {t("settings.connection.readiness.copy")}
          </button>
          {copied && <span style={SUCCESS_STYLE}>{t("settings.connection.readiness.copied")}</span>}
        </div>
      )}
      <div className="settings-row">
        <div className="settings-row-label" />
        <button
          type="button"
          className="btn"
          onClick={() => void readiness.refetch()}
          disabled={readiness.isFetching}
        >
          {readiness.isFetching
            ? t("settings.connection.readiness.inspecting")
            : t("settings.connection.readiness.inspect")}
        </button>
        <button
          type="button"
          className="btn primary"
          onClick={() => void handleRegister()}
          disabled={registering}
          style={{ marginLeft: 8 }}
        >
          {registering
            ? t("settings.connection.readiness.registering")
            : t("settings.connection.readiness.register")}
        </button>
      </div>
      {warnings.map((w) => (
        <div key={`${w.kind}:${w.message}`} style={ERROR_STYLE}>
          {w.message}
        </div>
      ))}
      {error && <div style={ERROR_STYLE}>{error}</div>}
    </div>
  );
}

/**
 * Connection targets, chosen per service. Local services show the live
 * sidecar URLs read-only (the dynamic ports are never persisted); remote ones
 * take the URL the app dials. The edits are applied from the save bar.
 */
export function ConnectionCard({
  localJobworkerpUrl,
  localMemoriesUrl,
  onDirtyChange,
  resetSignal,
}: {
  localJobworkerpUrl: string | null;
  localMemoriesUrl: string | null;
} & DirtyReporter<ConnectionConfig>) {
  const { t } = useTranslation();
  const { data } = useQuery({ queryKey: ["connection-config"], queryFn: getConnectionConfig });
  const trackerSettings = useQuery({
    queryKey: ["issue-tracker-settings"],
    queryFn: getIssueTrackerSettings,
  });
  const trackingEnabled = trackerSettings.data?.effective_enabled === true;

  const [draft, setDraft] = useState<ConnectionConfig>(LOCAL_CONNECTION_CONFIG);
  const [testing, setTesting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);

  // Seed the form from the persisted config once it loads, and again when
  // the save bar discards or applies the edits.
  // biome-ignore lint/correctness/useExhaustiveDependencies: resetSignal is the discard trigger
  useEffect(() => {
    if (data) setDraft(data);
  }, [data, resetSignal]);

  const payload = useMemo(() => normalizeConnectionConfig(draft), [draft]);
  const persisted = useMemo(
    () => (data ? JSON.stringify(normalizeConnectionConfig(data)) : null),
    [data],
  );
  // Compared in the normalised shape that is saved so a no-op edit doesn't
  // arm the guard. Until `data` loads there is nothing to diff.
  const edited = persisted != null && JSON.stringify(payload) !== persisted;
  const incomplete = missingRemoteUrl(draft, trackingEnabled);
  // The save bar applies the payload; a remote service without its URL is
  // edited but not saveable.
  useEffect(() => {
    onDirtyChange(edited && !incomplete ? payload : null, edited);
  }, [edited, incomplete, payload, onDirtyChange]);

  const update = <K extends keyof ConnectionConfig>(
    section: K,
    patch: Partial<ConnectionConfig[K]>,
  ) => setDraft((cfg) => ({ ...cfg, [section]: { ...cfg[section], ...patch } }));
  const serviceProps = (service: ConnectionService) => ({
    endpoint: draft[service],
    disabledReason: (mode: ConnectionMode) =>
      modeDisabledReason(draft, service, mode, trackingEnabled),
    onModeChange: (mode: ConnectionMode) => update(service, { mode }),
    onUrlChange: (remote_url: string) => update(service, { remote_url }),
  });

  const handleTest = async () => {
    setTesting(true);
    setError(null);
    setTestResult(null);
    try {
      await testConnectionConfig(payload);
      setTestResult(t("settings.connection.testOk"));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setTesting(false);
    }
  };

  const header = (
    <>
      <div className="settings-card-title">{t("settings.connection.title")}</div>
      <div className="settings-card-desc">{t("settings.connection.desc")}</div>
      <div style={{ ...HINT_STYLE, marginTop: 4 }}>{t("settings.connection.restartTarget")}</div>
    </>
  );
  // Editing before the persisted config arrives would be overwritten by the
  // seed effect, so the form waits for it.
  if (!data) {
    return (
      <div className="settings-card">
        {header}
        <div style={HINT_STYLE}>{t("settings.connection.starting")}</div>
      </div>
    );
  }

  const tracker = draft.issue_tracker;
  return (
    <div className="settings-card">
      {header}

      <ServiceTarget
        label="jobworkerp"
        localUrl={localJobworkerpUrl}
        placeholder="http://host:9000"
        {...serviceProps("jobworkerp")}
      />
      <ServiceTarget
        label="memories"
        localUrl={localMemoriesUrl}
        placeholder="http://host:9010"
        {...serviceProps("memories")}
      />
      {trackingEnabled && (
        <ServiceTarget
          label="Issue tracker"
          placeholder="http://host:9030"
          {...serviceProps("issue_tracker")}
        >
          <TextRow
            label="gateway control URL"
            value={tracker.remote_gateway_control_url}
            placeholder="http://host:9031"
            onChange={(remote_gateway_control_url) =>
              update("issue_tracker", { remote_gateway_control_url })
            }
          />
          <TextRow
            label="gateway worker URL"
            value={tracker.remote_gateway_worker_url}
            placeholder="http://host:9032"
            onChange={(remote_gateway_worker_url) =>
              update("issue_tracker", { remote_gateway_worker_url })
            }
          />
        </ServiceTarget>
      )}
      <ServiceTarget
        label="conductor"
        placeholder="http://host:9020"
        {...serviceProps("conductor")}
      />

      {hasRemoteService(draft) && (
        <details style={{ marginTop: 8 }}>
          <summary style={{ cursor: "pointer", color: "var(--label-secondary)", fontSize: 12 }}>
            {t("settings.connection.workerView.title")}
          </summary>
          <div style={HINT_STYLE}>{t("settings.connection.workerView.desc")}</div>
          {WORKER_VIEW_FIELDS.map(({ key, label }) => (
            <TextRow
              key={key}
              label={label}
              value={draft.worker_view[key]}
              onChange={(value) => update("worker_view", { [key]: value })}
            />
          ))}
        </details>
      )}

      {draft.jobworkerp.mode === "remote" && (
        <div style={{ marginTop: 8 }}>
          <div style={{ fontSize: 12 }}>{t("settings.connection.hostPaths.title")}</div>
          <div style={HINT_STYLE}>{t("settings.connection.hostPaths.desc")}</div>
          {HOST_PATH_FIELDS.map(({ key, labelKey }) => (
            <TextRow
              key={key}
              label={t(labelKey)}
              value={draft.jobworkerp_host_paths[key]}
              onChange={(value) => update("jobworkerp_host_paths", { [key]: value })}
            />
          ))}
        </div>
      )}

      {hasRemoteService(draft) && (
        <div className="settings-row">
          <div className="settings-row-label" />
          <button
            type="button"
            className="btn"
            onClick={() => void handleTest()}
            disabled={testing || incomplete}
          >
            {testing ? t("settings.connection.testing") : t("settings.connection.test")}
          </button>
          {testResult && !error && <span style={SUCCESS_STYLE}>{testResult}</span>}
        </div>
      )}
      {error && <div style={ERROR_STYLE}>{error}</div>}

      {data.jobworkerp.mode === "remote" && <RemoteJobworkerpReadinessPanel />}
    </div>
  );
}
