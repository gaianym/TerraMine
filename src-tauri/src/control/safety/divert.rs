//! Pool divert: the Stop / Start fallback for miners with no native sleep or
//! pause (e.g. a stock S9 without a miner-mode setting, AxeOS before 2.15).
//!
//! Divert = save the current pool list, point the miner at a dead local
//! pool so it stops doing work; restore = write the saved list back. Both
//! go through the engine's path selection (asic-rs first, unconfirmed paths
//! dry-run only), need the explicit pool-change OK, are dry run by default,
//! snapshot first, read back, roll back on a bad read-back, and are audited.
//! The saved list (with pool passwords) lives in the OS keychain, so a
//! diverted miner can still be restored after TerraMine restarts.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::audit::{now_ms, AuditRecord};
use super::engine::{SafetyEngine, Target, WriteStatus};
use super::policy::Confirmation;
use super::pools::{describe, normalize_url, same_pools, PoolGroupSpec, PoolSpec, Secret};
use super::settings::Settings;
use crate::control::driver::MinerAction;
use crate::control::ssh::store::SecretKv;

/// Nothing listens here: the miner keeps retrying and does no work.
pub const DIVERT_URL: &str = "stratum+tcp://127.0.0.1:1";
pub const DIVERT_USER: &str = "terramine-divert";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivertOp {
    /// Stop fallback: save pools, switch to the dead pool.
    Divert,
    /// Start fallback: write the saved pools back.
    Restore,
}

impl DivertOp {
    pub fn name(self) -> &'static str {
        match self {
            Self::Divert => "pool_divert",
            Self::Restore => "pool_restore",
        }
    }
    fn native(self) -> MinerAction {
        match self {
            Self::Divert => MinerAction::Stop,
            Self::Restore => MinerAction::Start,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DivertRequest {
    pub op: DivertOp,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default)]
    pub confirm: Confirmation,
}

#[derive(Debug, Clone, Serialize)]
pub struct DivertOutcome {
    pub ip: String,
    pub op: DivertOp,
    pub status: WriteStatus,
    pub msg: Option<String>,
    pub paths_tried: Vec<String>,
    /// Pools before (passwords redacted when serialised).
    pub before: Option<Vec<PoolGroupSpec>>,
    pub after: Option<Vec<PoolGroupSpec>>,
    pub snapshot_id: Option<String>,
}

pub fn divert_groups() -> Vec<PoolGroupSpec> {
    vec![PoolGroupSpec {
        name: "terramine-divert".into(),
        quota: 1,
        pools: vec![PoolSpec {
            url: DIVERT_URL.into(),
            user: DIVERT_USER.into(),
            password: Secret::Value("x".into()),
        }],
    }]
}

/// On the dead pool (and nothing else)?
pub fn is_diverted(groups: &[PoolGroupSpec]) -> bool {
    let all: Vec<&PoolSpec> = groups.iter().flat_map(|g| &g.pools).collect();
    !all.is_empty()
        && all
            .iter()
            .all(|p| normalize_url(&p.url) == normalize_url(DIVERT_URL))
}

// ---- saved pools, raw (passwords included), keychain only ----

#[derive(Serialize, Deserialize)]
struct RawPool {
    url: String,
    user: String,
    password: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RawGroup {
    name: String,
    quota: u32,
    pools: Vec<RawPool>,
}

fn kv_name(ip: &str) -> String {
    format!("pool-divert:{ip}")
}

fn save(kv: &dyn SecretKv, ip: &str, groups: &[PoolGroupSpec]) -> anyhow::Result<()> {
    let mut raw = Vec::new();
    for g in groups {
        let mut pools = Vec::new();
        for p in &g.pools {
            let password = match &p.password {
                Secret::Value(v) => Some(v.clone()),
                Secret::Unknown => None,
                Secret::Redacted => anyhow::bail!(
                    "pool password for {} came back redacted; it could not be restored",
                    p.url
                ),
            };
            pools.push(RawPool {
                url: p.url.clone(),
                user: p.user.clone(),
                password,
            });
        }
        raw.push(RawGroup {
            name: g.name.clone(),
            quota: g.quota,
            pools,
        });
    }
    kv.set_raw(&kv_name(ip), &serde_json::to_string(&raw)?)
}

/// The pools saved by a divert of `ip`, if any.
pub fn saved(kv: &dyn SecretKv, ip: &str) -> anyhow::Result<Option<Vec<PoolGroupSpec>>> {
    let Some(s) = kv.get_raw(&kv_name(ip))? else {
        return Ok(None);
    };
    let raw: Vec<RawGroup> = serde_json::from_str(&s)?;
    Ok(Some(
        raw.into_iter()
            .map(|g| PoolGroupSpec {
                name: g.name,
                quota: g.quota,
                pools: g
                    .pools
                    .into_iter()
                    .map(|p| PoolSpec {
                        url: p.url,
                        user: p.user,
                        password: p.password.map_or(Secret::Unknown, Secret::Value),
                    })
                    .collect(),
            })
            .collect(),
    ))
}

async fn timed<T>(
    d: Duration,
    f: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(d, f)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
}

struct Run<'a> {
    engine: &'a SafetyEngine,
    run_id: &'a str,
    t: &'a Target,
    req: &'a DivertRequest,
    out: DivertOutcome,
}

