use std::collections::HashSet;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

use super::super::staged_progress::{ProgressKind, StagedStep};
use super::super::{AppState, StepOutcome, StepStatus};
use super::super::{
    emit_event, emit_generated_refresh, generated_refresh_scopes_from_position, json_as_i64,
};
use super::guards::CANCELLED_ACTIVE_MESSAGE;
use super::request::{ImportNoticeCode, ImportStep};
use crate::jobworkerp::{
    JobworkerpHandle, StreamEvent, job_timeouts, run_cancellable_named_stream_with_uniq_key,
    workflow_terminal_reason, workflow_unsuccessful_status,
};

/// Condense a WORKFLOW chunk for the toast, carrying the last-seen progress
/// counter forward. Returns `(digest, progress_to_carry)`. Each chunk is a
/// `WorkflowResult` JSON (`{id, output, position, status, errorMessage}`) whose
/// `output` is the *entire* intermediate workflow context — for the summary
/// batch that's the full thread list (tens of KB), which floods the toast.
///
/// The `(processed/total)` counter is published by the batch's `reportProgress`
/// step (see each `*-batch.yaml`), but only the WorkflowResult for *that* set
/// step carries it in `output`; the long-running `invokeSingle` chunks that
/// follow (LLM generation) do not. Showing the counter only on the brief
/// `reportProgress` chunk would make it flicker and vanish. So we thread the last
/// counter through every chunk: `carried` is the value from the previous chunk,
/// and we return the updated value for the next call. This keeps `(N/M)` pinned
/// while a single thread is being processed.
///
/// The digest leads with `(N/M)` when known (most useful), then the current
/// step mapped from the JSON-pointer `position` via [`describe_position`]. The
/// full JSON goes to the log only. Non-JSON chunks fall back to a length-capped
/// raw string so nothing is silently dropped. Pure so it's unit-tested.
pub(crate) fn summarize_workflow_chunk(
    raw: &str,
    carried: Option<(i64, i64)>,
) -> (String, Option<(i64, i64)>) {
    const MAX_RAW: usize = 200;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (cap(raw, MAX_RAW), carried);
    };
    let Some(status) = v.get("status").and_then(|s| s.as_str()) else {
        return (cap(raw, MAX_RAW), carried);
    };
    let position = v.get("position").and_then(|p| p.as_str()).unwrap_or("");
    let step = describe_position(position);
    // `output` is a context snapshot (JSON *string*). The counter only appears
    // on the reportProgress chunk; fall back to the carried value otherwise.
    let progress = v
        .get("output")
        .and_then(|o| o.as_str())
        .and_then(extract_progress)
        .or(carried);

    let digest = match (progress, step) {
        (Some((p, t)), Some(desc)) => format!("({p}/{t}) {desc}"),
        (Some((p, t)), None) => format!("({p}/{t})"),
        (None, Some(desc)) => format!("{status} · {desc}"),
        (None, None) => status.to_string(),
    };
    (digest, progress)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ThreadGroupFailure {
    pub group_id: String,
    pub reason_code: String,
    pub detail: String,
}

/// A caught per-group failure is not an engine error. Read the small failure
/// entry from the streamed failure-recording step before a later for-loop
/// timeout can discard the batch's terminal output. Never log the workflow
/// context.
pub(crate) fn thread_group_failure_from_chunk(raw: &str) -> Option<ThreadGroupFailure> {
    // A group fails either while its members are read (phase 1) or while it
    // is aggregated (phase 3).
    const FAILURE_STEPS: &[(&str, &str)] = &[
        ("/collectGroupMembers/for/", "recordGroupReadFailure"),
        ("/summarizeEachGroup/for/", "recordFailed"),
    ];
    let chunk: serde_json::Value = serde_json::from_str(raw).ok()?;
    let position = chunk.get("position")?.as_str()?;
    if !FAILURE_STEPS.iter().any(|(loop_path, step)| {
        position.contains(loop_path) && position.split('/').any(|segment| segment == *step)
    }) {
        return None;
    }
    let output: serde_json::Value = serde_json::from_str(chunk.get("output")?.as_str()?).ok()?;
    let failure = output.get("failed_groups")?.as_array()?.last()?;
    let group_id = failure.get("group_id")?.as_str()?;
    if !group_id.bytes().all(|b| b.is_ascii_digit()) || group_id.is_empty() {
        return None;
    }
    Some(ThreadGroupFailure {
        group_id: group_id.to_owned(),
        reason_code: match failure.get("reason_code")?.as_str()? {
            "incomplete_input" => "incomplete_input",
            "workflow_failed" => "workflow_failed",
            _ => "other",
        }
        .to_owned(),
        detail: failure
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned(),
    })
}

pub(crate) fn log_thread_group_failure(raw: &str, seen: &mut HashSet<String>) {
    let Some(failure) = thread_group_failure_from_chunk(raw) else {
        return;
    };
    if !seen.insert(failure.group_id.clone()) {
        return;
    }
    tracing::warn!(
        target: "thread_group_summary",
        group_id = %failure.group_id,
        reason_code = %failure.reason_code,
        "group summary failed"
    );
    tracing::debug!(
        target: "thread_group_summary",
        group_id = %failure.group_id,
        detail = %cap(&failure.detail, 400),
        "group summary failure detail"
    );
}

fn log_workflow_done(step: ImportStep, raw: &str) {
    super::super::issue_resolution_diagnostics::warn_issue_resolution_errors(raw);
    if step == ImportStep::ThreadGroupSummary {
        tracing::info!(target: "import", step = ?step, "group summary workflow finished");
    } else {
        tracing::info!(target: "staged-progress", step = ?step, "workflow done: {raw}");
    }
}

pub(crate) fn staged_step_job_timeout_sec(step: ImportStep) -> Option<u64> {
    match step {
        ImportStep::ThreadImport => None,
        ImportStep::ThreadSummary => Some(job_timeouts::THREAD_SUMMARY_BATCH),
        ImportStep::ThreadGroupSummary => Some(job_timeouts::THREAD_GROUP_SUMMARY_BATCH),
        ImportStep::ThreadPersonality => Some(job_timeouts::PERSONALITY_BATCH),
        ImportStep::Reflection => Some(job_timeouts::REFLECTION_BATCH),
        ImportStep::Daily => Some(job_timeouts::DAILY_SUMMARY_BATCH),
        ImportStep::Weekly => Some(job_timeouts::WEEKLY_SUMMARY_BATCH),
        ImportStep::Monthly => Some(job_timeouts::MONTHLY_SUMMARY_BATCH),
    }
}

fn park_job_id_if_cancelled(
    current: &mut Option<jobworkerp_client::jobworkerp::data::JobId>,
    job_id: jobworkerp_client::jobworkerp::data::JobId,
    cancel_requested: bool,
) -> Option<jobworkerp_client::jobworkerp::data::JobId> {
    *current = Some(job_id);
    cancel_requested.then(|| current.take()).flatten()
}

/// Pull `(processed, total)` from a workflow context snapshot. The batch's
/// `reportProgress` step sets these via jq, which may serialize them as numbers
/// or numeric strings depending on the runtime, so accept both. Returns `None`
/// unless both are present and total is non-zero (a `(n/0)` would be nonsense).
fn extract_progress(output_json: &str) -> Option<(i64, i64)> {
    // Deserialize only the two counters; the rest of the context is skipped
    // without being materialized, since it is parsed for every streamed chunk.
    #[derive(serde::Deserialize)]
    struct Counters {
        progress_processed: Option<serde_json::Value>,
        progress_total: Option<serde_json::Value>,
    }
    let v: Counters = serde_json::from_str(output_json).ok()?;
    let processed = json_as_i64(&v.progress_processed?)?;
    let total = json_as_i64(&v.progress_total?)?;
    (total > 0).then_some((processed, total))
}

