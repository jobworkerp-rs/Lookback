import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import * as issueApi from "@/api";
import {
  createExplicitIssueFromCandidate,
  createIssueFromCanceled,
  getIssueDetail,
  getIssueDuplicates,
  getIssueEvidence,
  getIssueReviewPolicy,
  getWorkEligibility,
  listIssueHistory,
  listIssueReviewItems,
  listIssues,
  resolveIssueReview,
  transitionIssueState,
  updateIssueCandidate,
} from "@/api";
import { LabelFilter } from "@/components/LabelFilter";
import { Modal } from "@/components/Modal";
import { type OpenThreadState, ThreadDetail } from "@/components/ThreadDetail";
import { useLabelSelection } from "@/hooks/useLabelSelection";
import { useRejectedCandidateReextract } from "@/hooks/useRejectedCandidateReextract";
import { resolveThreadHighlight } from "@/lib/chatSourceNav";
import { synthesizeThreadSummary } from "@/lib/threadSummary";
import {
  AcceptDialog,
  CandidateEditDialog,
  DuplicatesDialog,
  EvidenceDialog,
  ExplicitIssueDialog,
  RejectDialog,
} from "@/pages/issues/dialogs/ReviewDialogs";
import { IssueDetailDialog, type IssueDetailDialogProps } from "@/pages/issues/IssueDetailDialog";
import {
  Empty,
  History,
  IssueInventory,
  PendingOperations,
  ReviewDetail,
  type ReviewDialog,
  ReviewTable,
  SummaryCard,
} from "@/pages/issues/IssueViews";
import { invalidateIssueQueries } from "@/pages/issues/queryInvalidation";
import {
  EMPTY_REVIEW_FILTERS,
  matchesSourceLabels,
  reviewMatches,
  uniqueSourceLabels,
} from "@/pages/issues/viewModel";
import type {
  ApplyIssueAdminActionRequest,
  IssueEvidence,
  IssueListItem,
  IssueOperationStatus,
  IssueReviewItem,
  LabelMatch,
  ListIssueReviewRequest,
  PendingIssueOperationView,
  ResolveIssueOperationRequest,
} from "@/types/api";

type View = "review" | "issues" | "history";
type SummaryFilter = "urgent" | "reviewable" | null;

type IssueDetailDialogSharedProps = Omit<
  IssueDetailDialogProps,
  "item" | "onClose" | "operationStatus"
>;

const REVIEW_ALL_QUERY_KEY = ["issue-review", "all"] as const;

function operationStatusIsSettled(status: IssueOperationStatus): boolean {
  if (status.resolution_required) return false;
  if (status.state === "COMMITTED") return true;
  return ["FAILED", "ARBITRATED"].includes(status.state) && !status.affects_ready;
}

