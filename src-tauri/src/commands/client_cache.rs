//! Reusable gRPC client cache keyed by the endpoint URL.

use std::future::Future;

use tokio::sync::Mutex;

use crate::error::AppResult;

/// Caches one client for the most recently requested URL. Polled commands use
/// it so each poll does not open a new HTTP/2 connection; a different URL
/// (e.g. after a sidecar restart re-selected ports) replaces the entry.
pub(crate) struct UrlKeyedClientCache<T> {
    entry: Mutex<Option<(String, T)>>,
}

impl<T: Clone> UrlKeyedClientCache<T> {
    pub(crate) fn new() -> Self {
        Self {
            entry: Mutex::new(None),
        }
    }

    /// The lock is held across `connect` so a burst of polls opens exactly
    /// one connection.
    pub(crate) async fn get_or_connect<F, Fut>(&self, url: &str, connect: F) -> AppResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let mut guard = self.entry.lock().await;
        if let Some((cached_url, client)) = guard.as_ref()
            && cached_url == url
        {
            return Ok(client.clone());
        }
        let client = connect().await?;
        *guard = Some((url.to_owned(), client.clone()));
        Ok(client)
    }

    pub(crate) async fn invalidate(&self) {
        *self.entry.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::AppError;

    async fn connect_counting(connects: &AtomicUsize) -> AppResult<usize> {
        Ok(connects.fetch_add(1, Ordering::SeqCst) + 1)
    }

    #[tokio::test]
    async fn reuses_the_client_for_the_same_url() {
        let cache = UrlKeyedClientCache::new();
        let connects = AtomicUsize::new(0);
        let first = cache
            .get_or_connect("http://a", || connect_counting(&connects))
            .await
            .unwrap();
        let second = cache
            .get_or_connect("http://a", || connect_counting(&connects))
            .await
            .unwrap();
        assert_eq!((first, second), (1, 1));
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconnects_when_the_url_changes_or_after_invalidation() {
        let cache = UrlKeyedClientCache::new();
        let connects = AtomicUsize::new(0);
        cache
            .get_or_connect("http://a", || connect_counting(&connects))
            .await
            .unwrap();
        let other = cache
            .get_or_connect("http://b", || connect_counting(&connects))
            .await
            .unwrap();
        assert_eq!(other, 2);
        cache.invalidate().await;
        let fresh = cache
            .get_or_connect("http://b", || connect_counting(&connects))
            .await
            .unwrap();
        assert_eq!(fresh, 3);
    }

    #[tokio::test]
    async fn a_failed_connect_is_not_cached() {
        let cache = UrlKeyedClientCache::<usize>::new();
        let failed = cache
            .get_or_connect("http://a", || async {
                Err(AppError::Config("down".into()))
            })
            .await;
        assert!(failed.is_err());
        let connects = AtomicUsize::new(0);
        let recovered = cache
            .get_or_connect("http://a", || connect_counting(&connects))
            .await
            .unwrap();
        assert_eq!(recovered, 1);
    }
}
