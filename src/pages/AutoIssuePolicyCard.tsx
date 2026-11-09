import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { getAutoIssuePolicy, setAutoIssuePolicy } from "@/api";
import { SettingsCheckboxRow } from "@/components/SettingsCheckboxRow";
import type { AutoIssuePolicy, AutoIssueResolutionMode, AutoRegisterPriority } from "@/types/api";

const priorities: AutoRegisterPriority[] = ["P0", "P1", "P2", "P3", "P4"];
const DEFAULT_RESOLUTION_MIN_CONFIDENCE = 0.9;
const kinds: Array<AutoIssuePolicy["allowed_kinds"][number]> = [
  "TASK",
  "BUG",
  "RESEARCH",
  "DECISION",
];

function clonePolicy(policy: AutoIssuePolicy): AutoIssuePolicy {
  return {
    ...policy,
    resolution_mode: policy.resolution_mode ?? "DISABLED",
    resolution_min_confidence:
      policy.resolution_min_confidence ?? DEFAULT_RESOLUTION_MIN_CONFIDENCE,
    auto_register_priority_range: { ...policy.auto_register_priority_range },
    allowed_kinds: [...policy.allowed_kinds],
  };
}

function validate(policy: AutoIssuePolicy): string | null {
  const confidence = policy.auto_register_min_confidence;
  if (confidence !== null && (!Number.isFinite(confidence) || confidence < 0 || confidence > 1)) {
    return "confidence";
  }
  if (
    !Number.isFinite(policy.resolution_min_confidence) ||
    policy.resolution_min_confidence < 0 ||
    policy.resolution_min_confidence > 1
  ) {
    return "resolutionConfidence";
  }
  if (
    priorities.indexOf(policy.auto_register_priority_range.lower) >
    priorities.indexOf(policy.auto_register_priority_range.upper)
  ) {
    return "priority";
  }
  if (!Number.isInteger(policy.extract_cap) || policy.extract_cap < 1 || policy.extract_cap > 20) {
    return "cap";
  }
  return null;
}