/// Decision for the terminal Done event of a batch workflow. Carries the
/// step status the toast should render plus an optional summary message
/// (e.g. "成功 12 / 失敗 3 (Gemini 429 ...)") that goes into the detail
/// dialog when the step degraded.
///
/// Pure so unit tests can pin each branch (all-failed / partial / no-counts
/// / all-success) without spinning up a workflow.
/// Read a batch workflow's terminal `WorkflowResult` JSON and classify it
/// into Done / Warning / Failed based on the `failed_count` / `processed_*`
/// counters the batch YAMLs export.
///
/// Why this lives next to `summarize_workflow_chunk`: the toast currently
/// treats every Done event as "緑 / 完了", which lies when the LLM provider
/// rate-limited part of the batch (every item failed but the workflow itself
/// reported `Completed`). The counters are already in the batch's
/// `output:` block; we just need to read them.
///
/// Decision table:
/// - `failed_count > 0` AND `failed_count >= processed`  → Failed (all items died)
/// - `failed_count > 0` AND processed > 0                → Warning (partial)
/// - `failed_count == 0` OR counters absent              → Done (unchanged)
///
/// Counters absent means an older batch YAML that hasn't been updated yet —
/// fall back to Done so a YAML drift doesn't downgrade a successful run to
/// Warning. `last_error` (when present) gets appended to the message so the
/// detail dialog surfaces the underlying failure cause.
pub(crate) fn summarize_workflow_outcome(raw: Option<&str>) -> StepOutcome {
    let Some(raw) = raw else {
        return StepOutcome {
            status: StepStatus::Done,
            message: None,
        };
    };
    if let Some(failure) = workflow_terminal_failure(raw) {
        return StepOutcome {
            status: StepStatus::Failed,
            message: Some(failure),
        };
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return StepOutcome {
            status: StepStatus::Done,
            message: Some(raw.to_string()),
        };
    };
    // The terminal chunk's `output` is the workflow's exported `output:`
    // block serialized as a JSON string. A workflow without an `output:`
    // block (or one whose serializer drops the field on Completed) leaves
    // this absent — handled by the "counters absent → Done" fallback.
    let output_obj = v
        .get("output")
        .and_then(|o| o.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let Some(output) = output_obj else {
        return StepOutcome {
            status: StepStatus::Done,
            message: None,
        };
    };

    let failed = output
        .get("failed_count")
        .and_then(json_as_i64)
        .or_else(|| {
            output
                .get("failed_groups")
                .and_then(serde_json::Value::as_array)
                .map(|groups| groups.len() as i64)
        })
        .unwrap_or(0);
    let succeeded = output
        .get("succeeded_count")
        .and_then(json_as_i64)
        .or_else(|| output.get("generated_count").and_then(json_as_i64));
    // Thread-group and period batches name their total `processed_groups`,
    // `processed_dates` / `processed_weeks` / `processed_months` instead of
    // `processed_threads`.
    // Accept all so a single classifier handles every batch shape.
    let processed = output
        .get("processed_threads")
        .or_else(|| output.get("processed_groups"))
        .or_else(|| output.get("processed_dates"))
        .or_else(|| output.get("processed_weeks"))
        .or_else(|| output.get("processed_months"))
        .and_then(json_as_i64);
    // Personality batch only: the count of already-processed threads we
    // deliberately did NOT re-extract because the signal budget was full.
    // Surfacing this in the digest is what replaces the misleading
    // mid-flight "既処理のため再抽出を省略中" label (which fired on every
    // iteration regardless of the `if:` guard outcome).
    let skipped = output
        .get("budget_skipped_count")
        .and_then(json_as_i64)
        .filter(|n| *n > 0);
    let failed_paths = output
        .get("failed_path_buckets")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);

    // No per-item failure: status stays Done. We still attach a message when
    // the budget skipped something so the user sees "完了 · 中略 N 件" in
    // the toast instead of bare 完了 (the YAML guard intentionally left
    // some threads alone, and that fact is worth surfacing).
    if failed <= 0 {
        if failed_paths > 0
            || output.get("outcome").and_then(serde_json::Value::as_str)
                == Some("completed_with_errors")
        {
            return StepOutcome {
                status: StepStatus::Warning,
                message: Some(format!(
                    "一部の path 要約に失敗しました（{failed_paths} 件）。利用可能な入力で要約を生成しました"
                )),
            };
        }
        if processed == Some(0) {
            return StepOutcome {
                status: StepStatus::Done,
                message: Some("対象なし".into()),
            };
        }
        return StepOutcome {
            status: StepStatus::Done,
            message: skipped.map(|n| format!("中略 {n} 件 (既存シグナル充足)")),
        };
    }
    let last_error = output
        .get("last_error")
        .map(|e| match e {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .map(|s| cap(&s, 400));
    let counter_line = match (succeeded, processed) {
        (Some(s), Some(p)) => format!("成功 {s} / 失敗 {failed} (合計 {p})"),
        (Some(s), None) => format!("成功 {s} / 失敗 {failed}"),
        (None, Some(p)) => format!("失敗 {failed} / 合計 {p}"),
        (None, None) => format!("失敗 {failed}"),
    };
    let mut lines: Vec<String> = vec![counter_line];
    if let Some(n) = skipped {
        lines.push(format!("中略 {n} 件 (既存シグナル充足)"));
    }
    if let Some(err) = last_error {
        lines.push(format!("直近のエラー: {err}"));
    }
    let message = Some(lines.join("\n"));
    let status = match processed {
        // "every item failed" — surface as a hard step failure so the toast
        // matches a workflow-level failure visually.
        Some(p) if p > 0 && failed >= p => StepStatus::Failed,
        _ => StepStatus::Warning,
    };
    StepOutcome { status, message }
}

/// Step status and message for a batch's terminal output: failed or
/// unaccounted items degrade the step instead of always reading Done, and
/// a completed run without counters keeps the progress digest as message.
pub(crate) fn terminal_batch_outcome(
    raw: Option<&str>,
    last_progress: Option<(i64, i64)>,
) -> (StepStatus, Option<String>) {
    let outcome = summarize_workflow_outcome(raw);
    let message = outcome
        .message
        .or_else(|| raw.map(|chunk| summarize_workflow_chunk(chunk, last_progress).0));
    (outcome.status, message)
}

/// Decode the terminal `output` block of a WORKFLOW result. Accept both the
/// usual JSON-string output and direct JSON used by older clients.
pub(crate) fn workflow_output_value(raw: &str) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    match value.get("output") {
        Some(serde_json::Value::String(output)) => serde_json::from_str(output).ok(),
        Some(output @ serde_json::Value::Object(_)) => Some(output.clone()),
        _ if value.get("status").is_none() => Some(value),
        _ => None,
    }
}

/// Turn a workflow `position` JSON pointer into a short Japanese label for the
/// toast. The raw pointer (e.g.
/// `/ROOT/do/6/summarizeEach/for/do/0/.../run/do/0/logError`) overflows the
/// toast, so we keep only meaningful *step names* (dropping the structural
/// `ROOT`/`do`/`for`/`try`/`catch` tokens and numeric indices) and translate
/// the deepest known one — that's the most specific "where are we now" signal.
///
/// The workflows are bundled and version-controlled with this app, so the step
/// names are a closed, known set ([`step_description`]). Unknown names pass
/// through verbatim (capped) rather than being hidden, so a workflow edit that
/// adds a step still shows *something* until the table is updated.
fn describe_position(position: &str) -> Option<String> {
    let steps = position_steps(position);
    let deepest = *steps.last()?;
    if deepest == "reportProgress"
        && let Some(stage) = steps[..steps.len() - 1]
            .iter()
            .rev()
            .find_map(|step| progress_stage_description(step))
    {
        return Some(format!("{stage} · 進捗を更新中"));
    }
    // The group batch delegates aggregation to its single worker; the generic
    // per-item label would read as a thread being processed.
    if deepest == "invokeSingle" && steps.contains(&"summarizeEachGroup") {
        return Some("グループ要約を生成中".to_string());
    }
    Some(match step_description(deepest) {
        Some(desc) => desc.to_string(),
        None => cap(deepest, 40),
    })
}

/// Workflow task names along a position pointer, outermost first.
fn position_steps(position: &str) -> Vec<&str> {
    position
        .split('/')
        .filter(|seg| !seg.is_empty() && !is_structural_segment(seg))
        .collect()
}

/// Structural tokens in a workflow position pointer that carry no semantic
/// meaning for the user (control-flow scaffolding + array indices).
fn is_structural_segment(seg: &str) -> bool {
    // `run` wraps a function/sub-workflow call (`run/function`,
    // `run/worker`); the meaningful step name is the segment *before* it
    // (e.g. `generateSummary/run`), so treat the call kind as scaffolding too.
    matches!(
        seg,
        "ROOT"
            | "do"
            | "for"
            | "try"
            | "catch"
            | "then"
            | "else"
            | "fork"
            | "branches"
            | "run"
            | "function"
            | "worker"
            | "set"
    ) || seg.chars().all(|c| c.is_ascii_digit())
}

fn progress_stage_description(step: &str) -> Option<&'static str> {
    match step {
        "summarizeEachDay"
        | "summarizeEachWeek"
        | "summarizeEachMonth"
        | "resolveEachCandidate"
        | "refreshMemberSummaries" => step_description(step),
        _ => None,
    }
}

/// User-facing failure of a terminal WorkflowResult JSON, or `None` when the
/// workflow completed. Names the failed task path and the reason.
pub(crate) fn workflow_terminal_failure(raw: &str) -> Option<String> {
    let result: serde_json::Value = serde_json::from_str(raw).ok()?;
    let cancelled = workflow_unsuccessful_status(&result)? == "Cancelled";
    let steps = position_steps(
        result
            .get("position")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    );
    let place = if steps.is_empty() {
        "ワークフロー".to_string()
    } else {
        steps.join(" › ")
    };
    if cancelled {
        return Some(format!("{place} で中断されました"));
    }
    let reason = workflow_terminal_reason(&result)
        .map(|reason| cap(&reason, 300))
        .unwrap_or_else(|| "原因不明のエラー".to_string());
    Some(format!("{place} で失敗しました: {reason}"))
}

