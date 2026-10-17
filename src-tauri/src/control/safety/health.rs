//! Post-change health watch: reachability, temperature and hashrate.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::settings::{HealthSample, SettingsDriver};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthPolicy {
    /// Total watch time after a change.
    pub window: Duration,
    pub interval: Duration,
    /// Hashrate is only judged after this (ramp-up after a change).
    pub settle: Duration,
    /// Post-settle hashrate must be ≥ ratio × pre-change hashrate.
    pub min_hashrate_ratio: f64,
    /// Any sample above this is unhealthy immediately.
    pub max_temp_c: Option<f64>,
    /// Consecutive unreachable polls tolerated before giving up.
    pub max_unreachable_polls: u32,
}

impl Default for HealthPolicy {
    /// PLACEHOLDER thresholds pending sign-off.
    fn default() -> Self {
        Self {
            window: Duration::from_secs(180),
            interval: Duration::from_secs(5),
            settle: Duration::from_secs(90),
            min_hashrate_ratio: 0.8,
            max_temp_c: None,
            max_unreachable_polls: 6,
        }
    }
}

impl HealthPolicy {
    /// Tighten the temperature ceiling to the device's limit.
    pub fn with_temp_ceiling(mut self, limit: f64) -> Self {
        self.max_temp_c = Some(self.max_temp_c.map_or(limit, |t| t.min(limit)));
        self
    }
}

/// Immediate failure for one sample (temperature only; reachability is
/// judged as a streak by [`watch`]).
pub fn judge_temp(p: &HealthPolicy, s: &HealthSample) -> Option<String> {
    match (p.max_temp_c, s.temp_c) {
        (Some(max), Some(t)) if t > max => Some(format!("temperature {t:.1}°C above {max:.1}°C")),
        _ => None,
    }
}

/// Hashrate check against the pre-change baseline. A baseline of zero or
/// unknown (e.g. paused miner) cannot be judged and passes.
pub fn judge_hashrate(p: &HealthPolicy, baseline: &HealthSample, s: &HealthSample) -> Option<String> {
    let base = baseline.hashrate_ghs.filter(|b| *b > 0.0)?;
    let floor = base * p.min_hashrate_ratio;
    match s.hashrate_ghs {
        Some(h) if h >= floor => None,
        Some(h) => Some(format!("hashrate {h:.1} GH/s below {floor:.1} GH/s ({:.0}% of {base:.1})", p.min_hashrate_ratio * 100.0)),
        None => Some("hashrate not reported".into()),
    }
}

/// Polls for `window`. Ok(last sample) when healthy, Err(reason) otherwise.
pub async fn watch(d: &dyn SettingsDriver, p: &HealthPolicy, baseline: HealthSample) -> Result<HealthSample, String> {
    let start = tokio::time::Instant::now();
    let mut streak = 0u32;
    let mut last_settled: Option<HealthSample> = None;
    loop {
        tokio::time::sleep(p.interval).await;
        let s = d.health().await;
        if s.reachable {
            streak = 0;
            if let Some(r) = judge_temp(p, &s) {
                return Err(r);
            }
            if start.elapsed() >= p.settle {
                last_settled = Some(s);
            }
        } else {
            streak += 1;
            if streak > p.max_unreachable_polls {
                return Err(format!("unreachable for {streak} consecutive polls"));
            }
        }
        if start.elapsed() >= p.window {
            let Some(last) = last_settled else {
                return Err("no reachable sample after settle time".into());
            };
            if !s.reachable {
                return Err("unreachable at end of watch window".into());
            }
            return match judge_hashrate(p, &baseline, &last) {
                Some(r) => Err(r),
                None => Ok(last),
            };
        }
    }
}
