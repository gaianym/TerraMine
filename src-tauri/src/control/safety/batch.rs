//! Batch settings jobs on top of the per-miner engine:
//!
//! - **Canary / stop-early**: the first wave runs alone; if its failure
//!   rate exceeds `halt_threshold` (default 20%) the rest of the job is
//!   reported as `skipped` with reason `halted` and never touched.
//! - **Network-or-pool check**: untouched miners (not in the job) are
//!   sampled during the job; failures whose watch window overlaps a time
//!   when at least half of those controls were degraded are marked
//!   `likely_external`.
//! - **One aggregated alert** per job: "N of M rolled back", grouped by
//!   cause, with per-miner before/after values and a retry request.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::engine::{SafeWriteRequest, SafetyEngine, Target, WriteOutcome, WriteStatus};
use super::health::FailureCause;
use super::settings::{HealthSample, Settings};
use crate::control::cache::{Resolved, Resolver};

#[derive(Debug, Clone)]
pub struct BatchOptions {
    pub concurrency: usize,
    pub stagger: Duration,
    /// First-wave size; `None` = `concurrency`.
    pub canary_size: Option<usize>,
    /// Halt when first-wave failures / first-wave size is above this.
    pub halt_threshold: f64,
    /// Untouched miners sampled for the network/pool check.
    pub controls: Vec<IpAddr>,
    pub control_interval: Duration,
    /// A control is degraded when unreachable or below ratio × baseline hashrate.
    pub control_hashrate_ratio: f64,
    pub resolve_timeout: Duration,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            stagger: Duration::from_millis(500),
            canary_size: None,
            halt_threshold: 0.2,
            controls: Vec::new(),
            control_interval: Duration::from_secs(5),
            control_hashrate_ratio: 0.8,
            resolve_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertMiner {
    pub ip: String,
    pub status: WriteStatus,
    pub before: Option<Settings>,
    pub attempted: Option<Settings>,
    pub after: Option<Settings>,
    pub sample: Option<HealthSample>,
    pub trigger: Option<FailureCause>,
    pub detail: Option<String>,
    pub likely_external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertGroup {
    pub cause: FailureCause,
    pub label: String,
    pub count: usize,
    pub miners: Vec<AlertMiner>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRequest {
    pub ips: Vec<String>,
    pub change: Settings,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobAlert {
    pub run_id: String,
    pub total: usize,
    pub rolled_back: usize,
    pub rollback_failed: usize,
    pub halted: usize,
    pub headline: String,
    pub likely_external: bool,
    pub external_note: Option<String>,
    pub groups: Vec<AlertGroup>,
    pub halted_ips: Vec<String>,
    pub retry: RetryRequest,
}

pub type OutcomeSink = Arc<dyn Fn(&WriteOutcome) + Send + Sync>;

struct ControlSample {
    at: Instant,
    degraded: usize,
    total: usize,
}

fn degraded(base: &HealthSample, s: &HealthSample, ratio: f64) -> bool {
    if !s.reachable {
        return true;
    }
    match (base.hashrate_ghs.filter(|b| *b > 0.0), s.hashrate_ghs) {
        (Some(b), Some(h)) => h < b * ratio,
        (Some(_), None) => true,
        _ => false,
    }
}

async fn sample_controls(
    resolver: Arc<Resolver>,
    controls: Vec<IpAddr>,
    opts: BatchOptions,
    stop: CancellationToken,
    out: Arc<Mutex<Vec<ControlSample>>>,
) {
    let mut drivers = Vec::new();
    for ip in controls {
        if let Resolved::Ready { driver, .. } = resolver.resolve(ip, opts.resolve_timeout).await {
            let base = driver.health().await;
            drivers.push((driver, base));
        }
    }
    if drivers.is_empty() {
        return;
    }
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(opts.control_interval) => {}
        }
        let mut n = 0;
        for (d, base) in &drivers {
            if degraded(base, &d.health().await, opts.control_hashrate_ratio) {
                n += 1;
            }
        }
        out.lock().unwrap().push(ControlSample { at: Instant::now(), degraded: n, total: drivers.len() });
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_settings_job(
    run_id: &str,
    ips: Vec<IpAddr>,
    req: SafeWriteRequest,
    resolver: Arc<Resolver>,
    engine: Arc<SafetyEngine>,
    opts: BatchOptions,
    cancel: CancellationToken,
    sink: OutcomeSink,
) -> (Vec<WriteOutcome>, Option<JobAlert>) {
    let start = Instant::now();
    let stop = CancellationToken::new();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let controls: Vec<IpAddr> = opts.controls.iter().copied().filter(|c| !ips.contains(c)).collect();
    let sampler = tokio::spawn(sample_controls(resolver.clone(), controls.clone(), opts.clone(), stop.clone(), samples.clone()));

    let concurrency = opts.concurrency.max(1);
    let canary = opts.canary_size.unwrap_or(concurrency).clamp(1, ips.len().max(1));
    let indexed: Vec<(usize, IpAddr)> = ips.iter().copied().enumerate().collect();
    let (first, rest) = indexed.split_at(canary.min(indexed.len()));

    let run_wave = |wave: Vec<(usize, IpAddr)>| {
        let resolver = resolver.clone();
        let engine = engine.clone();
        let req = req.clone();
        let cancel = cancel.clone();
        let sink = sink.clone();
        let stagger = opts.stagger;
        let to = opts.resolve_timeout;
        let run_id = run_id.to_string();
        async move {
            stream::iter(wave)
                .map(|(i, ip)| {
                    let (resolver, engine, req, cancel, sink, run_id) =
                        (resolver.clone(), engine.clone(), req.clone(), cancel.clone(), sink.clone(), run_id.clone());
                    async move {
                        let slot = start + stagger * (i as u32);
                        let o = tokio::select! {
                            _ = cancel.cancelled() => skipped(&engine, &run_id, &req, ip, "cancelled", "cancelled".into()),
                            o = async {
                                tokio::time::sleep_until(slot).await;
                                one(&run_id, ip, &req, &resolver, &engine, to).await
                            } => o,
                        };
                        sink(&o);
                        (i, o)
                    }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await
        }
    };

    let mut results = run_wave(first.to_vec()).await;
    let fails = results.iter().filter(|(_, o)| o.status.is_failure()).count();
    let rate = fails as f64 / first.len().max(1) as f64;
    if !rest.is_empty() {
        if rate > opts.halt_threshold {
            let msg = format!(
                "halted: {fails} of {} failed in the first wave ({:.0}% > {:.0}%)",
                first.len(),
                rate * 100.0,
                opts.halt_threshold * 100.0
            );
            for (i, ip) in rest {
                let o = skipped(&engine, run_id, &req, *ip, "halted", msg.clone());
                sink(&o);
                results.push((*i, o));
            }
        } else {
            results.extend(run_wave(rest.to_vec()).await);
        }
    }
    stop.cancel();
    let _ = sampler.await;
    results.sort_by_key(|(i, _)| *i);
    let mut outcomes: Vec<WriteOutcome> = results.into_iter().map(|(_, o)| o).collect();

    let samples = samples.lock().unwrap();
    for o in outcomes.iter_mut().filter(|o| o.status.is_failure()) {
        if let Some((a, b)) = o.window {
            o.likely_external = samples.iter().any(|s| s.at >= a && s.at <= b && s.degraded > 0 && s.degraded * 2 >= s.total);
        }
    }
    let alert = build_alert(run_id, &outcomes, &req, &controls);
    (outcomes, alert)
}

fn skipped(engine: &SafetyEngine, run_id: &str, req: &SafeWriteRequest, ip: IpAddr, reason: &str, msg: String) -> WriteOutcome {
    let mut o = WriteOutcome::new(ip.to_string(), WriteStatus::Skipped, Some(msg));
    o.skip_reason = Some(reason.into());
    o.attempted = Some(req.change.clone());
    let _ = engine.record_outcome(run_id, None, req, &o, "outcome");
    o
}

async fn one(run_id: &str, ip: IpAddr, req: &SafeWriteRequest, resolver: &Resolver, engine: &SafetyEngine, to: Duration) -> WriteOutcome {
    let fail = |status, msg: String| {
        let o = WriteOutcome::new(ip.to_string(), status, Some(msg));
        let _ = engine.record_outcome(run_id, None, req, &o, "outcome");
        o
    };
    match resolver.resolve(ip, to).await {
        Resolved::Ready { driver, .. } => engine.apply_settings(run_id, &Target::new(ip, driver), req).await,
        Resolved::Changed(msg) => fail(WriteStatus::Refused, msg),
        Resolved::NotFound(msg) => fail(WriteStatus::Failed, msg.unwrap_or_else(|| "miner not found".into())),
        Resolved::Failed(e) => fail(WriteStatus::Failed, e),
    }
}

pub fn build_alert(run_id: &str, outcomes: &[WriteOutcome], req: &SafeWriteRequest, controls: &[IpAddr]) -> Option<JobAlert> {
    let rolled_back = outcomes.iter().filter(|o| o.status == WriteStatus::RolledBack).count();
    let rollback_failed = outcomes.iter().filter(|o| o.status == WriteStatus::RollbackFailed).count();
    let halted_ips: Vec<String> =
        outcomes.iter().filter(|o| o.skip_reason.as_deref() == Some("halted")).map(|o| o.ip.clone()).collect();
    if rolled_back + rollback_failed == 0 && halted_ips.is_empty() {
        return None;
    }
    let mut groups: BTreeMap<FailureCause, Vec<AlertMiner>> = BTreeMap::new();
    for o in outcomes {
        if let Some(cause) = o.cause.filter(|_| matches!(o.status, WriteStatus::RolledBack | WriteStatus::RollbackFailed)) {
            groups.entry(cause).or_default().push(AlertMiner {
                ip: o.ip.clone(),
                status: o.status,
                before: o.before.clone(),
                attempted: o.attempted.clone(),
                after: o.after.clone(),
                sample: o.sample,
                trigger: o.trigger,
                detail: o.msg.clone(),
                likely_external: o.likely_external,
            });
        }
    }
    let ext: Vec<&WriteOutcome> = outcomes.iter().filter(|o| o.likely_external).collect();
    let likely_external = !ext.is_empty();
    let total = outcomes.len();
    let mut headline = format!("{rolled_back} of {total} rolled back");
    if rollback_failed > 0 {
        headline.push_str(&format!(", {rollback_failed} rollback failed"));
    }
    if !halted_ips.is_empty() {
        headline.push_str(&format!(", {} halted", halted_ips.len()));
    }
    if likely_external {
        headline.push_str(" — likely external (network/pool)");
    }
    let external_note = likely_external.then(|| {
        let ctl: Vec<String> = controls.iter().map(|c| c.to_string()).collect();
        format!(
            "likely external (network/pool): {} of {} failures coincided with degradation on untouched miners ({})",
            ext.len(),
            rolled_back + rollback_failed,
            ctl.join(", ")
        )
    });
    let mut retry_ips: Vec<String> = groups.values().flatten().map(|m| m.ip.clone()).collect();
    retry_ips.extend(halted_ips.iter().cloned());
    Some(JobAlert {
        run_id: run_id.into(),
        total,
        rolled_back,
        rollback_failed,
        halted: halted_ips.len(),
        headline,
        likely_external,
        external_note,
        groups: groups
            .into_iter()
            .map(|(cause, miners)| AlertGroup { cause, label: cause.label().into(), count: miners.len(), miners })
            .collect(),
        halted_ips,
        // Retry is always offered as a dry run first; a live re-send needs
        // the user to turn dry run off explicitly.
        retry: RetryRequest { ips: retry_ips, change: req.change.clone(), dry_run: true },
    })
}
