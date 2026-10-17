//! The guarded write sequence for settings changes:
//!
//! preflight → limits → open path (fallback chain) → [dry run stops here]
//! → audit "attempt" → snapshot → write → read-back → health watch
//! → (on any failure after the write) rollback to the snapshot → audit "outcome".

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::audit::{now_ms, AuditRecord, AuditSink};
use super::health::{self, HealthPolicy};
use super::limits::LimitsTable;
use super::paths::{FallbackChain, Target};
use super::policy::{preflight, ActionKind, Confirmation};
use super::settings::{Settings, SettingsDriver};
use super::snapshot::{Snapshot, SnapshotStore};

#[derive(Debug, Clone)]
pub struct SafetyOptions {
    /// Per-request I/O timeout (probe, read, write).
    pub op_timeout: Duration,
    /// How long read-back may retry (device may restart to apply).
    pub readback_timeout: Duration,
    pub health: HealthPolicy,
    /// How long rollback keeps retrying an unreachable device.
    pub rollback_timeout: Duration,
    /// Allow paths not yet confirmed on real hardware (mock tests only).
    pub allow_unconfirmed_paths: bool,
}

impl Default for SafetyOptions {
    fn default() -> Self {
        Self {
            op_timeout: Duration::from_secs(10),
            readback_timeout: Duration::from_secs(60),
            health: HealthPolicy::default(),
            rollback_timeout: Duration::from_secs(300),
            allow_unconfirmed_paths: false,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeWriteRequest {
    pub change: Settings,
    /// New write classes are dry run unless explicitly turned off.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default)]
    pub confirm: Confirmation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteStatus {
    DryRun,
    Refused,
    NoPath,
    Failed,
    Verified,
    RolledBack,
    RollbackFailed,
}

impl WriteStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Refused => "refused",
            Self::NoPath => "no_path",
            Self::Failed => "failed",
            Self::Verified => "verified",
            Self::RolledBack => "rolled_back",
            Self::RollbackFailed => "rollback_failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteOutcome {
    pub ip: String,
    pub status: WriteStatus,
    pub msg: Option<String>,
    pub snapshot_id: Option<String>,
    pub paths_tried: Vec<String>,
}

pub struct SafetyEngine {
    pub limits: Arc<LimitsTable>,
    pub chain: FallbackChain,
    pub snapshots: Arc<SnapshotStore>,
    pub audit: Arc<dyn AuditSink>,
    pub opts: SafetyOptions,
}

struct Ctx<'a> {
    run_id: &'a str,
    t: &'a Target,
    req: &'a SafeWriteRequest,
    paths: Vec<String>,
    snapshot_id: Option<String>,
}

impl SafetyEngine {
    pub fn new(audit: Arc<dyn AuditSink>, snapshots: Arc<SnapshotStore>, opts: SafetyOptions) -> Self {
        Self { limits: Arc::new(LimitsTable::builtin()), chain: FallbackChain::builtin(), snapshots, audit, opts }
    }

    fn record(&self, c: &Ctx<'_>, phase: &str, outcome: &str, msg: Option<String>) -> std::io::Result<()> {
        self.audit.record(&AuditRecord {
            ts_ms: now_ms(),
            run_id: c.run_id.to_string(),
            ip: c.t.ip.to_string(),
            action: ActionKind::ApplySettings.name().into(),
            phase: phase.into(),
            dry_run: c.req.dry_run,
            irreversible: false,
            device_class: Some(c.t.profile.class),
            model: Some(c.t.profile.model.clone()),
            firmware: Some(c.t.profile.firmware.clone()),
            requested: serde_json::to_value(&c.req.change).ok(),
            snapshot_id: c.snapshot_id.clone(),
            pre_state: None,
            paths_tried: c.paths.clone(),
            outcome: outcome.into(),
            msg,
        })
    }

    fn finish(&self, c: Ctx<'_>, status: WriteStatus, msg: Option<String>) -> WriteOutcome {
        if let Err(e) = self.record(&c, "outcome", status.as_str(), msg.clone()) {
            eprintln!("audit: failed to record outcome for {}: {e}", c.t.ip);
        }
        WriteOutcome { ip: c.t.ip.to_string(), status, msg, snapshot_id: c.snapshot_id, paths_tried: c.paths }
    }

    pub async fn apply_settings(&self, run_id: &str, t: &Target, req: &SafeWriteRequest) -> WriteOutcome {
        let mut c = Ctx { run_id, t, req, paths: Vec::new(), snapshot_id: None };
        let to = self.opts.op_timeout;
        if let Err(r) = preflight(ActionKind::ApplySettings, 1, req.dry_run, &req.confirm) {
            return self.finish(c, WriteStatus::Refused, Some(r.to_string()));
        }
        if req.change.is_empty() {
            return self.finish(c, WriteStatus::Refused, Some("nothing to change".into()));
        }
        let entry = match self.limits.check(&t.profile, &req.change) {
            Ok(e) => e.clone(),
            Err(v) => return self.finish(c, WriteStatus::Refused, Some(v.join("; "))),
        };
        let (opened, attempts) = self.chain.open(t, &req.change, self.opts.allow_unconfirmed_paths, to).await;
        c.paths = attempts.iter().map(|a| a.to_string()).collect();
        let Some((_, drv)) = opened else {
            let msg = format!("no write path accepted {}; tried: {}", t.ip, c.paths.join("; "));
            return self.finish(c, WriteStatus::NoPath, Some(msg));
        };
        let drv = drv.as_ref();

        if req.dry_run {
            let msg = match timed(to, drv.read()).await {
                Ok(cur) => format!("would set {}", cur.describe_change(&req.change)),
                Err(e) => format!("would set {:?} (current unreadable: {e})", req.change),
            };
            return self.finish(c, WriteStatus::DryRun, Some(msg));
        }

        // The attempt must be on record before the device is touched.
        if let Err(e) = self.record(&c, "attempt", "pending", None) {
            return self.finish(c, WriteStatus::Refused, Some(format!("audit log unavailable, refusing write: {e}")));
        }
        let current = match timed(to, drv.read()).await {
            Ok(s) => s,
            Err(e) => return self.finish(c, WriteStatus::Failed, Some(format!("cannot read settings for snapshot: {e}"))),
        };
        let restore = current.restrict_to(&req.change);
        let missing = missing_fields(&req.change, &restore);
        if !missing.is_empty() {
            let msg = format!("cannot snapshot every field being changed ({}); refusing write", missing.join(", "));
            return self.finish(c, WriteStatus::Refused, Some(msg));
        }
        let snap = match self.snapshots.take(&t.ip.to_string(), &t.profile, restore) {
            Ok(s) => s,
            Err(e) => return self.finish(c, WriteStatus::Failed, Some(format!("snapshot store unavailable: {e}"))),
        };
        c.snapshot_id = Some(snap.id.clone());
        let baseline = drv.health().await;
        let policy = self.opts.health.clone().with_temp_ceiling(entry.max_temp_c);

        if let Err(e) = timed(to, drv.write(&req.change)).await {
            return self.rollback(c, drv, &snap, format!("write failed: {e}")).await;
        }
        if let Err(r) = self.read_back(drv, &req.change).await {
            return self.rollback(c, drv, &snap, format!("read-back failed: {r}")).await;
        }
        match health::watch(drv, &policy, baseline).await {
            Ok(s) => {
                let msg = format!(
                    "applied {}; healthy (hashrate {:?} GH/s, temp {:?}°C)",
                    snap.settings.describe_change(&req.change),
                    s.hashrate_ghs,
                    s.temp_c
                );
                self.finish(c, WriteStatus::Verified, Some(msg))
            }
            Err(r) => self.rollback(c, drv, &snap, format!("unhealthy after change: {r}")).await,
        }
    }

    async fn read_back(&self, drv: &dyn SettingsDriver, want: &Settings) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + self.opts.readback_timeout;
        loop {
            let last = match timed(self.opts.op_timeout, drv.read()).await {
                Ok(got) => {
                    let mm = got.mismatches(want);
                    if mm.is_empty() {
                        return Ok(());
                    }
                    format!("mismatch: {}", mm.join(", "))
                }
                Err(e) => e.to_string(),
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(last);
            }
            tokio::time::sleep(self.opts.health.interval).await;
        }
    }

    async fn rollback(&self, c: Ctx<'_>, drv: &dyn SettingsDriver, snap: &Snapshot, reason: String) -> WriteOutcome {
        let deadline = tokio::time::Instant::now() + self.opts.rollback_timeout;
        let to = self.opts.op_timeout;
        loop {
            let last = match timed(to, drv.write(&snap.settings)).await {
                Ok(()) => match timed(to, drv.read()).await {
                    Ok(got) if got.mismatches(&snap.settings).is_empty() => {
                        let h = drv.health().await;
                        let msg = format!(
                            "{reason}; rolled back to snapshot {} (reachable {}, hashrate {:?} GH/s, temp {:?}°C)",
                            snap.id, h.reachable, h.hashrate_ghs, h.temp_c
                        );
                        return self.finish(c, WriteStatus::RolledBack, Some(msg));
                    }
                    Ok(got) => format!("read-back mismatch: {}", got.mismatches(&snap.settings).join(", ")),
                    Err(e) => e.to_string(),
                },
                Err(e) => e.to_string(),
            };
            if tokio::time::Instant::now() >= deadline {
                let msg = format!("{reason}; ROLLBACK FAILED to snapshot {} ({:?}): {last}", snap.id, snap.settings);
                return self.finish(c, WriteStatus::RollbackFailed, Some(msg));
            }
            tokio::time::sleep(self.opts.health.interval).await;
        }
    }
}

/// Fields being changed that the snapshot could not capture.
fn missing_fields(change: &Settings, snap: &Settings) -> Vec<String> {
    let mut out = Vec::new();
    let mut f = |n: &str, want: bool, have: bool| {
        if want && !have {
            out.push(n.to_string());
        }
    };
    f("frequency_mhz", change.frequency_mhz.is_some(), snap.frequency_mhz.is_some());
    f("core_voltage_mv", change.core_voltage_mv.is_some(), snap.core_voltage_mv.is_some());
    f("fan_pct", change.fan_pct.is_some(), snap.fan_pct.is_some());
    f("auto_fan", change.auto_fan.is_some(), snap.auto_fan.is_some());
    f("power_limit_w", change.power_limit_w.is_some(), snap.power_limit_w.is_some());
    out
}

async fn timed<T>(d: Duration, f: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(d, f).await.map_err(|_| anyhow::anyhow!("timeout"))?
}