impl Run<'_> {
    fn audit(&self, phase: &str) -> std::io::Result<()> {
        self.engine.audit.record(&AuditRecord {
            ts_ms: now_ms(),
            run_id: self.run_id.into(),
            ip: self.out.ip.clone(),
            action: self.req.op.name().into(),
            phase: phase.into(),
            dry_run: self.req.dry_run,
            irreversible: false,
            device_class: Some(self.t.profile.class),
            model: Some(self.t.profile.model.clone()),
            firmware: Some(self.t.profile.firmware.clone()),
            requested: serde_json::to_value(self.req.op).ok(),
            snapshot_id: self.out.snapshot_id.clone(),
            pre_state: self
                .out
                .before
                .as_ref()
                .and_then(|b| serde_json::to_value(b).ok()),
            paths_tried: self.out.paths_tried.clone(),
            outcome: if phase == "attempt" {
                "pending".into()
            } else {
                self.out.status.as_str().into()
            },
            msg: self.out.msg.clone(),
        })
    }
    fn finish(mut self, status: WriteStatus, msg: impl Into<String>) -> DivertOutcome {
        self.out.status = status;
        self.out.msg = Some(msg.into());
        if let Err(e) = self.audit("outcome") {
            eprintln!(
                "audit: failed to record {} outcome for {}: {e}",
                self.req.op.name(),
                self.out.ip
            );
        }
        self.out
    }
}

