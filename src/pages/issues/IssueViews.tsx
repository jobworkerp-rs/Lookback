import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { listIssueHistory } from "@/api";
import { LabelFilter } from "@/components/LabelFilter";
import {
  ACTIVE_ISSUE_STATES,
  formatIssueDate,
  reviewIntegrityLabel,
  reviewLabel,
  reviewResolutionLabel,
  splitSourceLabels,
  uniqueSourceLabels,
} from "@/pages/issues/viewModel";
import type {
  IssueListItem,
  IssueOperationStatus,
  IssueReviewItem,
  LabelMatch,
  PendingIssueOperationView,
} from "@/types/api";

export type ReviewDialog = "evidence" | "duplicates" | "accept" | "reject";

export function SummaryCard({
  selected,
  onClick,
  title,
  count,
  detail,
}: {
  selected: boolean;
  onClick: () => void;
  title: string;
  count: number;
  detail: string;
}) {
  return (
    <button
      type="button"
      aria-pressed={selected}
      className={`issues-summary-card${selected ? " selected" : ""}`}
      onClick={onClick}
    >
      <span>{title}</span>
      <strong>{count}</strong>
      <small>{detail}</small>
    </button>
  );
}

export function Empty({ text }: { text: string }) {
  return <div className="issues-empty">{text}</div>;
}

export function SourceLabels({
  labels,
  maxVisible,
  activeLabels,
  onToggleLabel,
}: {
  labels: string[];
  maxVisible?: number;
  activeLabels?: string[];
  onToggleLabel?: (label: string) => void;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const selectable = activeLabels !== undefined && onToggleLabel !== undefined;
  if (!labels.length) return null;
  const { visible, hiddenCount } =
    maxVisible !== undefined && !expanded
      ? splitSourceLabels(labels, maxVisible)
      : { visible: labels, hiddenCount: 0 };
  return (
    <fieldset className="issue-source-labels" aria-label="ソースラベル" title={labels.join(", ")}>
      {visible.map((label) => {
        const active = selectable && (activeLabels ?? []).includes(label);
        return (
          <button
            key={label}
            type="button"
            className="issue-source-label"
            aria-label={
              selectable
                ? t(`issues.labelToggle.${active ? "remove" : "add"}`, { label })
                : `${label} をコピー`
            }
            aria-pressed={selectable ? active : undefined}
            onClick={() => {
              if (selectable) {
                onToggleLabel?.(label);
                return;
              }
              void navigator.clipboard?.writeText(label);
            }}
          >
            {label}
          </button>
        );
      })}
      {hiddenCount > 0 && (
        <button
          type="button"
          className="issue-source-label issue-source-label-more"
          aria-label={t("labelFilter.showMoreLabels", { count: hiddenCount })}
          onClick={() => setExpanded(true)}
        >
          +{hiddenCount}
        </button>
      )}
    </fieldset>
  );
}

export function CopyIdButton({ id }: { id: string }) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      className="issue-copy-id"
      aria-label={`${t("issues.copyId")}: ${id}`}
      title={t("issues.copyId")}
      onClick={() => void navigator.clipboard?.writeText(id)}
    >
      ⧉
    </button>
  );
}

