//! The guarded write sequence for settings changes:
//!
//! preflight → pick backend (asic-rs if it supports the change, else the
//! AxeOS override, else `no_path` listing every path tried) → limits →
//! [dry run stops here] → audit "attempt" → snapshot → write → read-back → health watch
//! → (on any failure after the write) rollback to the snapshot → audit
//! "outcome".
//!
//! Backends may map a request (`translate`, e.g. the AxeOS power
//! stand-in; the result is limit-checked too) and may need a restart to
//! apply it (`restart_to_apply`): the restart is part of the write, the
//! engine waits for it (uptime reset, temperature watched) before
//! read-back, and a rollback writes the snapshot and restarts again.
//! Raising a frequency needs `Confirmation::allow_frequency_raise`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::audit::{now_ms, AuditRecord, AuditSink};
use super::health::{self, FailureCause, HealthPolicy, Unhealthy};
use super::limits::LimitsTable;
use super::policy::{preflight, ActionKind, Confirmation, LOW_FAN_PCT};
use super::profile::DeviceProfile;
use super::settings::{HealthSample, Settings, SettingsDriver};
use super::snapshot::{Snapshot, SnapshotStore};
use crate::control::driver::MinerDriver;

#[derive(Debug, Clone)]
pub struct SafetyOptions {
    /// Per-request I/O timeout (read, write).
    pub op_timeout: Duration,
    /// How long read-back may retry (device may restart to apply).
    pub readback_timeout: Duration,
    pub health: HealthPolicy,
    /// How long rollback keeps retrying an unreachable device.
    pub rollback_timeout: Duration,
    /// Allow backends not yet confirmed on real hardware (mock tests only).
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
    /// Not attempted (e.g. job halted after a bad first wave).
    Skipped,
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
            Self::Skipped => "skipped",
        }
    }
    /// Counts against the canary threshold.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::RolledBack | Self::RollbackFailed | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteOutcome {
    pub ip: String,
    pub status: WriteStatus,
    pub msg: Option<String>,
    pub snapshot_id: Option<String>,
    pub paths_tried: Vec<String>,
    /// Grouping key for alerts (`RollbackFailed` when the restore failed).
    pub cause: Option<FailureCause>,
    /// What triggered the rollback when the rollback itself failed.
    pub trigger: Option<FailureCause>,
    /// Snapshot values (before the change).
    pub before: Option<Settings>,
    pub attempted: Option<Settings>,
    /// Values read at the end (after verify or rollback).
    pub after: Option<Settings>,
    /// Health sample that failed the watch.
    pub sample: Option<HealthSample>,
    /// Set by the batch runner when untouched miners degraded concurrently.
    pub likely_external: bool,
    /// Skip reason, e.g. "halted".
    pub skip_reason: Option<String>,
    #[serde(skip)]
    pub window: Option<(Instant, Instant)>,
}

impl WriteOutcome {
    pub fn new(ip: impl Into<String>, status: WriteStatus, msg: Option<String>) -> Self {
        Self {
            ip: ip.into(),
            status,
            msg,
            snapshot_id: None,
            paths_tried: Vec::new(),
            cause: None,
            trigger: None,
            before: None,
            attempted: None,
            after: None,
            sample: None,
            likely_external: false,
            skip_reason: None,
            window: None,
        }
    }
}

/// A resolved device: identity from the cache plus its driver.
#[derive(Clone)]
pub struct Target {
    pub ip: IpAddr,
    pub profile: DeviceProfile,
    pub driver: Arc<dyn MinerDriver>,
}

impl Target {
    pub fn new(ip: IpAddr, driver: Arc<dyn MinerDriver>) -> Self {
        Self { ip, profile: driver.profile(), driver }
    }
}

pub struct SafetyEngine {
    pub limits: Arc<LimitsTable>,
    pub snapshots: Arc<SnapshotStore>,
    pub audit: Arc<dyn AuditSink>,
    pub opts: SafetyOptions,
}

struct Ctx<'a> {
    run_id: &'a str,
    t: &'a Target,
    req: &'a SafeWriteRequest,
    out: WriteOutcome,
    started: Option<Instant>,
}

