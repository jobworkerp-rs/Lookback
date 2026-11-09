use super::super::connection::GrpcAddress;
use super::super::{AppError, AppResult};
use super::guards::{REFLECTION_PROMPT_VERSION, summary_generation_id};
use crate::jobworkerp::job_timeouts;

const PERSONALITY_MAX_CONTEXT_CHARS: i64 = 150_000;
const PERSONALITY_MERGE_MAX_SIGNALS: i64 = 100;

/// Period-summary granularity. Each kind reads the layer below's output and
/// writes its own: daily aggregates per-thread `summary` memories, weekly
/// aggregates `daily_summary`, monthly aggregates `weekly_summary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeriodKind {
    Daily,
    Weekly,
    Monthly,
}

/// All per-kind constants for a period layer, kept in one table so the
/// worker name, workflow path, window field, and the source→output label
/// chain can't drift apart. Field keys (`*_label_key`) match the batch
/// workflow's input schema.
struct PeriodSpec {
    worker_name: &'static str,
    job_timeout_sec: u64,
    job_prefix: &'static str,
    last_n_field: &'static str,
    source_label_key: &'static str,
    source_label: &'static str,
    out_label_key: &'static str,
    out_label: &'static str,
}

impl PeriodKind {
    fn spec(self) -> PeriodSpec {
        match self {
            PeriodKind::Daily => PeriodSpec {
                worker_name: "memories-daily-summary-batch",
                job_timeout_sec: job_timeouts::DAILY_SUMMARY_BATCH,
                job_prefix: "daily",
                last_n_field: "last_n_days",
                source_label_key: "summary_label",
                source_label: "summary",
                out_label_key: "daily_label",
                out_label: "daily_summary",
            },
            PeriodKind::Weekly => PeriodSpec {
                worker_name: "memories-weekly-summary-batch",
                job_timeout_sec: job_timeouts::WEEKLY_SUMMARY_BATCH,
                job_prefix: "weekly",
                last_n_field: "last_n_weeks",
                source_label_key: "daily_label",
                source_label: "daily_summary",
                out_label_key: "weekly_label",
                out_label: "weekly_summary",
            },
            PeriodKind::Monthly => PeriodSpec {
                worker_name: "memories-monthly-summary-batch",
                job_timeout_sec: job_timeouts::MONTHLY_SUMMARY_BATCH,
                job_prefix: "monthly",
                last_n_field: "last_n_months",
                source_label_key: "weekly_label",
                source_label: "weekly_summary",
                out_label_key: "monthly_label",
                out_label: "monthly_summary",
            },
        }
    }

    /// Worker name the dispatch targets (`memories-{kind}-summary-batch`).
    pub fn worker_name(self) -> &'static str {
        self.spec().worker_name
    }

    /// Server-side job deadline for this layer's batch worker.
    pub fn job_timeout_sec(self) -> u64 {
        self.spec().job_timeout_sec
    }

    /// `job_id` prefix used to correlate the `summary://step` progress slot.
    pub fn job_prefix(self) -> &'static str {
        self.spec().job_prefix
    }
}

/// Staged `generate_summaries` request. The frontend already expanded the
/// dialog's range into per-layer inputs; Rust forwards them verbatim.
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct GenerateSummariesRequest {
    pub user_id: Option<i64>,
    pub run_per_thread: bool,
    /// ThreadGroup aggregation is opt-in for older manual clients.
    #[serde(default)]
    pub run_thread_group: bool,
    pub run_daily: bool,
    pub run_weekly: bool,
    pub run_monthly: bool,
    /// Skip later selected stages after the first observed failure.
    /// Absent clients retain the legacy best-effort behaviour.
    #[serde(default)]
    pub stop_on_error: bool,
    /// Per-thread window (epoch ms). `None` = unbounded.
    pub last_message_after_ms: Option<i64>,
    pub last_message_before_ms: Option<i64>,
    /// Exclusive epoch-ms window selecting the ThreadGroups to summarize
    /// (groups with a member active in the period). `None` = every group.
    #[serde(default)]
    pub thread_group_activity_after_ms: Option<i64>,
    #[serde(default)]
    pub thread_group_activity_before_ms: Option<i64>,
    /// Period range tokens; empty string = no range (batch falls back).
    #[serde(default)]
    pub daily_start: String,
    #[serde(default)]
    pub daily_end: String,
    #[serde(default)]
    pub weekly_start: String,
    #[serde(default)]
    pub weekly_end: String,
    #[serde(default)]
    pub monthly_start: String,
    #[serde(default)]
    pub monthly_end: String,
    /// Day-boundary tz for the period single workflows (not used for the
    /// range expansion, which is UTC-only).
    pub timezone_offset_hours: i32,
    /// Frontend-supplied dispatch id used as the cancel key. Older
    /// callers omit it; the analysis dispatch falls back to a
    /// timestamp-shaped id when absent.
    #[serde(default)]
    pub dispatch_id: Option<String>,
}

/// Metadata shared by every child batch in one staged summary generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryGenerationMetadata {
    pub generation_id: String,
    pub generation_started_at: String,
    pub source_scope_digest: String,
}

/// Which periods a batch run targets. `Auto` lets the batch pick its default
/// fallback ("last completed period only"); `LastN` back-fills N periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodRange {
    Auto,
    LastN(i32),
}

impl PeriodRange {
    /// Value for the batch's `last_n_*` field (0 = Auto fallback).
    fn last_n(self) -> i32 {
        match self {
            PeriodRange::Auto => 0,
            PeriodRange::LastN(n) => n,
        }
    }
}