export function ReviewTable({
  items,
  onOpen,
  activeLabels,
  onToggleLabel,
}: {
  items: IssueReviewItem[];
  onOpen: (item: IssueReviewItem) => void;
  activeLabels?: string[];
  onToggleLabel?: (label: string) => void;
}) {
  return (
    <section className="issues-table-wrap issues-review-table-wrap" aria-label="要確認一覧">
      <table className="issues-table">
        <thead>
          <tr>
            <th>項目</th>
            <th>対象・根拠</th>
            <th>優先度</th>
            <th>LLM 確信度</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {items.map((item) => (
            <tr key={item.id}>
              <td>
                <strong>{item.title}</strong>
                <small>{item.description}</small>
                <SourceLabels
                  labels={item.source_labels}
                  maxVisible={2}
                  activeLabels={activeLabels}
                  onToggleLabel={onToggleLabel}
                />
              </td>
              <td>
                <Badges item={item} />
              </td>
              <td className={`issue-priority ${item.priority.toLowerCase()}`}>{item.priority}</td>
              <td>{Math.round(item.confidence * 100)}%</td>
              <td>
                <button type="button" className="btn" onClick={() => onOpen(item)}>
                  レビュー
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

export function Badges({
  item,
}: {
  item:
    | Pick<IssueReviewItem, "review_state" | "integrity" | "resolution_state">
    | Pick<IssueListItem, "review_state">;
}) {
  const reviewItem = "integrity" in item;
  return (
    <span className="issue-badges">
      {reviewLabel(item.review_state) && (
        <span className="issue-badge review">{reviewLabel(item.review_state)}</span>
      )}
      {reviewItem && (
        <span className={`issue-badge ${item.integrity === "normal" ? "review" : "stale"}`}>
          {reviewIntegrityLabel(item.integrity)}
        </span>
      )}
      {reviewItem && (
        <span
          className={`issue-badge ${item.resolution_state === "actionable" ? "review" : "stale"}`}
        >
          {reviewResolutionLabel(item.resolution_state)}
        </span>
      )}
    </span>
  );
}

export function ReviewDetail({
  item,
  onBack,
  onDialog,
  onEdit,
  onCreate,
}: {
  item: IssueReviewItem;
  onBack: () => void;
  onDialog: (value: ReviewDialog) => void;
  onEdit: () => void;
  onCreate: () => void;
}) {
  const mutationAllowed = item.integrity === "normal" && item.resolution_state === "actionable";
  return (
    <section className="issues-panel issues-review-detail">
      <div className="issues-panel-head">
        <button type="button" className="btn" onClick={onBack}>
          ← 要確認へ戻る
        </button>
        <h2>{item.title}</h2>
        <Badges item={item} />
      </div>
      <div className="issues-detail-body">
        <p className="issues-help">
          LLM 確信度は候補らしさの提案であり、内容の正しさや Strong 評価を保証しません。
        </p>
        <dl className="issues-metrics">
          <div>
            <dt>優先度</dt>
            <dd>{item.priority}</dd>
          </div>
          <div>
            <dt>LLM 確信度</dt>
            <dd>{Math.round(item.confidence * 100)}%</dd>
          </div>
          <div>
            <dt>検出由来</dt>
            <dd>{item.detection}</dd>
          </div>
          <div>
            <dt>Strong 評価</dt>
            <dd>{item.strong_proof ? "あり" : "なし"}</dd>
          </div>
          {item.associated_issue_id && (
            <div>
              <dt>関連 Issue</dt>
              <dd>
                {item.associated_issue_id}
                {item.issue_state ? ` (${item.issue_state})` : ""}
              </dd>
            </div>
          )}
        </dl>
        <p className="issues-description">{item.description}</p>
        <SourceLabels labels={item.source_labels} />
        <div className="issues-callout">根拠参照 {item.evidence_count} 件</div>
        {item.integrity !== "normal" && (
          <div className="issues-callout">
            整合性異常（{reviewIntegrityLabel(item.integrity)}）のため候補操作を無効化しています。
            server の整合性回復後に再読み込みしてください。
          </div>
        )}
        {item.resolution_state === "resolution_pending" && (
          <div className="issues-callout">
            {reviewResolutionLabel(item.resolution_state)}。候補操作を無効化しています。
          </div>
        )}
        <div className="issues-detail-actions">
          <button type="button" className="btn" onClick={() => onDialog("evidence")}>
            根拠を見る
          </button>
          {item.duplicate_count > 0 && (
            <button type="button" className="btn" onClick={() => onDialog("duplicates")}>
              重複を比較する
            </button>
          )}
          {item.review_state === "candidate" && (
            <button type="button" className="btn" disabled={!mutationAllowed} onClick={onEdit}>
              候補を編集
            </button>
          )}
          {item.review_state === "candidate" && (
            <button type="button" className="btn" disabled={!mutationAllowed} onClick={onCreate}>
              明示 Issue を作成
            </button>
          )}
          <button
            type="button"
            className="btn primary"
            disabled={!mutationAllowed}
            onClick={() => onDialog("accept")}
          >
            採用する
          </button>
          <button
            type="button"
            className="btn"
            disabled={!mutationAllowed}
            onClick={() => onDialog("reject")}
          >
            却下する
          </button>
        </div>
      </div>
    </section>
  );
}

export function IssueInventory({
  items,
  labelItems,
  loading,
  error,
  board,
  onBoard,
  states,
  onStates,
  onOpen,
  labels,
  onLabels,
  labelMatch,
  onLabelMatch,
  onToggleLabel,
  onToggleManyLabels,
}: {
  items: IssueListItem[];
  labelItems: IssueListItem[];
  loading: boolean;
  error: string | null;
  board: boolean;
  onBoard: (value: boolean) => void;
  states: string[];
  onStates: (states: string[]) => void;
  onOpen: (item: IssueListItem) => void;
  labels: string[];
  onLabels: (labels: string[]) => void;
  labelMatch: LabelMatch;
  onLabelMatch: (match: LabelMatch) => void;
  onToggleLabel: (label: string) => void;
  onToggleManyLabels: (labels: string[], turnOn: boolean) => void;
}) {
  const active = items.filter((item) => ACTIVE_ISSUE_STATES.includes(item.state));
  return (
    <section className="issues-panel issues-inventory-panel">
      <div className="issues-panel-head">
        <h2>Issue 一覧</h2>
        <div className="issues-switch">
          <button type="button" className={!board ? "active" : ""} onClick={() => onBoard(false)}>
            一覧
          </button>
          <button type="button" className={board ? "active" : ""} onClick={() => onBoard(true)}>
            ボード
          </button>
        </div>
        <label className="issues-form-label">
          状態
          <select
            value={states[0] ?? "active"}
            onChange={(event) =>
              onStates(event.target.value === "active" ? [] : [event.target.value])
            }
          >
            <option value="active">活動中</option>
            <option value="DONE">完了済み</option>
            <option value="CANCELED">取消済み</option>
          </select>
        </label>
      </div>
      <div className="issues-label-filter-row">
        <LabelFilter
          labels={uniqueSourceLabels([
            ...labelItems.flatMap((item) => item.source_labels),
            ...labels,
          ]).map((label) => ({ label }))}
          selected={labels}
          match={labelMatch}
          onToggle={onToggleLabel}
          onToggleMany={onToggleManyLabels}
          onSetMatch={onLabelMatch}
          onClear={() => onLabels([])}
        />
      </div>
      {error ? (
        <Empty text={`Issue 一覧を取得できません: ${error}`} />
      ) : loading ? (
        <Empty text="読み込み中…" />
      ) : board ? (
        <Board items={active} onOpen={onOpen} activeLabels={labels} onToggleLabel={onToggleLabel} />
      ) : (
        <IssueTable
          items={states.length ? items : active}
          onOpen={onOpen}
          activeLabels={labels}
          onToggleLabel={onToggleLabel}
        />
      )}
    </section>
  );
}

export function PendingOperations({
  items,
  loading,
  error,
  retrying,
  retryError,
  result,
  onRetry,
  onOpenIssue,
}: {
  items: PendingIssueOperationView[];
  loading: boolean;
  error: string | null;
  retrying: boolean;
  retryError: Error | null;
  result: IssueOperationStatus | null;
  onRetry: (operationKey: string) => void;
  onOpenIssue: (issueId: string) => void;
}) {
  if (!loading && !error && items.length === 0 && !result) return null;
  return (
    <section className="issues-panel issues-pending-panel" aria-label="結果確認待ち">
      <div className="issues-panel-head">
        <h2>結果確認待ち</h2>
      </div>
      {error ? (
        <Empty text={`結果確認待ちの操作を取得できません: ${error}`} />
      ) : loading ? (
        <Empty text="読み込み中…" />
      ) : (
        <>
          <p className="issues-note">
            応答を確認できなかった操作です。同じ保存済み要求を再送して結果を確認できます。
          </p>
          {items.map((item) => (
            <article className="issues-pending-operation" key={item.operation_key}>
              <div>
                <button
                  type="button"
                  className="issue-link issue-id-link"
                  title={item.issue_id}
                  onClick={() => onOpenIssue(item.issue_id)}
                >
                  {item.issue_id}
                </button>
                <small>
                  {item.method} · {formatIssueDate(item.created_at_ms)} · key {item.operation_key}
                </small>
              </div>
              <button
                type="button"
                className="btn"
                disabled={retrying}
                onClick={() => onRetry(item.operation_key)}
              >
                {retrying ? "確認中…" : "同じ要求を再送"}
              </button>
            </article>
          ))}
          {retryError && <p className="issues-error">再送できません: {retryError.message}</p>}
          {result && (
            <div className="issues-callout" role="status">
              操作結果: {result.state}
              {result.operation_id ? ` (${result.operation_id})` : ""}
            </div>
          )}
        </>
      )}
    </section>
  );
}

export function IssueTable({
  items,
  onOpen,
  activeLabels,
  onToggleLabel,
}: {
  items: IssueListItem[];
  onOpen: (item: IssueListItem) => void;
  activeLabels?: string[];
  onToggleLabel?: (label: string) => void;
}) {
  return (
    <section className="issues-table-wrap issues-inventory-table-wrap" aria-label="Issue 一覧">
      <table className="issues-table">
        <thead>
          <tr>
            <th>タイトル</th>
            <th>状態</th>
            <th>優先度</th>
            <th>作成日時</th>
            <th>更新日時</th>
          </tr>
        </thead>
        <tbody>
          {items.map((item) => (
            <tr key={item.id}>
              <td>
                <div className="issue-title-row">
                  <button type="button" className="issue-link" onClick={() => onOpen(item)}>
                    {item.title}
                  </button>
                  <CopyIdButton id={item.id} />
                </div>
                <Badges item={item} />
                <SourceLabels
                  labels={item.source_labels}
                  maxVisible={2}
                  activeLabels={activeLabels}
                  onToggleLabel={onToggleLabel}
                />
              </td>
              <td>{item.state}</td>
              <td className={`issue-priority ${item.priority.toLowerCase()}`}>{item.priority}</td>
              <td>{formatIssueDate(item.created_at_ms)}</td>
              <td>{formatIssueDate(item.updated_at_ms)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

export function Board({
  items,
  onOpen,
  activeLabels,
  onToggleLabel,
}: {
  items: IssueListItem[];
  onOpen: (item: IssueListItem) => void;
  activeLabels?: string[];
  onToggleLabel?: (label: string) => void;
}) {
  return (
    <section className="issues-board" aria-label="Issue 一覧">
      {["OPEN", "IN_PROGRESS", "BLOCKED", "DEFERRED"].map((state) => (
        <section key={state}>
          <h3>{state}</h3>
          {items
            .filter((item) => item.state === state)
            .map((item) => (
              <article className="issue-board-card" key={item.id}>
                <button type="button" className="issue-board-open" onClick={() => onOpen(item)}>
                  <strong>{item.title}</strong>
                  <Badges item={item} />
                  <small>
                    {item.priority} · {formatIssueDate(item.updated_at_ms)}
                  </small>
                </button>
                <SourceLabels
                  labels={item.source_labels}
                  maxVisible={2}
                  activeLabels={activeLabels}
                  onToggleLabel={onToggleLabel}
                />
              </article>
            ))}
        </section>
      ))}
    </section>
  );
}

export function History({
  entries,
  truncated,
  loading,
  error,
  onOpenIssue,
}: {
  entries: Awaited<ReturnType<typeof listIssueHistory>>["entries"];
  truncated: boolean;
  loading: boolean;
  error: string | null;
  onOpenIssue: (issueId: string) => void;
}) {
  return (
    <section className="issues-panel issues-history-panel">
      <div className="issues-panel-head">
        <h2>判断履歴</h2>
      </div>
      {error ? (
        <Empty text={`判断履歴を取得できません: ${error}`} />
      ) : loading ? (
        <Empty text="読み込み中…" />
      ) : entries.length ? (
        <>
          {truncated && (
            <p className="issues-history-notice">履歴が多いため最新1,000件を表示しています</p>
          )}
          <section className="issues-history-list" aria-label="判断履歴一覧">
            {entries.map((entry) => (
              <article className="issues-history" key={entry.id}>
                <time>{formatIssueDate(entry.created_at_ms)}</time>
                <div>
                  <strong>{entry.decision}</strong>
                  <p>{entry.target_title}</p>
                  {entry.issue_ids.map((issueId) => (
                    <button
                      type="button"
                      className="issue-link issue-history-id"
                      title={issueId}
                      key={issueId}
                      aria-label={`Issue ${issueId} の詳細を開く`}
                      onClick={() => onOpenIssue(issueId)}
                    >
                      Issue {issueId}
                    </button>
                  ))}
                  {entry.rationale && <small>{entry.rationale}</small>}
                </div>
                <span>{entry.actor}</span>
              </article>
            ))}
          </section>
        </>
      ) : (
        <Empty text="判断履歴はありません。" />
      )}
    </section>
  );
}