impl SafetyEngine {
    pub fn new(audit: Arc<dyn AuditSink>, snapshots: Arc<SnapshotStore>, opts: SafetyOptions) -> Self {
        Self { limits: Arc::new(LimitsTable::builtin()), snapshots, audit, opts }
    }

    pub(crate) fn record_outcome(&self, run_id: &str, profile: Option<&DeviceProfile>, req: &SafeWriteRequest, o: &WriteOutcome, phase: &str) -> std::io::Result<()> {
        self.audit.record(&AuditRecord {
            ts_ms: now_ms(),
            run_id: run_id.to_string(),
            ip: o.ip.clone(),
            action: ActionKind::ApplySettings.name().into(),
            phase: phase.into(),
            dry_run: req.dry_run,
            irreversible: false,
            device_class: profile.map(|p| p.class),
            model: profile.map(|p| p.model.clone()),
            firmware: profile.map(|p| p.firmware.clone()),
            requested: serde_json::to_value(&req.change).ok(),
            snapshot_id: o.snapshot_id.clone(),
            pre_state: o.before.as_ref().and_then(|b| serde_json::to_value(b).ok()),
            paths_tried: o.paths_tried.clone(),
            outcome: if phase == "attempt" { "pending".into() } else { o.status.as_str().into() },
            msg: o.msg.clone(),
        })
    }

    fn finish(&self, mut c: Ctx<'_>, status: WriteStatus, msg: Option<String>) -> WriteOutcome {
        c.out.status = status;
        c.out.msg = msg;
        c.out.window = c.started.map(|s| (s, Instant::now()));
        if let Err(e) = self.record_outcome(c.run_id, Some(&c.t.profile), c.req, &c.out, "outcome") {
            eprintln!("audit: failed to record outcome for {}: {e}", c.t.ip);
        }
        c.out
    }

    /// asic-rs backend if it can do the change, else the AxeOS override,
    /// else none — each skip is reported with its reason. A backend not yet
    /// confirmed on real hardware is only selected for a dry run (or when
    /// the options explicitly allow it: mock tests).
    pub fn select(&self, t: &Target, change: &Settings, dry_run: bool) -> (Option<Box<dyn SettingsDriver>>, Vec<String>) {
        let mut tried = Vec::new();
        let backends = t.driver.settings_backends();
        if backends.is_empty() {
            tried.push(format!("{}: no settings write path for this device", t.driver.kind().label()));
        }
        for b in backends {
            if let Err(reason) = b.accepts(&t.profile, change) {
                tried.push(format!("{}: skipped ({reason})", b.id()));
            } else if !b.hardware_confirmed() && !self.opts.allow_unconfirmed_paths && !dry_run {
                tried.push(format!("{}: skipped (not yet confirmed on real hardware)", b.id()));
            } else if !b.hardware_confirmed() && !self.opts.allow_unconfirmed_paths {
                tried.push(format!("{}: selected for dry run only (not yet confirmed on real hardware)", b.id()));
                return (Some(b), tried);
            } else {
                tried.push(format!("{}: selected", b.id()));
                return (Some(b), tried);
            }
        }
        (None, tried)
    }