/// Resolved workflow input builders for the 3 batch workers. Pure JSON
/// construction so the tests can pin the wire-shape without spawning
/// a sidecar. `pub(super)` so the standalone `analysis_dispatch` commands
/// can reuse `summarize_input` / `personality_input` as the single source
/// of truth for the workflow-input shape (which must match the YAML schema).
pub struct BatchDispatch {
    pub(super) user_id: i64,
    /// memories gRPC callback coordinates the fanned-out workflows dial back
    /// at. Carries host/port/tls so a remote (incl. HTTPS) target propagates
    /// into the workflow input, not just into the app's own gRPC clients.
    pub(super) callback: GrpcAddress,
    /// Resolved once for this dispatch so policy and its runtime toggle use
    /// the same data root, even when a bootstrap override is configured.
    pub(super) data_paths: Option<crate::data::DataPaths>,
    /// Keep fixture-only roots alive for the duration of unit tests that return
    /// a dispatch value containing the root.
    #[cfg(test)]
    pub(super) _test_data_root: Option<tempfile::TempDir>,
    /// Optional conversation-time lower bound for manual batch dispatch.
    /// Import dispatch instead fills the thread-created window below.
    pub(super) last_message_after_ms: Option<i64>,
    /// Optional inclusive upper bound forwarded into the per-thread
    /// summary batch's `last_message_before_ms`. Set only by the range-mode
    /// generate dialog; `None` keeps the legacy "no upper bound" behaviour.
    pub(super) last_message_before_ms: Option<i64>,
    pub(super) thread_created_after_ms: Option<i64>,
    pub(super) thread_created_before_ms: Option<i64>,
    pub(super) target_thread_ids: Option<Vec<String>>,
    pub(super) rejected_candidate_ids_to_unsuppress_by_thread:
        Option<std::collections::BTreeMap<String, Vec<String>>>,
    /// Named worker that handles LLM completion. Defaults to `memories-llm`
    /// (local) or `memories-llm-external` (genai) based on the active LLM
    /// settings. Propagated to every workflow input as `llm_worker_name`.
    pub(super) llm_worker_name: String,
    /// Output language ("ja" | "en") forwarded as `output_language` so each
    /// batch resolves the matching language-specific single worker
    /// (`memories-<feature>-single-<lang>`).
    pub(super) output_language: String,
    /// Prompt-only writing targets selected from the active model context.
    pub(super) summary_output_guidance: serde_json::Value,
    /// Dispatch-time LLM contract. The single-summary workflows use this for
    /// exact `token_count` budgeting and reject a count performed by a
    /// different resolved model.
    pub(super) summary_llm_contract: SummaryLlmContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryBudget {
    pub max_completion_tokens: u32,
    pub input_byte_budget: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLlmContract {
    pub context_window_tokens: u32,
    pub expected_model: String,
    pub budget: SummaryBudget,
}

/// Resolve prompt-only period-summary guidance from the same effective context
/// contract used for the runner budget. Manual and periodic dispatch share it
/// so their LLM writing targets cannot drift after a settings change.
pub fn summary_output_guidance_for_context(
    guidance: &crate::commands::llm_settings::SummaryOutputGuidance,
    context_window_tokens: u32,
) -> serde_json::Value {
    let profile = crate::commands::llm_settings::summary_guide_profile_for_context(
        guidance,
        context_window_tokens,
    );
    serde_json::json!({
        "daily": {"item_chars": profile.daily.item_chars, "total_chars": profile.daily.total_chars},
        "weekly": {"item_chars": profile.weekly.item_chars, "total_chars": profile.weekly.total_chars},
        "monthly": {"item_chars": profile.monthly.item_chars, "total_chars": profile.monthly.total_chars},
        "max_purpose_groups": {
            "daily": guidance.limits.daily,
            "weekly": guidance.limits.weekly,
            "monthly": guidance.limits.monthly,
        },
        "max_source_memory_ids": guidance.limits.max_source_memory_ids,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryBudgetRequirement {
    RequiredForPeriodSummaries,
    SchemaPlaceholder,
}

// Exact counting is unavailable for some provider-side structured-output
// protocols. Reserve this fixed envelope for the prompt, response schema, and
// provider framing when the workflow must fall back to a byte-bounded source
// selection.
pub(super) const SUMMARY_SAFETY_MARGIN_TOKENS: u32 = 2_048;
pub(super) const SUMMARY_MIN_INPUT_TOKENS: u32 = 2_048;
pub(super) const SUMMARY_MIN_COMPLETION_TOKENS: u32 = 1_024;
pub(super) const SUMMARY_MIN_CONTEXT_TOKENS: u32 =
    SUMMARY_SAFETY_MARGIN_TOKENS + SUMMARY_MIN_INPUT_TOKENS + SUMMARY_MIN_COMPLETION_TOKENS;

/// Splits the dispatch-time context contract into a completion reservation and
/// a conservative source-selection budget. A context too small to retain both
/// source evidence and a complete structured result is rejected before a
/// workflow could request a one-token completion.
pub(super) fn summary_token_budget(context_window_tokens: u32) -> AppResult<SummaryBudget> {
    if context_window_tokens < SUMMARY_MIN_CONTEXT_TOKENS {
        return Err(AppError::Config(format!(
            "summary generation requires at least {SUMMARY_MIN_CONTEXT_TOKENS} context tokens (configured: {context_window_tokens})"
        )));
    }
    let desired_reserve =
        8_192u32.max((context_window_tokens.saturating_mul(25).saturating_add(99)) / 100);
    let max_reserve = context_window_tokens
        .saturating_sub(SUMMARY_SAFETY_MARGIN_TOKENS)
        .saturating_sub(SUMMARY_MIN_INPUT_TOKENS);
    let completion_tokens = desired_reserve
        .min(max_reserve)
        .max(SUMMARY_MIN_COMPLETION_TOKENS);
    // A UTF-8 byte is the conservative unit for the no-exact-count path: a
    // provider tokenizer cannot consume more tokens than source bytes. Do not
    // multiply this residual by an average bytes/token ratio, which would turn
    // an estimate into an unsafe upper bound for Japanese or code.
    let input_byte_budget = context_window_tokens
        .saturating_sub(completion_tokens.saturating_add(SUMMARY_SAFETY_MARGIN_TOKENS));
    Ok(SummaryBudget {
        max_completion_tokens: completion_tokens,
        input_byte_budget,
    })
}

/// Per-thread import, personality, and reflection jobs do not consume the
/// period-summary token fields. Keep those ordinary paths available for a
/// configured small context; period dispatches opt into the strict budget
/// check below.
pub(super) fn summary_budget_for(
    context_window_tokens: u32,
    requirement: SummaryBudgetRequirement,
) -> AppResult<SummaryBudget> {
    match summary_token_budget(context_window_tokens) {
        Ok(budget) => Ok(budget),
        // Thread-summary inputs carry the period budget fields for schema
        // compatibility even when no period stage is selected.
        // `1` is the smallest valid, unused placeholder for that case.
        Err(_) if requirement == SummaryBudgetRequirement::SchemaPlaceholder => Ok(SummaryBudget {
            max_completion_tokens: 1,
            input_byte_budget: 0,
        }),
        Err(error) => Err(error),
    }
}

/// Resolve the summary worker contract with the same precedence used when the
/// sidecar injects its local LLM environment at startup.
pub fn summary_llm_contract_with<F>(
    settings: &crate::commands::llm_settings::LlmSettings,
    env_lookup: F,
    requirement: SummaryBudgetRequirement,
) -> AppResult<SummaryLlmContract>
where
    F: Fn(&str) -> Option<String>,
{
    let (context_window_tokens, expected_model) = match settings.mode {
        crate::commands::llm_settings::LlmMode::Local => {
            let runtime = crate::commands::llm_settings::resolve_local_runtime(settings);
            let (model, _, context) =
                crate::commands::llm_settings::resolve_local_llm_env_triple(settings, env_lookup);
            (
                context.unwrap_or(runtime.ctx_size),
                model.unwrap_or(runtime.model_file),
            )
        }
        crate::commands::llm_settings::LlmMode::External => (
            settings.local_ctx_size.unwrap_or(8_192),
            settings.provider_model.clone().unwrap_or_default(),
        ),
    };
    let mut budget = summary_budget_for(context_window_tokens, requirement)?;
    // Only official endpoints use these reference capabilities. Compatible
    // endpoints may attach different models to these IDs. An explicit limit
    // can tighten, but cannot relax, a known provider ceiling.
    let external = settings.mode == crate::commands::llm_settings::LlmMode::External;
    let known_limit = (external
        && settings
            .base_url
            .as_deref()
            .is_none_or(|url| url.trim().is_empty())
        && matches!(expected_model.as_str(), "gpt-5.5" | "claude-sonnet-5"))
    .then_some(128_000);
    let output_limit = match (settings.model_max_output_tokens, known_limit) {
        (Some(configured), Some(known)) => Some(configured.min(known)),
        (Some(configured), None) => Some(configured),
        (None, Some(known)) => Some(known),
        // Unknown endpoints use the configured context budget. A guessed
        // fixed cap can make the requested writing target impossible.
        (None, None) => None,
    };
    if let Some(limit) = output_limit {
        if limit < SUMMARY_MIN_COMPLETION_TOKENS
            && requirement == SummaryBudgetRequirement::RequiredForPeriodSummaries
        {
            return Err(AppError::Config(
                "period summaries require an output limit of at least 1024 tokens".into(),
            ));
        }
        budget.max_completion_tokens = budget.max_completion_tokens.min(limit).max(1);
        if context_window_tokens >= SUMMARY_MIN_CONTEXT_TOKENS {
            budget.input_byte_budget =
                context_window_tokens - budget.max_completion_tokens - SUMMARY_SAFETY_MARGIN_TOKENS;
        }
    }
    Ok(SummaryLlmContract {
        context_window_tokens: if requirement == SummaryBudgetRequirement::SchemaPlaceholder {
            context_window_tokens.max(1024)
        } else {
            context_window_tokens
        },
        expected_model,
        budget,
    })
}

impl BatchDispatch {
    pub fn resolve(
        callback: &GrpcAddress,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
    ) -> AppResult<Self> {
        Self::resolve_with_window(
            callback,
            user_id,
            last_message_after_ms,
            None,
            llm_worker_name,
            output_language,
        )
    }

    pub fn resolve_for_period_summaries(
        callback: &GrpcAddress,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
    ) -> AppResult<Self> {
        Self::resolve_with_window_and_budget_requirement(
            callback,
            user_id,
            last_message_after_ms,
            None,
            llm_worker_name,
            output_language,
            SummaryBudgetRequirement::RequiredForPeriodSummaries,
        )
    }

    /// Resolve one staged summary dispatch, preserving the complete manual
    /// range while requiring a usable token budget whenever it includes a
    /// period-summary stage.
    pub fn resolve_for_summary_generation(
        callback: &GrpcAddress,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        last_message_before_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
        includes_period_summaries: bool,
    ) -> AppResult<Self> {
        Self::resolve_with_window_and_budget_requirement(
            callback,
            user_id,
            last_message_after_ms,
            last_message_before_ms,
            llm_worker_name,
            output_language,
            if includes_period_summaries {
                SummaryBudgetRequirement::RequiredForPeriodSummaries
            } else {
                SummaryBudgetRequirement::SchemaPlaceholder
            },
        )
    }

    pub fn resolve_with_window(
        callback: &GrpcAddress,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        last_message_before_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
    ) -> AppResult<Self> {
        Self::resolve_with_window_and_budget_requirement(
            callback,
            user_id,
            last_message_after_ms,
            last_message_before_ms,
            llm_worker_name,
            output_language,
            SummaryBudgetRequirement::SchemaPlaceholder,
        )
    }

    fn resolve_with_window_and_budget_requirement(
        callback: &GrpcAddress,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        last_message_before_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
        budget_requirement: SummaryBudgetRequirement,
    ) -> AppResult<Self> {
        let data_paths = crate::data::DataPaths::resolve()?;
        let settings =
            crate::commands::llm_settings::load_llm_settings(&data_paths.llm_settings_path());
        let summary_llm_contract = summary_llm_contract_with(
            &settings,
            crate::commands::process_env_lookup,
            budget_requirement,
        )?;
        let summary_output_guidance = summary_output_guidance_for_context(
            &settings.summary_output_guidance,
            summary_llm_contract.context_window_tokens,
        );
        Ok(Self {
            user_id,
            callback: callback.clone(),
            data_paths: Some(data_paths),
            #[cfg(test)]
            _test_data_root: None,
            last_message_after_ms,
            last_message_before_ms,
            thread_created_after_ms: None,
            thread_created_before_ms: None,
            target_thread_ids: None,
            rejected_candidate_ids_to_unsuppress_by_thread: None,
            llm_worker_name,
            output_language,
            summary_output_guidance,
            summary_llm_contract,
        })
    }

    /// Build the import input for the ThreadGroup batch. The group stage owns
    /// its active-group discovery, so it never inherits the import's thread
    /// target IDs; `activity_window` (exclusive epoch ms) limits it to groups
    /// with a member active while the imported conversations took place.
    pub fn thread_group_summary_input(
        &self,
        activity_window: Option<(i64, i64)>,
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "group_owner_user_id": self.user_id,
            "max_context_chars": 200_000,
            "force_resummarize": false,
            "trigger_kind": "import",
        });
        // Without a window the keys stay absent so the workflow's own
        // unbounded default applies.
        if let Some((after, before)) = activity_window {
            v["activity_after_ms"] = after.into();
            v["activity_before_ms"] = before.into();
        }
        self.merge_callback(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        self.inject_issue_extract_policy(&mut v);
        v
    }

    /// The gRPC callback fields every batch input shares.
    fn callback_fields(&self) -> serde_json::Value {
        serde_json::json!({
            "memories_grpc_host": self.callback.host,
            "memories_grpc_port": self.callback.port,
            "memories_grpc_tls": self.callback.tls,
        })
    }

    /// Inject `llm_worker_name` into a workflow input so the workflow
    /// routes LLM calls to the active worker (local or external).
    fn inject_llm_worker_name(&self, v: &mut serde_json::Value) {
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "llm_worker_name".to_string(),
                serde_json::Value::String(self.llm_worker_name.clone()),
            );
        }
    }

    /// Inject `output_language` so the batch picks the matching
    /// `memories-<feature>-single-<lang>` worker for each fan-out.
    fn inject_output_language(&self, v: &mut serde_json::Value) {
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "output_language".to_string(),
                serde_json::Value::String(self.output_language.clone()),
            );
        }
    }

