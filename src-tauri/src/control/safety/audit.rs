//! Audit trail: one record per attempt (and per outcome) for every write,
//! including refusals and dry runs.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::profile::DeviceClass;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts_ms: u64,
    pub run_id: String,
    pub ip: String,
    /// `ActionKind::name()`.
    pub action: String,
    /// "attempt" before touching the device, "outcome" after.
    pub phase: String,
    pub dry_run: bool,
    pub irreversible: bool,
    #[serde(default)]
    pub device_class: Option<DeviceClass>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub requested: Option<serde_json::Value>,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub pre_state: Option<serde_json::Value>,
    #[serde(default)]
    pub paths_tried: Vec<String>,
    /// Final status (e.g. "verified", "rolled_back", "refused").
    pub outcome: String,
    #[serde(default)]
    pub msg: Option<String>,
}

pub trait AuditSink: Send + Sync {
    fn record(&self, r: &AuditRecord) -> std::io::Result<()>;
}

#[derive(Default)]
pub struct NullAudit;
impl AuditSink for NullAudit {
    fn record(&self, _: &AuditRecord) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryAudit(pub Mutex<Vec<AuditRecord>>);
impl MemoryAudit {
    pub fn records(&self) -> Vec<AuditRecord> {
        self.0.lock().unwrap().clone()
    }
}
impl AuditSink for MemoryAudit {
    fn record(&self, r: &AuditRecord) -> std::io::Result<()> {
        self.0.lock().unwrap().push(r.clone());
        Ok(())
    }
}

/// Append-only JSON Lines file (one record per line).
pub struct JsonlAudit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlAudit {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl AuditSink for JsonlAudit {
    fn record(&self, r: &AuditRecord) -> std::io::Result<()> {
        let _g = self.lock.lock().unwrap();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(r)?)
    }
}