/// Map a known workflow step name to a Japanese label. Covers the batch
/// fan-out steps and the per-thread single-workflow steps across summary /
/// personality / reflection. Returns `None` for names not in the table.
fn step_description(step: &str) -> Option<&'static str> {
    let desc = match step {
        // ---- shared batch steps (summary / personality / reflection) ----
        "fetchByLabels" | "fetchByUser" => "対象スレッドを取得中",
        "filterThreads" => "対象スレッドを絞り込み中",
        "sortTargetThreadsNewestFirst" => "対象スレッドを並べ替え中",
        "resolveLastMessageAfter" | "resolveLastMessageBefore" => "対象期間を解決中",
        "summarizeEach" => "スレッドを要約中",
        "summarizeEachDay" => "日次要約を生成中",
        "summarizeEachWeek" => "週次要約を生成中",
        "summarizeEachMonth" => "月次要約を生成中",
        "resolveEachCandidate" => "グループ issue を解決中",
        "personalityEach" => "パーソナリティを抽出中",
        // ---- thread-group summary batch ----
        "fetchGroups" => "対象グループを取得中",
        "selectTargetGroups" | "resolveRetryDigests" => "対象グループを整理中",
        "validateTargetGroup" => "対象グループを検証中",
        "validateTargetSnapshot" => "グループ構成を検証中",
        "fetchLineage" => "グループ構成を取得中",
        "collectGroupMembers" | "readGroupMembers" | "recordGroupMembers" => {
            "グループメンバーを取得中"
        }
        "buildMemberRefreshTargets"
        | "refreshMemberSummaries"
        | "refreshMember"
        | "invokeMemberSingle"
        | "classifyMemberRefresh"
        | "collectMemberRefreshResult" => "メンバー要約を更新中",
        "recordMemberRefreshFailure" | "recordGroupReadFailure" | "classifyFailure" => {
            "エラーを記録中"
        }
        "summarizeEachGroup" => "グループを要約中",
        "rejectRefreshFailedMembers" | "rejectIncompleteInput" | "rejectLogicalFailure" => {
            "グループ要約の結果を検証中"
        }
        "recordOutcome" | "recordResolution" => "グループ要約を記録中",
        // The thread and period batches' success step.
        "recordGenerated" => "生成結果を記録中",
        "recordFailed" => "グループ要約のエラーを記録中",
        // ---- personality batch budget bookkeeping (early-break path) ----
        // `countExistingValidSignals` was renamed to
        // `fetchProcessedSignalMemories` to reflect that it returns both the
        // count AND the source-thread set used by `classifyThread`.
        //
        // The toast text deliberately avoids "予算" / "超過" wording — that
        // framing implied a cost/spend metaphor (none of this affects local
        // LLM cost) and left "what is exceeded" undefined. The actual
        // semantics: the loop processes new threads unconditionally, plus
        // already-processed threads only until cumulative valid-signal
        // count reaches `target_signal_count`; once that count is reached,
        // remaining already-processed threads are left alone (not "exceeded"
        // — just not re-extracted in this run, the existing extraction stays
        // valid).
        "fetchProcessedSignalMemories" => "既存シグナルを確認中",
        "initSignalBudget" => "処理上限を設定中",
        "classifyThread" => "対象スレッドを判定中",
        // `skipIfBudgetExhausted` is a `set:` step guarded by `if:`. The
        // jobworkerp WORKFLOW runner emits a position chunk for the step
        // regardless of whether the guard fires, so the toast briefly shows
        // this label even for NEW threads that will go on to actually run
        // the extraction. Keep the wording neutral ("再抽出要否を判定中")
        // so a transient chunk doesn't lie to the user; the actual skip
        // counter is surfaced post-batch via `budget_skipped_count` in the
        // workflow outcome digest (see `summarize_workflow_outcome`).
        "skipIfBudgetExhausted" => "再抽出要否を判定中",
        "recordSignalOutcome" => "抽出結果を記録中",
        // ---- personality batch / single shared guards ----
        "normalizePersonalityUserId" => "オーナーIDを解決中",
        "failOnSelfOwnedPersonality" | "failOnSummaryUserCollision" => "設定を検証中",
        "reflectEach" => "自省を生成中",
        "invokeSingleWithRetry" | "invokeSingle" => "個別スレッドを処理中",
        "reportProgress" => "進捗を更新中",
        "logError" | "logMergeError" => "エラーを記録中",
        // ---- summary single ----
        "fetchThread" => "スレッドを取得中",
        "fetchMessages" => "メッセージを取得中",
        "truncateMessages" => "メッセージを整形中",
        "generateSummary" => "要約を生成中 (LLM)",
        "updateThreadDescription" => "スレッド概要を更新中",
        "createMemoryThread" => "メモリスレッドを作成中",
        "updateSummary" | "addSummaryMemory" => "要約を保存中",
        "cleanupStaleSummaries" => "古い要約を整理中",
        // ---- personality single ----
        "generatePersonality" => "パーソナリティを生成中 (LLM)",
        "updateSignal" | "addSignalMemory" => "パーソナリティを保存中",
        "applySignalLabel" => "ラベルを付与中",
        // Personality single's new/renamed transition steps.
        "computeTruncationLevel" => "メッセージを整形中",
        "checkEligibility" => "再抽出要否を判定中",
        "skipIfNotEligible" => "更新不要のためスキップ中",
        "findExistingSignal" | "findMemoryThread" => "既存シグナルを確認中",
        // ---- personality merge (layer 2) ----
        "userPersonalityMerge" | "invokeMerge" => "プロファイルを統合中",
        "mergeProfile" => "プロファイルを統合中 (LLM)",
        "findProfileThread" => "シグナルを収集中",
        // findRecentSignals replaces the legacy
        // findSignalThreads + fetchSignalMemories N+1 fan-out.
        "findRecentSignals" => "シグナルを取得中",
        "confirmEntryDates" => "プロファイルを整形中",
        "updateProfile" | "addProfileMemory" => "プロファイルを保存中",
        "createProfileThread" => "プロファイルスレッドを作成中",
        "touchProfileThreadUpdatedAt" => "プロファイル更新時刻を反映中",
        "resolveProfileThreadId" => "プロファイルスレッドを解決中",
        "countTotalPersonalityMemories" | "computeNoSignalCount" => "no_signal 数を集計中",
        "buildSignalEntries" => "シグナルを整形中",
        "capSignals" | "truncateSignals" => "シグナルを切り詰め中",
        "buildSignalsLookup" => "シグナル索引を構築中",
        "skipIfNoSignals" => "シグナル不足のためスキップ中",
        "findExistingProfile" | "initExistingProfile" => "既存プロファイルを確認中",
        "initOutputVars"
        | "initCreatedMemoryThreadId"
        | "initCreatedProfileThreadId"
        | "initExistingSignal"
        | "initAccumulatedSignals" => "変数を初期化中",
        // ---- reflection single ----
        "fetchMemoriesAll" => "メモリを取得中",
        "prepareLlmInput" => "入力を準備中",
        "callReflectorLlm" | "callReflectorLlmWithFallback" => "自省を生成中 (LLM)",
        "validateLlmOutput" => "出力を検証中",
        "computeHeuristicScore" => "スコアを計算中",
        "buildParsedOutput" => "結果を構築中",
        "callFinalizeReflection" | "callFinalizeReflectionWithGuard" => "自省を確定中",
        _ => return None,
    };
    Some(desc)
}

/// Pull a short, human-readable reason out of a failed WORKFLOW chunk for the
/// toast / error dialog. Prefers `errorMessage`, then `status`, capped so a
/// payload-as-error (the server sometimes mirrors `output` into
/// `errorMessage`) can't blow up the UI. The untruncated text is logged
/// separately by the caller.
pub(crate) fn summarize_workflow_error(raw: &str) -> String {
    const MAX_ERR: usize = 300;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(msg) = v.get("errorMessage").and_then(|m| m.as_str()) {
            return cap(msg, MAX_ERR);
        }
        if let Some(status) = v.get("status").and_then(|s| s.as_str()) {
            return status.to_string();
        }
    }
    cap(raw, MAX_ERR)
}

/// Truncate `s` to at most `max` chars, appending an ellipsis + original
/// length when cut so the reader knows content was elided.
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… ({} chars total)", s.chars().count())
}