    pub fn summarize_input(&self) -> serde_json::Value {
        self.summarize_input_with(false)
    }

    /// Targeted rejected-candidate re-extraction must bypass the summary
    /// freshness check so its selected source reaches Issue Extract.
    pub fn summarize_input_with(&self, force_resummarize: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "memory_thread_label_prefix": "summary",
            "force_resummarize": force_resummarize,
            "min_message_count": 4,
            "max_context_chars": 200_000,
            "summary_output_guidance": self.summary_output_guidance,
            "llm_context_window_tokens": self.summary_llm_contract.context_window_tokens,
            "llm_expected_model": self.summary_llm_contract.expected_model,
            "summary_max_completion_tokens": self.summary_llm_contract.budget.max_completion_tokens,
            "summary_input_byte_budget": self.summary_llm_contract.budget.input_byte_budget,
        });
        self.merge_callback(&mut v);
        self.inject_last_message_after_ms(&mut v);
        self.inject_last_message_before_ms(&mut v);
        self.inject_thread_created_window(&mut v);
        self.inject_target_thread_ids(&mut v);
        self.inject_rejected_candidate_ids_to_unsuppress_by_thread(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        self.inject_issue_extract_policy(&mut v);
        v
    }

    pub fn target_only(mut self, thread_id: String, rejected_candidate_id: String) -> Self {
        self.target_thread_ids = Some(vec![thread_id.clone()]);
        self.rejected_candidate_ids_to_unsuppress_by_thread = Some(
            std::collections::BTreeMap::from([(thread_id, vec![rejected_candidate_id])]),
        );
        self
    }

    fn inject_issue_extract_policy(&self, value: &mut serde_json::Value) {
        inject_issue_extract_policy_snapshot(value, self.data_paths.as_ref());
        let Some(object) = value.as_object_mut() else {
            return;
        };
        if let Some(tracker) = crate::sidecar::lifecycle::published_issue_tracker_for_jobworkerp() {
            tracker.inject_issue_tracker_into(object);
        }
    }

    /// Import pipeline default: do not force re-extraction. Re-runs skip
    /// up-to-date threads via thread-personality-single's `existing_signal`
    /// check, and the batch may short-circuit at `target_signal_count`.
    pub fn personality_input(&self) -> serde_json::Value {
        self.personality_input_with(false)
    }

    /// `force_reextract = true` makes the per-thread pass re-run even on
    /// threads that already have a stored signal, and disables the batch's
    /// `target_signal_count` early-break. Used by the standalone
    /// "Force 再抽出" path on the Personality tab so a prompt change can be
    /// applied to the historical thread set.
    pub fn personality_input_with(&self, force_reextract: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            // `merge_enabled` makes the batch run the layer-2 merge itself
            // after the signal pass (calling `memories-user-personality-merge-
            // <lang>` by name), so both this and the standalone button (which
            // share this input) produce a profile. The merge YAML path is no
            // longer relayed — the batch resolves the lang-worker by name.
            "merge_enabled": true,
            "force_reextract": force_reextract,
            "min_message_count": 4,
            "min_user_messages": 2,
            "max_context_chars": PERSONALITY_MAX_CONTEXT_CHARS,
        });
        self.merge_callback(&mut v);
        self.inject_last_message_after_ms(&mut v);
        self.inject_thread_created_window(&mut v);
        self.inject_target_thread_ids(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Build the input for a standalone Layer-2 merge dispatch (the
    /// `memories-user-personality-merge-<lang>` worker). Mirrors what
    /// `thread-personality-batch.yaml::userPersonalityMerge` passes to the
    /// merge YAML when it runs as the batch's tail, MINUS anything specific
    /// to the per-thread fan-out (no `single_workflow_path`,
    /// `merge_workflow_path`, `target_signal_count`, etc — the merge YAML
    /// only needs the inputs declared in its schema).
    ///
    /// Use case: a previous batch produced valid layer-1 signals but never
    /// emitted a profile (e.g. external-LLM 429 storm during per-thread
    /// extraction left enough signals AND a populated personality store but
    /// the merge never landed). This input lets the Personality tab run
    /// the merge in isolation.
    pub fn merge_only_input(&self, force_remerge: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "max_context_chars": PERSONALITY_MAX_CONTEXT_CHARS,
            "force_remerge": force_remerge,
            // Hard-coded because the merge-only path has no batch parent.
            "max_signals": PERSONALITY_MERGE_MAX_SIGNALS,
        });
        self.merge_callback(&mut v);
        self.inject_llm_worker_name(&mut v);
        // The merge-only dispatch targets `memories-user-personality-merge-
        // <lang>` directly, so it needs the language too.
        self.inject_output_language(&mut v);
        v
    }

    pub fn reflection_input(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "prompt_version": REFLECTION_PROMPT_VERSION,
        });
        self.merge_callback(&mut v);
        self.inject_last_message_after_ms(&mut v);
        self.inject_thread_created_window(&mut v);
        self.inject_target_thread_ids(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Build the input for a period (daily/weekly/monthly) work-summary batch.
    /// Period summaries use the same real owner as their source lineage and
    /// scope their rows by period.
    pub fn period_input(
        &self,
        kind: PeriodKind,
        range: PeriodRange,
        stop_on_error: bool,
        dispatch_id: Option<&str>,
    ) -> serde_json::Value {
        let spec = kind.spec();
        let generation_started_at = chrono::Utc::now();
        let generation_id = dispatch_id
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| summary_generation_id(generation_started_at));
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "timezone_offset_hours": 9,
            "min_thread_count": 1,
            "max_context_chars": 200_000,
            "force_resummarize": false,
            "stop_on_error": stop_on_error,
            "source_scope_digest": crate::commands::summaries::source_scope_digest(&[], Some("LABEL_ALL")),
            "generation_id": generation_id,
            "generation_started_at": generation_started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "summary_output_guidance": self.summary_output_guidance,
            "llm_context_window_tokens": self.summary_llm_contract.context_window_tokens,
            "llm_expected_model": self.summary_llm_contract.expected_model,
            "summary_max_completion_tokens": self.summary_llm_contract.budget.max_completion_tokens,
            "summary_input_byte_budget": self.summary_llm_contract.budget.input_byte_budget,
        });
        if let Some(obj) = v.as_object_mut() {
            obj.insert(spec.source_label_key.into(), spec.source_label.into());
            obj.insert(spec.out_label_key.into(), spec.out_label.into());
            obj.insert(spec.last_n_field.into(), range.last_n().into());
        }
        self.merge_callback(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Resolve generation-wide identity once, before any child stage is
    /// dispatched. Reconciliation and all child period summaries use these
    /// exact values.
    pub fn summary_generation_metadata(
        &self,
        dispatch_id: Option<&str>,
    ) -> SummaryGenerationMetadata {
        let generation_started_at = chrono::Utc::now();
        let generation_id = dispatch_id
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| summary_generation_id(generation_started_at));
        SummaryGenerationMetadata {
            generation_id,
            generation_started_at: generation_started_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            source_scope_digest: crate::commands::summaries::source_scope_digest(
                &[],
                Some("LABEL_ALL"),
            ),
        }
    }

    /// Construct the direct worker input that the summaries workflow passed
    /// to each stage. The execution order and selection live in the app; the
    /// worker wire shapes stay the same, with generation metadata added
    /// consistently to every stage.
    pub fn summary_generation_input(
        &self,
        step: super::super::staged_progress::StagedStep,
        req: &GenerateSummariesRequest,
        metadata: &SummaryGenerationMetadata,
    ) -> Option<serde_json::Value> {
        use super::super::staged_progress::StagedStep;

        let mut input = match step {
            StagedStep::ThreadSummary => self.summarize_input(),
            StagedStep::ThreadGroupSummary => {
                let mut value = serde_json::json!({
                    "group_owner_user_id": self.user_id,
                    "issue_tracker_grpc_host": "",
                    "issue_tracker_grpc_port": 0,
                    "policy_json": "",
                    "stop_on_error": req.stop_on_error,
                    "trigger_kind": "manual",
                });
                if let (Some(after), Some(before)) = (
                    req.thread_group_activity_after_ms,
                    req.thread_group_activity_before_ms,
                ) {
                    value["activity_after_ms"] = after.into();
                    value["activity_before_ms"] = before.into();
                }
                self.merge_callback(&mut value);
                self.inject_llm_worker_name(&mut value);
                self.inject_output_language(&mut value);
                self.inject_issue_extract_policy(&mut value);
                value
            }
            StagedStep::Daily | StagedStep::Weekly | StagedStep::Monthly => {
                let (start_key, end_key, start, end) = match step {
                    StagedStep::Daily => {
                        ("start_date", "end_date", &req.daily_start, &req.daily_end)
                    }
                    StagedStep::Weekly => {
                        ("start_week", "end_week", &req.weekly_start, &req.weekly_end)
                    }
                    _ => (
                        "start_month",
                        "end_month",
                        &req.monthly_start,
                        &req.monthly_end,
                    ),
                };
                let mut value = serde_json::json!({
                    "user_id": self.user_id,
                    "timezone_offset_hours": req.timezone_offset_hours,
                    "stop_on_error": req.stop_on_error,
                    start_key: start,
                    end_key: end,
                    "summary_output_guidance": self.summary_output_guidance,
                    "llm_context_window_tokens": self.summary_llm_contract.context_window_tokens,
                    "llm_expected_model": self.summary_llm_contract.expected_model,
                    "summary_max_completion_tokens": self.summary_llm_contract.budget.max_completion_tokens,
                    "summary_input_byte_budget": self.summary_llm_contract.budget.input_byte_budget,
                });
                self.merge_callback(&mut value);
                self.inject_llm_worker_name(&mut value);
                self.inject_output_language(&mut value);
                value
            }
            StagedStep::ThreadImport | StagedStep::ThreadPersonality | StagedStep::Reflection => {
                return None;
            }
        };
        if let Some(object) = input.as_object_mut() {
            object.insert(
                "generation_id".into(),
                metadata.generation_id.clone().into(),
            );
            object.insert(
                "generation_started_at".into(),
                metadata.generation_started_at.clone().into(),
            );
            object.insert(
                "source_scope_digest".into(),
                metadata.source_scope_digest.clone().into(),
            );
        }
        Some(input)
    }

    fn merge_callback(&self, v: &mut serde_json::Value) {
        if let (Some(obj), Some(cb)) = (v.as_object_mut(), self.callback_fields().as_object()) {
            for (k, val) in cb {
                obj.insert(k.clone(), val.clone());
            }
        }
    }

    fn inject_last_message_after_ms(&self, v: &mut serde_json::Value) {
        if let Some(ms) = self.last_message_after_ms {
            v["last_message_after_ms"] = serde_json::Value::from(ms);
        }
    }

    fn inject_last_message_before_ms(&self, v: &mut serde_json::Value) {
        if let Some(ms) = self.last_message_before_ms {
            v["last_message_before_ms"] = serde_json::Value::from(ms);
        }
    }

    fn inject_thread_created_window(&self, v: &mut serde_json::Value) {
        if let Some(ms) = self.thread_created_after_ms {
            v["thread_created_after_ms"] = serde_json::Value::from(ms);
        }
        if let Some(ms) = self.thread_created_before_ms {
            v["thread_created_before_ms"] = serde_json::Value::from(ms);
        }
    }

    fn inject_target_thread_ids(&self, value: &mut serde_json::Value) {
        if let Some(ids) = &self.target_thread_ids {
            value["target_thread_ids"] = serde_json::json!(ids);
        }
    }

    fn inject_rejected_candidate_ids_to_unsuppress_by_thread(&self, value: &mut serde_json::Value) {
        if let Some(ids) = &self.rejected_candidate_ids_to_unsuppress_by_thread {
            value["rejected_candidate_ids_to_unsuppress_by_thread"] = serde_json::json!(ids);
        }
    }
}