export function AutoIssuePolicyCard({
  embedded = false,
  onDirtyChange,
  resetSignal,
}: {
  embedded?: boolean;
  onDirtyChange?: (policy: AutoIssuePolicy | null, edited: boolean) => void;
  resetSignal?: number;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const query = useQuery({ queryKey: ["auto-issue-policy"], queryFn: getAutoIssuePolicy });
  const [form, setForm] = useState<AutoIssuePolicy | null>(null);
  const [inputError, setInputError] = useState<string | null>(null);
  const lastResetSignal = useRef(resetSignal);

  useEffect(() => {
    const shouldReset = lastResetSignal.current !== resetSignal || form === null;
    if (query.data && shouldReset) {
      setForm(clonePolicy(query.data.policy));
      setInputError(null);
      lastResetSignal.current = resetSignal;
    }
  }, [form, query.data, resetSignal]);

  useEffect(() => {
    if (!onDirtyChange || !query.data || !form) return;
    const edited = JSON.stringify(form) !== JSON.stringify(clonePolicy(query.data.policy));
    onDirtyChange(edited && validate(form) === null ? clonePolicy(form) : null, edited);
  }, [form, onDirtyChange, query.data]);

  useEffect(() => {
    return () => onDirtyChange?.(null, false);
  }, [onDirtyChange]);

  const mutation = useMutation({
    mutationFn: setAutoIssuePolicy,
    onSuccess: async (snapshot) => {
      queryClient.setQueryData(["auto-issue-policy"], snapshot);
      await queryClient.invalidateQueries({ queryKey: ["auto-issue-policy"] });
    },
  });

  if (query.isPending || !form) {
    return null;
  }

  const update = (next: Partial<AutoIssuePolicy>) =>
    setForm((current) => (current ? { ...current, ...next } : current));
  const save = () => {
    const problem = validate(form);
    if (problem) {
      setInputError(problem);
      return;
    }
    setInputError(null);
    mutation.mutate({ policy: form });
  };
  const error = inputError
    ? t(`issues.autoPolicy.validation.${inputError}`)
    : mutation.isError
      ? `${t("issues.autoPolicy.error")}: ${mutation.error.message}`
      : null;
  const extractionDisabled = !form.extraction_enabled;
  const autoRegistrationDisabled = extractionDisabled || form.auto_register_min_confidence === null;

  return (
    <section className={embedded ? "issues-settings-section" : "settings-card"}>
      <div className="settings-card-title">{t("issues.autoPolicy.title")}</div>
      <div className="settings-card-desc">{t("issues.autoPolicy.description")}</div>

      <SettingsCheckboxRow
        id="auto-issue-extraction-enabled"
        checked={form.extraction_enabled}
        onCheckedChange={(checked) => update({ extraction_enabled: checked })}
        title={t("issues.autoPolicy.extractionEnabled")}
      />
      <label className="issues-form-label">
        {t("issues.autoPolicy.extractCap")}
        <input
          aria-label={t("issues.autoPolicy.extractCap")}
          type="number"
          min="1"
          max="20"
          disabled={extractionDisabled}
          value={form.extract_cap}
          onChange={(event) => update({ extract_cap: Number(event.target.value) })}
        />
      </label>
      {extractionDisabled && (
        <div className="settings-card-desc">{t("issues.autoPolicy.extractionDisabledHint")}</div>
      )}
      <fieldset className="auto-issue-extraction-options" disabled={extractionDisabled}>
        <legend>{t("issues.autoPolicy.extractionConditions")}</legend>
        <SettingsCheckboxRow
          id="auto-issue-auto-register"
          checked={form.auto_register_min_confidence !== null}
          disabled={extractionDisabled}
          onCheckedChange={(checked) =>
            update({ auto_register_min_confidence: checked ? 0.9 : null })
          }
          title={t("issues.autoPolicy.autoRegister")}
          description={t(
            form.auto_register_min_confidence === null
              ? "issues.autoPolicy.autoRegisterDisabledHint"
              : "issues.autoPolicy.autoRegisterEnabledHint",
          )}
        />
        <label className="issues-form-label">
          {t("issues.autoPolicy.minConfidence")}
          <input
            aria-label={t("issues.autoPolicy.minConfidence")}
            type="number"
            min="0"
            max="1"
            step="0.01"
            disabled={autoRegistrationDisabled}
            value={form.auto_register_min_confidence ?? ""}
            onChange={(event) =>
              update({ auto_register_min_confidence: Number(event.target.value) })
            }
          />
        </label>
        <div className="issues-form-label">
          {t("issues.autoPolicy.priorityRange")}
          <select
            aria-label={t("issues.autoPolicy.priorityLower")}
            value={form.auto_register_priority_range.lower}
            disabled={autoRegistrationDisabled}
            onChange={(event) =>
              update({
                auto_register_priority_range: {
                  ...form.auto_register_priority_range,
                  lower: event.target.value as AutoRegisterPriority,
                },
              })
            }
          >
            {priorities.map((priority) => (
              <option key={priority}>{priority}</option>
            ))}
          </select>
          <span>{t("issues.autoPolicy.to")}</span>
          <select
            aria-label={t("issues.autoPolicy.priorityUpper")}
            value={form.auto_register_priority_range.upper}
            disabled={autoRegistrationDisabled}
            onChange={(event) =>
              update({
                auto_register_priority_range: {
                  ...form.auto_register_priority_range,
                  upper: event.target.value as AutoRegisterPriority,
                },
              })
            }
          >
            {priorities.map((priority) => (
              <option key={priority}>{priority}</option>
            ))}
          </select>
        </div>
        <SettingsCheckboxRow
          id="auto-issue-explicit-only"
          checked={form.explicit_only}
          disabled={autoRegistrationDisabled}
          onCheckedChange={(checked) => update({ explicit_only: checked })}
          title={t("issues.autoPolicy.explicitOnly")}
        />
        <fieldset className="issues-form-label">
          <legend>{t("issues.autoPolicy.allowedKinds")}</legend>
          {kinds.map((kind) => (
            <label key={kind}>
              <input
                type="checkbox"
                checked={form.allowed_kinds.includes(kind)}
                disabled={extractionDisabled}
                onChange={(event) =>
                  update({
                    allowed_kinds: event.target.checked
                      ? [...form.allowed_kinds, kind]
                      : form.allowed_kinds.filter((value) => value !== kind),
                  })
                }
              />
              {kind}
            </label>
          ))}
        </fieldset>
      </fieldset>
      <fieldset className="auto-issue-resolution-options">
        <legend>{t("issues.autoPolicy.resolutionSettings")}</legend>
        <label className="issues-form-label">
          {t("issues.autoPolicy.resolutionMode")}
          <select
            aria-label={t("issues.autoPolicy.resolutionMode")}
            value={form.resolution_mode}
            onChange={(event) =>
              update({ resolution_mode: event.target.value as AutoIssueResolutionMode })
            }
          >
            <option value="DISABLED">{t("issues.autoPolicy.resolutionDisabled")}</option>
            <option value="APPLY">{t("issues.autoPolicy.resolutionApply")}</option>
          </select>
        </label>
        <label className="issues-form-label">
          {t("issues.autoPolicy.resolutionMinConfidence")}
          <input
            aria-label={t("issues.autoPolicy.resolutionMinConfidence")}
            type="number"
            min="0"
            max="1"
            step="0.01"
            disabled={form.resolution_mode === "DISABLED"}
            value={form.resolution_min_confidence}
            onChange={(event) => update({ resolution_min_confidence: Number(event.target.value) })}
          />
        </label>
      </fieldset>
      {error && <p className="issues-error">{error}</p>}
      {!onDirtyChange && (
        <button type="button" className="primary" disabled={mutation.isPending} onClick={save}>
          {t("issues.autoPolicy.save")}
        </button>
      )}
    </section>
  );
}