/// Cancellable variant of the original `run_stream_step`. Parks the
/// dispatched JobId into `current_job_id` so `start_import_cancel` can
/// issue `JobService/Delete` against it, and converts a Failed event
/// arriving *after* the cancel token fires into a "中断" message
/// (the underlying gRPC text is "stream error: …" once Delete closes
/// the stream from the server side).
// Eight arguments are inherent here: emit destinations (app/job_id/step),
// dispatch inputs (handle/worker/input), and the cancel handles (token +
// parked JobId). Bundling them into a struct buys nothing — every caller
// holds them as locals already and the struct would have the same fields.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_cancellable_step(
    app: &AppHandle,
    kind: ProgressKind,
    job_id: &str,
    step: ImportStep,
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: serde_json::Value,
    operation_key: Option<&str>,
    timeout_sec: Option<u64>,
    cancel: tokio_util::sync::CancellationToken,
    current_job_id: Arc<tokio::sync::Mutex<Option<jobworkerp_client::jobworkerp::data::JobId>>>,
) -> CancellableStepResult {
    let args = super::super::wrap_workflow_run_args(&input);
    let mut last_progress: Option<(i64, i64)> = None;
    let mut logged_group_failures = HashSet::new();
    let park_job_id = current_job_id.clone();
    let captured_job_id = Arc::new(tokio::sync::Mutex::new(None::<String>));
    let capture_for_park = captured_job_id.clone();
    let cancel_for_park = cancel.clone();
    let handle_for_park = handle.clone();
    let mut terminal_output = None::<String>;
    let mut terminal_outcome = None::<StepOutcome>;
    let mut stream_failed = false;
    let mut has_reported_failures = false;

    run_cancellable_named_stream_with_uniq_key(
        handle,
        worker_name,
        args,
        Some("run"),
        timeout_sec,
        operation_key,
        cancel.clone(),
        move |jid| async move {
            *capture_for_park.lock().await = Some(jid.value.to_string());
            let cancel_after_park = {
                let mut current = park_job_id.lock().await;
                park_job_id_if_cancelled(
                    &mut current,
                    jid,
                    cancel_for_park.is_cancelled(),
                )
            };
            if let Some(jid) = cancel_after_park
                && let Err(error) = handle_for_park.cancel(jid).await
            {
                tracing::warn!(%error, "JobService/Delete failed for a dispatch cancelled before its JobId was parked");
            }
        },
        |ev| {
            let (status, message) = match ev {
                StreamEvent::Active(msg) => {
                    if let Some(raw) = msg {
                        if step == ImportStep::ThreadGroupSummary {
                            log_thread_group_failure(raw, &mut logged_group_failures);
                            tracing::debug!(
                                target: "import",
                                step = ?step,
                                progress = %summarize_workflow_chunk(raw, last_progress).0,
                                "group summary workflow progress"
                            );
                        } else {
                            tracing::debug!(target: "staged-progress", step = ?step, "{raw}");
                        }
                        emit_generated_refresh(
                            app,
                            job_id,
                            generated_refresh_scopes_from_position(raw),
                        );
                    }
                    let digest = msg.map(|raw| {
                        let (digest, progress) = summarize_workflow_chunk(raw, last_progress);
                        last_progress = progress;
                        digest
                    });
                    (StepStatus::Active, digest)
                }
                StreamEvent::Done(msg) => {
                    if let Some(raw) = msg {
                        log_workflow_done(step, raw);
                    }
                    emit_step_terminal_refresh(app, job_id, step);
                    let outcome = match (kind, msg) {
                        (_, Some(raw)) if let Some(failure) = workflow_terminal_failure(raw) => {
                            stream_failed = true;
                            StepOutcome {
                                status: failed_or_cancelled(&cancel),
                                message: Some(failure),
                            }
                        }
                        (ProgressKind::SummaryGeneration, Some(raw)) => {
                            let output = workflow_output_value(raw);
                            let outcome = summary_stage_terminal_outcome(raw, step, last_progress);
                            let output_valid = output.as_ref().is_some_and(|value| {
                                value.get("completed").and_then(serde_json::Value::as_bool).is_some()
                            });
                            stream_failed |= !output_valid;
                            has_reported_failures =
                                output.as_ref().is_some_and(workflow_output_has_failures);
                            if output_valid {
                                terminal_output = Some(raw.to_string());
                            }
                            outcome
                        }
                        (ProgressKind::SummaryGeneration, None) => {
                            stream_failed = true;
                            StepOutcome {
                                status: StepStatus::Failed,
                                message: Some("バッチの終端出力がありません".into()),
                            }
                        }
                        (ProgressKind::Import, raw) => {
                            if let Some(raw) = raw {
                                terminal_output = Some(raw.to_string());
                            }
                            let (status, message) = terminal_batch_outcome(raw, last_progress);
                            StepOutcome { status, message }
                        }
                    };
                    terminal_outcome = Some(outcome.clone());
                    (outcome.status, outcome.message)
                }
                StreamEvent::Failed(msg) => {
                    stream_failed = true;
                    tracing::error!(target: "staged-progress", step = ?step, "workflow failed");
                    emit_step_terminal_refresh(app, job_id, step);
                    let message = if cancel.is_cancelled() {
                        CANCELLED_ACTIVE_MESSAGE.to_string()
                    } else {
                        summarize_workflow_error(msg)
                    };
                    let outcome = StepOutcome {
                        status: failed_or_cancelled(&cancel),
                        message: Some(message),
                    };
                    terminal_outcome = Some(outcome.clone());
                    (outcome.status, outcome.message)
                }
            };
            emit_staged_step(app, kind, job_id, step, status, message, None);
        },
    )
    .await;

    let dispatched_job_id = captured_job_id.lock().await.clone();
    *current_job_id.lock().await = None;
    CancellableStepResult {
        outcome: terminal_outcome.unwrap_or_else(|| StepOutcome {
            status: failed_or_cancelled(&cancel),
            message: Some("ワークフローの終端状態がありません".into()),
        }),
        terminal_output,
        dispatched_job_id,
        stream_failed,
        has_reported_failures,
    }
}

