import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { codingAgentCheckRepositoryHost } from "@/api";
import { SettingsHint } from "@/components/SettingsHint";
import type {
  CodingAgentRepositoryHost,
  CodingAgentRepositoryHostCheckResponse,
  CodingAgentRepositoryHostCheckStageName,
  CodingAgentRepositoryIdentity,
} from "@/types/api";
import { repositoryIdentityLabel } from "./repositoryIdentity";

const STAGES: readonly CodingAgentRepositoryHostCheckStageName[] = [
  "configuration",
  "trusted_policy",
  "git_read",
  "forge_api",
];

const KNOWN_CODES = new Set([
  "settings_valid",
  "invalid_settings",
  "revision_conflict",
  "policy_matched",
  "repository_required",
  "policy_missing",
  "policy_invalid",
  "repository_not_approved",
  "native_backend_not_approved",
  "policy_mismatch",
  "git_readable",
  "git_failed",
  "forge_readable",
  "forge_not_configured",
  "credential_not_configured",
  "authentication_failed",
  "access_denied",
  "repository_not_found",
  "redirect_rejected",
  "invalid_response",
  "credential_unavailable",
  "timeout",
  "response_too_large",
  "unavailable",
  "unsupported_capability",
  "prerequisite_failed",
]);

function checkFingerprint(
  host: CodingAgentRepositoryHost,
  expectedRevision: number,
  observedRevision: number,
  repositories: readonly CodingAgentRepositoryIdentity[],
): string {
  return JSON.stringify({ host, expectedRevision, observedRevision, repositories });
}

export function RepositoryHostCheckPanel({
  host,
  expectedRevision,
  observedRevision = expectedRevision,
  repositories,
  disabled = false,
}: {
  host: CodingAgentRepositoryHost;
  expectedRevision: number;
  observedRevision?: number;
  repositories: readonly CodingAgentRepositoryIdentity[];
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const [selectedRepositoryKey, setSelectedRepositoryKey] = useState("");
  const [result, setResult] = useState<CodingAgentRepositoryHostCheckResponse | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState(false);
  const requestToken = useRef(0);
  const fingerprint = useMemo(
    () => checkFingerprint(host, expectedRevision, observedRevision, repositories),
    [host, expectedRevision, observedRevision, repositories],
  );
  const fingerprintRef = useRef(fingerprint);
  const selectedKeyRef = useRef(selectedRepositoryKey);
  fingerprintRef.current = fingerprint;
  selectedKeyRef.current = selectedRepositoryKey;

  useEffect(() => {
    fingerprintRef.current = fingerprint;
    requestToken.current += 1;
    setResult(null);
    setError(false);
    setRunning(false);
    setSelectedRepositoryKey("");
  }, [fingerprint]);

  const selectedRepository = repositories.find(
    (repository) => repositoryIdentityLabel(repository) === selectedRepositoryKey,
  );

  const invalidateResult = () => {
    requestToken.current += 1;
    setResult(null);
    setError(false);
  };

  const check = async () => {
    if (running || disabled) return;
    const token = ++requestToken.current;
    const requestFingerprint = fingerprintRef.current;
    const requestRepositoryKey = selectedKeyRef.current;
    setRunning(true);
    setResult(null);
    setError(false);
    try {
      const response = await codingAgentCheckRepositoryHost({
        expectedRevision,
        host,
        repository: selectedRepository ?? null,
      });
      if (
        token !== requestToken.current ||
        requestFingerprint !== fingerprintRef.current ||
        requestRepositoryKey !== selectedKeyRef.current
      )
        return;
      setResult(response);
    } catch {
      if (
        token !== requestToken.current ||
        requestFingerprint !== fingerprintRef.current ||
        requestRepositoryKey !== selectedKeyRef.current
      )
        return;
      setError(true);
    } finally {
      if (token === requestToken.current) setRunning(false);
    }
  };

  const stageMessage = (code: string): string =>
    KNOWN_CODES.has(code)
      ? t(`codingAgent.settings.hostCheckCodes.${code}`, {
          defaultValue: t("codingAgent.settings.hostCheckUnknownCode"),
        })
      : t("codingAgent.settings.hostCheckUnknownCode");

  return (
    <section
      className="coding-agent-settings-host-check"
      aria-label={t("codingAgent.settings.hostCheckTitle")}
    >
      <div className="coding-agent-settings-panel-toolbar">
        <h4>{t("codingAgent.settings.hostCheckTitle")}</h4>
        <SettingsHint
          label={t("codingAgent.settings.hostCheckHelpLabel")}
          content={t("codingAgent.settings.hostCheckDescription")}
          tone="info"
        />
      </div>
      <label>
        {t("codingAgent.settings.hostCheckRepository")}
        <select
          aria-label={t("codingAgent.settings.hostCheckRepository")}
          value={selectedRepositoryKey}
          disabled={running}
          onChange={(event) => {
            setSelectedRepositoryKey(event.currentTarget.value);
            invalidateResult();
          }}
        >
          <option value="">{t("codingAgent.settings.hostCheckNoRepository")}</option>
          {repositories.map((repository) => (
            <option
              key={repositoryIdentityLabel(repository)}
              value={repositoryIdentityLabel(repository)}
            >
              {repositoryIdentityLabel(repository)}
            </option>
          ))}
        </select>
      </label>
      {!selectedRepository && <p>{t("codingAgent.settings.hostCheckStructuralOnly")}</p>}
      <button
        type="button"
        className="btn"
        disabled={running || disabled}
        onClick={() => void check()}
      >
        {running
          ? t("codingAgent.settings.hostCheckRunning")
          : t("codingAgent.settings.hostCheckRun")}
      </button>
      {error && <p role="alert">{t("codingAgent.settings.hostCheckError")}</p>}
      {error && (
        <button type="button" className="btn" onClick={() => void check()}>
          {t("codingAgent.settings.hostCheckRetry")}
        </button>
      )}
      {result && (
        <section aria-label={t("codingAgent.settings.hostCheckResults")}>
          <p>{t("codingAgent.settings.hostCheckReadOnlyScope")}</p>
          <ol>
            {STAGES.map((stageName) => {
              const stage = result.stages.find((entry) => entry.stage === stageName);
              const status = stage?.status ?? "skipped";
              const code = stage?.code ?? "unavailable";
              return (
                <li key={stageName}>
                  <strong>{t(`codingAgent.settings.hostCheckStages.${stageName}`)}</strong>{" "}
                  <span
                    className={`settings-diagnostic-icon settings-diagnostic-icon-${status}`}
                    aria-hidden="true"
                  >
                    {status === "passed" ? "✓" : status === "failed" ? "!" : "–"}
                  </span>{" "}
                  <span>{t(`codingAgent.settings.hostCheckStatuses.${status}`)}</span>
                  <p>{stageMessage(code)}</p>
                </li>
              );
            })}
          </ol>
        </section>
      )}
    </section>
  );
}
