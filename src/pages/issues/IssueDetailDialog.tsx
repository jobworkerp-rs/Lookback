import { useEffect, useRef, useState } from "react";
import { CodingAgentRunPanel } from "@/components/coding-agent/CodingAgentRunPanel";
import { NativeTerminalSessionPanel } from "@/components/coding-agent/NativeTerminalSessionPanel";
import { Modal } from "@/components/Modal";
import { Badges, CopyIdButton, SourceLabels } from "@/pages/issues/IssueViews";
import { type IssueHistoryPager, useIssueHistoryPager } from "@/pages/issues/issueHistoryPager";
import { formatIssueDate, userStateTransitionTarget } from "@/pages/issues/viewModel";
import type {
  ApplyIssueAdminActionRequest,
  CreateIssueFromCanceledRequest,
  IssueListItem,
  IssueOperationStatus,
  OwnershipDiagnosisView,
  ResolveIssueOperationRequest,
  TransitionIssueStateRequest,
} from "@/types/api";

export type IssueDetailDialogProps = {
  item: IssueListItem;
  busy: boolean;
  error: Error | null;
  onClose: () => void;
  onTransition: (request: TransitionIssueStateRequest) => void;
  onCreateFromCanceled: (request: CreateIssueFromCanceledRequest) => void;
  onAdminAction: (request: ApplyIssueAdminActionRequest) => void;
  diagnostics: OwnershipDiagnosisView[];
  operationStatus: IssueOperationStatus | null;
  onResolveOperation: (request: ResolveIssueOperationRequest) => void;
  onInspectOperation: (issueId: string, operationId: string) => void;
  onRefreshAfterError: (issueId: string) => Promise<void>;
  historyRefreshIssueId: string | null;
  historyRefreshToken: number;
};

const EVIDENCE_RESOLUTION_METHODS = new Set([
  "manager_apply_issue_admin_action",
  "manager_transition_issue",
  "manager_claim_issue",
  "manager_heartbeat_issue_claim",
  "manager_release_issue_claim",
  "manager_reconcile_expired_claim",
]);

type EvidenceResolutionDecision = "CONFIRM_APPLIED" | "CONFIRM_NOT_APPLIED";
type ArbitrationResolutionOutcome = "APPLIED" | "NOT_APPLIED" | "UNKNOWN_SETTLED";
type AdminActionContext = { issueId: string; revision: string };

function humanArbitrationAllowed(status: IssueOperationStatus): boolean {
  return (
    ["RECEIVED", "PENDING", "UNCONFIRMED", "MANUAL_RESOLUTION"].includes(status.state) ||
    (status.state === "FAILED" && status.resolution_required)
  );
}

function evidenceResolutionDecisions(status: IssueOperationStatus): EvidenceResolutionDecision[] {
  if (!EVIDENCE_RESOLUTION_METHODS.has(status.method) || status.evidence_refs.length === 0) {
    return [];
  }
  if (["UNCONFIRMED", "MANUAL_RESOLUTION"].includes(status.state)) {
    const decisions: EvidenceResolutionDecision[] = ["CONFIRM_APPLIED"];
    const errorSnapshotReference = status.operation_id
      ? `manager-operation:${status.operation_id}:error-snapshot`
      : null;
    if (
      status.resolution_required &&
      errorSnapshotReference &&
      status.evidence_refs.includes(errorSnapshotReference)
    ) {
      decisions.push("CONFIRM_NOT_APPLIED");
    }
    return decisions;
  }
  if (
    status.state === "FAILED" &&
    !status.affects_ready &&
    status.operation_id &&
    status.evidence_refs.includes(`manager-operation:${status.operation_id}:error-snapshot`)
  ) {
    return ["CONFIRM_NOT_APPLIED"];
  }
  return [];
}

