import { useState } from "react";
import { Modal } from "@/components/Modal";
import type {
  CreateExplicitIssueRequest,
  IssueDuplicate,
  IssueEvidence,
  IssueReviewItem,
  ResolveIssueReviewRequest,
  UpdateIssueCandidateRequest,
} from "@/types/api";

export function EvidenceDialog({
  items,
  loading,
  error,
  onClose,
  onOpenSource,
}: {
  items: IssueEvidence[];
  loading: boolean;
  error: string | null;
  onClose: () => void;
  onOpenSource: (item: IssueEvidence) => void;
}) {
  const [expanded, setExpanded] = useState<string | null>(null);
  return (
    <Modal onClose={onClose} ariaLabel="根拠">
      <div className="modal-body">
        <h2>根拠</h2>
        {error ? (
          <p className="issues-error">根拠を取得できません: {error}</p>
        ) : loading ? (
          <p>読み込み中…</p>
        ) : items.length ? (
          items.map((item) => (
            <article className="issues-evidence" key={`${item.thread_id}:${item.memory_id}`}>
              <button
                type="button"
                className="issue-link"
                onClick={() =>
                  setExpanded((current) =>
                    current === `${item.thread_id}:${item.memory_id}`
                      ? null
                      : `${item.thread_id}:${item.memory_id}`,
                  )
                }
              >
                Thread {item.thread_id} / Message {item.memory_id}
              </button>
              {expanded === `${item.thread_id}:${item.memory_id}` && (
                <p>{item.excerpt ?? "抜粋は利用できません。"}</p>
              )}
              {item.availability === "available" ? (
                <button type="button" className="btn" onClick={() => onOpenSource(item)}>
                  原文を開く
                </button>
              ) : (
                <p>原文への参照は利用できません。</p>
              )}
              {item.availability === "unavailable" && (
                <span className="issue-badge stale">
                  原文を開けません: {item.unavailable_reason}
                </span>
              )}
            </article>
          ))
        ) : (
          <p>表示できる根拠はありません。</p>
        )}
      </div>
    </Modal>
  );
}

export function DuplicatesDialog({
  source,
  items,
  loading,
  error,
  onClose,
}: {
  source: IssueReviewItem;
  items: IssueDuplicate[];
  loading: boolean;
  error: string | null;
  onClose: () => void;
}) {
  return (
    <Modal onClose={onClose} ariaLabel="重複候補">
      <div className="modal-body">
        <h2>重複候補</h2>
        <article className="issues-evidence">
          <strong>比較元: {source.title}</strong>
          <p>{source.description}</p>
          <small>
            状態: {source.issue_state ?? "候補"} · 根拠 {source.evidence_count} 件
          </small>
          <p>完了条件: {source.acceptance_criteria.join(" / ") || "—"}</p>
        </article>
        {error ? (
          <p className="issues-error">重複候補を取得できません: {error}</p>
        ) : loading ? (
          <p>読み込み中…</p>
        ) : (
          items.map((item) => (
            <article className="issues-evidence" key={item.id}>
              <strong>{item.title}</strong>
              <p>{item.description}</p>
              <small>
                {item.state} · 根拠 {item.evidence_count} 件
              </small>
              <p>完了条件: {item.completion_criteria.join(" / ") || "—"}</p>
            </article>
          ))
        )}
      </div>
    </Modal>
  );
}