pub(super) fn inject_issue_extract_policy_snapshot(
    value: &mut serde_json::Value,
    data_paths: Option<&crate::data::DataPaths>,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(data_paths) = data_paths else {
        return;
    };
    let issue_tracking_enabled =
        crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(data_paths);
    let Some(json) = crate::issue_extract::snapshot_json(&data_paths.root, issue_tracking_enabled)
    else {
        return;
    };
    object.insert("policy_json".into(), serde_json::Value::String(json));
}

#[cfg(test)]
mod tests {
    use super::super::super::connection::GrpcAddress;
    use super::super::super::staged_progress::StagedStep as ImportStep;
    use super::super::guards::summary_generation_id;
    use super::*;

    #[cfg(any(debug_assertions, feature = "experimental-issues"))]
    struct IssueTrackerEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    #[cfg(any(debug_assertions, feature = "experimental-issues"))]
    impl IssueTrackerEnvGuard {
        fn with_endpoint(host: &str, port: &str) -> Self {
            const KEYS: [&str; 4] = [
                "ISSUE_TRACKER_GRPC_HOST",
                "ISSUE_TRACKER_GRPC_PORT",
                "ISSUE_TRACKER_GATEWAY_WORKER_HOST",
                "ISSUE_TRACKER_GATEWAY_WORKER_PORT",
            ];
            let lock = crate::commands::issue_tracker_settings::ISSUE_TRACKER_ENV_TEST_LOCK
                .lock()
                .unwrap();
            let previous = KEYS
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            unsafe {
                std::env::set_var("ISSUE_TRACKER_GRPC_HOST", host);
                std::env::set_var("ISSUE_TRACKER_GRPC_PORT", port);
            }
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    #[cfg(any(debug_assertions, feature = "experimental-issues"))]
    impl Drop for IssueTrackerEnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in self.previous.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn dummy_callback() -> GrpcAddress {
        GrpcAddress {
            host: "127.0.0.1".into(),
            port: 9010,
            tls: false,
        }
    }

    fn dispatch() -> BatchDispatch {
        dispatch_with(None)
    }

    fn dispatch_with(last_message_after_ms: Option<i64>) -> BatchDispatch {
        dispatch_with_window(last_message_after_ms, None)
    }

    fn dispatch_with_window(
        last_message_after_ms: Option<i64>,
        last_message_before_ms: Option<i64>,
    ) -> BatchDispatch {
        let temp_root = tempfile::tempdir().unwrap();
        BatchDispatch {
            user_id: 1,
            callback: dummy_callback(),
            data_paths: Some(crate::data::DataPaths::with_root(temp_root.path())),
            _test_data_root: Some(temp_root),
            last_message_after_ms,
            last_message_before_ms,
            thread_created_after_ms: None,
            thread_created_before_ms: None,
            target_thread_ids: None,
            rejected_candidate_ids_to_unsuppress_by_thread: None,
            llm_worker_name: "memories-llm".to_string(),
            output_language: "ja".to_string(),
            summary_output_guidance: serde_json::json!({}),
            summary_llm_contract: SummaryLlmContract {
                context_window_tokens: 131_072,
                expected_model: "test.gguf".to_string(),
                budget: SummaryBudget {
                    max_completion_tokens: 32_768,
                    input_byte_budget: 390_000,
                },
            },
        }
    }

    #[test]
    fn dispatch_fixture_keeps_its_isolated_data_root_alive() {
        let dispatch = dispatch();
        let root = dispatch
            .data_paths
            .as_ref()
            .expect("test dispatch data paths")
            .root
            .clone();

        assert!(
            root.is_dir(),
            "the fixture data root must outlive dispatch()"
        );
    }

    #[test]
    fn batch_dispatch_summarize_input_matches_workflow_schema() {
        let v = dispatch().summarize_input();
        assert_eq!(v["user_id"], 1);
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
        // The batch resolves its language-specific single worker by name from
        // `output_language`; the old single-yaml path relay is gone.
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert!(v.get("summary_user_id").is_none());
        assert_eq!(v["max_context_chars"], 200_000);
    }

    #[test]
    fn targeted_summary_reextract_forces_resummarization() {
        let v = dispatch()
            .target_only("thread-1".into(), "candidate-1".into())
            .summarize_input_with(true);

        assert_eq!(v["force_resummarize"], true);
        assert_eq!(v["target_thread_ids"], serde_json::json!(["thread-1"]));
        assert_eq!(
            v["rejected_candidate_ids_to_unsuppress_by_thread"],
            serde_json::json!({"thread-1": ["candidate-1"]})
        );
    }

    #[test]
    #[cfg(any(debug_assertions, feature = "experimental-issues"))]
    fn summarize_input_injects_issue_extract_policy_and_tracker_endpoint() {
        let _env_guard = IssueTrackerEnvGuard::with_endpoint("127.0.0.1", "9555");
        let dispatch = dispatch();
        let expected_issue_tracking_enabled = dispatch
            .data_paths
            .as_ref()
            .map(crate::commands::issue_tracker_settings::effective_issue_tracking_enabled)
            .expect("test dispatch data paths");
        let v = dispatch.summarize_input();
        assert_eq!(v["issue_tracker_grpc_host"], "127.0.0.1");
        assert_eq!(v["issue_tracker_grpc_port"], 9555);
        let policy = v["policy_json"]
            .as_str()
            .expect("policy snapshot is injected");
        let policy: serde_json::Value = serde_json::from_str(policy).unwrap();
        assert_eq!(
            policy["extraction_enabled"],
            expected_issue_tracking_enabled
        );
        assert_eq!(policy["auto_register_min_confidence"], 0.90);
    }

    #[test]
    fn policy_snapshot_is_omitted_when_the_manual_input_has_no_data_root() {
        let mut input = serde_json::json!({});

        inject_issue_extract_policy_snapshot(&mut input, None);

        assert!(input.get("policy_json").is_none());
    }

    #[test]
    fn policy_snapshot_uses_the_saved_issue_tracker_toggle() {
        let directory = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(directory.path());
        crate::commands::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &crate::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();
        let mut input = serde_json::json!({});

        inject_issue_extract_policy_snapshot(&mut input, Some(&data));

        let policy: serde_json::Value =
            serde_json::from_str(input["policy_json"].as_str().unwrap()).unwrap();
        assert_eq!(policy["extraction_enabled"], false);
    }

    #[test]
    fn import_dispatch_uses_only_the_authoritative_thread_ids() {
        let mut dispatch = dispatch();
        dispatch.target_thread_ids = Some(vec!["9007199254740993".into(), "42".into()]);

        for input in [
            dispatch.summarize_input(),
            dispatch.personality_input(),
            dispatch.reflection_input(),
        ] {
            assert_eq!(
                input["target_thread_ids"],
                serde_json::json!(["9007199254740993", "42"])
            );
            assert!(input.get("thread_created_after_ms").is_none());
            assert!(input.get("thread_created_before_ms").is_none());
            assert!(input.get("last_message_after_ms").is_none());
            assert!(input.get("last_message_before_ms").is_none());
        }
    }

    #[test]
    fn thread_group_summary_input_is_untargeted_and_uses_import_contract() {
        let mut dispatch = dispatch();
        dispatch.user_id = 7;
        dispatch.target_thread_ids = Some(vec!["42".into()]);
        dispatch.last_message_after_ms = Some(10);
        dispatch.last_message_before_ms = Some(20);
        dispatch.thread_created_after_ms = Some(30);
        dispatch.thread_created_before_ms = Some(40);

        let input = dispatch.thread_group_summary_input(None);

        assert_eq!(input["group_owner_user_id"], 7);
        assert!(input.get("user_id").is_none());
        assert_eq!(input["memories_grpc_host"], "127.0.0.1");
        assert_eq!(input["memories_grpc_port"], 9010);
        assert_eq!(input["memories_grpc_tls"], false);
        assert_eq!(input["output_language"], "ja");
        assert_eq!(input["llm_worker_name"], "memories-llm");
        assert_eq!(input["trigger_kind"], "import");
        for key in [
            "target_thread_ids",
            "target_group_id",
            "target_group_ids",
            "last_message_after_ms",
            "last_message_before_ms",
            "thread_created_after_ms",
            "thread_created_before_ms",
            "activity_after_ms",
            "activity_before_ms",
        ] {
            assert!(input.get(key).is_none(), "untargeted input contains {key}");
        }
    }

    #[test]
    fn thread_group_summary_input_scopes_groups_to_the_import_activity_window() {
        let input = dispatch().thread_group_summary_input(Some((799, 2_001)));
        assert_eq!(input["activity_after_ms"], 799);
        assert_eq!(input["activity_before_ms"], 2_001);
        assert!(input.get("target_thread_ids").is_none());
    }

    #[test]
    #[cfg(any(debug_assertions, feature = "experimental-issues"))]
    fn thread_group_summary_input_relays_resolution_policy_and_tracker_endpoint() {
        let _env_guard = IssueTrackerEnvGuard::with_endpoint("tracker.example", "9555");
        let dispatch = dispatch();
        crate::commands::issue_tracker_settings::save_issue_tracker_settings(
            &dispatch
                .data_paths
                .as_ref()
                .expect("test dispatch data paths")
                .issue_tracker_settings_path(),
            &crate::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(true),
            },
        )
        .unwrap();
        let input = dispatch.thread_group_summary_input(None);

        assert_eq!(input["issue_tracker_grpc_host"], "tracker.example");
        assert_eq!(input["issue_tracker_grpc_port"], 9555);
        let policy: serde_json::Value =
            serde_json::from_str(input["policy_json"].as_str().unwrap()).unwrap();
        assert_eq!(policy["resolution_mode"], "DISABLED");
    }

