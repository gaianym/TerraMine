pub const MAX_ACTIVE_RUNS: usize = 4;
pub const MAX_TARGETS_LEN_BYTES: usize = 256 * 1024;
pub const MAX_EXPANDED_IPS: usize = 65_536;

pub const MAX_PROBE_CONCURRENCY: usize = 512;
pub const MAX_ENRICH_CONCURRENCY: usize = 128;
pub const MAX_PROBE_RATE_LIMIT_PER_SEC: u64 = 50_000;
pub const MAX_ENRICH_RATE_LIMIT_PER_SEC: u64 = 20_000;
pub const MAX_QUEUE_CAP: usize = 50_000;
pub const MAX_RETRY_JITTER_MS: u64 = 60_000;
pub const MIN_UI_COALESCE_MS: u64 = 0;
pub const MAX_UI_COALESCE_MS: u64 = 2000;