export function AcceptDialog({
  item,
  busy,
  error,
  requireHighPriorityReason,
  sourceAvailable,
  allowMerge,
  mutationAllowed,
  onClose,
  onResolve,
}: {
  item: IssueReviewItem;
  busy: boolean;
  error: Error | null;
  requireHighPriorityReason: boolean;
  sourceAvailable: boolean;
  allowMerge: boolean;
  mutationAllowed: boolean;
  onClose: () => void;
  onResolve: (request: ResolveIssueReviewRequest) => void;
}) {
  const [decision, setDecision] = useState<"confirm" | "merge" | "human_review" | "human_merge">(
    "confirm",
  );
  const [rationale, setRationale] = useState("");
  const [mergeTargetId, setMergeTargetId] = useState("");
  const [mergeTargetType, setMergeTargetType] = useState<"candidate" | "issue">("issue");
  const requiresReason =
    decision === "human_review" ||
    decision === "human_merge" ||
    (requireHighPriorityReason && decision === "confirm" && ["P0", "P1"].includes(item.priority));
  const valid =
    (!requiresReason || rationale.trim().length > 0) &&
    ((decision !== "merge" && decision !== "human_merge") || mergeTargetId.trim().length > 0) &&
    (sourceAvailable || decision === "human_review" || decision === "human_merge") &&
    mutationAllowed;
  return (
    <Modal onClose={onClose} ariaLabel="採用する">
      <div className="modal-body">
        <h2>採用する</h2>
        {sourceAvailable ? (
          <fieldset className="issues-form-label">
            <legend>採用方法</legend>
            <label>
              <input
                type="radio"
                name="accept-decision"
                checked={decision === "confirm"}
                onChange={() => setDecision("confirm")}
              />
              確認して採用
            </label>
            {allowMerge && (
              <label>
                <input
                  type="radio"
                  name="accept-decision"
                  checked={decision === "merge"}
                  onChange={() => setDecision("merge")}
                />
                既存項目へ統合
              </label>
            )}
            <label>
              <input
                type="radio"
                name="accept-decision"
                checked={decision === "human_review"}
                onChange={() => setDecision("human_review")}
              />
              人為確認で採用
            </label>
            {allowMerge && (
              <label>
                <input
                  type="radio"
                  name="accept-decision"
                  checked={decision === "human_merge"}
                  onChange={() => setDecision("human_merge")}
                />
                人為確認で統合
              </label>
            )}
          </fieldset>
        ) : (
          <fieldset className="issues-form-label">
            <legend>原文参照を開けないため、人為確認が必要です</legend>
            <label>
              <input
                type="radio"
                name="accept-decision"
                checked={decision === "human_review"}
                onChange={() => setDecision("human_review")}
              />
              人為確認で採用
            </label>
            {allowMerge && (
              <label>
                <input
                  type="radio"
                  name="accept-decision"
                  checked={decision === "human_merge"}
                  onChange={() => setDecision("human_merge")}
                />
                人為確認で統合
              </label>
            )}
          </fieldset>
        )}
        {(decision === "merge" || decision === "human_merge") && (
          <>
            <label className="issues-form-label">
              統合先
              <select
                value={mergeTargetType}
                onChange={(event) =>
                  setMergeTargetType(event.target.value as "candidate" | "issue")
                }
              >
                <option value="issue">Issue</option>
                <option value="candidate">候補</option>
              </select>
            </label>
            <label className="issues-form-label">
              統合先 ID
              <input
                value={mergeTargetId}
                onChange={(event) => setMergeTargetId(event.target.value)}
              />
            </label>
          </>
        )}
        <label className="issues-form-label">
          理由{requiresReason && <span className="required">（必須）</span>}
          <textarea value={rationale} onChange={(event) => setRationale(event.target.value)} />
        </label>
        {error && <p className="issues-error">{error.message}</p>}
        <div className="modal-actions">
          <button type="button" className="btn" onClick={onClose}>
            キャンセル
          </button>
          <button
            type="button"
            className="btn primary"
            disabled={!valid || busy || !mutationAllowed}
            onClick={() =>
              onResolve({
                candidate_id: item.id,
                expected_revision: item.revision,
                ...((item.associated_issue_id ?? item.issue_id)
                  ? { associated_issue_id: item.associated_issue_id ?? item.issue_id }
                  : {}),
                decision,
                rationale: rationale.trim() || null,
                merge_target_id:
                  decision === "merge" || decision === "human_merge" ? mergeTargetId.trim() : null,
                merge_target_type:
                  decision === "merge" || decision === "human_merge" ? mergeTargetType : null,
              })
            }
          >
            {busy ? "処理中…" : "確定"}
          </button>
        </div>
      </div>
    </Modal>
  );
}

