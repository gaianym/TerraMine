//! Pre-write snapshots. Kept in memory for rollback and, when a directory
//! is configured, appended to `snapshots.jsonl` so a crash mid-change still
//! leaves the previous values on disk.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::profile::DeviceProfile;
use super::settings::Settings;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub ip: String,
    pub taken_at_ms: u64,
    pub profile: DeviceProfile,
    /// Only the fields the change touches.
    pub settings: Settings,
}

pub struct SnapshotStore {
    inner: Mutex<Vec<Snapshot>>,
    cap: usize,
    file: Option<PathBuf>,
}

impl SnapshotStore {
    pub fn in_memory(cap: usize) -> Self {
        Self { inner: Mutex::new(Vec::new()), cap: cap.max(1), file: None }
    }

    pub fn with_file(cap: usize, path: PathBuf) -> Self {
        Self { file: Some(path), ..Self::in_memory(cap) }
    }

    pub fn take(&self, ip: &str, profile: &DeviceProfile, settings: Settings) -> std::io::Result<Snapshot> {
        let snap = Snapshot {
            id: uuid::Uuid::new_v4().to_string(),
            ip: ip.to_string(),
            taken_at_ms: super::audit::now_ms(),
            profile: profile.clone(),
            settings,
        };
        if let Some(p) = &self.file {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            writeln!(f, "{}", serde_json::to_string(&snap)?)?;
        }
        let mut v = self.inner.lock().unwrap();
        v.push(snap.clone());
        let over = v.len().saturating_sub(self.cap);
        v.drain(..over);
        Ok(snap)
    }

    pub fn get(&self, id: &str) -> Option<Snapshot> {
        self.inner.lock().unwrap().iter().find(|s| s.id == id).cloned()
    }

    pub fn latest_for(&self, ip: &str) -> Option<Snapshot> {
        self.inner.lock().unwrap().iter().rev().find(|s| s.ip == ip).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