    pub async fn apply_settings(&self, run_id: &str, t: &Target, req: &SafeWriteRequest) -> WriteOutcome {
        let mut c = Ctx { run_id, t, req, out: WriteOutcome::new(t.ip.to_string(), WriteStatus::Refused, None), started: None };
        c.out.attempted = Some(req.change.clone());
        let to = self.opts.op_timeout;
        if let Err(r) = preflight(ActionKind::ApplySettings, 1, req.dry_run, &req.confirm) {
            return self.finish(c, WriteStatus::Refused, Some(r.to_string()));
        }
        if req.change.is_empty() {
            return self.finish(c, WriteStatus::Refused, Some("nothing to change".into()));
        }
        // Path first: a device that cannot take the change at all reports
        // "not supported" with every path tried, whatever the values.
        let (backend, tried) = self.select(t, &req.change, req.dry_run);
        c.out.paths_tried = tried;
        let Some(drv) = backend else {
            let msg = format!("no write path accepted {}; tried: {}", t.ip, c.out.paths_tried.join("; "));
            return self.finish(c, WriteStatus::NoPath, Some(msg));
        };
        let drv = drv.as_ref();
        let entry = match self.limits.check(&t.profile, &req.change) {
            Ok(e) => e.clone(),
            Err(v) => return self.finish(c, WriteStatus::Refused, Some(v.join("; "))),
        };
        // Backend mapping (AxeOS power → frequency stand-in), limit-checked too.
        let (change, note) = match tokio::time::timeout(to, drv.translate(&req.change)).await {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => return self.finish(c, WriteStatus::Refused, Some(e)),
            Err(_) => return self.finish(c, WriteStatus::Refused, Some("timeout while mapping the request".into())),
        };
        if change != req.change {
            if let Err(v) = self.limits.check(&t.profile, &change) {
                let msg = format!("{}; {}", note.unwrap_or_default(), v.join("; "));
                return self.finish(c, WriteStatus::Refused, Some(msg));
            }
        }
        c.out.attempted = Some(change.clone());
        let note = note.map(|n| format!("{n}; ")).unwrap_or_default();

        if req.dry_run {
            let via = format!("via {}", drv.id());
            let unconfirmed = if drv.hardware_confirmed() || self.opts.allow_unconfirmed_paths {
                String::new()
            } else {
                "; path not yet confirmed on real hardware, a live write is refused".into()
            };
            let restart = if drv.restart_to_apply(&change).is_some() { " with a restart to apply" } else { "" };
            let msg = match timed(to, drv.read()).await {
                Ok(cur) => {
                    let hold: String = [raise_hold(&cur, &change, &req.confirm), fan_hold(&change, &req.confirm)]
                        .into_iter()
                        .flatten()
                        .map(|h| format!("; {h}"))
                        .collect();
                    format!("{note}would set {}{restart} {via}{hold}{unconfirmed}", cur.describe_change(&change))
                }
                Err(e) => format!("{note}would set {change:?}{restart} {via} (current unreadable: {e}){unconfirmed}"),
            };
            return self.finish(c, WriteStatus::DryRun, Some(msg));
        }

        // The attempt must be on record before the device is touched.
        if let Err(e) = self.record_outcome(run_id, Some(&t.profile), req, &c.out, "attempt") {
            return self.finish(c, WriteStatus::Refused, Some(format!("audit log unavailable, refusing write: {e}")));
        }
        let current = match timed(to, drv.read()).await {
            Ok(s) => s,
            Err(e) => return self.finish(c, WriteStatus::Failed, Some(format!("cannot read settings for snapshot: {e}"))),
        };
        if let Some(h) = raise_hold(&current, &change, &req.confirm).or_else(|| fan_hold(&change, &req.confirm)) {
            return self.finish(c, WriteStatus::Refused, Some(h));
        }
        // A fan change snapshots the whole fan state (mode, manual %, target).
        let restore = current.restrict_to(&change.with_fan_group());
        let mut missing = missing_fields(&change, &restore);
        if restore.auto_fan == Some(true) || (restore.auto_fan == Some(false) && restore.fan_pct.is_some()) {
            // The mode plus its own value restores the fan; the other mode's
            // value may simply not be reported.
            missing.retain(|f| f != "fan_pct" && f != "temp_target_c");
        }
        if !missing.is_empty() {
            let msg = format!("cannot snapshot every field being changed ({}); refusing write", missing.join(", "));
            return self.finish(c, WriteStatus::Refused, Some(msg));
        }
        let snap = match self.snapshots.take(&t.ip.to_string(), &t.profile, restore) {
            Ok(s) => s,
            Err(e) => return self.finish(c, WriteStatus::Failed, Some(format!("snapshot store unavailable: {e}"))),
        };
        c.out.snapshot_id = Some(snap.id.clone());
        c.out.before = Some(snap.settings.clone());
        let baseline = t.driver.health().await;
        let policy = self.opts.health.clone().with_temp_ceiling(entry.max_temp_c);
        c.started = Some(Instant::now());

        if let Err(e) = timed(to, drv.write(&change)).await {
            return self.rollback(c, drv, &snap, FailureCause::WriteFailed, format!("write failed: {e}"), None).await;
        }
        if let Err(u) = self.restart_and_wait(t, drv, &change, &policy).await {
            return self.rollback(c, drv, &snap, u.cause, u.reason, u.sample).await;
        }
        if let Err(r) = self.read_back(drv, &change).await {
            return self.rollback(c, drv, &snap, FailureCause::ReadBackMismatch, format!("read-back failed: {r}"), None).await;
        }
        match health::watch(t.driver.as_ref(), &policy, baseline).await {
            Ok(s) => {
                c.out.after = timed(to, drv.read()).await.ok().map(|a| a.restrict_to(&change));
                let msg = format!(
                    "{note}applied {}; healthy (hashrate {:?} GH/s, temp {:?}°C)",
                    snap.settings.describe_change(&change),
                    s.hashrate_ghs,
                    s.temp_c
                );
                self.finish(c, WriteStatus::Verified, Some(msg))
            }
            Err(u) => {
                let reason = format!("unhealthy after change: {}", u.reason);
                self.rollback(c, drv, &snap, u.cause, reason, u.sample).await
            }
        }
    }