export function RejectDialog({
  item,
  busy,
  error,
  mutationAllowed,
  onClose,
  onResolve,
}: {
  item: IssueReviewItem;
  busy: boolean;
  error: Error | null;
  mutationAllowed: boolean;
  onClose: () => void;
  onResolve: (request: ResolveIssueReviewRequest) => void;
}) {
  const [reason, setReason] = useState<"false_positive" | "completed" | "other">("false_positive");
  const [rationale, setRationale] = useState("");
  const requiresRationale = reason === "other";
  const valid = mutationAllowed && (!requiresRationale || rationale.trim().length > 0);
  const associatedIssueId = item.associated_issue_id ?? item.issue_id ?? null;
  const decision: ResolveIssueReviewRequest["decision"] = associatedIssueId
    ? reason === "false_positive"
      ? "false_positive"
      : "cancel"
    : "reject";
  return (
    <Modal onClose={onClose} ariaLabel="却下する">
      <div className="modal-body">
        <h2>却下する</h2>
        <fieldset className="issues-form-label">
          <legend>却下理由</legend>
          <label>
            <input
              type="radio"
              name="rejection-reason"
              checked={reason === "false_positive"}
              onChange={() => setReason("false_positive")}
            />
            誤検出
          </label>
          <label>
            <input
              type="radio"
              name="rejection-reason"
              checked={reason === "completed"}
              onChange={() => setReason("completed")}
            />
            完了済
          </label>
          <label>
            <input
              type="radio"
              name="rejection-reason"
              checked={reason === "other"}
              onChange={() => setReason("other")}
            />
            その他
          </label>
        </fieldset>
        {requiresRationale && (
          <label className="issues-form-label">
            理由<span className="required">（必須）</span>
            <textarea value={rationale} onChange={(event) => setRationale(event.target.value)} />
          </label>
        )}
        {error && <p className="issues-error">{error.message}</p>}
        <div className="modal-actions">
          <button type="button" className="btn" onClick={onClose}>
            キャンセル
          </button>
          <button
            type="button"
            className="btn primary"
            disabled={!valid || busy || !mutationAllowed}
            onClick={() =>
              onResolve({
                candidate_id: item.id,
                expected_revision: item.revision,
                ...(associatedIssueId ? { associated_issue_id: associatedIssueId } : {}),
                decision,
                rejection_reason: reason,
                rationale: rationale.trim() || null,
              })
            }
          >
            {busy ? "処理中…" : "確定"}
          </button>
        </div>
      </div>
    </Modal>
  );
}