    #[test]
    fn batch_dispatch_daily_input_matches_workflow_schema() {
        let v = dispatch().period_input(PeriodKind::Daily, PeriodRange::Auto, false, None);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        // Daily reads per-thread `summary` and writes `daily_summary`.
        assert_eq!(v["summary_label"], "summary");
        assert_eq!(v["daily_label"], "daily_summary");
        assert_eq!(v["last_n_days"], 0); // Auto -> fallback
        assert_eq!(v["timezone_offset_hours"], 9);
        assert_eq!(v["llm_context_window_tokens"], 131_072);
        assert_eq!(v["llm_expected_model"], "test.gguf");
        assert_eq!(v["summary_max_completion_tokens"], 32_768);
        assert_eq!(v["summary_input_byte_budget"], 390_000);
        assert_eq!(v["max_context_chars"], 200_000);
        assert_eq!(v["force_resummarize"], false);
        assert_eq!(v["stop_on_error"], false);
        assert!(v["generation_id"].as_str().is_some_and(|id| id.len() > 20));
        assert!(
            v["generation_started_at"]
                .as_str()
                .is_some_and(|at| at.ends_with('Z'))
        );
        // Callback propagated.
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
    }

    #[test]
    fn summary_workflow_token_target_leaves_room_for_complete_json() {
        for period in ["daily", "weekly", "monthly"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../workers/lang-workers/workers/{period}-work-summary/{period}-work-summary-single.yaml"));
            let body = std::fs::read_to_string(path).unwrap();
            let yaml: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
            let budget = yaml["do"]
                .as_sequence()
                .unwrap()
                .iter()
                .find_map(|task| task.get("deriveSummaryTokenBudget"))
                .unwrap();
            let expression = budget["set"]["summary_target_tokens"].as_str().unwrap();
            let filter = expression
                .trim()
                .strip_prefix("${")
                .unwrap()
                .strip_suffix('}')
                .unwrap();
            for reservation in [1024, 4096, 8192, 120_000, 262_144] {
                let workflow =
                    serde_json::json!({"input": {"summary_max_completion_tokens": reservation}});
                let output = std::process::Command::new("jq")
                    .args(["-n", "--argjson", "workflow", &workflow.to_string(), filter])
                    .output()
                    .expect("jq must be available for workflow expression tests");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let target: u32 = serde_json::from_slice(&output.stdout).unwrap();
                assert!(target > 0 && target < reservation);
                assert!(reservation - target >= reservation / 4);
            }
            // Both counting and generation must receive the same complete prompt.
            assert_eq!(
                body.matches("Aim for at most {{ summary_target_tokens }} output tokens")
                    .count(),
                2
            );
            assert_eq!(
                body.matches("The generation allowance is {{ summary_max_tokens }} tokens")
                    .count(),
                2
            );
            assert!(body.contains("output_budget_policy: 1"));
        }
    }

