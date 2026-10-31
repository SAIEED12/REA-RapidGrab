use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Global token-bucket-ish rate limiter shared by every segment worker.
///
/// Enforces an aggregate bytes/second budget (IDM's speed limiter is also global).
pub struct Limiter {
    rate: u64,
    start: Instant,
    consumed: Mutex<u64>,
}

impl Limiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: bytes_per_sec.max(1),
            start: Instant::now(),
            consumed: Mutex::new(0),
        }
    }

    /// Account for `bytes` and sleep if we are ahead of the budget.
    pub async fn acquire(&self, bytes: u64) {
        let mut consumed = self.consumed.lock().await;
        *consumed += bytes;
        let target = Duration::from_secs_f64(*consumed as f64 / self.rate as f64);
        let elapsed = self.start.elapsed();
        if target > elapsed {
            tokio::time::sleep(target - elapsed).await;
        }
    }
}
