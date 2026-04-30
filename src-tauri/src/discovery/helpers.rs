use std::net::IpAddr;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::rate_limiter::SharedRateLimiter;

pub fn row_id(index: usize) -> String {
    format!("tm_{index}")
}

pub fn ip_row(ip: IpAddr) -> Value {
    json!({ "ip": ip.to_string() })
}

pub fn probe_queue_cap(total: usize, cap: usize) -> usize {
    if cap == 0 {
        total
    } else {
        cap.min(total)
    }
}

pub async fn wait_rate_limiter(
    cancel: &CancellationToken,
    limiter: Option<SharedRateLimiter>,
) -> bool {
    match limiter {
        None => false,
        Some(l) => l.acquire(cancel).await,
    }
}

pub async fn sleep_with_cancel(cancel: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(d) => false,
    }
}