    #[test]
    fn unknown_external_summary_budget_uses_context_instead_of_fixed_output_cap() {
        let mut settings = crate::commands::llm_settings::LlmSettings {
            mode: crate::commands::llm_settings::LlmMode::External,
            provider_model: Some("openai::custom-model".into()),
            base_url: Some("https://compatible.example/v1".into()),
            local_ctx_size: Some(1_048_576),
            ..Default::default()
        };
        for limit in [None, Some(120_000), Some(4_096)] {
            settings.model_max_output_tokens = limit;
            let contract = summary_llm_contract_with(
                &settings,
                |_| None,
                SummaryBudgetRequirement::RequiredForPeriodSummaries,
            )
            .unwrap();
            assert_eq!(
                contract.budget.max_completion_tokens,
                limit.unwrap_or(262_144)
            );
            assert_eq!(
                contract.budget.input_byte_budget
                    + contract.budget.max_completion_tokens
                    + SUMMARY_SAFETY_MARGIN_TOKENS,
                1_048_576
            );
        }
    }

    #[test]
    fn summary_token_budget_preserves_source_capacity_for_default_external_context() {
        assert_eq!(
            summary_token_budget(8_192).unwrap(),
            SummaryBudget {
                max_completion_tokens: 4_096,
                input_byte_budget: 2_048,
            }
        );
    }

    #[test]
    fn summary_budget_respects_configured_output_limit_and_context() {
        for (context, limit) in [(1_000_000, 32_768), (1_000_000, 65_536), (8_192, 32_768)] {
            let settings = crate::commands::llm_settings::LlmSettings {
                mode: crate::commands::llm_settings::LlmMode::External,
                local_ctx_size: Some(context),
                model_max_output_tokens: Some(limit),
                ..Default::default()
            };
            let contract = summary_llm_contract_with(
                &settings,
                |_| None,
                SummaryBudgetRequirement::RequiredForPeriodSummaries,
            )
            .unwrap();
            assert_eq!(
                contract.budget.max_completion_tokens,
                limit.min(context - 4_096)
            );
            assert_eq!(
                contract.budget.input_byte_budget
                    + contract.budget.max_completion_tokens
                    + SUMMARY_SAFETY_MARGIN_TOKENS,
                context
            );
        }
    }

    #[test]
    fn legacy_context_is_placeholder_only_for_non_period_generation() {
        for mode in [
            crate::commands::llm_settings::LlmMode::Local,
            crate::commands::llm_settings::LlmMode::External,
        ] {
            let settings = crate::commands::llm_settings::LlmSettings {
                mode,
                local_preset_id: Some("custom".into()),
                local_ctx_size: Some(512),
                ..Default::default()
            };
            let contract = summary_llm_contract_with(
                &settings,
                |_| None,
                SummaryBudgetRequirement::SchemaPlaceholder,
            )
            .unwrap();
            assert!(contract.context_window_tokens >= 1024);
            assert_eq!(settings.local_ctx_size, Some(512));
            assert!(
                summary_llm_contract_with(
                    &settings,
                    |_| None,
                    SummaryBudgetRequirement::RequiredForPeriodSummaries
                )
                .is_err()
            );
        }
    }

    #[test]
    fn current_reference_models_use_their_output_capability() {
        for model in ["gpt-5.5", "claude-sonnet-5"] {
            let mut settings = crate::commands::llm_settings::LlmSettings {
                mode: crate::commands::llm_settings::LlmMode::External,
                provider_model: Some(model.into()),
                local_ctx_size: Some(1_000_000),
                ..Default::default()
            };
            let resolve = |settings: &crate::commands::llm_settings::LlmSettings| {
                summary_llm_contract_with(
                    settings,
                    |_| None,
                    SummaryBudgetRequirement::RequiredForPeriodSummaries,
                )
                .unwrap()
                .budget
                .max_completion_tokens
            };
            assert_eq!(resolve(&settings), 128_000);
            settings.model_max_output_tokens = Some(200_000);
            assert_eq!(resolve(&settings), 128_000);
            settings.model_max_output_tokens = Some(64_000);
            assert_eq!(resolve(&settings), 64_000);
            settings.base_url = Some("https://compatible.example/v1".into());
            settings.model_max_output_tokens = Some(200_000);
            assert_eq!(resolve(&settings), 200_000);
            settings.mode = crate::commands::llm_settings::LlmMode::Local;
            settings.local_preset_id = Some("custom".into());
            assert_eq!(resolve(&settings), 200_000);
        }
    }

    #[test]
    fn summary_token_budget_rejects_contexts_that_cannot_fit_input_and_output() {
        assert!(summary_token_budget(SUMMARY_MIN_CONTEXT_TOKENS - 1).is_err());
        let budget = summary_token_budget(SUMMARY_MIN_CONTEXT_TOKENS).unwrap();
        assert_eq!(budget.max_completion_tokens, SUMMARY_MIN_COMPLETION_TOKENS);
        assert_eq!(budget.input_byte_budget, SUMMARY_MIN_INPUT_TOKENS);
    }

    #[test]
    fn non_period_dispatch_does_not_reject_a_small_configured_context() {
        assert_eq!(
            summary_budget_for(
                SUMMARY_MIN_CONTEXT_TOKENS - 1,
                SummaryBudgetRequirement::SchemaPlaceholder,
            )
            .unwrap(),
            SummaryBudget {
                max_completion_tokens: 1,
                input_byte_budget: 0,
            }
        );
        assert!(
            summary_budget_for(
                SUMMARY_MIN_CONTEXT_TOKENS - 1,
                SummaryBudgetRequirement::RequiredForPeriodSummaries,
            )
            .is_err()
        );
    }

    #[test]
    fn summary_contract_honours_unsaved_local_environment_overrides() {
        let settings = crate::commands::llm_settings::LlmSettings::default();
        let contract = summary_llm_contract_with(
            &settings,
            |name| match name {
                "LOOKBACK_LLM_MODEL" => Some("dev-override.gguf".into()),
                "LOOKBACK_LLM_CTX_SIZE" => Some("8192".into()),
                _ => None,
            },
            SummaryBudgetRequirement::RequiredForPeriodSummaries,
        )
        .unwrap();
        assert_eq!(contract.context_window_tokens, 8_192);
        assert_eq!(contract.expected_model, "dev-override.gguf");
    }

    #[test]
    fn period_workflows_use_exact_candidates_before_the_byte_fallback() {
        for yaml in [
            include_str!(
                "../../../../workers/lang-workers/workers/daily-work-summary/daily-work-summary-single.yaml"
            ),
            include_str!(
                "../../../../workers/lang-workers/workers/weekly-work-summary/weekly-work-summary-single.yaml"
            ),
            include_str!(
                "../../../../workers/lang-workers/workers/monthly-work-summary/monthly-work-summary-single.yaml"
            ),
        ] {
            assert!(yaml.contains("truncated_entries: \"${ $target_memories }\""));
            assert!(yaml.contains("fallback_byte_budget_entries"));
            assert!(yaml.contains("thread_description: \" + ($e.threadDescription // \"\")"));
            assert!(yaml.contains(
                "memory_updated_at: \" + (($e.memory.data.updatedAt // \"\") | tostring)"
            ));
            assert!(yaml.contains("($truncated_entries | length) == 0"));
            assert!(yaml.contains("InputToExactBudget"));
            assert!(yaml.contains("AfterExactOverflow"));
            assert!(yaml.contains("FallbackContext"));
            assert!(yaml.contains("TOKEN_COUNT_EXACT_UNAVAILABLE"));
            assert!(yaml.contains("PROVIDER_CAPABILITY_UNAVAILABLE"));
            assert!(yaml.contains("renderedInputTokens | tonumber"));
            assert!(
                yaml.contains(
                    "summary_input_byte_budget: $workflow.input.summary_input_byte_budget"
                )
            );
            assert!(yaml.contains("else ({version: 3,"));
        }
    }

