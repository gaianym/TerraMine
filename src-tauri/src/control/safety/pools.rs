//! Pool and timezone settings (plan PR F): types, secret redaction and the
//! pure helpers the backends share.
//!
//! - A pool change is one of: `set` (the whole list), `add` (append one
//!   pool) or `replace` (one pool by position). Each backend resolves the
//!   operation against the miner's current pools ([`apply_op`]) so a fleet
//!   run works per miner, and the snapshot holds the full previous list.
//! - Read-back compares the normalised URL and worker of every pool, in
//!   order; passwords are never compared (most firmware does not report
//!   them) and never leave the process: [`Secret`] serialises as
//!   `"<redacted>"` (audit log, snapshot file, UI events) and a redacted
//!   value coming back (e.g. a retry) is refused, not written.
//! - A pool write needs `Confirmation::allow_pool_change` for a live run:
//!   a wrong URL or worker sends the hashrate somewhere else while the
//!   miner looks healthy.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const REDACTED: &str = "<redacted>";

/// A pool password. `Unknown` = not reported by the firmware / not sent.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Secret {
    #[default]
    Unknown,
    /// A value that was redacted on the way out and came back.
    Redacted,
    Value(String),
}

impl Secret {
    pub fn value(&self) -> Option<&str> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("Unknown"),
            Self::Redacted | Self::Value(_) => f.write_str(REDACTED),
        }
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown => s.serialize_none(),
            _ => s.serialize_str(REDACTED),
        }
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(d)? {
            None => Self::Unknown,
            Some(v) if v == REDACTED => Self::Redacted,
            Some(v) => Self::Value(v),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSpec {
    /// `stratum+tcp://host:port` (scheme optional; normalised for comparison).
    pub url: String,
    pub user: String,
    #[serde(default, skip_serializing_if = "Secret::is_unknown")]
    pub password: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolGroupSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default = "one")]
    pub quota: u32,
    pub pools: Vec<PoolSpec>,
}

fn one() -> u32 {
    1
}

/// What to do to each miner's pool list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PoolOp {
    /// Replace the whole configuration.
    Set { groups: Vec<PoolGroupSpec> },
    /// Append one pool to the first group.
    Add { pool: PoolSpec },
    /// Replace the pool at `index` (0-based, across groups in order).
    Replace { index: usize, pool: PoolSpec },
}

/// `stratum+tcp://host:port` form of a URL (scheme added when missing,
/// host lower-cased), via asic-rs's own parser.
pub fn normalize_url(url: &str) -> String {
    asic_rs_core::data::pool::PoolURL::from(url.trim().to_string()).to_string().to_ascii_lowercase()
}

/// Every pool, in order, as (normalised URL, worker).
pub fn flatten(groups: &[PoolGroupSpec]) -> Vec<(String, String)> {
    groups.iter().flat_map(|g| g.pools.iter()).map(|p| (normalize_url(&p.url), p.user.trim().to_string())).collect()
}

/// Same pools in the same order (passwords and group names ignored).
pub fn same_pools(a: &[PoolGroupSpec], b: &[PoolGroupSpec]) -> bool {
    flatten(a) == flatten(b)
}

pub fn describe(groups: &[PoolGroupSpec]) -> String {
    let f = flatten(groups);
    if f.is_empty() {
        return "[]".into();
    }
    format!("[{}]", f.iter().map(|(u, w)| format!("{u} {w}")).collect::<Vec<_>>().join(", "))
}

/// Static checks on the pools a request names (no I/O).
pub fn validate_pool(p: &PoolSpec) -> Result<(), String> {
    let url = p.url.trim();
    if url.is_empty() {
        return Err("pool URL is empty".into());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("pool URL {url:?} contains whitespace"));
    }
    // asic-rs's parser fills in port 80 when none is given; a stratum
    // port must be explicit.
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let authority = rest.split('/').next().unwrap_or("");
    let explicit_port = authority.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
    let n = asic_rs_core::data::pool::PoolURL::from(url.to_string());
    if n.host.is_empty() || !explicit_port {
        return Err(format!("pool URL {url:?} needs a host and a port (stratum+tcp://host:port)"));
    }
    if p.user.trim().is_empty() {
        return Err(format!("pool {url}: worker / user is empty"));
    }
    if p.password == Secret::Redacted {
        return Err(format!("pool {url}: password was redacted; re-enter it (or leave it empty)"));
    }
    Ok(())
}

pub fn validate_op(op: &PoolOp) -> Result<(), String> {
    match op {
        PoolOp::Set { groups } => {
            if groups.iter().all(|g| g.pools.is_empty()) {
                return Err("a pool set needs at least one pool (an empty list would stop mining)".into());
            }
            groups.iter().flat_map(|g| g.pools.iter()).try_for_each(validate_pool)
        }
        PoolOp::Add { pool } | PoolOp::Replace { pool, .. } => validate_pool(pool),
    }
}

/// The full pool list after `op` on `current`, at most `max` pools.
pub fn apply_op(current: &[PoolGroupSpec], op: &PoolOp, max: usize) -> Result<Vec<PoolGroupSpec>, String> {
    validate_op(op)?;
    let out = match op {
        PoolOp::Set { groups } => groups.clone(),
        PoolOp::Add { pool } => {
            let mut g = current.to_vec();
            if g.is_empty() {
                g.push(PoolGroupSpec { name: "default".into(), quota: 1, pools: Vec::new() });
            }
            if flatten(&g).contains(&(normalize_url(&pool.url), pool.user.trim().to_string())) {
                return Err(format!("pool {} {} is already configured", pool.url, pool.user));
            }
            g[0].pools.push(pool.clone());
            g
        }
        PoolOp::Replace { index, pool } => {
            let mut g = current.to_vec();
            let mut i = *index;
            let slot = g.iter_mut().flat_map(|g| g.pools.iter_mut()).find(|_| {
                let hit = i == 0;
                i = i.wrapping_sub(1);
                hit
            });
            match slot {
                Some(s) => *s = pool.clone(),
                None => return Err(format!("no pool at position {} (miner has {})", index + 1, flatten(current).len())),
            }
            g
        }
    };
    let n = flatten(&out).len();
    if n > max {
        return Err(format!("{n} pools requested; this firmware takes at most {max}"));
    }
    Ok(out)
}

/// IANA timezone name check (asic-rs uses `chrono_tz`).
pub fn parse_timezone(tz: &str) -> Result<asic_rs_core::config::timezone::Tz, String> {
    tz.trim().parse().map_err(|_| format!("{tz:?} is not an IANA timezone (e.g. America/Chicago)"))
}
