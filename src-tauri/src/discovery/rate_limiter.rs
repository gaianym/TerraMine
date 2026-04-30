use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct RateLimiter {
    min_interval: Duration,
    next_slot: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(per_sec: u64) -> Self {
        let per_sec = per_sec.max(1);
        Self {
            min_interval: Duration::from_secs_f64(1.0 / per_sec as f64),
            next_slot: Mutex::new(Instant::now()),
        }
    }

    pub async fn acquire(&self, cancel: &CancellationToken) -> bool {
        let wait = {
            let mut slot = self.next_slot.lock().await;
            let now = Instant::now();
            let start = (*slot).max(now);
            let w = start.saturating_duration_since(now);
            *slot = start + self.min_interval;
            w
        };

        if wait.is_zero() {
            return false;
        }

        tokio::select! {
            _ = cancel.cancelled() => true,
            _ = tokio::time::sleep(wait) => false,
        }
    }
}

pub type SharedRateLimiter = Arc<RateLimiter>;
