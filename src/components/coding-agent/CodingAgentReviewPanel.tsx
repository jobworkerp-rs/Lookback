import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  codingAgentApplyReviewAction,
  codingAgentAttachRun,
  codingAgentGetReview,
  codingAgentReassess,
} from "@/api";
import { Modal } from "@/components/Modal";
import type {
  CodingAgentReviewActionRequest,
  CodingAgentReviewSnapshot,
  CodingAgentRunStatus,
  CodingAgentSemanticReassessment,
} from "@/types/api";

const reviewKey = (runId: string) => ["coding-agent-review", runId] as const;
const MAX_RATIONALE_BYTES = 2_048;
const MAX_EVIDENCE_SUMMARY_BYTES = 4_096;

function reviewContextKey(snapshot: CodingAgentReviewSnapshot | null | undefined): string | null {
  if (!snapshot) return null;
  const { run, marker } = snapshot;
  return JSON.stringify([
    run.runId,
    run.issueId,
    run.issueRevision,
    run.revision,
    run.executionMode,
    run.purpose,
    run.state,
    run.stage,
    run.profileId,
    run.profileVersion,
    run.currentAttemptId,
    run.autoDone,
    run.evidence.map((reference) => [
      reference.evidenceId,
      reference.attemptId,
      reference.sha256,
      reference.sizeBytes,
      reference.saveConfirmed,
    ]),
    snapshot.issueState,
    snapshot.issueRevision,
    marker.runId,
    marker.fullCommitId,
    marker.evidenceReferences,
  ]);
}

function copyReviewContext(snapshot: CodingAgentReviewSnapshot): CodingAgentReviewSnapshot {
  return {
    ...snapshot,
    run: {
      ...snapshot.run,
      evidence: snapshot.run.evidence.map((reference) => ({ ...reference })),
    },
    marker: {
      ...snapshot.marker,
      evidenceReferences: [...snapshot.marker.evidenceReferences],
    },
  };
}

function snapshotCanBeReviewed(snapshot: CodingAgentReviewSnapshot, run: CodingAgentRunStatus) {
  return (
    snapshot.run.runId === run.runId &&
    snapshot.run.issueId === run.issueId &&
    snapshot.run.state === "awaiting_review" &&
    snapshot.issueState === "awaiting_review" &&
    snapshot.marker.runId === run.runId
  );
}

function validBoundedText(value: string, maximum: number): boolean {
  return (
    value.trim().length > 0 &&
    new TextEncoder().encode(value).byteLength <= maximum &&
    !/\p{Cc}/u.test(value)
  );
}

function isRevisionConflict(error: unknown): boolean {
  const message = error instanceof Error ? error.message : "";
  return /revision|changed|marker|refresh/i.test(message);
}

function isOwnedElsewhere(error: unknown): boolean {
  const message = error instanceof Error ? error.message : "";
  return /owned_by_another_window|another application window|owned by another window/i.test(
    message,
  );
}