export function CandidateEditDialog({
  item,
  busy,
  error,
  mutationAllowed,
  onClose,
  onSave,
}: {
  item: IssueReviewItem;
  busy: boolean;
  error: Error | null;
  mutationAllowed: boolean;
  onClose: () => void;
  onSave: (request: UpdateIssueCandidateRequest) => void;
}) {
  const [title, setTitle] = useState(item.title);
  const [description, setDescription] = useState(item.description);
  const [kind, setKind] = useState(item.kind);
  const [priority, setPriority] = useState(item.priority);
  const [suggestedState, setSuggestedState] = useState(item.suggested_state);
  const [acceptanceCriteria, setAcceptanceCriteria] = useState(item.acceptance_criteria.join("\n"));
  const valid = mutationAllowed && title.trim().length > 0 && description.trim().length > 0;
  return (
    <Modal onClose={onClose} ariaLabel="候補を編集">
      <div className="modal-body">
        <h2>候補を編集</h2>
        <label className="issues-form-label">
          タイトル
          <input value={title} onChange={(event) => setTitle(event.target.value)} />
        </label>
        <label className="issues-form-label">
          説明
          <textarea value={description} onChange={(event) => setDescription(event.target.value)} />
        </label>
        <label className="issues-form-label">
          種別
          <select value={kind} onChange={(event) => setKind(event.target.value)}>
            <option value="TASK">TASK</option>
            <option value="BUG">BUG</option>
            <option value="RESEARCH">RESEARCH</option>
            <option value="DECISION">DECISION</option>
          </select>
        </label>
        <label className="issues-form-label">
          優先度
          <select
            value={priority}
            onChange={(event) => setPriority(event.target.value as IssueReviewItem["priority"])}
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
              setSuggestedState(event.target.value as IssueReviewItem["suggested_state"])
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
        {error && <p className="issues-error">{error.message}</p>}
        <div className="modal-actions">
          <button type="button" className="btn" onClick={onClose}>
            キャンセル
          </button>
          <button
            type="button"
            className="btn primary"
            disabled={!valid || busy || !mutationAllowed}
            onClick={() =>
              onSave({
                candidate_id: item.id,
                expected_revision: item.revision,
                title: title.trim(),
                description: description.trim(),
                kind,
                priority,
                suggested_state: suggestedState,
                acceptance_criteria: acceptanceCriteria
                  .split("\n")
                  .map((criterion) => criterion.trim())
                  .filter(Boolean),
              })
            }
          >
            {busy ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
    </Modal>
  );
}

export function ExplicitIssueDialog({
  item,
  busy,
  error,
  mutationAllowed,
  onClose,
  onCreate,
}: {
  item: IssueReviewItem;
  busy: boolean;
  error: Error | null;
  mutationAllowed: boolean;
  onClose: () => void;
  onCreate: (request: CreateExplicitIssueRequest) => void;
}) {
  const [rationale, setRationale] = useState("");
  const [title, setTitle] = useState(item.title);
  const [description, setDescription] = useState(item.description);
  const [kind, setKind] = useState(item.kind);
  const [priority, setPriority] = useState(item.priority);
  const [suggestedState, setSuggestedState] = useState(item.suggested_state);
  const [acceptanceCriteria, setAcceptanceCriteria] = useState(item.acceptance_criteria.join("\n"));
  return (
    <Modal onClose={onClose} ariaLabel="新しい Issue に置換">
      <div className="modal-body">
        <h2>明示 Issue を作成</h2>
        <p className="issues-help">
          内容に責任を持って明示 Issue を作成します。既存 Issue
          がある場合は関連付けと取消を続け、後続操作が失敗しても新 Issue は保持されます。
        </p>
        <label className="issues-form-label">
          理由<span className="required">（必須）</span>
          <textarea value={rationale} onChange={(event) => setRationale(event.target.value)} />
        </label>
        <label className="issues-form-label">
          タイトル
          <input value={title} onChange={(event) => setTitle(event.target.value)} />
        </label>
        <label className="issues-form-label">
          説明
          <textarea value={description} onChange={(event) => setDescription(event.target.value)} />
        </label>
        <label className="issues-form-label">
          種別
          <select value={kind} onChange={(event) => setKind(event.target.value)}>
            <option value="TASK">TASK</option>
            <option value="BUG">BUG</option>
            <option value="RESEARCH">RESEARCH</option>
            <option value="DECISION">DECISION</option>
          </select>
        </label>
        <label className="issues-form-label">
          優先度
          <select
            value={priority}
            onChange={(event) => setPriority(event.target.value as IssueReviewItem["priority"])}
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
              setSuggestedState(event.target.value as IssueReviewItem["suggested_state"])
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
        {error && <p className="issues-error">{error.message}</p>}
        <div className="modal-actions">
          <button type="button" className="btn" onClick={onClose}>
            キャンセル
          </button>
          <button
            type="button"
            className="btn primary"
            disabled={!rationale.trim() || busy || !mutationAllowed}
            onClick={() =>
              onCreate({
                candidate_id: item.id,
                expected_revision: item.revision,
                rationale: rationale.trim(),
                title: title.trim(),
                description: description.trim(),
                kind,
                priority,
                suggested_state: suggestedState,
                acceptance_criteria: acceptanceCriteria
                  .split("\n")
                  .map((value) => value.trim())
                  .filter(Boolean),
              })
            }
          >
            {busy ? "作成中…" : "置換を実行"}
          </button>
        </div>
      </div>
    </Modal>
  );
}