    #[test]
    fn period_workflow_budget_fields_are_declared_and_relayed() {
        const FIELDS: [&str; 4] = [
            "llm_context_window_tokens",
            "llm_expected_model",
            "summary_max_completion_tokens",
            "summary_input_byte_budget",
        ];
        let batches = [
            include_str!(
                "../../../../workers/workflows/daily-work-summary/daily-work-summary-batch.yaml"
            ),
            include_str!(
                "../../../../workers/workflows/weekly-work-summary/weekly-work-summary-batch.yaml"
            ),
            include_str!(
                "../../../../workers/workflows/monthly-work-summary/monthly-work-summary-batch.yaml"
            ),
        ];
        for yaml in batches {
            for field in FIELDS {
                assert!(
                    yaml.contains(&format!("{field}: {{")),
                    "missing {field} schema"
                );
                assert!(
                    yaml.matches(&format!("$workflow.input.{field}")).count() >= 2,
                    "missing {field} batch relay"
                );
            }
        }
        // Guidance is an object schema written over multiple YAML lines, so
        // exercise it separately from the compact scalar field declarations.
        for yaml in batches {
            assert!(
                yaml.contains("summary_output_guidance:"),
                "missing guidance schema"
            );
            assert!(
                yaml.matches("$workflow.input.summary_output_guidance")
                    .count()
                    >= 2,
                "missing guidance batch relay"
            );
        }
        // The conductor-driven periodic wrapper bypasses BatchDispatch, so it
        // must carry the same snapshot into its six daily/weekly/monthly batch
        // calls.
        let periodic = include_str!("../../../../workers/workflows/lookback-periodic-run.yaml");
        for field in FIELDS {
            assert!(
                periodic.contains(&format!("{field}: {{")),
                "missing {field} periodic runtime schema"
            );
            assert!(
                periodic
                    .matches(&format!("$workflow.input.runtime.{field}"))
                    .count()
                    >= 6,
                "missing {field} periodic batch relay"
            );
        }
        assert!(
            periodic.contains("summary_output_guidance:"),
            "missing periodic runtime guidance schema"
        );
        assert!(
            periodic
                .matches("$workflow.input.runtime.summary_output_guidance")
                .count()
                >= 6,
            "missing periodic guidance batch relay"
        );
    }

    #[test]
    fn period_input_preserves_dispatch_id_and_stop_on_error() {
        let v = dispatch().period_input(
            PeriodKind::Daily,
            PeriodRange::Auto,
            true,
            Some("manual-period-20260903070632"),
        );

        assert_eq!(v["stop_on_error"], true);
        assert_eq!(v["generation_id"], "manual-period-20260903070632");
    }

    #[test]
    fn batch_dispatch_weekly_input_matches_workflow_schema() {
        let v = dispatch().period_input(PeriodKind::Weekly, PeriodRange::LastN(4), false, None);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        // Weekly reads `daily_summary` and writes `weekly_summary`.
        assert_eq!(v["daily_label"], "daily_summary");
        assert_eq!(v["weekly_label"], "weekly_summary");
        assert_eq!(v["last_n_weeks"], 4);
    }

    #[test]
    fn batch_dispatch_monthly_input_matches_workflow_schema() {
        let v = dispatch().period_input(PeriodKind::Monthly, PeriodRange::LastN(3), false, None);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        // Monthly reads `weekly_summary` and writes `monthly_summary`.
        assert_eq!(v["weekly_label"], "weekly_summary");
        assert_eq!(v["monthly_label"], "monthly_summary");
        assert_eq!(v["last_n_months"], 3);
    }

    #[test]
    fn period_kind_worker_names_and_prefixes() {
        assert_eq!(
            PeriodKind::Daily.worker_name(),
            "memories-daily-summary-batch"
        );
        assert_eq!(
            PeriodKind::Weekly.worker_name(),
            "memories-weekly-summary-batch"
        );
        assert_eq!(
            PeriodKind::Monthly.worker_name(),
            "memories-monthly-summary-batch"
        );
        assert_eq!(PeriodKind::Daily.job_prefix(), "daily");
        assert_eq!(PeriodKind::Weekly.job_prefix(), "weekly");
        assert_eq!(PeriodKind::Monthly.job_prefix(), "monthly");
    }

    #[test]
    fn batch_dispatch_summarize_input_includes_last_message_before_ms_when_set() {
        let d = dispatch_with_window(Some(1_746_057_600_000), Some(1_748_735_999_999));
        let v = d.summarize_input();
        assert_eq!(v["last_message_after_ms"], 1_746_057_600_000_i64);
        assert_eq!(v["last_message_before_ms"], 1_748_735_999_999_i64);
    }

    #[test]
    fn batch_dispatch_summarize_input_omits_last_message_before_ms_when_none() {
        let v = dispatch_with_window(Some(1), None).summarize_input();
        assert!(
            v.get("last_message_before_ms").is_none(),
            "must omit last_message_before_ms when None: {v}"
        );
    }

    fn summary_req() -> GenerateSummariesRequest {
        GenerateSummariesRequest {
            user_id: Some(1),
            run_per_thread: true,
            run_thread_group: true,
            run_daily: true,
            run_weekly: true,
            run_monthly: false,
            stop_on_error: false,
            last_message_after_ms: Some(1_746_057_600_000),
            last_message_before_ms: Some(1_748_735_999_999),
            thread_group_activity_after_ms: None,
            thread_group_activity_before_ms: None,
            daily_start: "2026-03-01".into(),
            daily_end: "2026-05-31".into(),
            weekly_start: "2026-W09".into(),
            weekly_end: "2026-W22".into(),
            monthly_start: "2026-03".into(),
            monthly_end: "2026-05".into(),
            timezone_offset_hours: 9,
            dispatch_id: None,
        }
    }

    #[test]
    fn summary_generation_ids_are_unique_for_same_start_instant() {
        let now = chrono::Utc::now();
        assert_ne!(summary_generation_id(now), summary_generation_id(now));
    }

    #[test]
    fn summary_generation_metadata_uses_dispatch_id_and_is_stable_for_all_stages() {
        let mut req = summary_req();
        req.dispatch_id = Some("summary-user-request-42".into());
        let dispatch = dispatch();
        let metadata = dispatch.summary_generation_metadata(req.dispatch_id.as_deref());
        assert_eq!(metadata.generation_id, "summary-user-request-42");
        assert!(!metadata.generation_started_at.is_empty());
        assert!(!metadata.source_scope_digest.is_empty());
    }

    #[test]
    fn generate_summaries_request_defaults_stop_on_error_to_false_and_forwards_true() {
        let defaulted: GenerateSummariesRequest = serde_json::from_str(
            r#"{"run_per_thread":false,"run_daily":false,"run_weekly":false,"run_monthly":false,"timezone_offset_hours":9}"#,
        )
        .unwrap();
        assert!(!defaulted.stop_on_error);

        let req: GenerateSummariesRequest = serde_json::from_str(
            r#"{"run_per_thread":false,"run_daily":false,"run_weekly":false,"run_monthly":false,"timezone_offset_hours":9,"stop_on_error":true}"#,
        )
        .unwrap();
        assert!(req.stop_on_error);
    }

    #[test]
    fn summary_generation_thread_input_omits_epoch_bounds_when_unbounded() {
        // per-thread-only recovery run: no range → bounds dropped so the
        // batch stays unbounded (mirrors enqueueSummaryJob({})).
        let req = GenerateSummariesRequest {
            run_per_thread: true,
            last_message_after_ms: None,
            last_message_before_ms: None,
            ..Default::default()
        };
        let dispatch = dispatch();
        let metadata = dispatch.summary_generation_metadata(None);
        let v = dispatch
            .summary_generation_input(ImportStep::ThreadSummary, &req, &metadata)
            .unwrap();
        assert!(
            v.get("last_message_after_ms").is_none(),
            "unbounded run must omit last_message_after_ms: {v}"
        );
        assert!(
            v.get("last_message_before_ms").is_none(),
            "unbounded run must omit last_message_before_ms: {v}"
        );
    }

