//! Test helper proving that fake RPCs are in flight at the same time.

use std::time::Duration;

use tokio::sync::Barrier;

/// Every call waits until `expected` calls have arrived. A sequential
/// implementation never lets the first call through, so it times out
/// instead of passing by luck; no wall-clock delays are involved.
pub(crate) struct ConcurrencyGate {
    barrier: Barrier,
}

impl ConcurrencyGate {
    pub(crate) fn new(expected: usize) -> Self {
        Self {
            barrier: Barrier::new(expected),
        }
    }

    pub(crate) async fn call<T>(&self, value: T) -> T {
        tokio::time::timeout(Duration::from_secs(5), self.barrier.wait())
            .await
            .expect("calls were not in flight concurrently");
        value
    }
}
