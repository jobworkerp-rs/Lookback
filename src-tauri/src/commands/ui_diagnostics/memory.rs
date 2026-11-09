use super::UiActivityCounters;

#[cfg(target_os = "linux")]
use std::collections::{HashSet, VecDeque};
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::io::{self, Read};
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use serde::Serialize;

#[cfg(target_os = "linux")]
const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(target_os = "linux")]
const MAX_MEMORY_SAMPLES: usize = 6;
#[cfg(target_os = "linux")]
const MAX_PROC_FILE_BYTES: usize = 16 * 1024;
#[cfg(target_os = "linux")]
const MAX_PROCESS_DEPTH: usize = 8;
#[cfg(target_os = "linux")]
const MAX_PROCESSES: usize = 256;
#[cfg(target_os = "linux")]
const MAX_TASK_CHILD_READS: usize = 1_024;
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct FrontendActivity {
    counters: UiActivityCounters,
    received_at: Instant,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct MemorySample {
    sampled_at: Instant,
    app_rss_bytes: Option<u64>,
    webkit_web_process_count: Option<u32>,
    webkit_web_process_rss_bytes: Option<u64>,
    frontend_age_ms: Option<u64>,
    counters: Option<UiActivityCounters>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RendererMetrics {
    webkit_web_process_count: Option<u32>,
    webkit_web_process_rss_bytes: Option<u64>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct ProcessMetrics {
    app_rss_bytes: Option<u64>,
    renderer: Option<RendererMetrics>,
}

#[cfg(target_os = "linux")]
impl ProcessMetrics {
    fn unknown() -> Self {
        Self {
            app_rss_bytes: None,
            renderer: None,
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct SampleRing {
    samples: [Option<MemorySample>; MAX_MEMORY_SAMPLES],
    next: usize,
    len: usize,
}

#[cfg(target_os = "linux")]
impl SampleRing {
    fn push(&mut self, sample: MemorySample) {
        self.samples[self.next] = Some(sample);
        self.next = (self.next + 1) % MAX_MEMORY_SAMPLES;
        self.len = (self.len + 1).min(MAX_MEMORY_SAMPLES);
    }

    fn oldest_first(&self) -> Vec<MemorySample> {
        let start = (self.next + MAX_MEMORY_SAMPLES - self.len) % MAX_MEMORY_SAMPLES;
        (0..self.len)
            .filter_map(|offset| self.samples[(start + offset) % MAX_MEMORY_SAMPLES])
            .collect()
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct MemoryTelemetry {
    latest_activity: Option<FrontendActivity>,
    samples: SampleRing,
    stopped: bool,
}

#[cfg(target_os = "linux")]
impl MemoryTelemetry {
    fn record_sample(&mut self, metrics: ProcessMetrics, sampled_at: Instant) -> bool {
        if self.stopped {
            return false;
        }
        let activity = self.latest_activity;
        self.samples.push(MemorySample {
            sampled_at,
            app_rss_bytes: metrics.app_rss_bytes,
            webkit_web_process_count: metrics
                .renderer
                .and_then(|renderer| renderer.webkit_web_process_count),
            webkit_web_process_rss_bytes: metrics
                .renderer
                .and_then(|renderer| renderer.webkit_web_process_rss_bytes),
            frontend_age_ms: activity.map(|activity| elapsed_ms(sampled_at, activity.received_at)),
            counters: activity.map(|activity| activity.counters),
        });
        true
    }
}

#[cfg(target_os = "linux")]
fn telemetry() -> &'static Arc<parking_lot::Mutex<MemoryTelemetry>> {
    static TELEMETRY: OnceLock<Arc<parking_lot::Mutex<MemoryTelemetry>>> = OnceLock::new();
    TELEMETRY.get_or_init(|| Arc::new(parking_lot::Mutex::new(MemoryTelemetry::default())))
}

#[cfg(target_os = "linux")]
fn elapsed_ms(later: Instant, earlier: Instant) -> u64 {
    later
        .saturating_duration_since(earlier)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(super) fn store_ui_activity_sample(counters: UiActivityCounters) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut telemetry = telemetry().lock();
        if !telemetry.stopped {
            telemetry.latest_activity = Some(FrontendActivity {
                counters,
                received_at: Instant::now(),
            });
        }
        true
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = counters;
        false
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct SamplerLifecycle {
    started: bool,
    stopped: bool,
    cancellation: Option<tokio_util::sync::CancellationToken>,
}

#[cfg(target_os = "linux")]
impl SamplerLifecycle {
    fn start(&mut self) -> Option<tokio_util::sync::CancellationToken> {
        if self.started || self.stopped {
            return None;
        }
        self.started = true;
        let cancellation = tokio_util::sync::CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        Some(cancellation)
    }

    fn stop(&mut self) {
        self.stopped = true;
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }
}

#[cfg(target_os = "linux")]
fn sampler_lifecycle() -> &'static parking_lot::Mutex<SamplerLifecycle> {
    static LIFECYCLE: OnceLock<parking_lot::Mutex<SamplerLifecycle>> = OnceLock::new();
    LIFECYCLE.get_or_init(|| parking_lot::Mutex::new(SamplerLifecycle::default()))
}

#[cfg(target_os = "linux")]
pub(super) fn start_sampler() {
    let cancellation = sampler_lifecycle().lock().start();
    let Some(cancellation) = cancellation else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        run_sampler(cancellation).await;
    });
}

pub(super) fn stop_sampler() {
    #[cfg(target_os = "linux")]
    {
        telemetry().lock().stopped = true;
        sampler_lifecycle().lock().stop();
    }
}

#[cfg(target_os = "linux")]
async fn run_sampler(cancellation: tokio_util::sync::CancellationToken) {
    run_sampler_with(
        cancellation,
        Arc::clone(telemetry()),
        MEMORY_SAMPLE_INTERVAL,
        || async {
            tokio::task::spawn_blocking(collect_process_metrics)
                .await
                .unwrap_or_else(|_| ProcessMetrics::unknown())
        },
    )
    .await;
}

#[cfg(target_os = "linux")]
async fn run_sampler_with<F, Fut>(
    cancellation: tokio_util::sync::CancellationToken,
    telemetry: Arc<parking_lot::Mutex<MemoryTelemetry>>,
    interval_duration: Duration,
    mut collect: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ProcessMetrics>,
{
    let mut interval = tokio::time::interval(interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            _ = interval.tick() => {
                if cancellation.is_cancelled() {
                    return;
                }
                let metrics = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = collect() => result,
                };
                if cancellation.is_cancelled() {
                    return;
                }
                telemetry.lock().record_sample(metrics, Instant::now());
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_process_metrics() -> ProcessMetrics {
    let proc_root = Path::new("/proc");
    let pid = std::process::id();
    let app_rss_bytes = read_proc_file(&proc_root.join(pid.to_string()).join("status"))
        .ok()
        .and_then(|contents| parse_vmrss_bytes(&contents));
    let renderer = collect_renderer_metrics(proc_root, pid);
    ProcessMetrics {
        app_rss_bytes,
        renderer,
    }
}

#[cfg(target_os = "linux")]
fn collect_renderer_metrics(proc_root: &Path, root_pid: u32) -> Option<RendererMetrics> {
    if root_pid == 0 {
        return None;
    }

    let mut pending = VecDeque::from([(root_pid, 0usize)]);
    let mut discovered = HashSet::with_capacity(MAX_PROCESSES);
    discovered.insert(root_pid);
    let mut task_child_reads = 0usize;
    let mut renderer_count = 0u32;
    let mut renderer_rss_bytes = 0u64;

    while let Some((pid, depth)) = pending.pop_front() {
        let process_dir = proc_root.join(pid.to_string());
        let executable = fs::read_link(process_dir.join("exe")).ok()?;
        if executable.file_name() == Some(OsStr::new("WebKitWebProcess")) {
            let status = read_proc_file(&process_dir.join("status")).ok()?;
            let rss_bytes = parse_vmrss_bytes(&status)?;
            renderer_count = renderer_count.checked_add(1)?;
            renderer_rss_bytes = renderer_rss_bytes.checked_add(rss_bytes)?;
        }

        let tasks = fs::read_dir(process_dir.join("task")).ok()?;
        for task in tasks {
            let task = task.ok()?;
            let tid = task.file_name().into_string().ok()?.parse::<u32>().ok()?;
            if tid == 0 {
                return None;
            }
            if task_child_reads >= MAX_TASK_CHILD_READS {
                return None;
            }
            task_child_reads += 1;
            let children = read_proc_file(
                &process_dir
                    .join("task")
                    .join(tid.to_string())
                    .join("children"),
            )
            .ok()?;
            for child_pid in parse_child_pids(&children)? {
                if child_pid == 0 || !discovered.insert(child_pid) {
                    continue;
                }
                if discovered.len() > MAX_PROCESSES || depth >= MAX_PROCESS_DEPTH {
                    return None;
                }
                pending.push_back((child_pid, depth + 1));
            }
        }
    }

    Some(RendererMetrics {
        webkit_web_process_count: Some(renderer_count),
        webkit_web_process_rss_bytes: (renderer_count > 0).then_some(renderer_rss_bytes),
    })
}

#[cfg(target_os = "linux")]
fn read_proc_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut contents = Vec::with_capacity(MAX_PROC_FILE_BYTES.min(4_096));
    file.take((MAX_PROC_FILE_BYTES + 1) as u64)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_PROC_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "proc file exceeded read limit",
        ));
    }
    Ok(contents)
}

#[cfg(target_os = "linux")]
fn parse_child_pids(contents: &[u8]) -> Option<Vec<u32>> {
    let contents = std::str::from_utf8(contents).ok()?;
    contents
        .split_ascii_whitespace()
        .map(|child| {
            let pid = child.parse::<u32>().ok()?;
            (pid > 0).then_some(pid)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_vmrss_bytes(contents: &[u8]) -> Option<u64> {
    let contents = std::str::from_utf8(contents).ok()?;
    let mut vmrss = None;
    for line in contents.lines() {
        let Some(value) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        if vmrss.is_some() {
            return None;
        }
        let mut fields = value.split_ascii_whitespace();
        let kib = fields.next()?.parse::<u64>().ok()?;
        if fields.next()? != "kB" || fields.next().is_some() {
            return None;
        }
        vmrss = Some(kib.checked_mul(1024)?);
    }
    vmrss
}

#[cfg(target_os = "linux")]
#[derive(Debug, Serialize)]
struct LoggedMemorySample {
    sample_age_ms: u64,
    app_rss_bytes: Option<u64>,
    webkit_web_process_count: Option<u32>,
    webkit_web_process_rss_bytes: Option<u64>,
    frontend_age_ms: Option<u64>,
    counters: Option<UiActivityCounters>,
}

#[cfg(target_os = "linux")]
pub(super) fn latest_sample_json() -> String {
    let now = Instant::now();
    let samples = telemetry().lock().samples.oldest_first();
    memory_samples_json(&samples, now)
}

#[cfg(target_os = "linux")]
fn memory_samples_json(samples: &[MemorySample], now: Instant) -> String {
    let logged = samples
        .iter()
        .map(|sample| LoggedMemorySample {
            sample_age_ms: elapsed_ms(now, sample.sampled_at),
            app_rss_bytes: sample.app_rss_bytes,
            webkit_web_process_count: sample.webkit_web_process_count,
            webkit_web_process_rss_bytes: sample.webkit_web_process_rss_bytes,
            frontend_age_ms: sample.frontend_age_ms,
            counters: sample.counters,
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&logged).unwrap_or_else(|_| "[]".to_owned())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn non_linux_sampler_surface_compiles_and_is_inactive() {
        // Select the real non-Linux branches on the host without requiring a
        // cross-platform standard library or the platform's GUI toolchain.
        let source = include_str!("memory.rs").replace(
            "target_os = \"linux\"",
            "feature = \"memory_diagnostics_linux\"",
        );
        let fixture = format!(
            "mod diagnostics {{
                #[derive(Clone, Copy)] pub struct UiActivityCounters;
                mod memory {{ {source} }}
                pub fn check() {{
                    assert!(!memory::store_ui_activity_sample(UiActivityCounters));
                    memory::stop_sampler();
                }}
             }}
             fn main() {{ diagnostics::check(); }}"
        );
        let directory = tempfile::tempdir().expect("portable sampler fixture directory");
        let source_path = directory.path().join("portable.rs");
        let binary_path = directory.path().join("portable");
        fs::write(&source_path, fixture).expect("write portable sampler fixture");
        let output =
            std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg("--edition=2024")
                .arg(&source_path)
                .arg("-o")
                .arg(&binary_path)
                .output()
                .expect("compile non-Linux sampler branches");
        assert!(
            output.status.success(),
            "non-Linux sampler branches failed to compile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::process::Command::new(binary_path)
                .status()
                .expect("run non-Linux sampler fixture")
                .success()
        );
    }

    fn counters() -> UiActivityCounters {
        UiActivityCounters {
            staged_received_total: 1,
            staged_published_total: 2,
            generated_refresh_total: 3,
            summaries_invalidated_total: 4,
            summary_list_render_attempts_total: 5,
            summary_card_render_attempts_total: 6,
        }
    }

    fn decode_counters(input: &str) -> Result<UiActivityCounters, serde_json::Error> {
        let value = serde_json::from_str(input).expect("valid JSON fixture");
        serde_json::from_value(value)
    }

    #[test]
    fn activity_counters_accept_only_six_safe_nonnegative_integers() {
        let mut valid = counters();
        assert!(valid.validate().is_ok());
        valid.summary_card_render_attempts_total = super::super::MAX_SAFE_INTEGER;
        assert!(valid.validate().is_ok());
        valid.summary_card_render_attempts_total += 1;
        assert!(valid.validate().is_err());

        let valid_value = serde_json::to_value(counters()).expect("serialize counters");
        assert!(serde_json::from_value::<UiActivityCounters>(valid_value).is_ok());
        let negative = decode_counters(
            r#"{"staged_received_total":-1,"staged_published_total":0,"generated_refresh_total":0,"summaries_invalidated_total":0,"summary_list_render_attempts_total":0,"summary_card_render_attempts_total":0}"#
        )
        .expect_err("negative counter must be rejected");
        assert_eq!(
            negative.to_string(),
            super::super::INVALID_ACTIVITY_COUNTERS
        );
        let fractional = decode_counters(
            r#"{"staged_received_total":1.5,"staged_published_total":0,"generated_refresh_total":0,"summaries_invalidated_total":0,"summary_list_render_attempts_total":0,"summary_card_render_attempts_total":0}"#
        )
        .expect_err("fractional counter must be rejected");
        assert_eq!(
            fractional.to_string(),
            super::super::INVALID_ACTIVITY_COUNTERS
        );
        let above_safe_integer = decode_counters(
            r#"{"staged_received_total":9007199254740992,"staged_published_total":0,"generated_refresh_total":0,"summaries_invalidated_total":0,"summary_list_render_attempts_total":0,"summary_card_render_attempts_total":0}"#
        )
        .expect("u64-shaped counter should deserialize before range validation");
        assert_eq!(
            above_safe_integer.validate(),
            Err(super::super::INVALID_ACTIVITY_COUNTERS)
        );
        let too_large = decode_counters(
            r#"{"staged_received_total":0,"staged_published_total":0,"generated_refresh_total":0,"summaries_invalidated_total":0,"summary_list_render_attempts_total":0,"summary_card_render_attempts_total":0,"thread_id":"private"}"#
        )
        .expect_err("unknown fields must be rejected");
        assert_eq!(
            too_large.to_string(),
            super::super::INVALID_ACTIVITY_COUNTERS
        );
        assert_eq!(
            super::super::report_ui_activity_sample(above_safe_integer),
            Err(super::super::INVALID_ACTIVITY_COUNTERS.to_owned())
        );
    }

    #[test]
    fn sample_ring_keeps_only_the_six_newest_samples_after_thousands_of_ticks() {
        let mut ring = SampleRing::default();
        for tick in 0..10_000 {
            ring.push(test_sample(tick));
        }

        let retained = ring.oldest_first();
        assert_eq!(retained.len(), MAX_MEMORY_SAMPLES);
        assert_eq!(
            retained
                .iter()
                .map(|sample| sample.app_rss_bytes.expect("fixture RSS"))
                .collect::<Vec<_>>(),
            (9_994..10_000).collect::<Vec<_>>()
        );
    }

    #[test]
    fn latest_frontend_activity_replaces_the_previous_fixed_six_counters() {
        let mut telemetry = MemoryTelemetry::default();
        for total in 0..10_000 {
            telemetry.latest_activity = Some(FrontendActivity {
                counters: UiActivityCounters {
                    staged_received_total: total,
                    ..counters()
                },
                received_at: Instant::now(),
            });
        }
        let latest = telemetry.latest_activity.expect("latest report");
        assert_eq!(latest.counters.staged_received_total, 9_999);
        assert_eq!(latest.counters.summary_card_render_attempts_total, 6);
    }

    #[test]
    fn stopped_telemetry_does_not_append_a_late_sample() {
        let mut telemetry = MemoryTelemetry {
            stopped: true,
            ..MemoryTelemetry::default()
        };
        assert!(!telemetry.record_sample(ProcessMetrics::unknown(), Instant::now()));
        assert!(telemetry.samples.oldest_first().is_empty());
    }

    #[test]
    fn sampler_lifecycle_starts_once_and_cancels_only_on_stop() {
        let mut lifecycle = SamplerLifecycle::default();
        let cancellation = lifecycle.start().expect("first start");
        assert!(lifecycle.start().is_none());
        assert!(!cancellation.is_cancelled());

        lifecycle.stop();
        assert!(cancellation.is_cancelled());
        assert!(lifecycle.start().is_none());
    }

    #[tokio::test]
    async fn cancellation_during_collection_does_not_append_a_late_sample() {
        let telemetry = Arc::new(parking_lot::Mutex::new(MemoryTelemetry::default()));
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_telemetry = Arc::clone(&telemetry);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let mut started_sender = Some(started_sender);
        let task = tokio::spawn(async move {
            run_sampler_with(
                task_cancellation,
                task_telemetry,
                Duration::from_secs(60),
                move || {
                    if let Some(started_sender) = started_sender.take() {
                        let _ = started_sender.send(());
                    }
                    std::future::pending::<ProcessMetrics>()
                },
            )
            .await;
        });

        started_receiver.await.expect("collector started");
        cancellation.cancel();
        task.await.expect("sampler task joined");
        assert!(telemetry.lock().samples.oldest_first().is_empty());
    }

    #[tokio::test]
    async fn a_slow_collection_never_overlaps_a_later_interval_tick() {
        struct ActiveCollection(Arc<std::sync::atomic::AtomicUsize>);

        impl Drop for ActiveCollection {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let telemetry = Arc::new(parking_lot::Mutex::new(MemoryTelemetry::default()));
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_active = Arc::clone(&active);
        let task_peak = Arc::clone(&peak);
        let task_telemetry = Arc::clone(&telemetry);
        let task = tokio::spawn(async move {
            run_sampler_with(
                task_cancellation,
                task_telemetry,
                Duration::from_millis(5),
                move || {
                    let current = task_active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    task_peak.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                    let active = Arc::clone(&task_active);
                    async move {
                        let _active = ActiveCollection(active);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        ProcessMetrics {
                            app_rss_bytes: Some(1),
                            renderer: None,
                        }
                    }
                },
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(90)).await;
        cancellation.cancel();
        task.await.expect("sampler task joined");

        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(telemetry.lock().samples.len >= 2);
    }

    #[test]
    fn abnormal_memory_payload_contains_only_bounded_numeric_sample_fields() {
        let sampled_at = Instant::now() - Duration::from_secs(3);
        let sample = MemorySample {
            sampled_at,
            app_rss_bytes: Some(1_000),
            webkit_web_process_count: Some(2),
            webkit_web_process_rss_bytes: Some(2_000),
            frontend_age_ms: Some(500),
            counters: Some(counters()),
        };
        let payload = memory_samples_json(&[sample], Instant::now());
        let value: serde_json::Value = serde_json::from_str(&payload).expect("sample JSON");
        let samples = value.as_array().expect("sample list");
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert!(sample["sample_age_ms"].as_u64().unwrap_or_default() >= 3_000);
        assert_eq!(sample["app_rss_bytes"], 1_000);
        assert_eq!(sample["webkit_web_process_count"], 2);
        assert_eq!(sample["webkit_web_process_rss_bytes"], 2_000);
        assert_eq!(sample["frontend_age_ms"], 500);
        assert_eq!(sample["counters"]["staged_received_total"], 1);
        assert_eq!(sample.as_object().expect("fixed sample object").len(), 6);
        assert!(!payload.contains("pid"));
        assert!(!payload.contains("thread_id"));
        assert!(!payload.contains("/proc"));
    }

    #[test]
    fn proc_tree_sums_only_exact_webkit_renderer_descendants() {
        let fixture = ProcFixture::new();
        fixture.process(1, "/app/lookback", 7, "101 102");
        fixture.process(101, "/usr/lib/WebKitWebProcess", 11, "");
        fixture.process(102, "/usr/lib/WebKitNetworkProcess", 13, "103");
        fixture.process(103, "/opt/WebKitWebProcess", 17, "");

        let renderer = collect_renderer_metrics(&fixture.root, 1).expect("complete proc tree");
        assert_eq!(renderer.webkit_web_process_count, Some(2));
        assert_eq!(
            renderer.webkit_web_process_rss_bytes,
            Some((11 + 17) * 1024)
        );
    }

    #[test]
    fn proc_tree_reports_zero_candidates_without_claiming_zero_rss() {
        let fixture = ProcFixture::new();
        fixture.process(1, "/app/lookback", 7, "2");
        fixture.process(2, "/usr/lib/WebKitNetworkProcess", 13, "");

        let renderer = collect_renderer_metrics(&fixture.root, 1).expect("complete proc tree");
        assert_eq!(renderer.webkit_web_process_count, Some(0));
        assert_eq!(renderer.webkit_web_process_rss_bytes, None);
    }

    #[test]
    fn proc_tree_returns_unknown_when_a_child_disappears_or_is_unreadable() {
        let gone = ProcFixture::new();
        gone.process(1, "/app/lookback", 7, "2");
        assert!(collect_renderer_metrics(&gone.root, 1).is_none());

        let unreadable = ProcFixture::new();
        unreadable.process(1, "/app/lookback", 7, "");
        let children = unreadable.root.join("1/task/1/children");
        fs::remove_file(&children).expect("remove readable children fixture");
        fs::create_dir_all(children).expect("make unreadable children entry");
        assert!(collect_renderer_metrics(&unreadable.root, 1).is_none());
    }

    #[test]
    fn proc_tree_returns_unknown_for_file_depth_and_process_limits() {
        let oversized = ProcFixture::new();
        oversized.process(1, "/app/lookback", 7, "");
        fs::write(
            oversized.root.join("1/task/1/children"),
            vec![b' '; MAX_PROC_FILE_BYTES + 1],
        )
        .expect("write oversized children fixture");
        assert!(collect_renderer_metrics(&oversized.root, 1).is_none());

        let too_deep = ProcFixture::new();
        for depth in 0..=MAX_PROCESS_DEPTH {
            let pid = depth as u32 + 1;
            let child = (depth as u32 + 2).to_string();
            too_deep.process(pid, "/app/lookback", 1, &child);
        }
        let final_pid = MAX_PROCESS_DEPTH as u32 + 2;
        too_deep.process(final_pid, "/app/lookback", 1, "");
        assert!(collect_renderer_metrics(&too_deep.root, 1).is_none());

        let too_many = ProcFixture::new();
        let children = (2..=(MAX_PROCESSES as u32 + 1))
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        too_many.process(1, "/app/lookback", 1, &children);
        for pid in 2..=(MAX_PROCESSES as u32 + 1) {
            too_many.process(pid, "/app/lookback", 1, "");
        }
        assert!(collect_renderer_metrics(&too_many.root, 1).is_none());
    }

    #[test]
    fn proc_tree_returns_unknown_when_task_read_budget_is_exceeded() {
        let fixture = ProcFixture::new();
        fixture.process(1, "/app/lookback", 7, "");
        let task_dir = fixture.root.join("1/task");
        for tid in 1..=(MAX_TASK_CHILD_READS + 1) {
            let children = task_dir.join(tid.to_string()).join("children");
            fs::create_dir_all(children.parent().expect("task parent"))
                .expect("create task fixture");
            fs::write(children, "").expect("create children fixture");
        }

        assert!(collect_renderer_metrics(&fixture.root, 1).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_tree_returns_unknown_for_rss_parse_and_sum_overflow() {
        let malformed = ProcFixture::new();
        malformed.process(1, "/app/lookback", 7, "2");
        malformed.process_with_status(2, "/usr/lib/WebKitWebProcess", "VmRSS: unknown kB\n", "");
        assert!(collect_renderer_metrics(&malformed.root, 1).is_none());

        let overflow = ProcFixture::new();
        overflow.process(1, "/app/lookback", 7, "2 3");
        overflow.process(2, "/usr/lib/WebKitWebProcess", u64::MAX / 1024, "");
        overflow.process(3, "/usr/lib/WebKitWebProcess", u64::MAX / 1024, "");
        assert!(collect_renderer_metrics(&overflow.root, 1).is_none());
    }

    fn test_sample(value: u64) -> MemorySample {
        MemorySample {
            sampled_at: Instant::now(),
            app_rss_bytes: Some(value),
            webkit_web_process_count: Some(0),
            webkit_web_process_rss_bytes: None,
            frontend_age_ms: None,
            counters: None,
        }
    }

    struct ProcFixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
    }

    impl ProcFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary proc fixture");
            let root = directory.path().join("proc");
            fs::create_dir_all(&root).expect("create proc fixture root");
            Self {
                _directory: directory,
                root,
            }
        }

        fn process(&self, pid: u32, executable: &str, rss_kib: u64, children: &str) {
            self.process_with_status(
                pid,
                executable,
                &format!("Name:\ttest\nVmRSS:\t{rss_kib} kB\n"),
                children,
            );
        }

        fn process_with_status(&self, pid: u32, executable: &str, status: &str, children: &str) {
            let process_dir = self.root.join(pid.to_string());
            let task_dir = process_dir.join("task").join(pid.to_string());
            fs::create_dir_all(&task_dir).expect("create process task fixture");
            fs::write(process_dir.join("status"), status).expect("write status fixture");
            fs::write(task_dir.join("children"), children).expect("write children fixture");
            symlink(executable, process_dir.join("exe")).expect("write executable fixture");
        }
    }
}