    #[test]
    fn staged_summary_inputs_preserve_pipeline_child_values_and_share_generation_metadata() {
        let mut req = summary_req();
        req.run_monthly = true;
        req.monthly_start = "2026-02".into();
        req.monthly_end = "2026-04".into();
        req.thread_group_activity_after_ms = Some(1_700_000_000_000);
        req.thread_group_activity_before_ms = Some(1_700_086_400_000);
        req.stop_on_error = true;

        let mut dispatch =
            dispatch_with_window(req.last_message_after_ms, req.last_message_before_ms);
        dispatch.callback.tls = true;
        dispatch.llm_worker_name = "memories-llm-custom".into();
        dispatch.output_language = "en".into();
        let metadata = SummaryGenerationMetadata {
            generation_id: "generation-shared".into(),
            generation_started_at: "2026-10-15T10:11:12Z".into(),
            source_scope_digest: "scope-shared".into(),
        };
        let inputs = [
            ImportStep::ThreadSummary,
            ImportStep::ThreadGroupSummary,
            ImportStep::Daily,
            ImportStep::Weekly,
            ImportStep::Monthly,
        ]
        .map(|step| {
            let input = dispatch
                .summary_generation_input(step, &req, &metadata)
                .expect("selected stage input");
            for key in [
                "generation_id",
                "generation_started_at",
                "source_scope_digest",
            ] {
                assert_eq!(input[key], metadata_value(&metadata, key), "{step:?}/{key}");
            }
            input
        });

        assert_eq!(inputs[0]["last_message_after_ms"], 1_746_057_600_000_i64);
        assert_eq!(inputs[0]["last_message_before_ms"], 1_748_735_999_999_i64);
        assert!(inputs[0]["policy_json"].is_string());
        assert_eq!(inputs[1]["group_owner_user_id"], 1);
        assert_eq!(inputs[1]["trigger_kind"], "manual");
        assert_eq!(inputs[1]["activity_after_ms"], 1_700_000_000_000_i64);
        assert_eq!(inputs[1]["activity_before_ms"], 1_700_086_400_000_i64);
        assert_eq!(inputs[1]["stop_on_error"], true);
        assert_eq!(inputs[2]["start_date"], "2026-03-01");
        assert_eq!(inputs[2]["end_date"], "2026-05-31");
        assert_eq!(inputs[2]["stop_on_error"], true);
        assert_eq!(inputs[3]["start_week"], "2026-W09");
        assert_eq!(inputs[3]["end_week"], "2026-W22");
        assert_eq!(inputs[4]["start_month"], "2026-02");
        assert_eq!(inputs[4]["end_month"], "2026-04");
        for input in inputs {
            assert_eq!(input["memories_grpc_host"], "127.0.0.1");
            assert_eq!(input["memories_grpc_port"], 9010);
            assert_eq!(input["memories_grpc_tls"], true);
            assert_eq!(input["llm_worker_name"], "memories-llm-custom");
            assert_eq!(input["output_language"], "en");
        }
    }

    fn metadata_value(metadata: &SummaryGenerationMetadata, key: &str) -> serde_json::Value {
        match key {
            "generation_id" => metadata.generation_id.clone().into(),
            "generation_started_at" => metadata.generation_started_at.clone().into(),
            "source_scope_digest" => metadata.source_scope_digest.clone().into(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn batch_dispatch_propagates_remote_tls_callback() {
        // A remote HTTPS memories must flow host/port/tls into every batch
        // input so the fanned-out workflows dial back over TLS.
        let d = BatchDispatch {
            user_id: 1,
            callback: GrpcAddress {
                host: "memories.example.com".into(),
                port: 8443,
                tls: true,
            },
            data_paths: None,
            _test_data_root: None,
            last_message_after_ms: None,
            last_message_before_ms: None,
            thread_created_after_ms: None,
            thread_created_before_ms: None,
            target_thread_ids: None,
            rejected_candidate_ids_to_unsuppress_by_thread: None,
            llm_worker_name: "memories-llm".to_string(),
            output_language: "ja".to_string(),
            summary_output_guidance: serde_json::json!({}),
            summary_llm_contract: SummaryLlmContract {
                context_window_tokens: 131_072,
                expected_model: "test.gguf".to_string(),
                budget: SummaryBudget {
                    max_completion_tokens: 32_768,
                    input_byte_budget: 390_000,
                },
            },
        };
        for v in [
            d.summarize_input(),
            d.personality_input(),
            d.reflection_input(),
        ] {
            assert_eq!(v["memories_grpc_host"], "memories.example.com");
            assert_eq!(v["memories_grpc_port"], 8443);
            assert_eq!(v["memories_grpc_tls"], true);
        }
    }

    #[test]
    fn batch_dispatch_personality_input_matches_workflow_schema() {
        let v = dispatch().personality_input();
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert!(v.get("personality_user_id").is_none());
        assert_eq!(v["min_user_messages"], 2);
        assert_eq!(v["max_context_chars"], 150_000);
    }

    #[test]
    fn batch_dispatch_personality_input_defaults_force_reextract_false() {
        // The import pipeline must never force re-extraction silently; only
        // the Personality tab's "Force 再抽出" checkbox does that explicitly
        // via `personality_input_with(true)`.
        let v = dispatch().personality_input();
        assert_eq!(v["force_reextract"], false);
    }

    #[test]
    fn batch_dispatch_personality_input_with_true_sets_force_reextract() {
        // Drift guard for the standalone Force re-extract path. Layer-1
        // batch reads `force_reextract` to disable the
        // `target_signal_count` short-circuit AND forwards it to single, so
        // both legs flip with this one flag.
        let v = dispatch().personality_input_with(true);
        assert_eq!(v["force_reextract"], true);
    }

    #[test]
    fn batch_dispatch_personality_input_enables_merge() {
        // The batch chains layer-2 merge itself (so both the standalone button
        // and the import path produce a profile). It now resolves the
        // `memories-user-personality-merge-<lang>` worker by name, gated by the
        // `merge_enabled` flag — the old merge YAML path relay is gone.
        let v = dispatch().personality_input();
        assert_eq!(v["merge_enabled"], true);
        assert!(v.get("merge_workflow_path").is_none());
    }

    #[test]
    fn batch_dispatch_merge_only_input_omits_per_thread_fields() {
        // The merge-only dispatch targets user-personality-merge directly,
        // so it must NOT carry per-thread batch fields — those are unused
        // by the merge YAML and a stray `single_workflow_path` would just
        // be ignored, but a stray `merge_workflow_path` is doubly confusing
        // (it's the dispatch target, not an input). Lock the shape so a
        // future "tidy up" copy/paste from `personality_input_with` cannot
        // accidentally re-introduce them.
        let v = dispatch().merge_only_input(false);
        assert!(v.get("single_workflow_path").is_none());
        assert!(v.get("merge_workflow_path").is_none());
        assert!(v.get("force_reextract").is_none());
    }

    #[test]
    fn batch_dispatch_merge_only_input_forwards_force_remerge_and_callback() {
        // Force-on must reach the merge YAML so the eligibility short-circuit
        // can be bypassed. The callback (memories_grpc_host / port / tls)
        // and llm_worker_name must ride along — without them the merge has
        // no path back to memories nor a way to call the LLM.
        let v = dispatch().merge_only_input(true);
        assert_eq!(v["force_remerge"], true);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("personality_user_id").is_none());
        assert!(v.get("summary_user_id").is_none());
        assert_eq!(v["max_signals"], 100);
        assert_eq!(v["max_context_chars"], 150_000);
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
        assert_eq!(v["llm_worker_name"], "memories-llm");
    }

    #[test]
    fn batch_dispatch_merge_only_input_defaults_force_false() {
        let v = dispatch().merge_only_input(false);
        assert_eq!(v["force_remerge"], false);
    }

    #[test]
    fn batch_dispatch_reflection_input_matches_workflow_schema() {
        let v = dispatch().reflection_input();
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert_eq!(v["prompt_version"], REFLECTION_PROMPT_VERSION);
    }

    #[test]
    fn batch_dispatch_inputs_include_last_message_after_ms_when_set() {
        let d = dispatch_with(Some(1_746_057_600_000));
        for v in [
            d.summarize_input(),
            d.personality_input(),
            d.reflection_input(),
        ] {
            assert_eq!(
                v["last_message_after_ms"], 1_746_057_600_000_i64,
                "input missing last_message_after_ms: {v}"
            );
        }
    }

    #[test]
    fn batch_dispatch_inputs_omit_last_message_after_ms_when_none() {
        let d = dispatch_with(None);
        for v in [
            d.summarize_input(),
            d.personality_input(),
            d.reflection_input(),
        ] {
            assert!(
                v.get("last_message_after_ms").is_none(),
                "input must omit last_message_after_ms when None: {v}"
            );
        }
    }
}