export function IssueDetailDialog({
  item,
  busy,
  error,
  onClose,
  onTransition,
  onCreateFromCanceled,
  onAdminAction,
  diagnostics,
  operationStatus,
  onResolveOperation,
  onInspectOperation,
  onRefreshAfterError,
  historyRefreshIssueId,
  historyRefreshToken,
}: IssueDetailDialogProps) {
  const latestItemRef = useRef(item);
  latestItemRef.current = item;
  const [rationale, setRationale] = useState("");
  const [copying, setCopying] = useState(false);
  const [auditOpen, setAuditOpen] = useState(false);
  const previousHistoryRefreshToken = useRef(historyRefreshToken);
  const recoveryHistory = useIssueHistoryPager({
    issueId: item.id,
    mode: "recovery",
    enabled: true,
  });
  const auditHistory = useIssueHistoryPager({
    issueId: item.id,
    mode: "audit",
    enabled: auditOpen,
  });
  const target = userStateTransitionTarget(item.state);
  const canceledReopen = item.state === "CANCELED" && target === "OPEN";
  const [adminAction, setAdminAction] = useState<ApplyIssueAdminActionRequest["action"] | null>(
    null,
  );
  const [adminActionContext, setAdminActionContext] = useState<AdminActionContext | null>(null);
  const [adminRationale, setAdminRationale] = useState("");
  const [completionMode, setCompletionMode] = useState<"verified" | "unverified">("unverified");
  const [completionReason, setCompletionReason] = useState("");
  const [residualRisk, setResidualRisk] = useState("");
  const [cancelClass, setCancelClass] = useState<"FALSE_POSITIVE" | "OTHER">("OTHER");
  const [dependencyOverride, setDependencyOverride] = useState(false);
  const [dependencyOverrideReason, setDependencyOverrideReason] = useState("");
  const [inconsistentClaimResolution, setInconsistentClaimResolution] = useState(false);
  const [inconsistentClaimReason, setInconsistentClaimReason] = useState("");
  const [diagnosisIndex, setDiagnosisIndex] = useState(0);
  const [operationKey, setOperationKey] = useState(() => crypto.randomUUID());
  const [resolutionDecision, setResolutionDecision] =
    useState<EvidenceResolutionDecision>("CONFIRM_APPLIED");
  const [arbitrationOutcome, setArbitrationOutcome] =
    useState<ArbitrationResolutionOutcome>("APPLIED");
  const [resolutionMode, setResolutionMode] =
    useState<ResolveIssueOperationRequest["mode"]>("HUMAN_ARBITRATION");
  const [resolutionEvidenceRefs, setResolutionEvidenceRefs] = useState("");
  const [resolutionReason, setResolutionReason] = useState("");
  const [resolutionAcknowledged, setResolutionAcknowledged] = useState(false);
  const [resolutionKey, setResolutionKey] = useState(() => crypto.randomUUID());
  const selectedDiagnosis = diagnostics[diagnosisIndex] ?? diagnostics[0];
  const resolutionOperationId = operationStatus?.resolution_operation_id;
  const operationId = operationStatus?.operation_id;
  const evidenceDecisions = operationStatus ? evidenceResolutionDecisions(operationStatus) : [];
  const canHumanArbitrate = operationStatus ? humanArbitrationAllowed(operationStatus) : false;
  const canEvidenceConfirm = evidenceDecisions.length > 0;
  const canResolveOperation = Boolean(operationId && (canHumanArbitrate || canEvidenceConfirm));
  const activeResolutionMode =
    resolutionMode === "HUMAN_ARBITRATION" && canHumanArbitrate
      ? "HUMAN_ARBITRATION"
      : canEvidenceConfirm
        ? "EVIDENCE_CONFIRMATION"
        : "HUMAN_ARBITRATION";
  const activeEvidenceDecision = evidenceDecisions.includes(resolutionDecision)
    ? resolutionDecision
    : (evidenceDecisions[0] ?? "CONFIRM_APPLIED");
  const canTerminate = item.state !== "DONE" && item.state !== "CANCELED";
  const awaitsCodingAgentReview = item.state.toUpperCase() === "AWAITING_REVIEW";
  const adminActionContextIsCurrent = Boolean(
    adminAction &&
      adminActionContext?.issueId === item.id &&
      adminActionContext.revision === item.revision &&
      !(adminAction === "COMPLETE" && awaitsCodingAgentReview),
  );
  const canManageOwnership = canTerminate || Boolean(item.claim) || diagnostics.length > 0;
  useEffect(() => {
    if (!adminAction || !adminActionContext) return;
    if (
      adminActionContext.issueId === item.id &&
      adminActionContext.revision === item.revision &&
      !(adminAction === "COMPLETE" && awaitsCodingAgentReview)
    ) {
      return;
    }
    setAdminAction(null);
    setAdminActionContext(null);
    setAdminRationale("");
    setCompletionMode("unverified");
    setCompletionReason("");
    setResidualRisk("");
    setDependencyOverride(false);
    setDependencyOverrideReason("");
    setInconsistentClaimResolution(false);
    setInconsistentClaimReason("");
  }, [adminAction, adminActionContext, awaitsCodingAgentReview, item.id, item.revision]);
  useEffect(() => {
    // Evidence references are bound to one source operation. Never carry a
    // selection (or its idempotency key) when the user opens another history
    // entry in the same dialog.
    if (operationId === undefined) return;
    setResolutionEvidenceRefs("");
    setResolutionKey(crypto.randomUUID());
  }, [operationId]);
  useEffect(() => {
    const confirmedConflict =
      operationStatus?.issue_id === item.id &&
      (operationStatus.error_code === "REVISION_CONFLICT" ||
        operationStatus.error_code === "GENERATION_CONFLICT");
    if (confirmedConflict) {
      // These errors are definitive: the server rejected the compare-and-swap
      // fence. A fresh key is safe only after that confirmation. Transport or
      // result-unknown errors keep the original key for pending replay.
      if (operationStatus.method === "manager_resolve_issue_operation") {
        setResolutionKey(crypto.randomUUID());
      } else if (operationStatus.method === "manager_apply_issue_admin_action") {
        setOperationKey(crypto.randomUUID());
      }
      void onRefreshAfterError(item.id);
    }
  }, [item.id, onRefreshAfterError, operationStatus]);
  useEffect(() => {
    const refreshChanged = previousHistoryRefreshToken.current !== historyRefreshToken;
    previousHistoryRefreshToken.current = historyRefreshToken;
    if (!refreshChanged || historyRefreshToken === 0 || historyRefreshIssueId !== item.id) return;
    recoveryHistory.reload();
    if (auditOpen) auditHistory.reload();
  }, [
    auditHistory.reload,
    auditOpen,
    historyRefreshIssueId,
    historyRefreshToken,
    item.id,
    recoveryHistory.reload,
  ]);
  return (
    <Modal onClose={onClose} wide ariaLabel="Issue 詳細">
      <div className="modal-body">
        {copying ? (
          <CanceledIssueCopyForm
            item={item}
            busy={busy}
            error={error}
            onBack={() => setCopying(false)}
            onCreate={onCreateFromCanceled}
          />
        ) : (
          <>
            <h2>{item.title}</h2>
            <Badges item={item} />
            <SourceLabels labels={item.source_labels} />
            <dl className="issues-definition">
              <dt>ID</dt>
              <dd className="issue-definition-id">
                <span>{item.id}</span>
                <CopyIdButton id={item.id} />
              </dd>
              <dt>Description</dt>
              <dd>{item.description}</dd>
              <dt>Kind</dt>
              <dd>{item.kind}</dd>
              <dt>状態</dt>
              <dd>{item.state}</dd>
              <dt>優先度</dt>
              <dd>{item.priority}</dd>
              <dt>完了条件</dt>
              <dd>{item.completion_criteria ?? "—"}</dd>
              <dt>作成日時</dt>
              <dd>{formatIssueDate(item.created_at_ms)}</dd>
              <dt>更新日時</dt>
              <dd>{formatIssueDate(item.updated_at_ms)}</dd>
              <dt>登録経路</dt>
              <dd>{item.registration_path ?? "—"}</dd>
              <dt>Policy / proof</dt>
              <dd>
                {item.policy ?? "—"} / {item.proof ?? "—"}
              </dd>
              <dt>所有権</dt>
              <dd>
                {item.claim ? item.claim.lease_status : "lease_absent"}
                {item.claim?.lease_expires_at_ms
                  ? ` · ${formatIssueDate(item.claim.lease_expires_at_ms)}`
                  : ""}
              </dd>
            </dl>
            <CodingAgentRunPanel issue={item} />
            <NativeTerminalSessionPanel issueId={item.id} />
            {diagnostics.length > 0 && (
              <div className="issues-callout">
                所有権診断:{" "}
                {diagnostics
                  .map((diagnosis) => `${diagnosis.diagnosis_class} (${diagnosis.state})`)
                  .join(", ")}
              </div>
            )}
            {operationStatus && (
              <div className="issues-callout" role="status">
                操作結果: {operationStatus.state}
                {operationStatus.operation_id ? ` (${operationStatus.operation_id})` : ""}
                {operationStatus.resolution_required ? " · 手動解決が必要です" : ""}
                {operationStatus.ownership_cleared !== null &&
                operationStatus.ownership_cleared !== undefined
                  ? ` · 所有権解消: ${operationStatus.ownership_cleared ? "適用" : "なし"}`
                  : ""}
                {operationStatus.dependency_override_applied ? " · 依存関係 override: 適用" : ""}
                {operationStatus.inconsistent_claim_resolution_applied
                  ? " · 不整合 claim 解消: 適用"
                  : ""}
                {operationStatus.observed_lease_classification
                  ? ` · lease: ${operationStatus.observed_lease_classification}`
                  : ""}
                {operationStatus.arbitration_outcome
                  ? ` · 仲裁結論: ${operationStatus.arbitration_outcome}`
                  : ""}
                {operationStatus.resulting_revision
                  ? ` · revision: ${operationStatus.resulting_revision}`
                  : ""}
                {operationStatus.resulting_state
                  ? ` · 状態: ${operationStatus.resulting_state}`
                  : ""}
                {operationStatus.diagnosis ? ` · ${operationStatus.diagnosis}` : ""}
                {resolutionOperationId && (
                  <button
                    type="button"
                    className="issue-link"
                    onClick={() => {
                      if (resolutionOperationId) onInspectOperation(item.id, resolutionOperationId);
                    }}
                  >
                    仲裁操作を確認
                  </button>
                )}
              </div>
            )}
            {canResolveOperation && operationStatus && (
              <section className="issues-admin-form">
                <h3>結果不明の操作を解決</h3>
                <p className="issues-note">
                  {["RECEIVED", "PENDING"].includes(operationStatus.state)
                    ? "操作はまだ処理中の可能性があります。再試行と競合するため、現在の結果を確認してから判断してください。"
                    : "サーバが確定できないため、結果確認を監査記録として登録します。"}
                </p>
                <label className="issues-form-label">
                  解決方法
                  <select
                    value={activeResolutionMode}
                    disabled={busy}
                    onChange={(event) => {
                      setResolutionMode(event.target.value as typeof resolutionMode);
                      setResolutionKey(crypto.randomUUID());
                    }}
                  >
                    {canHumanArbitrate && <option value="HUMAN_ARBITRATION">人間の最終判断</option>}
                    {canEvidenceConfirm && (
                      <option value="EVIDENCE_CONFIRMATION">サーバ evidence で確認</option>
                    )}
                  </select>
                </label>
                <label className="issues-form-label">
                  判断
                  <select
                    value={
                      activeResolutionMode === "EVIDENCE_CONFIRMATION"
                        ? activeEvidenceDecision
                        : arbitrationOutcome
                    }
                    disabled={busy}
                    onChange={(event) => {
                      if (activeResolutionMode === "EVIDENCE_CONFIRMATION") {
                        setResolutionDecision(event.target.value as typeof resolutionDecision);
                      } else {
                        setArbitrationOutcome(event.target.value as typeof arbitrationOutcome);
                      }
                      setResolutionKey(crypto.randomUUID());
                    }}
                  >
                    {activeResolutionMode === "EVIDENCE_CONFIRMATION" ? (
                      <>
                        {evidenceDecisions.includes("CONFIRM_APPLIED") && (
                          <option value="CONFIRM_APPLIED">適用済み</option>
                        )}
                        {evidenceDecisions.includes("CONFIRM_NOT_APPLIED") && (
                          <option value="CONFIRM_NOT_APPLIED">未適用</option>
                        )}
                      </>
                    ) : (
                      <>
                        <option value="APPLIED">適用済み</option>
                        <option value="NOT_APPLIED">未適用</option>
                        <option value="UNKNOWN_SETTLED">適用状態不明のまま終結</option>
                      </>
                    )}
                  </select>
                </label>
                {activeResolutionMode === "EVIDENCE_CONFIRMATION" ? (
                  <fieldset className="issues-form-label" disabled={busy}>
                    <legend>サーバ evidence reference（1件以上）</legend>
                    {operationStatus.evidence_refs.length > 0 ? (
                      operationStatus.evidence_refs.map((reference) => {
                        const selected = resolutionEvidenceRefs
                          .split("\n")
                          .map((value) => value.trim())
                          .includes(reference);
                        return (
                          <label key={reference}>
                            <input
                              type="checkbox"
                              checked={selected}
                              onChange={() => {
                                const refs = new Set(
                                  resolutionEvidenceRefs
                                    .split("\n")
                                    .map((value) => value.trim())
                                    .filter(Boolean),
                                );
                                if (selected) refs.delete(reference);
                                else refs.add(reference);
                                setResolutionEvidenceRefs([...refs].join("\n"));
                                setResolutionKey(crypto.randomUUID());
                              }}
                            />{" "}
                            <code>{reference}</code>
                          </label>
                        );
                      })
                    ) : (
                      <p className="issues-note">
                        この操作には利用可能なサーバ evidence reference がありません。
                      </p>
                    )}
                  </fieldset>
                ) : (
                  <>
                    <label className="issues-form-label">
                      <input
                        type="checkbox"
                        checked={resolutionAcknowledged}
                        disabled={busy}
                        onChange={(event) => {
                          setResolutionAcknowledged(event.target.checked);
                          setResolutionKey(crypto.randomUUID());
                        }}
                      />{" "}
                      結果不明であることを了承する
                    </label>
                    <label className="issues-form-label">
                      判断理由
                      <textarea
                        value={resolutionReason}
                        disabled={busy}
                        onChange={(event) => {
                          setResolutionReason(event.target.value);
                          setResolutionKey(crypto.randomUUID());
                        }}
                      />
                    </label>
                  </>
                )}
                <button
                  type="button"
                  className="btn primary"
                  disabled={
                    busy ||
                    (activeResolutionMode === "EVIDENCE_CONFIRMATION"
                      ? !resolutionEvidenceRefs.split("\n").some((ref) => ref.trim())
                      : !resolutionAcknowledged || !resolutionReason.trim())
                  }
                  onClick={() => {
                    if (!operationId || !operationStatus) return;
                    const common = {
                      issue_id: item.id,
                      operation_id: operationId,
                      expected_operation_generation: operationStatus.generation,
                      expected_revision: item.revision,
                      operation_key: resolutionKey,
                    };
                    if (activeResolutionMode === "EVIDENCE_CONFIRMATION") {
                      onResolveOperation({
                        ...common,
                        mode: activeResolutionMode,
                        decision: activeEvidenceDecision,
                        evidence_refs: resolutionEvidenceRefs
                          .split("\n")
                          .map((ref) => ref.trim())
                          .filter(Boolean),
                      });
                    } else {
                      onResolveOperation({
                        ...common,
                        mode: activeResolutionMode,
                        arbitration_outcome: arbitrationOutcome,
                        uncertainty_acknowledged: true,
                        arbitration_reason: resolutionReason.trim(),
                      });
                    }
                  }}
                >
                  判断を登録
                </button>
              </section>
            )}
            <IssueHistoryPagerSection
              title="要確認の操作"
              emptyText="要確認の操作はありません。"
              pager={recoveryHistory}
              onInspectOperation={(operationId) => onInspectOperation(item.id, operationId)}
            />
            <details
              className="issues-history-inline"
              onToggle={(event) => setAuditOpen(event.currentTarget.open)}
            >
              <summary>全操作履歴</summary>
              {auditOpen && (
                <IssueHistoryPagerSection
                  title="監査履歴"
                  emptyText="監査履歴はありません。"
                  pager={auditHistory}
                  onInspectOperation={(operationId) => onInspectOperation(item.id, operationId)}
                />
              )}
            </details>
            {target ? (
              <>
                <label className="issues-form-label">
                  {target === "BLOCKED"
                    ? "自動実行停止の理由"
                    : canceledReopen
                      ? "取消を戻す理由（任意）"
                      : "自動実行再開の理由"}
                  <textarea
                    value={rationale}
                    onChange={(event) => setRationale(event.target.value)}
                  />
                </label>
                {error && <p className="issues-error">{error.message}</p>}
                <button
                  type="button"
                  className="btn"
                  disabled={(!canceledReopen && !rationale.trim()) || busy}
                  onClick={() =>
                    onTransition({
                      issue_id: item.id,
                      expected_revision: item.revision,
                      target_state: target,
                      rationale: rationale.trim() || null,
                    })
                  }
                >
                  {target === "BLOCKED"
                    ? "自動実行を停止"
                    : canceledReopen
                      ? "取消を戻す"
                      : "自動実行を再開"}
                </button>
                {canceledReopen && (
                  <>
                    <p className="issues-note">
                      取消済みの内容を編集して、新しい Issue として作成することもできます。元の
                      Issue と取消の記録は変更しません。
                    </p>
                    <button type="button" className="btn" onClick={() => setCopying(true)}>
                      内容を複製して新規 Issue を作成
                    </button>
                  </>
                )}
              </>
            ) : (
              <p className="issues-note">完了済みの Issue は、この画面から状態変更できません。</p>
            )}
            {canManageOwnership && (
              <div className="issues-admin-actions">
                <h3>人間による管理操作</h3>
                <p className="issues-note">
                  操作はサーバで再検証され、実行中 worker の停止を保証しません。
                </p>
                <div className="issues-detail-actions">
                  {canTerminate && (
                    <>
                      {!awaitsCodingAgentReview && (
                        <button
                          type="button"
                          className="btn"
                          onClick={() => {
                            setAdminAction("COMPLETE");
                            setAdminActionContext({ issueId: item.id, revision: item.revision });
                            setOperationKey(crypto.randomUUID());
                          }}
                        >
                          完了
                        </button>
                      )}
                      <button
                        type="button"
                        className="btn"
                        onClick={() => {
                          setAdminAction("CANCEL");
                          setAdminActionContext({ issueId: item.id, revision: item.revision });
                          setOperationKey(crypto.randomUUID());
                        }}
                      >
                        取消
                      </button>
                    </>
                  )}
                  <button
                    type="button"
                    className="btn"
                    onClick={() => {
                      setAdminAction("RECOVER");
                      setAdminActionContext({ issueId: item.id, revision: item.revision });
                      setOperationKey(crypto.randomUUID());
                    }}
                  >
                    所有権を回復
                  </button>
                  <button
                    type="button"
                    className="btn"
                    onClick={() => {
                      setAdminAction("REVOKE");
                      setAdminActionContext({ issueId: item.id, revision: item.revision });
                      setOperationKey(crypto.randomUUID());
                    }}
                  >
                    所有権を失効
                  </button>
                </div>
              </div>
            )}
            {adminAction && adminActionContextIsCurrent && (
              <section className="issues-admin-form">
                <h3>{adminAction} を実行</h3>
                <label className="issues-form-label">
                  操作理由
                  <textarea
                    value={adminRationale}
                    onChange={(event) => {
                      setAdminRationale(event.target.value);
                      setOperationKey(crypto.randomUUID());
                    }}
                  />
                </label>
                {adminAction === "COMPLETE" && (
                  <>
                    <label className="issues-form-label">
                      完了報告
                      <select
                        value={completionMode}
                        onChange={(event) => {
                          setCompletionMode(event.target.value as typeof completionMode);
                          setOperationKey(crypto.randomUUID());
                        }}
                      >
                        <option value="unverified">検証不能</option>
                        <option value="verified">検証済み evidence</option>
                      </select>
                    </label>
                    {completionMode === "verified" ? (
                      <label className="issues-form-label">
                        Evidence summary
                        <input
                          value={completionReason}
                          onChange={(event) => {
                            setCompletionReason(event.target.value);
                            setOperationKey(crypto.randomUUID());
                          }}
                        />
                      </label>
                    ) : (
                      <>
                        <label className="issues-form-label">
                          検証不能理由
                          <input
                            value={completionReason}
                            onChange={(event) => {
                              setCompletionReason(event.target.value);
                              setOperationKey(crypto.randomUUID());
                            }}
                          />
                        </label>
                        <label className="issues-form-label">
                          残存 risk
                          <input
                            value={residualRisk}
                            onChange={(event) => {
                              setResidualRisk(event.target.value);
                              setOperationKey(crypto.randomUUID());
                            }}
                          />
                        </label>
                      </>
                    )}
                  </>
                )}
                {adminAction === "CANCEL" && (
                  <label className="issues-form-label">
                    取消分類
                    <select
                      value={cancelClass}
                      onChange={(event) => {
                        setCancelClass(event.target.value as typeof cancelClass);
                        setOperationKey(crypto.randomUUID());
                      }}
                    >
                      <option value="OTHER">その他</option>
                      <option value="FALSE_POSITIVE">誤検知</option>
                    </select>
                  </label>
                )}
                {adminAction === "COMPLETE" && (
                  <>
                    <label className="issues-form-label">
                      <input
                        type="checkbox"
                        checked={dependencyOverride}
                        onChange={(event) => {
                          setDependencyOverride(event.target.checked);
                          setOperationKey(crypto.randomUUID());
                        }}
                      />{" "}
                      未完了の依存 Issue を明示的に override する
                    </label>
                    {dependencyOverride && (
                      <label className="issues-form-label">
                        override 理由
                        <input
                          value={dependencyOverrideReason}
                          onChange={(event) => {
                            setDependencyOverrideReason(event.target.value);
                            setOperationKey(crypto.randomUUID());
                          }}
                        />
                      </label>
                    )}
                  </>
                )}
                {diagnostics.length > 0 &&
                  (adminAction === "COMPLETE" || adminAction === "CANCEL") && (
                    <>
                      <label className="issues-form-label">
                        <input
                          type="checkbox"
                          checked={inconsistentClaimResolution}
                          onChange={(event) => {
                            setInconsistentClaimResolution(event.target.checked);
                            setOperationKey(crypto.randomUUID());
                          }}
                        />{" "}
                        診断済みの claim 不整合を同時に解消する
                      </label>
                      {inconsistentClaimResolution && (
                        <>
                          {diagnostics.length > 1 && (
                            <label className="issues-form-label">
                              診断
                              <select
                                value={diagnosisIndex}
                                onChange={(event) => {
                                  setDiagnosisIndex(Number(event.target.value));
                                  setOperationKey(crypto.randomUUID());
                                }}
                              >
                                {diagnostics.map((diagnosis, index) => (
                                  <option
                                    key={`${diagnosis.target_kind}:${diagnosis.target_id}:${diagnosis.diagnosis_class}:${diagnosis.diagnosis_generation}:${diagnosis.observation_token}`}
                                    value={index}
                                  >
                                    {diagnosis.diagnosis_class} · {diagnosis.target_id}
                                  </option>
                                ))}
                              </select>
                            </label>
                          )}
                          <label className="issues-form-label">
                            不整合解消理由
                            <input
                              value={inconsistentClaimReason}
                              onChange={(event) => {
                                setInconsistentClaimReason(event.target.value);
                                setOperationKey(crypto.randomUUID());
                              }}
                            />
                          </label>
                        </>
                      )}
                    </>
                  )}
                {error && <p className="issues-error">{error.message}</p>}
                <button
                  type="button"
                  className="btn primary"
                  disabled={
                    busy ||
                    !adminActionContextIsCurrent ||
                    !adminRationale.trim() ||
                    (adminAction === "COMPLETE" &&
                      (!completionReason.trim() ||
                        (completionMode === "unverified" && !residualRisk.trim()) ||
                        (dependencyOverride && !dependencyOverrideReason.trim()))) ||
                    (inconsistentClaimResolution &&
                      (!inconsistentClaimReason.trim() || !selectedDiagnosis))
                  }
                  onClick={() => {
                    const latestItem = latestItemRef.current;
                    if (
                      !adminActionContext ||
                      !adminActionContextIsCurrent ||
                      latestItem.id !== adminActionContext.issueId ||
                      latestItem.revision !== adminActionContext.revision ||
                      (adminAction === "COMPLETE" &&
                        latestItem.state.toUpperCase() === "AWAITING_REVIEW")
                    ) {
                      return;
                    }
                    const completion =
                      adminAction === "COMPLETE"
                        ? completionMode === "verified"
                          ? {
                              verified_evidence: [
                                {
                                  kind: "OTHER_EVIDENCE" as const,
                                  summary: completionReason.trim(),
                                },
                              ],
                            }
                          : {
                              unverified_reason: completionReason.trim(),
                              residual_risk: residualRisk.trim(),
                            }
                        : null;
                    onAdminAction({
                      issue_id: adminActionContext.issueId,
                      expected_revision: adminActionContext.revision,
                      action: adminAction,
                      rationale: adminRationale.trim(),
                      completion,
                      cancellation_classification: adminAction === "CANCEL" ? cancelClass : null,
                      dependency_override:
                        adminAction === "COMPLETE" && dependencyOverride
                          ? { reason: dependencyOverrideReason.trim() }
                          : null,
                      inconsistent_claim_resolution: inconsistentClaimResolution
                        ? { reason: inconsistentClaimReason.trim() }
                        : null,
                      ownership_diagnosis:
                        inconsistentClaimResolution && selectedDiagnosis
                          ? {
                              target_kind: selectedDiagnosis.target_kind,
                              target_id: selectedDiagnosis.target_id,
                              diagnosis_class: selectedDiagnosis.diagnosis_class,
                              evidence_digest: selectedDiagnosis.evidence_digest,
                              diagnosis_generation: selectedDiagnosis.diagnosis_generation,
                              observation_token: selectedDiagnosis.observation_token,
                              repair_epoch: selectedDiagnosis.repair_epoch,
                            }
                          : null,
                      operation_key: operationKey,
                    });
                  }}
                >
                  実行
                </button>
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    setAdminAction(null);
                    setAdminActionContext(null);
                  }}
                >
                  戻る
                </button>
              </section>
            )}
          </>
        )}
      </div>
    </Modal>
  );
}

