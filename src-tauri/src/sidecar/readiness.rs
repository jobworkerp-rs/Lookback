//! Bounded gRPC readiness probes for local sidecars.

use std::future::Future;
use std::time::Duration;

use tokio::time::{Instant, sleep, timeout};

/// The terminal result of a bounded readiness retry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadinessRetryError<E> {
    Probe(E),
    TimedOut,
}

/// Retry an asynchronous readiness probe until it succeeds or the deadline
/// passes. The probe owns each attempt so callers can create a fresh HTTP/2
/// channel after a transport error instead of reusing a poisoned connection.
/// Each probe is itself bounded by the remaining global budget: a connected
/// but non-responsive HTTP/2 server must not turn a 30-second startup wait
/// into the handle's normal multi-hour request timeout.
pub(crate) async fn retry_until_ready<T, E, F, Fut, R>(
    within: Duration,
    retry_interval: Duration,
    mut probe: F,
    should_retry: R,
) -> Result<T, ReadinessRetryError<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    R: Fn(&E) -> bool,
{
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, probe()).await {
            Ok(Ok(ready)) => return Ok(ready),
            Ok(Err(error)) => {
                if !should_retry(&error) || Instant::now() >= deadline {
                    return Err(ReadinessRetryError::Probe(error));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                sleep(retry_interval.min(remaining)).await;
            }
            Err(_) => return Err(ReadinessRetryError::TimedOut),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{ReadinessRetryError, retry_until_ready};

    #[tokio::test]
    async fn recreates_the_probe_after_transient_failures() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_probe = attempts.clone();

        let ready = retry_until_ready(
            Duration::from_secs(1),
            Duration::ZERO,
            move || {
                let attempts = attempts_for_probe.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        Err("transient h2 error")
                    } else {
                        Ok("fresh connection")
                    }
                }
            },
            |_| true,
        )
        .await;

        assert_eq!(ready, Ok("fresh connection"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn returns_the_last_error_at_the_deadline() {
        let result = retry_until_ready(
            Duration::ZERO,
            Duration::ZERO,
            || async { Err::<(), _>("h2 protocol error") },
            |_| true,
        )
        .await;

        assert_eq!(result, Err(ReadinessRetryError::Probe("h2 protocol error")));
    }

    #[tokio::test]
    async fn times_out_a_hung_probe_at_the_global_deadline() {
        let result = retry_until_ready(
            Duration::ZERO,
            Duration::ZERO,
            || async { std::future::pending::<Result<(), &str>>().await },
            |_| true,
        )
        .await;

        assert_eq!(result, Err(ReadinessRetryError::TimedOut));
    }

    #[tokio::test]
    async fn does_not_retry_a_permanent_error() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_probe = attempts.clone();

        let result = retry_until_ready(
            Duration::from_secs(1),
            Duration::ZERO,
            move || {
                let attempts = attempts_for_probe.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>("incompatible service")
                }
            },
            |_| false,
        )
        .await;

        assert_eq!(
            result,
            Err(ReadinessRetryError::Probe("incompatible service"))
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
