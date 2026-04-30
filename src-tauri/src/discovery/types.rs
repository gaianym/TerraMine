use serde::{Deserialize, Serialize};

use super::limits::{
    MAX_ENRICH_CONCURRENCY, MAX_ENRICH_RATE_LIMIT_PER_SEC, MAX_PROBE_CONCURRENCY,
    MAX_PROBE_RATE_LIMIT_PER_SEC, MAX_QUEUE_CAP, MAX_RETRY_JITTER_MS, MAX_UI_COALESCE_MS,
    MIN_UI_COALESCE_MS,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySettings {
    pub probe_timeout_sec: f64,
    pub probe_concurrency: usize,
    pub probe_rate_limit_per_sec: u64,
    pub enrich_timeout_sec: f64,
    pub enrich_concurrency: usize,
    #[serde(default)]
    pub enrich_rate_limit_per_sec: u64,
    #[serde(default = "default_retry_jitter_ms")]
    pub retry_jitter_ms: u64,
    pub probe_queue_cap: usize,
    pub enrich_queue_cap: usize,
    pub ui_coalesce_ms: u64,
}

impl DiscoverySettings {
    pub fn validate(&self) -> Result<(), String> {
        validate_positive_finite("probe_timeout_sec", self.probe_timeout_sec)?;
        validate_positive_finite("enrich_timeout_sec", self.enrich_timeout_sec)?;
        validate_usize_range(
            "probe_concurrency",
            self.probe_concurrency,
            1,
            MAX_PROBE_CONCURRENCY,
        )?;
        validate_usize_range(
            "enrich_concurrency",
            self.enrich_concurrency,
            1,
            MAX_ENRICH_CONCURRENCY,
        )?;
        validate_u64_max(
            "probe_rate_limit_per_sec",
            self.probe_rate_limit_per_sec,
            MAX_PROBE_RATE_LIMIT_PER_SEC,
        )?;
        validate_u64_max(
            "enrich_rate_limit_per_sec",
            self.enrich_rate_limit_per_sec,
            MAX_ENRICH_RATE_LIMIT_PER_SEC,
        )?;
        validate_usize_range("probe_queue_cap", self.probe_queue_cap, 1, MAX_QUEUE_CAP)?;
        validate_usize_range(
            "enrich_queue_cap",
            self.enrich_queue_cap,
            1,
            MAX_QUEUE_CAP,
        )?;
        validate_u64_range(
            "ui_coalesce_ms",
            self.ui_coalesce_ms,
            MIN_UI_COALESCE_MS,
            MAX_UI_COALESCE_MS,
        )?;
        validate_u64_max("retry_jitter_ms", self.retry_jitter_ms, MAX_RETRY_JITTER_MS)?;
        Ok(())
    }
}

const MIN_TIMEOUT_SEC: f64 = 0.001;
const MAX_TIMEOUT_SEC: f64 = 3600.0;

fn validate_positive_finite(field: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || !(MIN_TIMEOUT_SEC..=MAX_TIMEOUT_SEC).contains(&value) {
        return Err(format!(
            "{field} must be in [{MIN_TIMEOUT_SEC}, {MAX_TIMEOUT_SEC}] seconds (got {value})"
        ));
    }
    Ok(())
}

fn validate_usize_range(field: &str, value: usize, min: usize, max: usize) -> Result<(), String> {
    if value < min || value > max {
        return Err(format!("{field} must be in [{min}, {max}] (got {value})"));
    }
    Ok(())
}

fn validate_u64_range(field: &str, value: u64, min: u64, max: u64) -> Result<(), String> {
    if value < min || value > max {
        return Err(format!("{field} must be in [{min}, {max}] (got {value})"));
    }
    Ok(())
}

fn validate_u64_max(field: &str, value: u64, max: u64) -> Result<(), String> {
    if value > max {
        return Err(format!("{field} must be <= {max} (got {value})"));
    }
    Ok(())
}

fn default_retry_jitter_ms() -> u64 {
    250
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryProgress {
    pub total_targets: usize,
    pub probed: usize,
    pub alive: usize,
    pub missed: usize,
    pub enriched: usize,
    pub partial: usize,
    pub cancelled: usize,
}

impl DiscoveryProgress {
    pub fn new(total_targets: usize) -> Self {
        Self {
            total_targets,
            probed: 0,
            alive: 0,
            missed: 0,
            enriched: 0,
            partial: 0,
            cancelled: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum RowStatus {
    Queued,
    Probing,
    Alive,
    Enriching,
    Enriched,
    Partial,
    Miss,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryErrorCode {
    Cancelled,
    InvalidInput,
    InternalError,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryErrorEvent {
    pub code: DiscoveryErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryRowEvent {
    pub id: String,
    pub status: RowStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<serde_json::Value>,
}

pub fn seconds_to_duration(seconds: f64) -> std::time::Duration {
    assert!(
        seconds.is_finite() && seconds >= 0.0,
        "seconds_to_duration: invalid {seconds}"
    );
    std::time::Duration::from_secs_f64(seconds)
}