    async fn applied(&self, drv: &dyn SettingsDriver, want: &Settings) -> bool {
        matches!(tokio::time::timeout(self.opts.op_timeout, drv.check_applied(want)).await, Ok(Ok(())))
    }

    /// The restart a backend needs to apply `change` (none for most), then
    /// wait for proof it happened: uptime reset (or seen offline, then
    /// back). Temperature is watched throughout; bounded by
    /// `readback_timeout`. A failed restart request is not fatal by itself
    /// (the device may drop the connection while restarting); only the
    /// missing evidence is.
    async fn restart_and_wait(&self, t: &Target, drv: &dyn SettingsDriver, change: &Settings, policy: &HealthPolicy) -> Result<(), Unhealthy> {
        let to = self.opts.op_timeout;
        let Some(restart) = drv.restart_to_apply(change) else { return Ok(()) };
        let up0 = timed(to, drv.uptime_s()).await.ok().flatten();
        let sent = timed(to, restart).await;
        let deadline = Instant::now() + self.opts.readback_timeout;
        let mut saw_down = false;
        loop {
            tokio::time::sleep(policy.interval).await;
            let h = t.driver.health().await;
            if h.reachable {
                if let Some(r) = health::judge_temp(policy, &h) {
                    return Err(Unhealthy { cause: FailureCause::Overheat, reason: format!("during restart: {r}"), sample: Some(h) });
                }
                let up = timed(to, drv.uptime_s()).await.ok().flatten();
                let restarted = match (up0, up) {
                    // Seen offline and back with no more uptime than before
                    // also counts (uptime has whole-second resolution).
                    (Some(a), Some(b)) => b < a || (saw_down && b <= a),
                    _ => saw_down,
                };
                if restarted {
                    return Ok(());
                }
            } else {
                saw_down = true;
            }
            if Instant::now() >= deadline {
                let (cause, reason) = if !h.reachable {
                    (FailureCause::Unreachable, "did not come back after the restart to apply".to_string())
                } else if let Err(e) = &sent {
                    (FailureCause::WriteFailed, format!("restart to apply not observed (restart request failed: {e})"))
                } else {
                    (FailureCause::WriteFailed, "restart to apply not observed (uptime did not reset)".to_string())
                };
                return Err(Unhealthy { cause, reason, sample: Some(h) });
            }
        }
    }

    async fn read_back(&self, drv: &dyn SettingsDriver, want: &Settings) -> Result<(), String> {
        let deadline = Instant::now() + self.opts.readback_timeout;
        loop {
            let last = match timed(self.opts.op_timeout, drv.read()).await {
                Ok(got) => {
                    let mm = got.mismatches(want);
                    if mm.is_empty() {
                        match tokio::time::timeout(self.opts.op_timeout, drv.check_applied(want)).await {
                            Ok(Ok(())) => return Ok(()),
                            Ok(Err(e)) => e,
                            Err(_) => "timeout checking the applied value".into(),
                        }
                    } else {
                        format!("mismatch: {}", mm.join(", "))
                    }
                }
                Err(e) => e.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(last);
            }
            tokio::time::sleep(self.opts.health.interval).await;
        }
    }

