import { useTranslation } from "react-i18next";
import { SettingsHint } from "@/components/SettingsHint";
import { useCodingAgentSettingsEditor } from "./CodingAgentSettingsEditorContext";

const DEFAULT_POLICY = { maxRetries: 0, deadlineSeconds: 125 };

/**
 * Issue-facing controls that are persisted in the Coding Agent snapshot.
 * The editor remains owned by Settings so this card participates in the
 * common footer save operation without introducing a second snapshot.
 */
export interface CodingAgentIssuePolicyCardProps {
  issueEnabled?: boolean;
}

export function CodingAgentIssuePolicyCard({
  issueEnabled = true,
}: CodingAgentIssuePolicyCardProps) {
  const { t } = useTranslation();
  const editor = useCodingAgentSettingsEditor();
  const settings = editor.settings;
  if (!settings) return null;
  const policy = settings.semanticJudgePolicy ?? DEFAULT_POLICY;
  const enabled = settings.semanticJudgeEnabled;

  return (
    <section
      className="settings-card"
      aria-label={t("codingAgent.settings.issuePolicyAriaLabel", {
        defaultValue: "Coding agentによるIssue判定",
      })}
    >
      <div className="coding-agent-settings-section-toolbar">
        <div>
          <div className="settings-card-title">
            {t("codingAgent.settings.issuePolicyTitle", {
              defaultValue: "Coding agentによる要件達成・完了判定",
            })}
          </div>
          <div className="settings-card-desc">
            {t("codingAgent.settings.issuePolicyDescription", {
              defaultValue: "Issueの要件達成と完了条件を判定する全体設定です。",
            })}
          </div>
        </div>
        <SettingsHint
          label={t("codingAgent.settings.issuePolicyHelpLabel", {
            defaultValue: "Issue判定設定の説明",
          })}
          content={t("codingAgent.settings.issuePolicyHelp", {
            defaultValue: "判定を有効にすると、設定した条件に基づいてIssueの完了可否を評価します。",
          })}
        />
      </div>
      {!issueEnabled && (
        <div className="settings-inline-notice" role="status">
          <span className="settings-status-icon" aria-hidden="true">
            !
          </span>
          <span>{t("codingAgent.settings.issuePolicyDisabledNotice")}</span>
        </div>
      )}
      <div className="settings-row">
        <label>
          <input
            type="checkbox"
            checked={enabled}
            disabled={editor.saving}
            onChange={(event) =>
              editor.update((current) => ({
                ...current,
                semanticJudgeEnabled: event.target.checked,
                autoDoneEnabled: event.target.checked && current.autoDoneEnabled,
              }))
            }
          />
          {t("codingAgent.settings.semanticJudgeEnabled")}
        </label>
        <label>
          <input
            type="checkbox"
            checked={settings.autoDoneEnabled}
            disabled={!enabled || editor.saving}
            onChange={(event) =>
              editor.update((current) => ({ ...current, autoDoneEnabled: event.target.checked }))
            }
          />
          {t("codingAgent.settings.autoDoneEnabled")}
        </label>
      </div>
      <details className="coding-agent-settings-details">
        <summary>
          {t("codingAgent.settings.issuePolicyAdvanced", {
            defaultValue: "判定の詳細設定",
          })}
        </summary>
        <div className="settings-row">
          <label>
            {t("codingAgent.settings.semanticRetryCount")}
            <input
              type="number"
              min={0}
              max={5}
              step={1}
              disabled={!enabled || editor.saving}
              value={Number.isFinite(policy.maxRetries) ? policy.maxRetries : ""}
              onChange={(event) =>
                editor.update((current) => ({
                  ...current,
                  semanticJudgePolicy: {
                    ...(current.semanticJudgePolicy ?? DEFAULT_POLICY),
                    maxRetries: event.target.value === "" ? Number.NaN : Number(event.target.value),
                  },
                }))
              }
            />
          </label>
          <label>
            {t("codingAgent.settings.semanticDeadline")}
            <input
              type="number"
              min={10}
              max={600}
              step={1}
              disabled={!enabled || editor.saving}
              value={Number.isFinite(policy.deadlineSeconds) ? policy.deadlineSeconds : ""}
              onChange={(event) =>
                editor.update((current) => ({
                  ...current,
                  semanticJudgePolicy: {
                    ...(current.semanticJudgePolicy ?? DEFAULT_POLICY),
                    deadlineSeconds:
                      event.target.value === "" ? Number.NaN : Number(event.target.value),
                  },
                }))
              }
            />
          </label>
        </div>
      </details>
    </section>
  );
}