export default function IssuesPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [view, setView] = useState<View>("review");
  const [summary, setSummary] = useState<SummaryFilter>(null);
  const [filters, setFilters] = useState<ListIssueReviewRequest>(EMPTY_REVIEW_FILTERS);
  const [selected, setSelected] = useState<IssueReviewItem | null>(null);
  const [issueDetail, setIssueDetail] = useState<IssueListItem | null>(null);
  const [operationStatus, setOperationStatus] = useState<IssueOperationStatus | null>(null);
  const [historyIssueId, setHistoryIssueId] = useState<string | null>(null);
  const [historyRefresh, setHistoryRefresh] = useState({
    issueId: null as string | null,
    token: 0,
  });
  const [evidenceThread, setEvidenceThread] = useState<OpenThreadState | null>(null);
  const [dialog, setDialog] = useState<ReviewDialog | null>(null);
  const [editor, setEditor] = useState(false);
  const [explicitDialog, setExplicitDialog] = useState(false);
  const [board, setBoard] = useState(false);
  const [issueStates, setIssueStates] = useState<string[]>([]);
  const {
    selectedLabels: reviewLabels,
    setSelectedLabels: setReviewLabels,
    toggleLabel: toggleReviewLabel,
    toggleManyLabels: toggleManyReviewLabels,
  } = useLabelSelection();
  const [reviewLabelMatch, setReviewLabelMatch] = useState<LabelMatch>("any");
  const {
    selectedLabels: issueLabels,
    setSelectedLabels: setIssueLabels,
    toggleLabel: toggleIssueLabel,
    toggleManyLabels: toggleManyIssueLabels,
  } = useLabelSelection();
  const [issueLabelMatch, setIssueLabelMatch] = useState<LabelMatch>("any");
  const {
    rejectedCandidateId,
    setRejectedCandidate,
    reextract,
    isReextracting,
    error: reextractError,
  } = useRejectedCandidateReextract();
  // One unfiltered review query backs both the list and the summary; filters
  // are applied locally so changing them never refetches every review page.
  const review = useQuery({
    queryKey: REVIEW_ALL_QUERY_KEY,
    queryFn: () => listIssueReviewItems(EMPTY_REVIEW_FILTERS),
  });
  const reviewItems = useMemo(
    () => (review.data ?? []).filter((item) => reviewMatches(item, filters)),
    [review.data, filters],
  );
  const reviewHasPending = useMemo(
    () => (review.data ?? []).some((item) => item.resolution_state === "resolution_pending"),
    [review.data],
  );
  useEffect(() => {
    if (!reviewHasPending) return;
    const timer = window.setInterval(() => {
      void queryClient.refetchQueries({ queryKey: ["issue-review"] });
    }, 5_000);
    return () => window.clearInterval(timer);
  }, [queryClient, reviewHasPending]);
  useEffect(() => {
    if (!selected) return;
    const latest = reviewItems.find((item) => item.id === selected.id);
    if (!latest) {
      setSelected(null);
      setDialog(null);
      setEditor(false);
      setExplicitDialog(false);
    } else if (latest !== selected) {
      setSelected(latest);
    }
  }, [reviewItems, selected]);
  const issues = useQuery({
    queryKey: ["issues", issueStates],
    queryFn: () => listIssues({ states: issueStates }),
    enabled: view === "issues",
  });
  const workEligibility = useQuery({
    queryKey: ["issue-work-eligibility"],
    queryFn: getWorkEligibility,
  });
  const reviewPolicy = useQuery({
    queryKey: ["issue-review-policy"],
    queryFn: getIssueReviewPolicy,
  });
  const history = useQuery({
    queryKey: ["issue-history"],
    queryFn: listIssueHistory,
    enabled: view === "history",
  });
  const pendingOperations = useQuery({
    queryKey: ["issue-pending-operations"],
    queryFn: () => {
      const handler = issueApi.listPendingIssueOperations;
      return handler ? handler() : Promise.resolve<PendingIssueOperationView[]>([]);
    },
    enabled: view === "issues" || view === "history",
    refetchInterval: view === "issues" ? 5_000 : false,
  });
  const historyIssue = useQuery({
    queryKey: ["issue-detail", historyIssueId],
    queryFn: () => {
      if (historyIssueId === null) throw new Error("Issue ID is required for the history detail.");
      return getIssueDetail(historyIssueId);
    },
    enabled: historyIssueId !== null,
  });
  const ownershipDiagnostics = useQuery({
    queryKey: ["issue-ownership-diagnostics", issueDetail?.id ?? historyIssueId],
    queryFn: () =>
      issueApi.listIssueOwnershipDiagnostics(issueDetail?.id ?? historyIssueId ?? undefined),
    enabled: issueDetail !== null || historyIssueId !== null,
  });
  const evidence = useQuery({
    queryKey: ["issue-evidence", selected?.id],
    queryFn: () => (selected ? getIssueEvidence(selected.id) : Promise.resolve([])),
    enabled: (dialog === "evidence" || dialog === "accept") && !!selected,
  });
  const duplicates = useQuery({
    queryKey: ["issue-duplicates", selected?.id],
    queryFn: () => (selected ? getIssueDuplicates(selected.id) : Promise.resolve([])),
    enabled: dialog === "duplicates" && !!selected,
  });
  const resolve = useMutation({
    mutationFn: resolveIssueReview,
    onSuccess: async (_, request) => {
      setDialog(null);
      setSelected(null);
      if (
        request.decision === "reject" ||
        request.decision === "false_positive" ||
        request.decision === "cancel"
      )
        setRejectedCandidate(request.candidate_id);
      await invalidateIssueQueries(queryClient, "all");
    },
  });
  const update = useMutation({
    mutationFn: updateIssueCandidate,
    onSuccess: async () => {
      setEditor(false);
      setSelected(null);
      await invalidateIssueQueries(queryClient, "review");
    },
  });
  const explicit = useMutation({
    mutationFn: createExplicitIssueFromCandidate,
    onSuccess: async () => {
      setExplicitDialog(false);
      setSelected(null);
      await invalidateIssueQueries(queryClient, "all");
    },
  });
  const transition = useMutation({
    mutationFn: transitionIssueState,
    onSuccess: async () => {
      setIssueDetail(null);
      setHistoryIssueId(null);
      await invalidateIssueQueries(queryClient, "inventory");
    },
  });
  const refreshConfirmedIssue = useCallback(
    async (status: IssueOperationStatus) => {
      setHistoryRefresh((previous) => ({
        issueId: status.issue_id,
        token: previous.token + 1,
      }));
      await Promise.all([
        invalidateIssueQueries(queryClient, "all"),
        queryClient.invalidateQueries({ queryKey: ["issue-detail", status.issue_id] }),
        queryClient.invalidateQueries({ queryKey: ["issue-pending-operations"] }),
        queryClient.invalidateQueries({
          queryKey: ["issue-ownership-diagnostics", status.issue_id],
        }),
      ]);
      try {
        const current = await getIssueDetail(status.issue_id);
        setIssueDetail((previous) => (previous?.id === status.issue_id ? current : previous));
      } catch (error) {
        console.error("get_issue_detail refresh failed", error);
      }
    },
    [queryClient],
  );
  const refreshIssueAfterError = useCallback(
    async (issueId: string) => {
      await queryClient.invalidateQueries({ queryKey: ["issue-detail", issueId] });
      try {
        const current = await getIssueDetail(issueId);
        setIssueDetail((previous) => (previous?.id === issueId ? current : previous));
      } catch (error) {
        console.error("get_issue_detail conflict refresh failed", error);
      }
    },
    [queryClient],
  );
  const adminAction = useMutation({
    mutationFn: (request: ApplyIssueAdminActionRequest) => {
      const handler = issueApi.applyIssueAdminAction;
      return handler
        ? handler(request)
        : Promise.reject(new Error("Issue admin actions are unavailable"));
    },
    onSuccess: async (status) => {
      setOperationStatus(status);
      await queryClient.invalidateQueries({ queryKey: ["issue-pending-operations"] });
      if (operationStatusIsSettled(status)) {
        await refreshConfirmedIssue(status);
      }
    },
  });
  const operationResolution = useMutation({
    mutationFn: (request: ResolveIssueOperationRequest) => {
      const handler = issueApi.resolveIssueOperation;
      return handler
        ? handler(request)
        : Promise.reject(new Error("Issue operation resolution is unavailable"));
    },
    onSuccess: async (status) => {
      setOperationStatus(status);
      await queryClient.invalidateQueries({ queryKey: ["issue-pending-operations"] });
      if (operationStatusIsSettled(status)) {
        await refreshConfirmedIssue(status);
      }
    },
  });
  const retryPendingOperation = useMutation({
    mutationFn: (operationKey: string) => {
      const handler = issueApi.retryPendingIssueOperation;
      return handler
        ? handler({ operation_key: operationKey })
        : Promise.reject(new Error("Pending operation replay is unavailable"));
    },
    onSuccess: async (status) => {
      setOperationStatus(status);
      await queryClient.invalidateQueries({ queryKey: ["issue-pending-operations"] });
      if (operationStatusIsSettled(status)) {
        await refreshConfirmedIssue(status);
      }
    },
  });
  const operationPoll = useQuery({
    queryKey: ["issue-operation-status", operationStatus?.issue_id, operationStatus?.operation_id],
    queryFn: () => {
      const current = operationStatus;
      if (!current?.operation_id) return Promise.reject(new Error("Operation ID is required"));
      return issueApi.getIssueOperationStatus({
        issue_id: current.issue_id,
        operation_id: current.operation_id,
        operation_key: current.operation_key,
      });
    },
    enabled:
      !!operationStatus?.operation_id && ["RECEIVED", "PENDING"].includes(operationStatus.state),
    refetchInterval: 1000,
  });
  useEffect(() => {
    const status = operationPoll.data;
    if (status) {
      setOperationStatus(status);
      if (
        operationStatusIsSettled(status) ||
        (["UNCONFIRMED", "MANUAL_RESOLUTION"].includes(status.state) && !status.resolution_required)
      ) {
        void refreshConfirmedIssue(status);
      }
    }
  }, [operationPoll.data, refreshConfirmedIssue]);
  const recreateCanceled = useMutation({
    mutationFn: createIssueFromCanceled,
    onSuccess: async (created) => {
      setHistoryIssueId(null);
      setIssueDetail(created);
      await invalidateIssueQueries(queryClient, "all");
    },
  });

  const counts = useMemo(() => {
    const items = review.data ?? [];
    return {
      urgent: items.filter(
        (item) =>
          workEligibility.data?.work_eligibility === "REVIEWED_ONLY" &&
          item.review_state === "unreviewed" &&
          ["P0", "P1"].includes(item.priority),
      ).length,
      reviewable: items.length,
    };
  }, [review.data, workEligibility.data]);
  const filteredReviewItems = useMemo(
    () => reviewItems.filter((item) => matchesSourceLabels(item, reviewLabels, reviewLabelMatch)),
    [reviewItems, reviewLabelMatch, reviewLabels],
  );
  const filteredIssueItems = useMemo(
    () =>
      (issues.data ?? []).filter((item) => matchesSourceLabels(item, issueLabels, issueLabelMatch)),
    [issueLabelMatch, issueLabels, issues.data],
  );

  const selectSummary = (next: Exclude<SummaryFilter, null>) => {
    if (summary === next) {
      setSummary(null);
      setFilters(EMPTY_REVIEW_FILTERS);
      return;
    }
    setSummary(next);
    if (next === "urgent") {
      setFilters({
        ...EMPTY_REVIEW_FILTERS,
        target: "unreviewed",
        priorities: ["P0", "P1"],
      });
    }
    if (next === "reviewable") {
      setFilters(EMPTY_REVIEW_FILTERS);
    }
  };
  const changeFilter = (patch: Partial<ListIssueReviewRequest>) => {
    setSummary(null);
    setFilters((current) => ({ ...current, ...patch, priorities: [] }));
  };
  const clearFilters = () => {
    setSummary(null);
    setFilters(EMPTY_REVIEW_FILTERS);
    setReviewLabels([]);
  };
  const openEvidenceSource = async (item: IssueEvidence) => {
    setEvidenceThread({
      thread: synthesizeThreadSummary({ id: item.thread_id }),
      highlight: await resolveThreadHighlight(item.thread_id, item.memory_id),
    });
  };
  const inspectIssueOperation = (issueId: string, operationId: string) => {
    const handler = issueApi.getIssueOperationStatus;
    if (!handler) return;
    void handler({ issue_id: issueId, operation_id: operationId })
      .then(setOperationStatus)
      .catch((error) => console.error("get_issue_operation_status failed", error));
  };
  const issueDetailDialogSharedProps: IssueDetailDialogSharedProps = {
    busy:
      transition.isPending ||
      recreateCanceled.isPending ||
      adminAction.isPending ||
      operationResolution.isPending,
    error:
      transition.error ?? recreateCanceled.error ?? adminAction.error ?? operationResolution.error,
    onTransition: (request) => transition.mutate(request),
    onCreateFromCanceled: (request) => recreateCanceled.mutate(request),
    onAdminAction: (request) => adminAction.mutate(request),
    diagnostics: ownershipDiagnostics.data ?? [],
    onResolveOperation: (request) => operationResolution.mutate(request),
    onInspectOperation: inspectIssueOperation,
    onRefreshAfterError: refreshIssueAfterError,
    historyRefreshIssueId: historyRefresh.issueId,
    historyRefreshToken: historyRefresh.token,
  };

  return (
    <div className="content issues-page">
      <header className="issues-header">
        <div>
          <h1>{t("issues.title")}</h1>
          <p>{t("issues.description")}</p>
        </div>
      </header>
      <nav className="issues-tabs" aria-label={t("issues.title")}>
        {(["review", "issues", "history"] as const).map((entry) => (
          <button
            key={entry}
            type="button"
            className={view === entry ? "active" : ""}
            onClick={() => setView(entry)}
          >
            {t(`issues.view.${entry}`)}
          </button>
        ))}
      </nav>

      {view === "review" && (
        <>
          <section className="issues-summary" aria-label={t("issues.review.summary")}>
            {counts.urgent > 0 && (
              <SummaryCard
                selected={summary === "urgent"}
                onClick={() => selectSummary("urgent")}
                title={t("issues.summary.urgent")}
                count={counts.urgent}
                detail={t("issues.summary.urgentDetail")}
              />
            )}
            <SummaryCard
              selected={summary === "reviewable"}
              onClick={() => selectSummary("reviewable")}
              title={t("issues.summary.reviewable")}
              count={counts.reviewable}
              detail={t("issues.summary.reviewableDetail")}
            />
          </section>
          {!selected ? (
            <section className="issues-panel">
              <div className="issues-panel-head">
                <h2>{t("issues.review.choose")}</h2>
                <div className="issues-filters">
                  <label>
                    {t("issues.filter.target")}
                    <select
                      value={filters.target ?? "all"}
                      onChange={(event) =>
                        changeFilter({
                          target: event.target.value as ListIssueReviewRequest["target"],
                        })
                      }
                    >
                      <option value="all">{t("issues.filter.all")}</option>
                      <option value="candidate">{t("issues.candidate")}</option>
                      <option value="unreviewed">{t("issues.unreviewed")}</option>
                    </select>
                  </label>
                  <label>
                    優先度
                    <select
                      value={filters.priority ?? "all"}
                      onChange={(event) =>
                        changeFilter({
                          priority: event.target.value as ListIssueReviewRequest["priority"],
                        })
                      }
                    >
                      <option value="all">すべて</option>
                      {["P0", "P1", "P2", "P3", "P4"].map((value) => (
                        <option key={value} value={value}>
                          {value}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label>
                    最低確信度
                    <input
                      type="number"
                      min="0"
                      max="1"
                      step="0.05"
                      value={filters.min_confidence ?? ""}
                      onChange={(event) =>
                        changeFilter({
                          min_confidence:
                            event.target.value === "" ? null : Number(event.target.value),
                        })
                      }
                    />
                  </label>
                  <label>
                    検出由来
                    <select
                      value={filters.detection ?? "all"}
                      onChange={(event) =>
                        changeFilter({
                          detection: event.target.value as ListIssueReviewRequest["detection"],
                        })
                      }
                    >
                      <option value="all">すべて</option>
                      <option value="explicit">明示</option>
                      <option value="inferred">推定</option>
                    </select>
                  </label>
                  <button type="button" className="btn" onClick={clearFilters}>
                    {t("issues.filter.clear")}
                  </button>
                </div>
              </div>
              <div className="issues-label-filter-row">
                <LabelFilter
                  labels={uniqueSourceLabels([
                    ...reviewItems.flatMap((item) => item.source_labels),
                    ...reviewLabels,
                  ]).map((label) => ({ label }))}
                  selected={reviewLabels}
                  match={reviewLabelMatch}
                  onToggle={toggleReviewLabel}
                  onToggleMany={toggleManyReviewLabels}
                  onSetMatch={setReviewLabelMatch}
                  onClear={() => setReviewLabels([])}
                />
              </div>
              {review.isError ? (
                <Empty text={`要確認を取得できません: ${review.error.message}`} />
              ) : review.isPending ? (
                <Empty text={t("common.loading")} />
              ) : filteredReviewItems.length ? (
                <ReviewTable
                  items={filteredReviewItems}
                  onOpen={setSelected}
                  activeLabels={reviewLabels}
                  onToggleLabel={toggleReviewLabel}
                />
              ) : (
                <Empty text={t("issues.empty.review")} />
              )}
            </section>
          ) : (
            <ReviewDetail
              item={selected}
              onBack={() => setSelected(null)}
              onDialog={setDialog}
              onEdit={() => setEditor(true)}
              onCreate={() => setExplicitDialog(true)}
            />
          )}
        </>
      )}

      {view === "issues" && (
        <>
          <PendingOperations
            items={pendingOperations.data ?? []}
            loading={pendingOperations.isPending}
            error={pendingOperations.isError ? pendingOperations.error.message : null}
            retrying={retryPendingOperation.isPending}
            retryError={retryPendingOperation.error}
            result={operationStatus}
            onRetry={(operationKey) => retryPendingOperation.mutate(operationKey)}
            onOpenIssue={(issueId) => {
              setOperationStatus(null);
              setHistoryIssueId(issueId);
            }}
          />
          <IssueInventory
            items={filteredIssueItems}
            labelItems={issues.data ?? []}
            loading={issues.isPending}
            error={issues.isError ? issues.error.message : null}
            board={board}
            onBoard={setBoard}
            states={issueStates}
            onStates={setIssueStates}
            onOpen={(item) => {
              setOperationStatus(null);
              setIssueDetail(item);
            }}
            labels={issueLabels}
            onLabels={setIssueLabels}
            labelMatch={issueLabelMatch}
            onLabelMatch={setIssueLabelMatch}
            onToggleLabel={toggleIssueLabel}
            onToggleManyLabels={toggleManyIssueLabels}
          />
        </>
      )}
      {view === "history" && (
        <History
          entries={history.data?.entries ?? []}
          truncated={history.data?.truncated ?? false}
          loading={history.isPending}
          error={history.isError ? history.error.message : null}
          onOpenIssue={(issueId) => {
            setOperationStatus(null);
            setHistoryIssueId(issueId);
          }}
        />
      )}
      {historyIssueId && historyIssue.isPending && (
        <Modal onClose={() => setHistoryIssueId(null)} ariaLabel="Issue 詳細">
          <div className="modal-body">読み込み中…</div>
        </Modal>
      )}
      {historyIssueId && historyIssue.isError && (
        <Modal onClose={() => setHistoryIssueId(null)} ariaLabel="Issue 詳細">
          <div className="modal-body">
            <p className="issues-error">Issue 詳細を取得できません: {historyIssue.error.message}</p>
          </div>
        </Modal>
      )}
      {historyIssueId && historyIssue.data && (
        <IssueDetailDialog
          {...issueDetailDialogSharedProps}
          item={historyIssue.data}
          onClose={() => setHistoryIssueId(null)}
          operationStatus={
            operationStatus?.issue_id === historyIssue.data.id ? operationStatus : null
          }
        />
      )}
      {dialog === "evidence" && selected && (
        <EvidenceDialog
          items={evidence.data ?? []}
          loading={evidence.isPending}
          error={evidence.isError ? evidence.error.message : null}
          onClose={() => setDialog(null)}
          onOpenSource={openEvidenceSource}
        />
      )}
      {evidenceThread && (
        <ThreadDetail
          thread={evidenceThread.thread}
          highlight={evidenceThread.highlight}
          onClose={() => setEvidenceThread(null)}
        />
      )}
      {dialog === "duplicates" && selected && (
        <DuplicatesDialog
          source={selected}
          items={duplicates.data ?? []}
          loading={duplicates.isPending}
          error={duplicates.isError ? duplicates.error.message : null}
          onClose={() => setDialog(null)}
        />
      )}
      {dialog === "accept" && selected && (
        <AcceptDialog
          item={selected}
          busy={resolve.isPending || evidence.isPending}
          error={resolve.error}
          requireHighPriorityReason={reviewPolicy.data?.require_high_priority_reason ?? false}
          sourceAvailable={
            (evidence.data ?? []).length > 0 &&
            (evidence.data ?? []).every((item) => item.availability === "available")
          }
          allowMerge={selected.review_state === "candidate"}
          mutationAllowed={
            selected.integrity === "normal" && selected.resolution_state === "actionable"
          }
          onClose={() => setDialog(null)}
          onResolve={(request) => resolve.mutate(request)}
        />
      )}
      {dialog === "reject" && selected && (
        <RejectDialog
          item={selected}
          busy={resolve.isPending}
          error={resolve.error}
          mutationAllowed={
            selected.integrity === "normal" && selected.resolution_state === "actionable"
          }
          onClose={() => setDialog(null)}
          onResolve={(request) => resolve.mutate(request)}
        />
      )}
      {editor && selected && (
        <CandidateEditDialog
          item={selected}
          busy={update.isPending}
          error={update.error}
          mutationAllowed={
            selected.integrity === "normal" && selected.resolution_state === "actionable"
          }
          onClose={() => setEditor(false)}
          onSave={(request) => update.mutate(request)}
        />
      )}
      {explicitDialog && selected && (
        <ExplicitIssueDialog
          item={selected}
          busy={explicit.isPending}
          error={explicit.error}
          mutationAllowed={
            selected.integrity === "normal" && selected.resolution_state === "actionable"
          }
          onClose={() => setExplicitDialog(false)}
          onCreate={(request) => explicit.mutate(request)}
        />
      )}
      {issueDetail && (
        <IssueDetailDialog
          {...issueDetailDialogSharedProps}
          item={issueDetail}
          onClose={() => setIssueDetail(null)}
          operationStatus={operationStatus}
        />
      )}
      {rejectedCandidateId && (
        <div className="issues-reextract">
          <span>候補を却下しました。内容を修正・再評価する場合だけ、明示的に再抽出できます。</span>
          <button type="button" className="btn" disabled={isReextracting} onClick={reextract}>
            {isReextracting ? "再抽出中…" : "この候補を再抽出"}
          </button>
          {reextractError && <span className="issues-error">{reextractError}</span>}
        </div>
      )}
    </div>
  );
}