/// Divert or restore one miner.
pub async fn run_divert(
    engine: &SafetyEngine,
    kv: &dyn SecretKv,
    run_id: &str,
    t: &Target,
    req: &DivertRequest,
) -> DivertOutcome {
    let ip = t.ip.to_string();
    let out = DivertOutcome {
        ip: ip.clone(),
        op: req.op,
        status: WriteStatus::Refused,
        msg: None,
        paths_tried: Vec::new(),
        before: None,
        after: None,
        snapshot_id: None,
    };
    let mut r = Run {
        engine,
        run_id,
        t,
        req,
        out,
    };
    let to = engine.opts.op_timeout;

    // Native sleep / wake first: divert is only the fallback.
    let native = req.op.native();
    let caps = t.driver.caps();
    if let Some(route) = caps.route(native) {
        r.out.paths_tried.push(format!(
            "native {}: supported ({route:?}); divert not needed",
            native.name()
        ));
        return r.finish(
            WriteStatus::Refused,
            format!(
                "this miner has a native {} route; use {} instead of a pool divert",
                native.name(),
                native.name()
            ),
        );
    }
    r.out
        .paths_tried
        .push(format!("native {}: not supported", native.name()));
    r.out.paths_tried.extend(t.driver.paths_tried(native));

    if !req.dry_run && !req.confirm.allow_pool_change {
        return r.finish(WriteStatus::Refused, "a pool divert changes the pool list and needs the explicit pool-change OK (allow_pool_change)");
    }
    let stored = match saved(kv, &ip) {
        Ok(s) => s,
        Err(e) => {
            return r.finish(
                WriteStatus::Failed,
                format!("keychain: cannot read saved pools: {e}"),
            )
        }
    };
    let target = match (req.op, &stored) {
        (DivertOp::Divert, Some(_)) => {
            return r.finish(
                WriteStatus::Refused,
                "already diverted (saved pools on record); restore it first",
            )
        }
        (DivertOp::Divert, None) => divert_groups(),
        (DivertOp::Restore, Some(s)) => s.clone(),
        (DivertOp::Restore, None) => {
            return r.finish(
                WriteStatus::Refused,
                "no diverted pools on record for this miner; nothing to restore",
            )
        }
    };
    let change = Settings {
        pools: Some(target.clone()),
        ..Default::default()
    };
    if let Err(v) = engine.limits.check_pools(&t.profile, &change) {
        r.out
            .paths_tried
            .push(format!("pool write: {}", v.join("; ")));
        let msg = format!(
            "no pool write path; tried: {}",
            r.out.paths_tried.join("; ")
        );
        return r.finish(WriteStatus::NoPath, msg);
    }
    let (backend, tried) = engine.select(t, &change, req.dry_run);
    r.out.paths_tried.extend(tried);
    let Some(drv) = backend else {
        let msg = format!(
            "no pool write path; tried: {}",
            r.out.paths_tried.join("; ")
        );
        return r.finish(WriteStatus::NoPath, msg);
    };
    let drv = drv.as_ref();

    let current = match timed(to, drv.read()).await {
        Ok(s) => s.pools.unwrap_or_default(),
        Err(e) => {
            return r.finish(
                WriteStatus::Failed,
                format!("cannot read current pools: {e}"),
            )
        }
    };
    r.out.before = Some(current.clone());
    match req.op {
        DivertOp::Divert if is_diverted(&current) => {
            return r.finish(WriteStatus::Refused, "already on the divert pool, but no saved pools are on record; set the pools by hand");
        }
        DivertOp::Divert
            if current
                .iter()
                .flat_map(|g| &g.pools)
                .any(|p| p.password == Secret::Redacted) =>
        {
            return r.finish(
                WriteStatus::Refused,
                "a pool password is unreadable; a divert could not restore it",
            );
        }
        DivertOp::Restore if !is_diverted(&current) => {
            return r.finish(WriteStatus::Refused, "the miner is no longer on the divert pool (its pools changed since); not overwriting them");
        }
        _ => {}
    }

    if req.dry_run {
        let unconfirmed = if drv.confirmed_for(&change) || engine.opts.allow_unconfirmed_paths {
            ""
        } else {
            "; path not yet confirmed on real hardware, a live write is refused"
        };
        let msg = format!(
            "would set pools {} -> {} via {}{unconfirmed}",
            describe(&current),
            describe(&target),
            drv.id()
        );
        r.out.after = Some(target);
        return r.finish(WriteStatus::DryRun, msg);
    }

    if let Err(e) = r.audit("attempt") {
        return r.finish(
            WriteStatus::Refused,
            format!("audit log unavailable, refusing write: {e}"),
        );
    }
    let snap = match engine.snapshots.take(
        &ip,
        &t.profile,
        Settings {
            pools: Some(current.clone()),
            ..Default::default()
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            return r.finish(
                WriteStatus::Refused,
                format!("cannot store snapshot, refusing write: {e}"),
            )
        }
    };
    r.out.snapshot_id = Some(snap.id.clone());
    // Save the original before touching the miner.
    if req.op == DivertOp::Divert {
        if let Err(e) = save(kv, &ip, &current) {
            return r.finish(
                WriteStatus::Refused,
                format!("cannot save the current pools to the keychain, refusing divert: {e}"),
            );
        }
    }

    let written = async {
        timed(to, drv.write(&change)).await?;
        if let Some(f) = drv.restart_to_apply(&change) {
            timed(to, f).await?;
        }
        anyhow::Ok(())
    }
    .await;
    let verified = match written {
        Err(e) => Err(format!("write failed: {e}")),
        Ok(()) => readback(drv, &target, engine.opts.readback_timeout, to).await,
    };
    match verified {
        Ok(after) => {
            r.out.after = Some(after);
            if req.op == DivertOp::Restore {
                if let Err(e) = kv.delete_raw(&kv_name(&ip)) {
                    return r.finish(WriteStatus::Verified, format!("pools restored and read back, but the saved copy could not be cleared: {e}"));
                }
            }
            let msg = match req.op {
                DivertOp::Divert => format!("diverted to {DIVERT_URL} (read back); original pools saved, restore with Start"),
                DivertOp::Restore => format!("restored {} (read back); mining resumes once the pool connects", describe(&target)),
            };
            r.finish(WriteStatus::Verified, msg)
        }
        Err(reason) => {
            // Roll back to the snapshot pools.
            let back = Settings {
                pools: Some(current.clone()),
                ..Default::default()
            };
            let rolled = async {
                timed(to, drv.write(&back)).await?;
                if let Some(f) = drv.restart_to_apply(&back) {
                    timed(to, f).await?;
                }
                anyhow::Ok(())
            }
            .await;
            let rb = match rolled {
                Ok(()) => {
                    readback(
                        drv,
                        &current,
                        engine
                            .opts
                            .rollback_timeout
                            .min(engine.opts.readback_timeout),
                        to,
                    )
                    .await
                }
                Err(e) => Err(e.to_string()),
            };
            match rb {
                Ok(after) => {
                    r.out.after = Some(after);
                    if req.op == DivertOp::Divert {
                        let _ = kv.delete_raw(&kv_name(&ip));
                    }
                    r.finish(
                        WriteStatus::RolledBack,
                        format!("{reason}; rolled back to snapshot {}", snap.id),
                    )
                }
                Err(e) => r.finish(
                    WriteStatus::RollbackFailed,
                    format!(
                        "{reason}; ROLLBACK FAILED ({e}); snapshot {} and the saved pools are kept",
                        snap.id
                    ),
                ),
            }
        }
    }
}

async fn readback(
    drv: &dyn super::settings::SettingsDriver,
    want: &[PoolGroupSpec],
    within: Duration,
    to: Duration,
) -> Result<Vec<PoolGroupSpec>, String> {
    let deadline = Instant::now() + within;
    let mut last;
    loop {
        match timed(to, drv.read()).await {
            Ok(s) => {
                let got = s.pools.unwrap_or_default();
                if same_pools(&got, want) {
                    return Ok(got);
                }
                last = format!("read back {} (expected {})", describe(&got), describe(want));
            }
            Err(e) => last = format!("read-back failed: {e}"),
        }
        if Instant::now() >= deadline {
            return Err(last);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
