import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getIssueReviewPolicy, setIssueReviewPolicy } from "@/api";
import { SettingsCheckboxRow } from "@/components/SettingsCheckboxRow";

export function IssueReviewPolicyCard({ embedded = false }: { embedded?: boolean }) {
  const queryClient = useQueryClient();
  const policy = useQuery({ queryKey: ["issue-review-policy"], queryFn: getIssueReviewPolicy });
  const mutation = useMutation({
    mutationFn: setIssueReviewPolicy,
    onSuccess: (value) => queryClient.setQueryData(["issue-review-policy"], value),
  });
  const required = policy.data?.require_high_priority_reason ?? false;
  return (
    <section className={embedded ? "issues-settings-section" : "settings-card"}>
      <div className="settings-card-title">レビュー要件</div>
      <div className="settings-card-desc">高優先度の確認時に、判断理由を必須にします。</div>
      <SettingsCheckboxRow
        id="issue-review-high-priority-reason-required"
        checked={required}
        disabled={policy.isPending || mutation.isPending}
        onCheckedChange={(checked) =>
          mutation.mutate({ policy: { require_high_priority_reason: checked } })
        }
        title="P0/P1 の確認では理由を必須にする"
      />
      {mutation.isError && <p className="issues-error">保存できません: {mutation.error.message}</p>}
    </section>
  );
}
