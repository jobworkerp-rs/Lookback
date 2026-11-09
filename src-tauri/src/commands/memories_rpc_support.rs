//! Prefer newer memories RPCs while staying compatible with older servers.
//!
//! A remote memories deployment can lag behind the bundled sidecar, so each
//! newer RPC is tried first and, only when the server reports the RPC as
//! unknown (see [`is_rpc_unsupported`]), the caller's legacy path runs
//! instead. Every fallback branch is removable once the
//! minimum supported memories server includes the corresponding RPC.

use std::collections::HashSet;
use std::future::Future;

use parking_lot::Mutex;
use tonic::Code;

use crate::error::{AppError, AppResult};

/// Newer memories RPCs that older servers may not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MemoriesRpc {
    CountThreadsByUserId,
    CountThreadGroups,
    FindThreadGroupSummaries,
}

/// Remembers which RPCs the current memories server rejected so polled
/// commands do not send a failing call on every tick.
pub(crate) struct UnsupportedMemoriesRpcs {
    entry: Mutex<Option<(String, HashSet<MemoriesRpc>)>>,
}

impl UnsupportedMemoriesRpcs {
    pub(crate) fn new() -> Self {
        Self {
            entry: Mutex::new(None),
        }
    }

    pub(crate) fn is_unsupported(&self, url: &str, rpc: MemoriesRpc) -> bool {
        self.entry
            .lock()
            .as_ref()
            .is_some_and(|(cached_url, rpcs)| cached_url == url && rpcs.contains(&rpc))
    }

    pub(crate) fn mark_unsupported(&self, url: &str, rpc: MemoriesRpc) {
        let mut guard = self.entry.lock();
        match guard.as_mut() {
            Some((cached_url, rpcs)) if cached_url == url => {
                rpcs.insert(rpc);
            }
            // Marks for a previous server say nothing about the new one.
            _ => *guard = Some((url.to_owned(), HashSet::from([rpc]))),
        }
    }

    /// Reconnects may land on an upgraded server, so support is re-probed.
    pub(crate) fn invalidate(&self) {
        *self.entry.lock() = None;
    }
}

/// True when the server does not know the RPC at all. Older tonic servers
/// answer an unknown service with `UNKNOWN` + "unknown service" rather than
/// `UNIMPLEMENTED`, so both shapes count as "not supported".
pub(crate) fn is_rpc_unsupported(status: &tonic::Status) -> bool {
    status.code() == Code::Unimplemented
        || (status.code() == Code::Unknown
            && status
                .message()
                .to_ascii_lowercase()
                .contains("unknown service"))
}