fn failed_or_cancelled(cancel: &tokio_util::sync::CancellationToken) -> StepStatus {
    if cancel.is_cancelled() {
        StepStatus::Cancelled
    } else {
        StepStatus::Failed
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CancellableStepResult {
    pub outcome: StepOutcome,
    pub terminal_output: Option<String>,
    pub dispatched_job_id: Option<String>,
    pub stream_failed: bool,
    pub has_reported_failures: bool,
}

/// Terminal batch output plus its `completed` flag, or the Failed outcome to
/// report when either is unreadable. `subject` names the batch in that message.
fn completed_output(raw: &str, subject: &str) -> Result<(serde_json::Value, bool), StepOutcome> {
    let failed = |message: String| StepOutcome {
        status: StepStatus::Failed,
        message: Some(message),
    };
    let output = workflow_output_value(raw)
        .ok_or_else(|| failed(format!("{subject}の終端出力を解釈できません")))?;
    let completed = output
        .get("completed")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| failed(format!("{subject}の完了状態を確認できません")))?;
    Ok((output, completed))
}

fn summary_stage_terminal_outcome(
    raw: &str,
    step: StagedStep,
    last_progress: Option<(i64, i64)>,
) -> StepOutcome {
    let (output, completed) = match completed_output(raw, "バッチ") {
        Ok(parsed) => parsed,
        Err(outcome) => return outcome,
    };
    if step == StagedStep::ThreadGroupSummary {
        return thread_group_summary_outcome(&output, completed);
    }
    let (status, message) = terminal_batch_outcome(Some(raw), last_progress);
    if !completed && status == StepStatus::Done {
        return StepOutcome {
            status: StepStatus::Failed,
            message: Some("バッチが未完了のまま終了しました".into()),
        };
    }
    StepOutcome { status, message }
}

pub(crate) fn thread_group_summary_terminal_outcome(raw: &str) -> StepOutcome {
    match completed_output(raw, "グループ要約") {
        Ok((output, completed)) => thread_group_summary_outcome(&output, completed),
        Err(outcome) => outcome,
    }
}

fn thread_group_summary_outcome(output: &serde_json::Value, completed: bool) -> StepOutcome {
    let failed_count = output
        .get("failed_count")
        .and_then(json_as_i64)
        .or_else(|| {
            output
                .get("failed_groups")
                .and_then(serde_json::Value::as_array)
                .map(|groups| groups.len() as i64)
        })
        .unwrap_or(0);
    let processed_count = output
        .get("processed_groups")
        .and_then(json_as_i64)
        .unwrap_or(0);
    if failed_count > 0 || !completed {
        let generated_count = output
            .get("generated_count")
            .and_then(json_as_i64)
            .unwrap_or(0);
        return StepOutcome {
            status: if generated_count > 0 {
                StepStatus::Warning
            } else {
                StepStatus::Failed
            },
            message: Some(format!(
                "グループ要約の一部または全部に失敗しました（処理 {processed_count} 件、生成 {generated_count} 件、失敗 {failed_count} 件）"
            )),
        };
    }
    if processed_count == 0 {
        return StepOutcome {
            status: StepStatus::Done,
            message: Some("対象なし".into()),
        };
    }
    StepOutcome {
        status: StepStatus::Done,
        message: Some(format!("グループ要約を {processed_count} 件処理しました")),
    }
}

fn workflow_output_has_failures(output: &serde_json::Value) -> bool {
    output
        .get("failed_count")
        .and_then(json_as_i64)
        .is_some_and(|count| count > 0)
        || [
            "failed_groups",
            "failed_path_buckets",
            "failed_dates",
            "failed_weeks",
            "failed_months",
        ]
        .into_iter()
        .filter_map(|key| output.get(key).and_then(serde_json::Value::as_array))
        .any(|items| !items.is_empty())
        || output.get("outcome").and_then(serde_json::Value::as_str)
            == Some("completed_with_errors")
        || output.get("completed").and_then(serde_json::Value::as_bool) == Some(false)
}

fn emit_step_terminal_refresh(app: &AppHandle, job_id: &str, step: ImportStep) {
    if let Some(scope) = import_terminal_refresh_scope(step) {
        emit_generated_refresh(app, job_id, vec![scope]);
    }
}

fn import_terminal_refresh_scope(step: ImportStep) -> Option<super::super::GeneratedRefreshScope> {
    match step {
        ImportStep::ThreadSummary => Some(super::super::GeneratedRefreshScope::ThreadSummary),
        ImportStep::ThreadGroupSummary => {
            Some(super::super::GeneratedRefreshScope::ThreadGroupSummary)
        }
        ImportStep::ThreadPersonality => Some(super::super::GeneratedRefreshScope::Personality),
        ImportStep::Reflection => Some(super::super::GeneratedRefreshScope::Reflection),
        ImportStep::ThreadImport => None,
        ImportStep::Daily => Some(super::super::GeneratedRefreshScope::DailySummary),
        ImportStep::Weekly => Some(super::super::GeneratedRefreshScope::WeeklySummary),
        ImportStep::Monthly => Some(super::super::GeneratedRefreshScope::MonthlySummary),
    }
}

/// Scrape the trailing `<label> summary:` block printed by
/// `memories-import` (see `agent-chat-import/src/main.rs::print_summary`).
pub(crate) fn extract_summary_block(lines: &[String]) -> Option<String> {
    let start = lines
        .iter()
        .rposition(|l| l.trim_end().ends_with(" summary:"))?;
    let mut out = Vec::with_capacity(8);
    out.push(lines[start].trim_end().to_string());
    for line in &lines[start + 1..] {
        if line.starts_with("  ") {
            out.push(line.trim_end().to_string());
        } else {
            break;
        }
    }
    Some(out.join("\n"))
}

pub(super) fn emit_step(
    app: &AppHandle,
    job_id: &str,
    step: ImportStep,
    status: StepStatus,
    message: Option<String>,
) {
    emit_step_with_notice(app, job_id, step, status, message, None);
}

pub(super) fn emit_step_with_notice(
    app: &AppHandle,
    job_id: &str,
    step: ImportStep,
    status: StepStatus,
    message: Option<String>,
    notice_code: Option<ImportNoticeCode>,
) {
    emit_staged_step(
        app,
        ProgressKind::Import,
        job_id,
        step,
        status,
        message,
        notice_code,
    );
}

pub(crate) fn emit_staged_step(
    app: &AppHandle,
    kind: ProgressKind,
    job_id: &str,
    step: StagedStep,
    status: StepStatus,
    message: Option<String>,
    notice_code: Option<ImportNoticeCode>,
) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if let Some(snapshot) =
        state
            .import_progress
            .update_execution(kind, job_id, step, status, message, notice_code)
    {
        emit_event(app, "staged://step", snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::StepStatus;
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone)]
    struct InfoCapture(Arc<parking_lot::Mutex<Vec<String>>>);

    struct EventFields(Vec<String>);

    impl tracing::field::Visit for EventFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push(format!("{}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push(format!("{}={value}", field.name()));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for InfoCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::INFO {
                return;
            }
            let mut fields = EventFields(Vec::new());
            event.record(&mut fields);
            self.0.lock().push(fields.0.join(" "));
        }
    }

    #[test]
    fn job_id_received_after_cancellation_is_returned_for_delete() {
        let job_id = jobworkerp_client::jobworkerp::data::JobId { value: 4242 };
        let mut current = None;

        let to_cancel = park_job_id_if_cancelled(&mut current, job_id, true);

        assert_eq!(to_cancel, Some(job_id));
        assert_eq!(current, None);
    }

    #[test]
    fn uncancelled_job_id_remains_parked_for_later_cancel() {
        let job_id = jobworkerp_client::jobworkerp::data::JobId { value: 4242 };
        let mut current = None;

        let to_cancel = park_job_id_if_cancelled(&mut current, job_id, false);

        assert_eq!(to_cancel, None);
        assert_eq!(current, Some(job_id));
    }

    #[test]
    fn thread_group_terminal_log_does_not_include_raw_workflow_payload() {
        let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(InfoCapture(logs.clone()));
        let raw = r#"{"failed_groups":[{"detail":"TERMINAL_PRIVATE_SENTINEL"}]}"#;

        tracing::subscriber::with_default(subscriber, || {
            log_workflow_done(ImportStep::ThreadGroupSummary, raw);
        });

        let logs = logs.lock();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("group summary workflow finished"));
        assert!(!logs[0].contains("TERMINAL_PRIVATE_SENTINEL"));
    }

    /// Test helper: digest with no carried progress (the common case).
    fn digest(raw: &str) -> String {
        summarize_workflow_chunk(raw, None).0
    }

    #[test]
    fn group_failure_chunk_extracts_only_the_failed_group_not_the_full_context() {
        let output = serde_json::json!({
            "target_groups": [{"description":"private conversation"}],
            "failed_groups": [{"group_id":"42", "reason_code":"incomplete_input", "detail":"refresh failed"}]
        });
        let raw = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/4/summarizeEachGroup/for/0/do/1/summarizeGroup/try/catch/do/0/recordFailed/set",
            "output": output.to_string()
        }).to_string();
        assert_eq!(
            thread_group_failure_from_chunk(&raw),
            Some(ThreadGroupFailure {
                group_id: "42".into(),
                reason_code: "incomplete_input".into(),
                detail: "refresh failed".into(),
            })
        );
        let unrelated = raw.replace("/recordFailed/set", "/fetchGroups/run");
        assert_eq!(thread_group_failure_from_chunk(&unrelated), None);
        assert_eq!(thread_group_failure_from_chunk("not JSON"), None);
        let mut seen = HashSet::new();
        log_thread_group_failure(&raw, &mut seen);
        log_thread_group_failure(&raw, &mut seen);
        assert_eq!(seen, HashSet::from(["42".to_owned()]));

        // Phase 1 records a group whose lineage could not be read.
        let read_failure = raw.replace(
            "/summarizeEachGroup/for/0/do/1/summarizeGroup/try/catch/do/0/recordFailed/set",
            "/collectGroupMembers/for/0/do/0/readGroupMembers/try/catch/do/0/recordGroupReadFailure/set",
        );
        assert_eq!(
            thread_group_failure_from_chunk(&read_failure).map(|failure| failure.group_id),
            Some("42".to_owned())
        );
        let unrelated_read =
            read_failure.replace("/collectGroupMembers/", "/refreshMemberSummaries/");
        assert_eq!(thread_group_failure_from_chunk(&unrelated_read), None);

        let unexpected_reason = raw.replace("incomplete_input", "secret-bearing-reason");
        assert_eq!(
            thread_group_failure_from_chunk(&unexpected_reason)
                .unwrap()
                .reason_code,
            "other"
        );
    }

    #[test]
    fn staged_step_jobs_use_their_batch_deadlines_within_the_client_deadline() {
        assert_eq!(staged_step_job_timeout_sec(ImportStep::ThreadImport), None);
        for (step, expected) in [
            (
                ImportStep::ThreadSummary,
                job_timeouts::THREAD_SUMMARY_BATCH,
            ),
            (
                ImportStep::ThreadGroupSummary,
                job_timeouts::THREAD_GROUP_SUMMARY_BATCH,
            ),
            (
                ImportStep::ThreadPersonality,
                job_timeouts::PERSONALITY_BATCH,
            ),
            (ImportStep::Reflection, job_timeouts::REFLECTION_BATCH),
            (ImportStep::Daily, job_timeouts::DAILY_SUMMARY_BATCH),
            (ImportStep::Weekly, job_timeouts::WEEKLY_SUMMARY_BATCH),
            (ImportStep::Monthly, job_timeouts::MONTHLY_SUMMARY_BATCH),
        ] {
            assert_eq!(staged_step_job_timeout_sec(step), Some(expected));
            assert!(
                expected <= job_timeouts::STAGED_GENERATION_CLIENT,
                "{step:?}"
            );
        }
    }

    #[test]
    fn import_terminal_refresh_scope_covers_each_generation_step_only() {
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadSummary),
            Some(super::super::super::GeneratedRefreshScope::ThreadSummary)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadGroupSummary),
            Some(super::super::super::GeneratedRefreshScope::ThreadGroupSummary)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadPersonality),
            Some(super::super::super::GeneratedRefreshScope::Personality)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::Reflection),
            Some(super::super::super::GeneratedRefreshScope::Reflection)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadImport),
            None
        );
    }

    #[test]
    fn summarize_workflow_chunk_condenses_running_result() {
        // A real Running chunk whose `output` is the whole thread list must
        // collapse to a one-line `status · <step label>` digest mapped from the
        // position pointer, not the payload nor the raw pointer.
        let raw = r#"{"id":"abc","output":"{\"target_threads\":[ ...huge... ]}","position":"/ROOT/do/5/filterThreads","status":"Running","errorMessage":"{\"target_threads\": ...}"}"#;
        assert_eq!(digest(raw), "Running · 対象スレッドを絞り込み中");
    }

    #[test]
    fn summarize_workflow_chunk_maps_deep_nested_position() {
        // The user-reported long pointer must collapse to the deepest known
        // step label (logError), never the full path.
        let raw = r#"{"position":"/ROOT/do/6/summarizeEach/for/do/0/invokeSingleWithRetry/try/do/0/invokeSingle/run/do/0/logError","status":"Running","output":"{}"}"#;
        assert_eq!(digest(raw), "Running · エラーを記録中");
    }

    #[test]
    fn summarize_workflow_chunk_keeps_period_stage_name_at_report_progress() {
        let output = serde_json::json!({
            "progress_processed": 2,
            "progress_total": 5,
        })
        .to_string();
        let chunk = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/5/summarizeEachDay/for/do/0/reportProgress/set",
            "output": output,
        })
        .to_string();

        assert_eq!(digest(&chunk), "(2/5) 日次要約を生成中 · 進捗を更新中");
    }

    #[test]
    fn summarize_workflow_chunk_names_weekly_monthly_and_issue_resolution_loops() {
        for (position, expected) in [
            (
                "/ROOT/do/6/summarizeEachWeek/for/do/0/reportProgress/set",
                "Running · 週次要約を生成中 · 進捗を更新中",
            ),
            (
                "/ROOT/do/7/summarizeEachMonth/for/do/0/reportProgress/set",
                "Running · 月次要約を生成中 · 進捗を更新中",
            ),
            (
                "/ROOT/do/3/resolveEachCandidate/for/do/0/reportProgress/set",
                "Running · グループ issue を解決中 · 進捗を更新中",
            ),
        ] {
            let chunk = serde_json::json!({
                "status": "Running",
                "position": position,
                "output": "{}",
            })
            .to_string();
            assert_eq!(digest(&chunk), expected, "position {position}");
        }
    }

    #[test]
    fn summarize_workflow_chunk_counts_member_refresh_through_each_single_call() {
        let progress = serde_json::json!({
            "progress_processed": 4,
            "progress_total": 65,
        })
        .to_string();
        let report = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/16/refreshMemberSummaries/for/do/0/reportProgress/set",
            "output": progress,
        })
        .to_string();
        let (d, carried) = summarize_workflow_chunk(&report, None);
        assert_eq!(d, "(4/65) メンバー要約を更新中 · 進捗を更新中");
        assert_eq!(carried, Some((4, 65)));

        // The member's single call itself carries no counter.
        let refresh = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/16/refreshMemberSummaries/for/do/1/refreshMember/try/do/0/invokeMemberSingle/run/function",
            "output": "{}",
        })
        .to_string();
        let (d, _) = summarize_workflow_chunk(&refresh, carried);
        assert_eq!(d, "(4/65) メンバー要約を更新中");
    }

    #[test]
    fn summarize_workflow_chunk_shows_progress_counter() {
        // When the loop context carries progress, lead with `(N/M)` + step.
        // `output` is a JSON *string* (the context snapshot), so build it via
        // serde to get the escaping exactly right.
        let output = serde_json::json!({
            "progress_processed": 3,
            "progress_total": 36,
            "target_threads": ["...elided..."]
        })
        .to_string();
        let chunk = serde_json::json!({
                "status": "Running",
                "position": "/ROOT/do/6/summarizeEach/for/do/0/invokeSingleWithRetry/try/do/0/invokeSingle/run/do/8/generateSummary/run",
                "output": output,
            })
            .to_string();
        let (d, p) = summarize_workflow_chunk(&chunk, None);
        assert_eq!(d, "(3/36) 要約を生成中 (LLM)");
        assert_eq!(p, Some((3, 36)));
    }

    #[test]
    fn summarize_workflow_chunk_carries_progress_to_later_chunks() {
        // The counter only appears on the reportProgress chunk; a following
        // invokeSingle chunk (no progress in output) must still show (N/M) by
        // carrying the previous value forward.
        let no_progress = serde_json::json!({
                "status": "Running",
                "position": "/ROOT/do/6/summarizeEach/for/do/0/invokeSingleWithRetry/try/do/0/invokeSingle/run/do/8/generateSummary/run",
                "output": "{\"thread\":{}}",
            })
            .to_string();
        // Without a carried value: no counter, just the step.
        let (d0, _) = summarize_workflow_chunk(&no_progress, None);
        assert_eq!(d0, "Running · 要約を生成中 (LLM)");
        // With a carried value from an earlier reportProgress chunk: counter shown.
        let (d1, p1) = summarize_workflow_chunk(&no_progress, Some((4, 36)));
        assert_eq!(d1, "(4/36) 要約を生成中 (LLM)");
        assert_eq!(p1, Some((4, 36)), "carried value must persist");
    }

    #[test]
    fn summarize_workflow_chunk_progress_accepts_string_numbers() {
        // jq `set` may serialize the counter as numeric strings; accept those.
        let output = r#"{"progress_processed":"5","progress_total":"10"}"#;
        let chunk = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/6/summarizeEach",
            "output": output,
        })
        .to_string();
        assert_eq!(digest(&chunk), "(5/10) スレッドを要約中");
    }

    #[test]
    fn extract_progress_rejects_zero_total() {
        // `(n/0)` is meaningless — treat as no progress so the caller falls
        // back to the status/step digest.
        assert_eq!(
            extract_progress(r#"{"progress_processed":1,"progress_total":0}"#),
            None
        );
        assert_eq!(extract_progress(r#"{"progress_processed":2}"#), None);
        assert_eq!(extract_progress("{}"), None);
    }

    #[test]
    fn summarize_workflow_chunk_status_only_when_no_position() {
        let raw = r#"{"status":"Completed","output":"{}"}"#;
        assert_eq!(digest(raw), "Completed");
    }

    /// Build a fake `WorkflowResult` terminal chunk whose `output` is the
    /// batch's `output:` block serialized as a JSON string — same shape as
    /// what the WORKFLOW runner emits on the wire.
    fn done_chunk(output: &serde_json::Value) -> String {
        let output_str = output.to_string();
        let env = serde_json::json!({
            "id": "wf-id",
            "status": "Completed",
            "position": "/ROOT",
            "output": output_str,
            "errorMessage": null,
        });
        env.to_string()
    }

    #[test]
    fn terminal_batch_outcome_reflects_failed_items_instead_of_always_done() {
        let all_ok = done_chunk(&serde_json::json!({
            "completed": true, "failed_count": 0, "succeeded_count": 3, "processed_dates": 3
        }));
        assert_eq!(
            terminal_batch_outcome(Some(&all_ok), None).0,
            StepStatus::Done
        );

        let partial = done_chunk(&serde_json::json!({
            "completed": false, "failed_count": 1, "succeeded_count": 2, "processed_dates": 3,
            "last_error": "1 item(s) ended without an outcome (timed out or aborted)"
        }));
        let (status, message) = terminal_batch_outcome(Some(&partial), None);
        assert_eq!(status, StepStatus::Warning);
        assert!(message.unwrap().contains("ended without an outcome"));
        assert_eq!(terminal_batch_outcome(None, None), (StepStatus::Done, None));
    }

    #[test]
    fn terminal_batch_outcome_shows_empty_target_and_path_only_warnings() {
        let empty = done_chunk(&serde_json::json!({
            "completed": true,
            "outcome": "succeeded",
            "processed_dates": 0,
            "failed_count": 0,
        }));
        assert_eq!(
            terminal_batch_outcome(Some(&empty), None),
            (StepStatus::Done, Some("対象なし".into()))
        );

        let path_only = done_chunk(&serde_json::json!({
            "completed": true,
            "outcome": "completed_with_errors",
            "processed_dates": 2,
            "failed_count": 0,
            "failed_path_buckets": [{"period_key": "2026-10-13"}],
        }));
        let (status, message) = terminal_batch_outcome(Some(&path_only), None);
        assert_eq!(status, StepStatus::Warning);
        assert!(message.unwrap().contains("path"));
    }

    #[test]
    fn staged_summary_terminal_outcomes_reject_missing_output_and_report_empty_or_path_only() {
        assert_eq!(
            summary_stage_terminal_outcome(r#"{"status":"Completed"}"#, ImportStep::Daily, None)
                .status,
            StepStatus::Failed
        );

        let empty_group = done_chunk(&serde_json::json!({
            "completed": true,
            "processed_groups": 0,
            "generated_count": 0,
            "failed_count": 0,
        }));
        assert_eq!(
            summary_stage_terminal_outcome(&empty_group, ImportStep::ThreadGroupSummary, None),
            StepOutcome {
                status: StepStatus::Done,
                message: Some("対象なし".into()),
            }
        );

        let path_only = done_chunk(&serde_json::json!({
            "completed": true,
            "outcome": "completed_with_errors",
            "processed_dates": 1,
            "failed_count": 0,
            "failed_path_buckets": [{"period_key": "2026-10-13"}],
        }));
        let outcome = summary_stage_terminal_outcome(&path_only, ImportStep::Daily, None);
        assert_eq!(outcome.status, StepStatus::Warning);
        assert!(outcome.message.unwrap().contains("path"));
        assert!(workflow_output_has_failures(&serde_json::json!({
            "completed": true,
            "outcome": "completed_with_errors",
        })));
    }

    #[test]
    fn incomplete_thread_group_terminal_output_is_not_reported_as_empty_success() {
        let incomplete = done_chunk(&serde_json::json!({
            "completed": false,
            "processed_groups": 0,
            "generated_count": 0,
            "failed_count": 0,
        }));
        let outcome =
            summary_stage_terminal_outcome(&incomplete, ImportStep::ThreadGroupSummary, None);
        assert_eq!(outcome.status, StepStatus::Failed);
        assert!(outcome.message.unwrap().contains("失敗"));
    }

    #[test]
    fn incomplete_period_terminal_output_without_accounted_failures_is_a_failure() {
        let incomplete = done_chunk(&serde_json::json!({
            "completed": false,
            "failed_count": 0,
            "processed_dates": 0,
        }));
        let outcome = summary_stage_terminal_outcome(&incomplete, ImportStep::Daily, None);
        assert_eq!(outcome.status, StepStatus::Failed);
        assert!(outcome.message.unwrap().contains("未完了"));
    }

    #[test]
    fn workflow_terminal_failure_classifies_each_status() {
        assert_eq!(
            workflow_terminal_failure(r#"{"status":"Completed","output":"{}"}"#),
            None
        );
        assert_eq!(workflow_terminal_failure("not json"), None);
        assert_eq!(
            workflow_terminal_failure(
                r#"{"status":"Cancelled","position":"/ROOT/do/1/dailyStage"}"#
            )
            .as_deref(),
            Some("dailyStage で中断されました")
        );
        assert_eq!(
            workflow_terminal_failure(r#"{"status":"Faulted","errorMessage":"boom"}"#).as_deref(),
            Some("ワークフロー で失敗しました: boom")
        );
        assert_eq!(
            workflow_terminal_failure(r#"{"status":"Faulted"}"#).as_deref(),
            Some("ワークフロー で失敗しました: 原因不明のエラー")
        );
        let long = "x".repeat(400);
        let capped = workflow_terminal_failure(
            &serde_json::json!({"status": "Faulted", "errorMessage": long}).to_string(),
        )
        .unwrap();
        assert!(capped.ends_with("… (400 chars total)"));
    }

    #[test]
    fn workflow_terminal_failure_names_the_timed_out_stage_path() {
        let faulted = serde_json::json!({
            "status": "Faulted",
            "position": "/ROOT/do/3/threadGroupStage/do/0/runThreadGroupBatch/try/do/0/invokeThreadGroupBatch/run/function",
            "errorMessage": "Failed to execute workflow: Error { detail: None, status: 408, title: Some(\"Task 'runThreadGroupBatch' timed out after 3600s\"), type_: UriTemplate(\"http-error://requesttimeout\") }",
        })
        .to_string();
        assert_eq!(
            workflow_terminal_failure(&faulted).as_deref(),
            Some(
                "threadGroupStage › runThreadGroupBatch › invokeThreadGroupBatch で失敗しました: \
                 Task 'runThreadGroupBatch' timed out after 3600s"
            )
        );
    }

    // The current server renders `errorMessage` with Display, which joins
    // title, detail and position with bare spaces; the problem the stream's
    // End attached is the structured reason.
    #[test]
    fn workflow_terminal_failure_prefers_the_attached_problem() {
        let faulted = serde_json::json!({
            "status": "Faulted",
            "position": "/ROOT/do/2/summarizeEach/run/function",
            "errorMessage": "Failed to execute workflow: Error: Failed to execute by jobworkerp runner 'X' not found /ROOT/do/2",
            "problem": {
                "type": "https://open-workflow-specification.org/spec/1.0.0/errors/runtime",
                "status": 500,
                "title": "Failed to execute by jobworkerp",
                "detail": "runner 'X' not found",
            },
        })
        .to_string();
        assert_eq!(
            workflow_terminal_failure(&faulted).as_deref(),
            Some(
                "summarizeEach で失敗しました: Failed to execute by jobworkerp: runner 'X' not found"
            )
        );
        let long = serde_json::json!({
            "status": "Faulted",
            "problem": {"type": "urn:t", "title": "t", "detail": "d".repeat(400)},
        })
        .to_string();
        assert!(
            workflow_terminal_failure(&long)
                .unwrap()
                .ends_with("… (403 chars total)")
        );
    }

    // Without a problem (older servers) the Display rendering cannot be split,
    // so it is shown as is.
    #[test]
    fn workflow_terminal_failure_shows_a_display_rendering_whole() {
        let message = "Failed to execute workflow: Error: Task 'summarizeEach' timed out after 43200s No details /ROOT/do/2/summarizeEach";
        let faulted = serde_json::json!({
            "status": "Faulted",
            "position": "/ROOT/do/2/summarizeEach/run/function",
            "errorMessage": message,
        })
        .to_string();
        assert_eq!(
            workflow_terminal_failure(&faulted),
            Some(format!("summarizeEach で失敗しました: {message}"))
        );
    }

    #[test]
    fn summarize_workflow_outcome_fails_a_faulted_workflow_with_its_stage() {
        let faulted = serde_json::json!({
            "id": "error",
            "status": "Faulted",
            "position": "/ROOT/do/2/summarizeEach/run/function",
            "output": "",
            "errorMessage": "Failed to execute workflow: Error { title: Some(\"Task 'summarizeEach' timed out after 43200s\") }",
        })
        .to_string();
        let outcome = summarize_workflow_outcome(Some(&faulted));
        assert_eq!(outcome.status, StepStatus::Failed);
        assert_eq!(
            outcome.message.as_deref(),
            Some("summarizeEach で失敗しました: Task 'summarizeEach' timed out after 43200s")
        );
    }

    #[test]
    fn summarize_workflow_outcome_done_when_no_failures() {
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
            "succeeded_count": 12,
            "failed_count": 0,
            "processed_threads": 12,
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Done);
        assert!(outcome.message.is_none());
    }

    #[test]
    fn summarize_workflow_outcome_warning_when_partial_failure() {
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "succeeded_count": 9,
            "failed_count": 3,
            "processed_threads": 12,
            "last_error": "429 Too Many Requests",
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Warning);
        let msg = outcome.message.expect("warning carries a digest");
        assert!(msg.contains("成功 9"));
        assert!(msg.contains("失敗 3"));
        assert!(msg.contains("429"));
    }

    #[test]
    fn summarize_workflow_outcome_accepts_numeric_and_string_failed_counts() {
        for failed_count in [serde_json::json!(3), serde_json::json!("3")] {
            let raw = done_chunk(&serde_json::json!({
                "succeeded_count": 9,
                "failed_count": failed_count,
                "processed_threads": 12,
            }));

            assert_eq!(
                summarize_workflow_outcome(Some(&raw)).status,
                StepStatus::Warning
            );
        }
    }

    #[test]
    fn summarize_workflow_outcome_failed_when_every_item_died() {
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "succeeded_count": 0,
            "failed_count": 12,
            "processed_threads": 12,
            "last_error": "RelativeUrlWithoutBase",
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        // Every input failed — the batch ran to completion technically, but
        // there's no successful output; surface it as a hard step failure.
        assert_eq!(outcome.status, StepStatus::Failed);
        let msg = outcome.message.expect("failed carries a digest");
        assert!(msg.contains("失敗 12"));
    }

    #[test]
    fn summarize_workflow_outcome_classifies_all_failed_groups_as_failed() {
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "generated_count": 0,
            "failed_count": 2,
            "processed_groups": 2,
        }));

        let outcome = summarize_workflow_outcome(Some(&raw));

        assert_eq!(outcome.status, StepStatus::Failed);
        assert!(outcome.message.unwrap().contains("合計 2"));
    }

    #[test]
    fn summarize_workflow_outcome_uses_group_failure_list_when_count_is_absent() {
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "generated_count": 1,
            "processed_groups": 2,
            "failed_groups": [{"group_id": "7"}],
        }));

        assert_eq!(
            summarize_workflow_outcome(Some(&raw)).status,
            StepStatus::Warning
        );
    }

    #[test]
    fn summarize_workflow_outcome_handles_period_batch_counter_names() {
        // daily / weekly / monthly batches use `processed_dates` /
        // `processed_weeks` / `processed_months` instead of
        // `processed_threads`. The classifier must accept all three so a
        // period batch failure is rendered correctly.
        let raw = done_chunk(&serde_json::json!({
            "succeeded_count": 0,
            "failed_count": 7,
            "processed_dates": 7,
        }));
        assert_eq!(
            summarize_workflow_outcome(Some(&raw)).status,
            StepStatus::Failed
        );
    }

    #[test]
    fn summarize_workflow_outcome_falls_back_to_done_when_counters_missing() {
        // A batch YAML that hasn't been updated yet (or whose `output:` was
        // dropped) — don't downgrade to Warning, the previous behaviour wins.
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
        }));
        assert_eq!(
            summarize_workflow_outcome(Some(&raw)).status,
            StepStatus::Done
        );
    }

    #[test]
    fn summarize_workflow_outcome_done_attaches_skipped_digest() {
        // Personality batch's normal path: every NEW thread extracted
        // successfully, plus the budget gate decided not to re-extract
        // several already-processed threads. The step must stay green
        // (Done) but the toast should still surface the "中略 N 件" line so
        // the user knows the existing-signal cap silenced part of the run
        // — same information the mid-flight `skipIfBudgetExhausted` label
        // used to (mis)claim.
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
            "succeeded_count": 12,
            "failed_count": 0,
            "processed_threads": 12,
            "budget_skipped_count": 3,
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Done);
        let msg = outcome.message.expect("skipped count carries a digest");
        assert!(msg.contains("中略 3"));
    }

    #[test]
    fn summarize_workflow_outcome_warning_includes_skipped_when_present() {
        // The warning path must surface BOTH the partial-failure counter
        // and the skip counter so the detail dialog reflects the full
        // breakdown (success / failure / skipped).
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "succeeded_count": 9,
            "failed_count": 3,
            "processed_threads": 14,
            "budget_skipped_count": 2,
            "last_error": "429 Too Many Requests",
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Warning);
        let msg = outcome.message.expect("warning carries a digest");
        assert!(msg.contains("成功 9"));
        assert!(msg.contains("失敗 3"));
        assert!(msg.contains("中略 2"));
        assert!(msg.contains("429"));
    }

    #[test]
    fn summarize_workflow_outcome_done_skips_skipped_digest_when_zero() {
        // budget_skipped_count == 0 means the budget gate never fired —
        // don't pollute the toast with a "中略 0 件" line.
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
            "succeeded_count": 12,
            "failed_count": 0,
            "processed_threads": 12,
            "budget_skipped_count": 0,
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Done);
        assert!(outcome.message.is_none());
    }

    #[test]
    fn summarize_workflow_outcome_done_for_none_or_invalid_chunk() {
        // No chunk at all (a stream that closed without a final body) /
        // non-JSON chunk — treat as Done (the workflow itself reported no
        // error). The latter still surfaces the raw text in the message so
        // it isn't silently swallowed.
        assert_eq!(summarize_workflow_outcome(None).status, StepStatus::Done);
        let outcome = summarize_workflow_outcome(Some("not json"));
        assert_eq!(outcome.status, StepStatus::Done);
        assert_eq!(outcome.message.as_deref(), Some("not json"));
    }

    #[test]
    fn summarize_workflow_chunk_falls_back_for_non_json() {
        // Plain (non-JSON) chunk passes through, length-capped.
        assert_eq!(digest("dispatching x"), "dispatching x");
    }

    #[test]
    fn describe_position_maps_known_deepest_step() {
        assert_eq!(
            describe_position("/ROOT/do/6/summarizeEach").as_deref(),
            Some("スレッドを要約中")
        );
        assert_eq!(
            describe_position("/ROOT/do/8/generateSummary/run").as_deref(),
            Some("要約を生成中 (LLM)")
        );
    }

    #[test]
    fn describe_position_maps_personality_batch_budget_steps() {
        // The early-break path adds new steps in thread-personality-batch
        // that the digest must label, or the toast falls back to the raw
        // step name (looking like a workflow bug).
        assert_eq!(
            describe_position("/ROOT/do/6/sortTargetThreadsNewestFirst").as_deref(),
            Some("対象スレッドを並べ替え中"),
        );
        // Renamed from `countExistingValidSignals` — the step now fetches
        // both the count and the processed-thread set used by the
        // per-iteration `classifyThread` guard.
        assert_eq!(
            describe_position("/ROOT/do/7/fetchProcessedSignalMemories").as_deref(),
            Some("既存シグナルを確認中"),
        );
        assert_eq!(
            describe_position("/ROOT/do/8/initSignalBudget").as_deref(),
            Some("処理上限を設定中"),
        );
        assert_eq!(
            describe_position("/ROOT/do/9/personalityEach/for/do/1/classifyThread").as_deref(),
            Some("対象スレッドを判定中"),
        );
        // Regression guard: this step runs for EVERY iteration of the
        // personality batch loop (only the `set:` body is `if:`-gated, the
        // step itself emits a position chunk regardless), so a label that
        // claimed "省略中" would lie to the user whenever a NEW thread was
        // about to be extracted. The label must stay neutral.
        assert_eq!(
            describe_position("/ROOT/do/9/personalityEach/for/do/2/skipIfBudgetExhausted")
                .as_deref(),
            Some("再抽出要否を判定中"),
        );
    }

    #[test]
    fn describe_position_maps_thread_group_summary_steps() {
        assert_eq!(
            describe_position("/ROOT/do/0/fetchGroups").as_deref(),
            Some("対象グループを取得中")
        );
        assert_eq!(
            describe_position("/ROOT/do/2/summarizeEachGroup/for/do/0/recordFailed").as_deref(),
            Some("グループ要約のエラーを記録中")
        );
        for (position, label) in [
            ("/ROOT/do/9/selectTargetGroups", "対象グループを整理中"),
            (
                "/ROOT/do/11/collectGroupMembers/for/0/do/0/readGroupMembers/try/1/fetchLineage/run",
                "グループ構成を取得中",
            ),
            (
                "/ROOT/do/11/collectGroupMembers/for/0/do/0/readGroupMembers/try/2/recordGroupMembers/set",
                "グループメンバーを取得中",
            ),
            (
                "/ROOT/do/13/buildMemberRefreshTargets/set",
                "メンバー要約を更新中",
            ),
            (
                "/ROOT/do/14/refreshMemberSummaries/for/0/do/1/refreshMember/try/0/invokeMemberSingle/run",
                "メンバー要約を更新中",
            ),
            (
                "/ROOT/do/14/refreshMemberSummaries/for/0/do/1/refreshMember/try/1/collectMemberRefreshResult/set",
                "メンバー要約を更新中",
            ),
            (
                "/ROOT/do/14/refreshMemberSummaries/for/0/do/1/refreshMember/catch/do/0/recordMemberRefreshFailure/set",
                "エラーを記録中",
            ),
            (
                "/ROOT/do/14/summarizeEachGroup/for/0/do/1/summarizeGroup/try/1/invokeSingle/run",
                "グループ要約を生成中",
            ),
            (
                "/ROOT/do/14/summarizeEachGroup/for/0/do/1/summarizeGroup/try/4/recordOutcome/set",
                "グループ要約を記録中",
            ),
        ] {
            assert_eq!(
                describe_position(position).as_deref(),
                Some(label),
                "{position}"
            );
        }
        // Outside the group loop the per-item single keeps its generic label.
        assert_eq!(
            describe_position("/ROOT/do/6/summarizeEach/for/0/do/1/invokeSingle/run").as_deref(),
            Some("個別スレッドを処理中")
        );
    }

    #[test]
    fn describe_position_maps_layer_two_recent_signals_step() {
        // findRecentSignals replaces the legacy findSignalThreads +
        // fetchSignalMemories pair. Both legacy names are deliberately
        // dropped from the table; if the toast still renders them, the
        // workflow YAML is out of sync.
        assert_eq!(
            describe_position("/ROOT/do/5/findRecentSignals").as_deref(),
            Some("シグナルを取得中"),
        );
        // Legacy names must NOT map any more — if they did, a toast
        // surfacing them would hide a stale YAML.
        assert_eq!(
            describe_position("/ROOT/do/5/findSignalThreads").as_deref(),
            Some("findSignalThreads"),
        );
        assert_eq!(
            describe_position("/ROOT/do/5/fetchSignalMemories").as_deref(),
            Some("fetchSignalMemories"),
        );
    }

    #[test]
    fn describe_position_passes_unknown_step_through() {
        // An unmapped step name (e.g. a future workflow edit) still shows
        // *something* rather than vanishing.
        assert_eq!(
            describe_position("/ROOT/do/0/someBrandNewStep").as_deref(),
            Some("someBrandNewStep")
        );
    }

    #[test]
    fn describe_position_none_when_only_structural() {
        // Pointer with no meaningful step (only ROOT/do/index) → None so the
        // caller shows the bare status.
        assert_eq!(describe_position("/ROOT/do/3"), None);
        assert_eq!(describe_position(""), None);
    }

    #[test]
    fn is_structural_segment_classifies_scaffolding_and_indices() {
        for s in [
            "ROOT", "do", "for", "try", "catch", "then", "else", "run", "0", "42",
        ] {
            assert!(is_structural_segment(s), "{s} should be structural");
        }
        for s in ["summarizeEach", "generateSummary", "logError"] {
            assert!(!is_structural_segment(s), "{s} should be a step name");
        }
    }

    #[test]
    fn summarize_workflow_error_prefers_error_message_capped() {
        let big = "x".repeat(1000);
        let raw = format!(r#"{{"status":"Faulted","errorMessage":"{big}"}}"#);
        let out = summarize_workflow_error(&raw);
        assert!(out.starts_with(&"x".repeat(300)));
        assert!(out.contains("chars total"));
        assert!(out.len() < big.len());
    }

    #[test]
    fn summarize_workflow_error_uses_status_when_no_message() {
        let raw = r#"{"status":"Cancelled"}"#;
        assert_eq!(summarize_workflow_error(raw), "Cancelled");
    }

    #[test]
    fn cap_leaves_short_strings_untouched() {
        assert_eq!(cap("short", 200), "short");
    }

    #[test]
    fn cap_truncates_and_reports_length() {
        let out = cap(&"a".repeat(10), 4);
        assert_eq!(out, "aaaa… (10 chars total)");
    }

    #[test]
    fn extract_summary_block_captures_label_and_indented_body() {
        let lines: Vec<String> = [
            "Starting import…",
            "Processing session abc",
            "",
            "[dry-run] claude-code summary:",
            "  Sessions processed: 587",
            "  Threads created: 587",
            "  Memories imported: 12345",
            "  Memories skipped (duplicate): 10",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = extract_summary_block(&lines).expect("summary block present");
        assert_eq!(
            out,
            "[dry-run] claude-code summary:\n  Sessions processed: 587\n  Threads created: 587\n  Memories imported: 12345\n  Memories skipped (duplicate): 10"
        );
    }

    #[test]
    fn extract_summary_block_stops_at_non_indented_line() {
        let lines: Vec<String> = [
            "claude-code summary:",
            "  Sessions processed: 1",
            "thread-summary-batch dispatch:",
            "  irrelevant: true",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = extract_summary_block(&lines).unwrap();
        assert_eq!(out, "claude-code summary:\n  Sessions processed: 1");
    }

    #[test]
    fn extract_summary_block_returns_none_when_no_marker() {
        let lines: Vec<String> = ["nothing", "to see"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(extract_summary_block(&lines).is_none());
    }

    #[test]
    fn extract_summary_block_uses_last_marker_for_multi_source_output() {
        let lines: Vec<String> = [
            "[dry-run] claude-code summary:",
            "  Sessions processed: 5",
            "[dry-run] codex summary:",
            "  Sessions processed: 3",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = extract_summary_block(&lines).unwrap();
        assert_eq!(out, "[dry-run] codex summary:\n  Sessions processed: 3");
    }
}
