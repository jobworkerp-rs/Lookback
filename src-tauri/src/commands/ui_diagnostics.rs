use std::collections::HashSet;
use std::sync::OnceLock;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

mod memory;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const INVALID_ACTIVITY_COUNTERS: &str = "invalid UI activity counters";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UiActivityCounters {
    pub staged_received_total: u64,
    pub staged_published_total: u64,
    pub generated_refresh_total: u64,
    pub summaries_invalidated_total: u64,
    pub summary_list_render_attempts_total: u64,
    pub summary_card_render_attempts_total: u64,
}

impl UiActivityCounters {
    fn validate(&self) -> Result<(), &'static str> {
        let counters = [
            self.staged_received_total,
            self.staged_published_total,
            self.generated_refresh_total,
            self.summaries_invalidated_total,
            self.summary_list_render_attempts_total,
            self.summary_card_render_attempts_total,
        ];
        if counters.into_iter().any(|value| value > MAX_SAFE_INTEGER) {
            return Err(INVALID_ACTIVITY_COUNTERS);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for UiActivityCounters {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Keep malformed IPC failures fixed; do not reflect an unknown field name or value.
        let counters = UiActivityCountersWire::deserialize(deserializer)
            .map_err(|_| D::Error::custom(INVALID_ACTIVITY_COUNTERS))?;
        Ok(Self {
            staged_received_total: counters.staged_received_total,
            staged_published_total: counters.staged_published_total,
            generated_refresh_total: counters.generated_refresh_total,
            summaries_invalidated_total: counters.summaries_invalidated_total,
            summary_list_render_attempts_total: counters.summary_list_render_attempts_total,
            summary_card_render_attempts_total: counters.summary_card_render_attempts_total,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UiActivityCountersWire {
    staged_received_total: u64,
    staged_published_total: u64,
    generated_refresh_total: u64,
    summaries_invalidated_total: u64,
    summary_list_render_attempts_total: u64,
    summary_card_render_attempts_total: u64,
}

const MAX_DIAGNOSTIC_LOCATIONS: usize = 4;
const MAX_PROCESS_DIAGNOSTICS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiDiagnosticSource {
    ReactCaught,
    ReactUncaught,
    WindowError,
    UnhandledRejection,
}

impl UiDiagnosticSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReactCaught => "react-caught",
            Self::ReactUncaught => "react-uncaught",
            Self::WindowError => "window-error",
            Self::UnhandledRejection => "unhandled-rejection",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum UiErrorKind {
    Error,
    TypeError,
    RangeError,
    ReferenceError,
    SyntaxError,
    #[serde(rename = "URIError")]
    UriError,
    EvalError,
    AggregateError,
    Unknown,
}

impl UiErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::ReferenceError => "ReferenceError",
            Self::SyntaxError => "SyntaxError",
            Self::UriError => "URIError",
            Self::EvalError => "EvalError",
            Self::AggregateError => "AggregateError",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiDiagnosticCode {
    MaximumUpdateDepth,
    TooManyRerenders,
    Unknown,
}

impl UiDiagnosticCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::MaximumUpdateDepth => "maximum-update-depth",
            Self::TooManyRerenders => "too-many-rerenders",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiDiagnosticArea {
    Entry,
    App,
    Summaries,
    StagedProgressHook,
    StagedProgressPanel,
    Bundle,
    Other,
}

impl UiDiagnosticArea {
    fn as_str(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::App => "app",
            Self::Summaries => "summaries",
            Self::StagedProgressHook => "staged-progress-hook",
            Self::StagedProgressPanel => "staged-progress-panel",
            Self::Bundle => "bundle",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiDiagnosticLocation {
    pub area: UiDiagnosticArea,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiDiagnostic {
    pub source: UiDiagnosticSource,
    pub error_kind: UiErrorKind,
    pub code: Option<UiDiagnosticCode>,
    pub locations: Vec<UiDiagnosticLocation>,
}

impl UiDiagnostic {
    fn validate(&self) -> Result<(), &'static str> {
        if self.locations.len() > MAX_DIAGNOSTIC_LOCATIONS {
            return Err("too many UI diagnostic locations");
        }
        if self
            .locations
            .iter()
            .any(|location| location.line == 0 || location.column == 0)
        {
            return Err("UI diagnostic positions must be positive");
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum NativeTerminationReason {
    Crashed,
    ExceededMemoryLimit,
    Unknown,
}

#[cfg(target_os = "linux")]
impl NativeTerminationReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Crashed => "crashed",
            Self::ExceededMemoryLimit => "exceeded-memory-limit",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DiagnosticSignature {
    Frontend(UiDiagnostic),
    #[cfg(target_os = "linux")]
    Native(NativeTerminationReason),
}

#[derive(Default)]
struct DiagnosticLedger {
    signatures: HashSet<DiagnosticSignature>,
    emitted_count: usize,
}

impl DiagnosticLedger {
    fn record(&mut self, signature: DiagnosticSignature, emit: impl FnOnce()) -> bool {
        if self.emitted_count >= MAX_PROCESS_DIAGNOSTICS || !self.signatures.insert(signature) {
            return false;
        }
        self.emitted_count += 1;
        emit();
        true
    }
}

static DIAGNOSTIC_LEDGER: OnceLock<parking_lot::Mutex<DiagnosticLedger>> = OnceLock::new();

fn global_diagnostic_ledger() -> &'static parking_lot::Mutex<DiagnosticLedger> {
    DIAGNOSTIC_LEDGER.get_or_init(|| parking_lot::Mutex::new(DiagnosticLedger::default()))
}

#[tauri::command]
pub fn report_ui_diagnostic(diagnostic: UiDiagnostic) -> Result<(), String> {
    diagnostic.validate().map_err(str::to_owned)?;
    let mut ledger = global_diagnostic_ledger().lock();
    record_frontend_diagnostic_with(&mut ledger, diagnostic, log_frontend_diagnostic);
    Ok(())
}

#[tauri::command]
pub fn report_ui_activity_sample(counters: UiActivityCounters) -> Result<bool, String> {
    counters.validate().map_err(str::to_owned)?;
    Ok(memory::store_ui_activity_sample(counters))
}

#[cfg(target_os = "linux")]
pub(crate) fn start_memory_sampler() {
    memory::start_sampler();
}

pub(crate) fn stop_memory_sampler() {
    memory::stop_sampler();
}

fn record_frontend_diagnostic_with(
    ledger: &mut DiagnosticLedger,
    diagnostic: UiDiagnostic,
    emit: impl FnOnce(&UiDiagnostic),
) -> bool {
    ledger.record(DiagnosticSignature::Frontend(diagnostic.clone()), || {
        emit(&diagnostic);
    })
}

fn log_frontend_diagnostic(diagnostic: &UiDiagnostic) {
    let locations = diagnostic
        .locations
        .iter()
        .map(|location| (location.area.as_str(), location.line, location.column))
        .collect::<Vec<_>>();
    tracing::error!(
        target: "ui_diagnostics",
        source = diagnostic.source.as_str(),
        error_kind = diagnostic.error_kind.as_str(),
        code = diagnostic.code.map(UiDiagnosticCode::as_str),
        locations = ?locations,
        "frontend UI exception"
    );
}

#[cfg(target_os = "linux")]
fn record_native_termination_with(
    ledger: &mut DiagnosticLedger,
    reason: Option<NativeTerminationReason>,
    emit: impl FnOnce(NativeTerminationReason),
) -> bool {
    let Some(reason) = reason else {
        return false;
    };
    ledger.record(DiagnosticSignature::Native(reason), || emit(reason))
}

#[cfg(target_os = "linux")]
fn record_native_termination_with_samples(
    ledger: &mut DiagnosticLedger,
    reason: Option<NativeTerminationReason>,
    samples: &str,
    emit: impl FnOnce(NativeTerminationReason, &str),
) -> bool {
    record_native_termination_with(ledger, reason, |reason| emit(reason, samples))
}

#[cfg(target_os = "linux")]
fn log_native_termination_with_samples(reason: NativeTerminationReason, samples: &str) {
    tracing::warn!(
        target: "ui_diagnostics",
        reason = reason.as_str(),
        memory_samples = %samples,
        "WebKit renderer process terminated"
    );
}

#[cfg(target_os = "linux")]
fn map_web_process_termination(
    reason: webkit2gtk::WebProcessTerminationReason,
) -> Option<NativeTerminationReason> {
    use webkit2gtk::WebProcessTerminationReason;

    match reason {
        WebProcessTerminationReason::Crashed => Some(NativeTerminationReason::Crashed),
        WebProcessTerminationReason::ExceededMemoryLimit => {
            Some(NativeTerminationReason::ExceededMemoryLimit)
        }
        WebProcessTerminationReason::TerminatedByApi => None,
        _ => Some(NativeTerminationReason::Unknown),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn report_web_process_termination(reason: webkit2gtk::WebProcessTerminationReason) {
    let Some(reason) = map_web_process_termination(reason) else {
        return;
    };
    let samples = memory::latest_sample_json();
    let mut ledger = global_diagnostic_ledger().lock();
    record_native_termination_with_samples(
        &mut ledger,
        Some(reason),
        &samples,
        log_native_termination_with_samples,
    );
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    fn sample_diagnostic(line: u32) -> UiDiagnostic {
        UiDiagnostic {
            source: UiDiagnosticSource::ReactCaught,
            error_kind: UiErrorKind::TypeError,
            code: Some(UiDiagnosticCode::MaximumUpdateDepth),
            locations: vec![UiDiagnosticLocation {
                area: UiDiagnosticArea::Summaries,
                line,
                column: 7,
            }],
        }
    }

    #[test]
    fn rejects_unrecognized_values_extra_fields_and_unbounded_locations() {
        let unknown_enum = r#"{
            "source":"react-caught",
            "error_kind":"SecretError",
            "code":"unknown",
            "locations":[]
        }"#;
        assert!(serde_json::from_str::<UiDiagnostic>(unknown_enum).is_err());
        assert!(serde_json::from_str::<UiDiagnosticSource>(r#""unapproved-source""#).is_err());
        assert!(serde_json::from_str::<UiDiagnosticCode>(r#""secret-code""#).is_err());
        assert!(serde_json::from_str::<UiDiagnosticArea>(r#""/home/user/private.ts""#).is_err());
        assert_eq!(
            serde_json::from_str::<UiErrorKind>(r#""URIError""#).unwrap(),
            UiErrorKind::UriError
        );

        let extra_field = r#"{
            "source":"react-caught",
            "error_kind":"TypeError",
            "code":"unknown",
            "locations":[],
            "message":"secret-message"
        }"#;
        assert!(serde_json::from_str::<UiDiagnostic>(extra_field).is_err());

        let mut invalid = sample_diagnostic(0);
        assert!(invalid.validate().is_err());
        invalid.locations[0].line = u32::MAX;
        invalid.locations[0].column = 1;
        assert!(invalid.validate().is_ok());

        let mut too_many = sample_diagnostic(1);
        too_many.locations = (1..=5)
            .map(|line| UiDiagnosticLocation {
                area: UiDiagnosticArea::Other,
                line,
                column: 1,
            })
            .collect();
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn emitted_frontend_logs_contain_only_the_fixed_diagnostic_contract() {
        let output = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(output.clone())
            .finish();
        let mut ledger = DiagnosticLedger::default();

        tracing::subscriber::with_default(subscriber, || {
            record_frontend_diagnostic_with(&mut ledger, sample_diagnostic(42), |diagnostic| {
                log_frontend_diagnostic(diagnostic);
            });
        });

        let log = output.as_string();
        assert!(log.contains("frontend UI exception"));
        assert!(log.contains("react-caught"));
        assert!(log.contains("TypeError"));
        assert!(log.contains("maximum-update-depth"));
        assert!(log.contains("summaries"));
        assert!(log.contains("42"));
        assert!(!log.contains("secret-message"));
        assert!(!log.contains("https://"));
    }

    #[test]
    fn ordinary_activity_samples_and_validation_failures_do_not_log() {
        let output = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(output.clone())
            .finish();
        let counters = UiActivityCounters {
            staged_received_total: 1,
            staged_published_total: 2,
            generated_refresh_total: 3,
            summaries_invalidated_total: 4,
            summary_list_render_attempts_total: 5,
            summary_card_render_attempts_total: 6,
        };

        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(
                report_ui_activity_sample(counters),
                Ok(cfg!(target_os = "linux"))
            );
            let invalid = UiActivityCounters {
                staged_received_total: MAX_SAFE_INTEGER + 1,
                ..counters
            };
            assert_eq!(
                report_ui_activity_sample(invalid),
                Err(INVALID_ACTIVITY_COUNTERS.to_owned())
            );
        });

        assert!(output.as_string().is_empty());
    }

    #[test]
    fn deduplicates_frontend_reports_and_caps_all_backend_events_at_sixteen() {
        let mut ledger = DiagnosticLedger::default();
        let mut emitted = Vec::new();

        assert!(record_frontend_diagnostic_with(
            &mut ledger,
            sample_diagnostic(1),
            |diagnostic| emitted.push(diagnostic.clone()),
        ));
        assert!(!record_frontend_diagnostic_with(
            &mut ledger,
            sample_diagnostic(1),
            |diagnostic| emitted.push(diagnostic.clone()),
        ));
        for line in 2..=20 {
            record_frontend_diagnostic_with(&mut ledger, sample_diagnostic(line), |diagnostic| {
                emitted.push(diagnostic.clone());
            });
        }

        assert_eq!(emitted.len(), 16);
        assert_eq!(ledger.emitted_count, 16);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expected_webkit_shutdown_is_ignored_and_unknown_reasons_are_fixed() {
        let mut ledger = DiagnosticLedger::default();
        let mut reasons = Vec::new();

        assert!(!record_native_termination_with(
            &mut ledger,
            None,
            |reason| reasons.push(reason),
        ));
        assert!(record_native_termination_with(
            &mut ledger,
            Some(NativeTerminationReason::Crashed),
            |reason| reasons.push(reason),
        ));
        assert!(!record_native_termination_with(
            &mut ledger,
            Some(NativeTerminationReason::Crashed),
            |reason| reasons.push(reason),
        ));
        assert!(record_native_termination_with(
            &mut ledger,
            Some(NativeTerminationReason::Unknown),
            |reason| reasons.push(reason),
        ));

        assert_eq!(
            reasons,
            [
                NativeTerminationReason::Crashed,
                NativeTerminationReason::Unknown
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn changing_memory_values_do_not_bypass_reason_deduplication() {
        let mut ledger = DiagnosticLedger::default();
        let mut emitted_samples = Vec::new();

        assert!(record_native_termination_with_samples(
            &mut ledger,
            Some(NativeTerminationReason::Crashed),
            "first sample",
            |_, samples| emitted_samples.push(samples.to_owned()),
        ));
        assert!(!record_native_termination_with_samples(
            &mut ledger,
            Some(NativeTerminationReason::Crashed),
            "later sample",
            |_, samples| emitted_samples.push(samples.to_owned()),
        ));

        assert_eq!(emitted_samples, ["first sample".to_owned()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expected_api_termination_does_not_emit_even_when_samples_are_available() {
        let mut ledger = DiagnosticLedger::default();
        let mut emitted = false;
        assert!(!record_native_termination_with_samples(
            &mut ledger,
            None,
            "private sample text is never emitted",
            |_, _| emitted = true,
        ));
        assert!(!emitted);
        assert_eq!(ledger.emitted_count, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn webkit_reasons_map_to_fixed_logged_values_without_raw_reason_codes() {
        use webkit2gtk::WebProcessTerminationReason;

        let output = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(output.clone())
            .finish();
        let mut ledger = DiagnosticLedger::default();

        tracing::subscriber::with_default(subscriber, || {
            for reason in [
                WebProcessTerminationReason::Crashed,
                WebProcessTerminationReason::ExceededMemoryLimit,
                WebProcessTerminationReason::TerminatedByApi,
                WebProcessTerminationReason::__Unknown(43),
            ] {
                record_native_termination_with(
                    &mut ledger,
                    map_web_process_termination(reason),
                    |reason| log_native_termination_with_samples(reason, "[]"),
                );
            }
        });

        let log = output.as_string();
        assert_eq!(log.matches("WebKit renderer process terminated").count(), 3);
        assert!(log.contains("crashed"));
        assert!(log.contains("exceeded-memory-limit"));
        assert!(log.contains("unknown"));
        assert!(!log.contains("reason=\"43\""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn abnormal_native_log_appends_only_the_fixed_memory_sample_payload() {
        let output = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(output.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            log_native_termination_with_samples(
                NativeTerminationReason::ExceededMemoryLimit,
                r#"[{"sample_age_ms":12,"app_rss_bytes":1200,"webkit_web_process_count":1,"webkit_web_process_rss_bytes":900,"frontend_age_ms":24,"counters":{"staged_received_total":1,"staged_published_total":2,"generated_refresh_total":3,"summaries_invalidated_total":4,"summary_list_render_attempts_total":5,"summary_card_render_attempts_total":6}}]"#,
            );
        });

        let log = output.as_string();
        assert!(log.contains("exceeded-memory-limit"));
        assert!(log.contains("memory_samples="));
        assert!(log.contains("staged_received_total"));
        assert!(!log.contains("thread_id"));
        assert!(!log.contains("/proc/"));
    }

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn as_string(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer lock").clone())
                .expect("UTF-8 tracing output")
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("buffer lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
}