impl UnsupportedMemoriesRpcs {
    /// Runs `preferred` against the server at `url` unless it is known to
    /// lack `rpc`. Only an unsupported-RPC status triggers `fallback`; any
    /// other status is mapped with `map_status` and returned so real
    /// failures are never masked.
    pub(crate) async fn call<T, P, PF, F, FF>(
        &self,
        url: &str,
        rpc: MemoriesRpc,
        preferred: P,
        map_status: impl FnOnce(tonic::Status) -> AppError,
        fallback: F,
    ) -> AppResult<T>
    where
        P: FnOnce() -> PF,
        PF: Future<Output = Result<T, tonic::Status>>,
        F: FnOnce() -> FF,
        FF: Future<Output = AppResult<T>>,
    {
        if !self.is_unsupported(url, rpc) {
            match preferred().await {
                Ok(value) => return Ok(value),
                Err(status) if is_rpc_unsupported(&status) => {
                    tracing::info!(?rpc, url, "memories RPC unsupported; using fallback");
                    self.mark_unsupported(url, rpc);
                }
                Err(status) => return Err(map_status(status)),
            }
        }
        fallback().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::AppError;

    const URL: &str = "http://memories-a";

    async fn call(
        cache: &UnsupportedMemoriesRpcs,
        url: &str,
        preferred_calls: &AtomicUsize,
        fallback_calls: &AtomicUsize,
        preferred: Result<i64, tonic::Status>,
    ) -> AppResult<i64> {
        cache
            .call(
                url,
                MemoriesRpc::CountThreadGroups,
                || async {
                    preferred_calls.fetch_add(1, Ordering::SeqCst);
                    preferred
                },
                AppError::Grpc,
                || async {
                    fallback_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(-1)
                },
            )
            .await
    }

    #[tokio::test]
    async fn supported_rpc_result_is_used_without_fallback() {
        let cache = UnsupportedMemoriesRpcs::new();
        let (preferred, fallback) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let value = call(&cache, URL, &preferred, &fallback, Ok(7))
            .await
            .unwrap();
        assert_eq!(value, 7);
        assert_eq!(fallback.load(Ordering::SeqCst), 0);
        assert!(!cache.is_unsupported(URL, MemoriesRpc::CountThreadGroups));
    }

    #[tokio::test]
    async fn unimplemented_falls_back_and_skips_the_rpc_afterwards() {
        let cache = UnsupportedMemoriesRpcs::new();
        let (preferred, fallback) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let first = call(
            &cache,
            URL,
            &preferred,
            &fallback,
            Err(tonic::Status::unimplemented("CountThreadGroups")),
        )
        .await
        .unwrap();
        let second = call(&cache, URL, &preferred, &fallback, Ok(7))
            .await
            .unwrap();
        assert_eq!((first, second), (-1, -1));
        assert_eq!(preferred.load(Ordering::SeqCst), 1);
        assert_eq!(fallback.load(Ordering::SeqCst), 2);
        // Other RPCs on the same server are still attempted.
        assert!(!cache.is_unsupported(URL, MemoriesRpc::FindThreadGroupSummaries));
    }

    #[tokio::test]
    async fn unknown_service_falls_back_like_unimplemented() {
        let cache = UnsupportedMemoriesRpcs::new();
        let (preferred, fallback) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let value = call(
            &cache,
            URL,
            &preferred,
            &fallback,
            Err(tonic::Status::unknown(
                "Unknown service llm_memory.service.X",
            )),
        )
        .await
        .unwrap();
        assert_eq!(value, -1);
        assert!(cache.is_unsupported(URL, MemoriesRpc::CountThreadGroups));
    }

    #[tokio::test]
    async fn other_unknown_errors_propagate() {
        let cache = UnsupportedMemoriesRpcs::new();
        let (preferred, fallback) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let err = call(
            &cache,
            URL,
            &preferred,
            &fallback,
            Err(tonic::Status::unknown("handler panicked")),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Grpc(status) if status.code() == tonic::Code::Unknown));
        assert_eq!(fallback.load(Ordering::SeqCst), 0);
        assert!(!cache.is_unsupported(URL, MemoriesRpc::CountThreadGroups));
    }

    #[tokio::test]
    async fn other_errors_propagate_without_fallback_or_marking() {
        let cache = UnsupportedMemoriesRpcs::new();
        let (preferred, fallback) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let err = call(
            &cache,
            URL,
            &preferred,
            &fallback,
            Err(tonic::Status::unavailable("down")),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Grpc(status) if status.code() == tonic::Code::Unavailable));
        assert_eq!(fallback.load(Ordering::SeqCst), 0);
        assert!(!cache.is_unsupported(URL, MemoriesRpc::CountThreadGroups));
    }

    #[test]
    fn unsupported_marks_are_scoped_to_the_url_and_cleared_by_invalidate() {
        let cache = UnsupportedMemoriesRpcs::new();
        cache.mark_unsupported(URL, MemoriesRpc::CountThreadsByUserId);
        assert!(cache.is_unsupported(URL, MemoriesRpc::CountThreadsByUserId));
        // A different server (e.g. after switching remote/local) is retried.
        assert!(!cache.is_unsupported("http://memories-b", MemoriesRpc::CountThreadsByUserId));
        cache.mark_unsupported("http://memories-b", MemoriesRpc::CountThreadGroups);
        assert!(!cache.is_unsupported(URL, MemoriesRpc::CountThreadsByUserId));

        cache.invalidate();
        assert!(!cache.is_unsupported("http://memories-b", MemoriesRpc::CountThreadGroups));
    }
}