function HistoryErrorBlock({
  error,
  loading,
  onReload,
}: {
  error: Error;
  loading: boolean;
  onReload: () => void;
}) {
  return (
    <>
      <p className="issues-error">操作履歴を取得できません: {error.message}</p>
      <button type="button" className="btn" onClick={onReload} disabled={loading}>
        再読み込み
      </button>
    </>
  );
}

function IssueHistoryPagerSection({
  title,
  emptyText,
  pager,
  onInspectOperation,
}: {
  title: string;
  emptyText: string;
  pager: IssueHistoryPager;
  onInspectOperation: (operationId: string) => void;
}) {
  const loadMoreButton = pager.hasNextPage ? (
    <button type="button" className="btn" onClick={pager.loadMore} disabled={pager.loadingMore}>
      {pager.loadingMore ? "読み込み中…" : "さらに読み込む"}
    </button>
  ) : null;

  return (
    <section className="issues-history-pager" aria-label={title}>
      <h3>{title}</h3>
      {pager.resetNotice && (
        <p className="issues-note" role="status">
          履歴が更新されたため、先頭から読み込み直しました。
        </p>
      )}
      {pager.error && pager.entries.length === 0 ? (
        <HistoryErrorBlock error={pager.error} loading={pager.loading} onReload={pager.reload} />
      ) : pager.loading && pager.entries.length === 0 ? (
        <p className="issues-note">読み込み中…</p>
      ) : pager.entries.length === 0 ? (
        <>
          <p className="issues-note">{emptyText}</p>
          {loadMoreButton}
        </>
      ) : (
        <>
          {pager.error && (
            <HistoryErrorBlock
              error={pager.error}
              loading={pager.loading}
              onReload={pager.reload}
            />
          )}
          {pager.entries.map((entry) => (
            <div key={entry.id}>
              <button
                type="button"
                className="issue-link"
                onClick={() => onInspectOperation(entry.id)}
              >
                <strong>{entry.decision}</strong> · {entry.state}
              </button>
            </div>
          ))}
          {loadMoreButton}
        </>
      )}
    </section>
  );
}