    async fn rollback(
        &self,
        mut c: Ctx<'_>,
        drv: &dyn SettingsDriver,
        snap: &Snapshot,
        cause: FailureCause,
        reason: String,
        sample: Option<HealthSample>,
    ) -> WriteOutcome {
        c.out.sample = sample;
        let deadline = Instant::now() + self.opts.rollback_timeout;
        let to = self.opts.op_timeout;
        loop {
            // The restored state decides, not the restart evidence: if the
            // snapshot values are stored and running, the rollback worked.
            let written = match timed(to, drv.write(&snap.settings)).await {
                Ok(()) => Ok(self.restart_and_wait(c.t, drv, &snap.settings, &self.opts.health).await.err().map(|u| u.reason)),
                Err(e) => Err(e.to_string()),
            };
            let last = match written {
                Ok(restart_err) => match timed(to, drv.read()).await {
                    Ok(got) if got.mismatches(&snap.settings).is_empty() && self.applied(drv, &snap.settings).await => {
                        c.out.after = Some(got.restrict_to(&snap.settings));
                        c.out.cause = Some(cause);
                        let h = c.t.driver.health().await;
                        let msg = format!(
                            "{reason}; rolled back to snapshot {} (reachable {}, hashrate {:?} GH/s, temp {:?}°C)",
                            snap.id, h.reachable, h.hashrate_ghs, h.temp_c
                        );
                        return self.finish(c, WriteStatus::RolledBack, Some(msg));
                    }
                    Ok(got) => {
                        c.out.after = Some(got.restrict_to(&snap.settings));
                        let mm = got.mismatches(&snap.settings);
                        let what = if mm.is_empty() { "snapshot stored but not running".to_string() } else { mm.join(", ") };
                        match restart_err {
                            Some(r) => format!("read-back mismatch: {what} ({r})"),
                            None => format!("read-back mismatch: {what}"),
                        }
                    }
                    Err(e) => e.to_string(),
                },
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                c.out.cause = Some(FailureCause::RollbackFailed);
                c.out.trigger = Some(cause);
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
    f("temp_target_c", change.temp_target_c.is_some(), snap.temp_target_c.is_some());
    f("temp_hot_c", change.temp_hot_c.is_some(), snap.temp_hot_c.is_some());
    f("temp_danger_c", change.temp_danger_c.is_some(), snap.temp_danger_c.is_some());
    f("auto_fan", change.auto_fan.is_some(), snap.auto_fan.is_some());
    f("power_limit_w", change.power_limit_w.is_some(), snap.power_limit_w.is_some());
    f("tuning_pct", change.tuning_pct.is_some(), snap.tuning_pct.is_some());
    out
}

/// A frequency increase over the current value is held unless the
/// operator explicitly allowed it. Unknown current frequency counts as a
/// possible raise.
pub fn raise_hold(current: &Settings, change: &Settings, c: &Confirmation) -> Option<String> {
    let want = change.frequency_mhz?;
    if c.allow_frequency_raise {
        return None;
    }
    match current.frequency_mhz {
        Some(cur) if want <= cur => None,
        Some(cur) => Some(format!("frequency raise {cur} → {want} MHz is held: needs an explicit OK (allow_frequency_raise)")),
        None => Some(format!("frequency {want} MHz held: current frequency unknown, needs an explicit OK (allow_frequency_raise)")),
    }
}

/// A manual fan speed below [`LOW_FAN_PCT`] is held unless forced.
pub fn fan_hold(change: &Settings, c: &Confirmation) -> Option<String> {
    let p = change.fan_pct?;
    if p >= LOW_FAN_PCT || change.auto_fan == Some(true) || c.force_low_fan {
        return None;
    }
    Some(format!("manual fan {p}% below {LOW_FAN_PCT}% is held: needs force_low_fan (overheat risk)"))
}

async fn timed<T>(d: Duration, f: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(d, f).await.map_err(|_| anyhow::anyhow!("timeout"))?
}
