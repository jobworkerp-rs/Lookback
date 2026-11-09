import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { getWorkEligibility, setWorkEligibility } from "@/api";

/** Gateway registration is updated before the selection is persisted by the
 * command. Keeping this card separate makes a failed registration visible
 * instead of presenting an optimistic setting that is not effective. */
export function IssueWorkPolicyCard({ embedded = false }: { embedded?: boolean }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const policy = useQuery({ queryKey: ["issue-work-eligibility"], queryFn: getWorkEligibility });
  const mutation = useMutation({
    mutationFn: setWorkEligibility,
    onSuccess: async (data) => {
      queryClient.setQueryData(["issue-work-eligibility"], data);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["issues"] }),
        queryClient.invalidateQueries({ queryKey: ["issue-review"] }),
      ]);
    },
  });
  const current = policy.data?.work_eligibility ?? "REVIEWED_ONLY";
  return (
    <section className={embedded ? "issues-settings-section" : "settings-card"}>
      <div className="settings-card-title">{t("issues.policy.title")}</div>
      <div className="settings-card-desc">{t("issues.policy.description")}</div>
      <label className="issues-form-label">
        {t("issues.policy.label")}
        <select
          aria-label={t("issues.policy.label")}
          value={current}
          disabled={policy.isPending || mutation.isPending}
          onChange={(event) =>
            mutation.mutate({
              work_eligibility: event.target.value as "REVIEWED_ONLY" | "INCLUDE_AUTO_REGISTERED",
            })
          }
        >
          <option value="REVIEWED_ONLY">{t("issues.policy.reviewedOnly")}</option>
          <option value="INCLUDE_AUTO_REGISTERED">
            {t("issues.policy.includeAutoRegistered")}
          </option>
        </select>
      </label>
      {mutation.isError && (
        <p className="issues-error">
          {t("issues.policy.error")}: {mutation.error.message}
        </p>
      )}
    </section>
  );
}