function CanceledIssueCopyForm({
  item,
  busy,
  error,
  onBack,
  onCreate,
}: {
  item: IssueListItem;
  busy: boolean;
  error: Error | null;
  onBack: () => void;
  onCreate: (request: CreateIssueFromCanceledRequest) => void;
}) {
  const [title, setTitle] = useState(item.title);
  const [description, setDescription] = useState(item.description);
  const [kind, setKind] = useState(item.kind);
  const [priority, setPriority] = useState(item.priority);
  const [suggestedState, setSuggestedState] =
    useState<CreateIssueFromCanceledRequest["suggested_state"]>("OPEN");
  const [acceptanceCriteria, setAcceptanceCriteria] = useState(item.completion_criteria ?? "");
  const [rationale, setRationale] = useState("");
  const [linkToSource, setLinkToSource] = useState(false);
  const [operationId] = useState(() => crypto.randomUUID());

  return (
    <>
      <h2>新規 Issue を作成</h2>
      <p className="issues-note">取消済み Issue の内容を下書きとして複製しています。</p>
      <label className="issues-form-label">
        タイトル
        <input value={title} onChange={(event) => setTitle(event.target.value)} />
      </label>
      <label className="issues-form-label">
        Description
        <textarea value={description} onChange={(event) => setDescription(event.target.value)} />
      </label>
      <label className="issues-form-label">
        Kind
        <select value={kind} onChange={(event) => setKind(event.target.value)}>
          {["TASK", "BUG", "RESEARCH", "DECISION"].map((value) => (
            <option key={value} value={value}>
              {value}
            </option>
          ))}
        </select>
      </label>
      <label className="issues-form-label">
        優先度
        <select
          value={priority}
          onChange={(event) => setPriority(event.target.value as typeof priority)}
        >
          {["P0", "P1", "P2", "P3", "P4"].map((value) => (
            <option key={value} value={value}>
              {value}
            </option>
          ))}
        </select>
      </label>
      <label className="issues-form-label">
        初期状態
        <select
          value={suggestedState}
          onChange={(event) =>
            setSuggestedState(
              event.target.value as CreateIssueFromCanceledRequest["suggested_state"],
            )
          }
        >
          {["OPEN", "IN_PROGRESS", "BLOCKED", "DEFERRED"].map((value) => (
            <option key={value} value={value}>
              {value}
            </option>
          ))}
        </select>
      </label>
      <label className="issues-form-label">
        完了条件（1行につき1件）
        <textarea
          value={acceptanceCriteria}
          onChange={(event) => setAcceptanceCriteria(event.target.value)}
        />
      </label>
      <label>
        <input
          type="checkbox"
          checked={linkToSource}
          onChange={(event) => setLinkToSource(event.target.checked)}
        />
        元 Issue と関連付ける
      </label>
      <label className="issues-form-label">
        新規作成する理由
        <textarea value={rationale} onChange={(event) => setRationale(event.target.value)} />
      </label>
      {error && <p className="issues-error">{error.message}</p>}
      <div className="modal-actions">
        <button type="button" className="btn" disabled={busy} onClick={onBack}>
          戻る
        </button>
        <button
          type="button"
          className="btn primary"
          disabled={!title.trim() || !description.trim() || !rationale.trim() || busy}
          onClick={() =>
            onCreate({
              source_issue_id: item.id,
              expected_source_revision: item.revision,
              operation_id: operationId,
              title: title.trim(),
              description: description.trim(),
              kind,
              priority,
              suggested_state: suggestedState,
              acceptance_criteria: acceptanceCriteria
                .split("\n")
                .map((value) => value.trim())
                .filter(Boolean),
              rationale: rationale.trim(),
              link_to_source: linkToSource,
            })
          }
        >
          {busy ? "作成中…" : "新規 Issue を作成"}
        </button>
      </div>
    </>
  );
}