export function CodingAgentReviewPanel({ run }: { run: CodingAgentRunStatus }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [action, setAction] = useState<
    CodingAgentReviewActionRequest["action"] | "reassess" | null
  >(null);
  const [reassessment, setReassessment] = useState<CodingAgentSemanticReassessment | null>(null);
  const reassessmentKey = useRef<string | null>(null);
  const [rationale, setRationale] = useState("");
  const [evidenceSummary, setEvidenceSummary] = useState("");
  const [evidenceKind, setEvidenceKind] = useState<"test_result" | "artifact" | "other_evidence">(
    "test_result",
  );
  const [confirmedContext, setConfirmedContext] = useState<CodingAgentReviewSnapshot | null>(null);
  const [busy, setBusy] = useState(false);
  const [stale, setStale] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<string | null>(null);
  const latestRunRef = useRef(run);
  latestRunRef.current = run;
  const reviewQuery = useQuery({
    queryKey: reviewKey(run.runId),
    enabled: run.state === "awaiting_review",
    retry: false,
    queryFn: async () => {
      const owner = await codingAgentAttachRun(run.runId);
      if (owner.runId !== run.runId) throw new Error("run owner response mismatch");
      const current = await codingAgentGetReview(run.runId);
      if (!snapshotCanBeReviewed(current, run)) {
        throw new Error("review context is no longer current");
      }
      return current;
    },
  });
  const review = reviewQuery.data;
  const snapshotMatchesRun = Boolean(review && snapshotCanBeReviewed(review, run));
  const currentReviewKey = reviewContextKey(review);
  const confirmedReviewKey = reviewContextKey(confirmedContext);
  const confirmationMatchesCurrent = Boolean(
    confirmedContext && currentReviewKey && confirmedReviewKey === currentReviewKey,
  );
  useEffect(() => {
    if (confirmedContext && confirmedReviewKey !== currentReviewKey) {
      setConfirmedContext(null);
    }
  }, [confirmedContext, confirmedReviewKey, currentReviewKey]);
  const canSubmit = Boolean(
    action &&
      review &&
      snapshotMatchesRun &&
      !reviewQuery.isFetching &&
      !reviewQuery.isError &&
      !busy &&
      !stale &&
      (action !== "reassess" || review.semanticReassessmentAvailable === true) &&
      confirmationMatchesCurrent &&
      (action === "reassess" || validBoundedText(rationale, MAX_RATIONALE_BYTES)) &&
      (action !== "complete" || validBoundedText(evidenceSummary, MAX_EVIDENCE_SUMMARY_BYTES)),
  );

  const openAction = (nextAction: CodingAgentReviewActionRequest["action"] | "reassess") => {
    if (busy) return;
    setAction(nextAction);
    setRationale("");
    setEvidenceSummary("");
    setEvidenceKind("test_result");
    setConfirmedContext(null);
    setActionError(null);
    reassessmentKey.current = null;
  };

  const applyAction = async () => {
    if (!canSubmit || !action || !review || !confirmedContext) return;
    const queryState = queryClient.getQueryState(reviewKey(run.runId));
    const latestReview = queryClient.getQueryData<CodingAgentReviewSnapshot>(reviewKey(run.runId));
    const confirmedKey = reviewContextKey(confirmedContext);
    const latestReviewKey = reviewContextKey(latestReview);
    if (
      !latestReview ||
      !queryState ||
      queryState.fetchStatus !== "idle" ||
      queryState.status !== "success" ||
      !snapshotCanBeReviewed(latestReview, run) ||
      !confirmedKey ||
      confirmedKey !== latestReviewKey ||
      confirmedKey !== currentReviewKey
    ) {
      setConfirmedContext(null);
      return;
    }
    const submittedContext = copyReviewContext(confirmedContext);
    const request = {
      runId: submittedContext.run.runId,
      expectedRunRevision: submittedContext.run.revision,
      expectedIssueRevision: submittedContext.issueRevision,
      expectedMarker: {
        runId: submittedContext.marker.runId,
        fullCommitId: submittedContext.marker.fullCommitId,
        evidenceReferences: [...submittedContext.marker.evidenceReferences],
      },
      rationale: rationale.trim(),
    };
    setBusy(true);
    setConfirmedContext(null);
    setActionError(null);
    try {
      if (action === "reassess") {
        reassessmentKey.current ??= crypto.randomUUID();
        const result = await codingAgentReassess({
          runId: request.runId,
          expectedRunRevision: request.expectedRunRevision,
          expectedIssueRevision: request.expectedIssueRevision,
          expectedMarker: request.expectedMarker,
          requestKey: reassessmentKey.current,
        });
        const latest = queryClient.getQueryData<CodingAgentReviewSnapshot>(reviewKey(run.runId));
        if (
          latestRunRef.current.runId !== request.runId ||
          reviewContextKey(latest) !== confirmedKey ||
          result.runId !== request.runId ||
          result.attemptId !== submittedContext.run.currentAttemptId ||
          result.expectedRunRevision !== request.expectedRunRevision ||
          result.requestKey !== reassessmentKey.current ||
          result.issueRevision !== request.expectedIssueRevision
        ) {
          setStale(true);
          setAction(null);
          setActionError(t("codingAgent.review.revisionConflict"));
          return;
        }
        setReassessment(result);
        setAction(null);
        setStale(true);
        await Promise.all([
          queryClient.invalidateQueries({ queryKey: ["coding-agent-runs", run.issueId] }),
          queryClient.invalidateQueries({ queryKey: ["issues"] }),
          queryClient.invalidateQueries({ queryKey: ["issue-detail", run.issueId] }),
          queryClient.invalidateQueries({ queryKey: reviewKey(run.runId) }),
        ]);
        return;
      }
      const response = await codingAgentApplyReviewAction({
        ...request,
        action,
        report:
          action === "complete"
            ? { evidence: [{ kind: evidenceKind, summary: evidenceSummary.trim() }] }
            : null,
      });
      const latestAfterAction = queryClient.getQueryData<CodingAgentReviewSnapshot>(
        reviewKey(run.runId),
      );
      if (
        latestRunRef.current.runId !== request.runId ||
        reviewContextKey(latestAfterAction) !== confirmedKey
      ) {
        setAction(null);
        setStale(true);
        setActionError(t("codingAgent.review.revisionConflict"));
        return;
      }
      setOutcome(response.outcome);
      setAction(null);
      setStale(true);
      await queryClient.invalidateQueries({ queryKey: ["coding-agent-runs", run.issueId] });
    } catch (error) {
      setConfirmedContext(null);
      if (isRevisionConflict(error)) {
        setStale(true);
        setAction(null);
        setActionError(t("codingAgent.review.revisionConflict"));
      } else {
        setActionError(t("codingAgent.review.actionUnavailable"));
      }
    } finally {
      setBusy(false);
    }
  };

  const reloadReview = async () => {
    setActionError(null);
    setOutcome(null);
    setConfirmedContext(null);
    const refreshed = await reviewQuery.refetch();
    if (!refreshed.isError && refreshed.data && snapshotCanBeReviewed(refreshed.data, run)) {
      setStale(false);
    }
  };

  return (
    <section aria-label={t("codingAgent.review.title")} className="coding-agent-review">
      <h4>{t("codingAgent.review.title")}</h4>
      {reviewQuery.isPending ? (
        <p role="status">{t("common.loading")}</p>
      ) : reviewQuery.isError || !snapshotMatchesRun || !review ? (
        <div>
          <p role="alert" className="settings-error">
            {reviewQuery.isError && isOwnedElsewhere(reviewQuery.error)
              ? t("codingAgent.review.ownerUnavailable")
              : t("codingAgent.review.unavailable")}
          </p>
          <button type="button" className="btn" onClick={() => void reloadReview()}>
            {t("codingAgent.review.reload")}
          </button>
        </div>
      ) : (
        <>
          <p className="settings-card-desc">{t("codingAgent.review.authoritativeNotice")}</p>
          <dl className="issues-definition">
            <dt>{t("codingAgent.review.runRevision")}</dt>
            <dd>{review.run.revision}</dd>
            <dt>{t("codingAgent.review.issueState")}</dt>
            <dd>{review.issueState}</dd>
            <dt>{t("codingAgent.review.issueRevision")}</dt>
            <dd>{review.issueRevision}</dd>
            <dt>{t("codingAgent.review.commit")}</dt>
            <dd>
              <code>{review.marker.fullCommitId}</code>
            </dd>
          </dl>
          <div>
            <p>{t("codingAgent.review.sealedEvidence")}</p>
            {review.marker.evidenceReferences.length > 0 ? (
              <ul>
                {review.marker.evidenceReferences.map((reference) => (
                  <li key={reference}>
                    <code>{reference}</code>
                  </li>
                ))}
              </ul>
            ) : (
              <p role="status">{t("codingAgent.review.noSealedEvidence")}</p>
            )}
          </div>
          {outcome && (
            <p role="status">
              {outcome === "completed"
                ? t("codingAgent.review.completed")
                : outcome === "correction_requested"
                  ? t("codingAgent.review.correctionRequested")
                  : t("codingAgent.review.actionRecorded")}
            </p>
          )}
          {actionError && (
            <p role="alert" className="settings-error">
              {actionError}
            </p>
          )}
          {reassessment &&
            reassessment.issueRevision === review.issueRevision &&
            reassessment.expectedRunRevision === review.run.revision && (
              <div role="status">
                <strong>{reassessment.assessment.outcome}</strong>
                <p>{reassessment.assessment.reason}</p>
                <p>
                  {t("codingAgent.review.issueRevision")}: {reassessment.issueRevision}
                </p>
                <code>{reassessment.sourceSha256}</code>
                <p>{t("codingAgent.review.reassessmentRecorded")}</p>
              </div>
            )}
          {stale ? (
            <button type="button" className="btn" onClick={() => void reloadReview()}>
              {t("codingAgent.review.reload")}
            </button>
          ) : (
            <div className="settings-row">
              {review.semanticReassessmentAvailable && (
                <button
                  type="button"
                  className="btn"
                  disabled={busy}
                  onClick={() => openAction("reassess")}
                >
                  {t("codingAgent.review.reassess")}
                </button>
              )}
              <button
                type="button"
                className="btn primary"
                disabled={busy}
                onClick={() => openAction("complete")}
              >
                {t("codingAgent.review.complete")}
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy}
                onClick={() => openAction("request_changes")}
              >
                {t("codingAgent.review.requestChanges")}
              </button>
            </div>
          )}
        </>
      )}

      {action && review && (
        <Modal
          onClose={() => !busy && setAction(null)}
          ariaLabel={t("codingAgent.review.confirmation")}
        >
          <div className="modal-head">
            <div className="modal-title">{t("codingAgent.review.confirmation")}</div>
          </div>
          <div className="modal-body" style={{ display: "grid", gap: 8 }}>
            <p>{t("codingAgent.review.confirmContext")}</p>
            {action === "reassess" && (
              <>
                <p>{t("codingAgent.review.reassessmentCost")}</p>
                <p>
                  {t(
                    review.run.autoDone
                      ? "codingAgent.review.reassessmentAutoDone"
                      : "codingAgent.review.reassessmentManualDone",
                  )}
                </p>
              </>
            )}
            <p>
              {t("codingAgent.review.issueRevision")}: {review.issueRevision} ·{" "}
              {t("codingAgent.review.runRevision")}: {review.run.revision}
            </p>
            <code>{review.marker.fullCommitId}</code>
            {action !== "reassess" && (
              <label>
                {t("codingAgent.review.rationale")}
                <textarea
                  maxLength={MAX_RATIONALE_BYTES}
                  value={rationale}
                  onChange={(event) => setRationale(event.currentTarget.value)}
                />
              </label>
            )}
            {action === "complete" && (
              <>
                <label>
                  {t("codingAgent.review.evidenceKind")}
                  <select
                    value={evidenceKind}
                    onChange={(event) =>
                      setEvidenceKind(
                        event.currentTarget.value as "test_result" | "artifact" | "other_evidence",
                      )
                    }
                  >
                    <option value="test_result">{t("codingAgent.review.testResult")}</option>
                    <option value="artifact">{t("codingAgent.review.artifact")}</option>
                    <option value="other_evidence">{t("codingAgent.review.otherEvidence")}</option>
                  </select>
                </label>
                <label>
                  {t("codingAgent.review.evidenceSummary")}
                  <textarea
                    maxLength={MAX_EVIDENCE_SUMMARY_BYTES}
                    value={evidenceSummary}
                    onChange={(event) => setEvidenceSummary(event.currentTarget.value)}
                  />
                </label>
              </>
            )}
            <label>
              <input
                type="checkbox"
                checked={confirmationMatchesCurrent}
                disabled={
                  busy || reviewQuery.isFetching || reviewQuery.isError || !snapshotMatchesRun
                }
                onChange={(event) => {
                  if (!event.currentTarget.checked) {
                    setConfirmedContext(null);
                    return;
                  }
                  const cachedReview = queryClient.getQueryData<CodingAgentReviewSnapshot>(
                    reviewKey(run.runId),
                  );
                  const cachedQueryState = queryClient.getQueryState(reviewKey(run.runId));
                  if (
                    review &&
                    cachedReview &&
                    cachedQueryState?.fetchStatus === "idle" &&
                    cachedQueryState.status === "success" &&
                    reviewContextKey(cachedReview) === currentReviewKey &&
                    snapshotCanBeReviewed(cachedReview, run)
                  ) {
                    setConfirmedContext(copyReviewContext(cachedReview));
                  } else {
                    setConfirmedContext(null);
                  }
                }}
              />{" "}
              {t("codingAgent.review.acknowledge")}
            </label>
          </div>
          <div className="modal-foot">
            <button type="button" className="btn" disabled={busy} onClick={() => setAction(null)}>
              {t("common.cancel")}
            </button>
            <button
              type="button"
              className="btn primary"
              disabled={!canSubmit}
              onClick={() => void applyAction()}
            >
              {busy ? t("codingAgent.review.applying") : t("codingAgent.review.confirmAction")}
            </button>
          </div>
        </Modal>
      )}
    </section>
  );
}
