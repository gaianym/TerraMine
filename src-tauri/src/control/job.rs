//! Action runner: bounded concurrency, staggered dispatch, per-miner
//! timeout and cancellation. UI-agnostic; events go to a sink callback.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::cache::{DeviceCache, Resolved, Resolver};
use super::driver::{DriverFactory, MinerAction, MinerDriver, MinerStatus, Route};
use super::safety::audit::{now_ms, AuditRecord, AuditSink, NullAudit};
use super::safety::{preflight, ActionKind, Confirmation, DeviceProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionStatus {
    Queued,
    Sent,
    Ok,
    Rebooting,
    Refused,
    Failed,
    Unsupported,
    NotFound,
    DryRun,
    Cancelled,
    Verifying,
    Verified,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRowEvent {
    pub ip: String,
    pub action: MinerAction,
    pub status: ActionStatus,
    pub msg: Option<String>,
    /// Route the action was sent on (set once the capability gate passed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
}

#[derive(Debug, Clone)]
pub struct JobConfig {
    pub action: MinerAction,
    pub concurrency: usize,
    /// Minimum delay between successive dispatches (power-swing safety).
    pub stagger: Duration,
    pub timeout: Duration,
    pub dry_run: bool,
    /// Re-poll after the command; None = fire-and-forget.
    pub verify: Option<VerifyConfig>,
    /// Send on routes not yet confirmed on real hardware. Never set by the
    /// app; mock tests only.
    pub allow_unconfirmed: bool,
}

impl JobConfig {
    /// Defaults for `action`: dry run follows the action class (on for
    /// every class newer than start/stop/reboot), verify on.
    pub fn new(action: MinerAction) -> Self {
        Self {
            action,
            concurrency: 4,
            stagger: Duration::from_millis(500),
            timeout: Duration::from_secs(10),
            dry_run: ActionKind::from(action).default_dry_run(),
            verify: Some(VerifyConfig { interval: Duration::from_secs(5), timeout: Duration::from_secs(180) }),
            allow_unconfirmed: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub interval: Duration,
    /// Total time allowed for the miner to reach the expected state.
    pub timeout: Duration,
}

pub type Sink = Arc<dyn Fn(ActionRowEvent) + Send + Sync>;

/// Safety-layer context for a run: audit trail, run id, device cache.
#[derive(Clone)]
pub struct SafetyCtx {
    pub run_id: String,
    pub audit: Arc<dyn AuditSink>,
    /// Drivers are reused from here; identification only on miss/mismatch.
    pub cache: Arc<DeviceCache>,
}

impl Default for SafetyCtx {
    fn default() -> Self {
        Self { run_id: "local".into(), audit: Arc::new(NullAudit), cache: Arc::new(DeviceCache::default()) }
    }
}

/// What the runner learned about one miner, for the audit record.
#[derive(Default)]
struct Trace {
    profile: Option<DeviceProfile>,
    pre_state: Option<MinerStatus>,
}

fn audit(ctx: &SafetyCtx, cfg: &JobConfig, e: &ActionRowEvent, t: &Trace, phase: &str) {
    let kind = ActionKind::from(cfg.action);
    let r = AuditRecord {
        ts_ms: now_ms(),
        run_id: ctx.run_id.clone(),
        ip: e.ip.clone(),
        action: kind.name().into(),
        phase: phase.into(),
        dry_run: cfg.dry_run,
        irreversible: kind.irreversible(),
        device_class: t.profile.as_ref().map(|p| p.class),
        model: t.profile.as_ref().map(|p| p.model.clone()),
        firmware: t.profile.as_ref().map(|p| p.firmware.clone()),
        requested: None,
        snapshot_id: None,
        pre_state: t.pre_state.and_then(|s| serde_json::to_value(s).ok()),
        paths_tried: Vec::new(),
        outcome: serde_json::to_value(e.status).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
        msg: e.msg.clone(),
    };
    // Best effort for phase-1 actions: an audit failure must not change
    // start/stop/reboot behaviour.
    if let Err(err) = ctx.audit.record(&r) {
        eprintln!("audit: failed to record {} for {}: {err}", kind.name(), e.ip);
    }
}

fn ev(ip: IpAddr, action: MinerAction, status: ActionStatus, msg: Option<String>) -> ActionRowEvent {
    ActionRowEvent { ip: ip.to_string(), action, status, msg, route: None }
}

impl ActionRowEvent {
    fn via(mut self, route: Route) -> Self {
        self.route = Some(route);
        self
    }
}

/// Runs `cfg.action` against every IP. Returns the final event per IP, in input order.
pub async fn run_action(
    ips: Vec<IpAddr>,
    cfg: JobConfig,
    factory: Arc<dyn DriverFactory>,
    cancel: CancellationToken,
    sink: Sink,
) -> Vec<ActionRowEvent> {
    run_action_guarded(ips, cfg, factory, cancel, sink, SafetyCtx::default()).await
}

/// [`run_action`] through the safety layer: pre-flight gate plus an audit
/// record per attempt and per outcome.
pub async fn run_action_guarded(
    ips: Vec<IpAddr>,
    cfg: JobConfig,
    factory: Arc<dyn DriverFactory>,
    cancel: CancellationToken,
    sink: Sink,
    ctx: SafetyCtx,
) -> Vec<ActionRowEvent> {
    let action = cfg.action;
    for ip in &ips {
        sink(ev(*ip, action, ActionStatus::Queued, None));
    }
    // Phase-1 actions are reversible, so this never refuses them today; it
    // is the single gate every action class passes through.
    if let Err(r) = preflight(ActionKind::from(action), ips.len(), cfg.dry_run, &Confirmation::default()) {
        return ips
            .iter()
            .map(|ip| {
                let e = ev(*ip, action, ActionStatus::Refused, Some(r.to_string()));
                audit(&ctx, &cfg, &e, &Trace::default(), "outcome");
                sink(e.clone());
                e
            })
            .collect();
    }
    let concurrency = cfg.concurrency.max(1);
    let stagger = cfg.stagger;
    let start = tokio::time::Instant::now();
    let resolver = Arc::new(Resolver::new(factory, ctx.cache.clone()));

    let results = stream::iter(ips.into_iter().enumerate())
        .map(|(i, ip)| {
            let resolver = resolver.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            let cfg = cfg.clone();
            let ctx = ctx.clone();
            async move {
                // Stagger: miner i is not dispatched before start + i * stagger.
                let slot = start + stagger * (i as u32);
                let mut trace = Trace::default();
                let fin = tokio::select! {
                    _ = cancel.cancelled() => ev(ip, action, ActionStatus::Cancelled, None),
                    r = async {
                        tokio::time::sleep_until(slot).await;
                        one(ip, &cfg, &resolver, &sink, &ctx, &mut trace).await
                    } => r,
                };
                audit(&ctx, &cfg, &fin, &trace, "outcome");
                sink(fin.clone());
                (i, fin)
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;

    let mut results = results;
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, e)| e).collect()
}

async fn one(
    ip: IpAddr,
    cfg: &JobConfig,
    resolver: &Resolver,
    sink: &Sink,
    ctx: &SafetyCtx,
    trace: &mut Trace,
) -> ActionRowEvent {
    let action = cfg.action;
    let (driver, obs) = match resolver.resolve(ip, cfg.timeout).await {
        Resolved::Ready { driver, obs } => (driver, obs),
        Resolved::Changed(msg) => return ev(ip, action, ActionStatus::Refused, Some(msg)),
        Resolved::NotFound(msg) => return ev(ip, action, ActionStatus::NotFound, msg),
        Resolved::Failed(e) => return ev(ip, action, ActionStatus::Failed, Some(e)),
    };
    let profile = driver.profile();
    trace.profile = Some(profile.clone());
    // Capability gate: the per-action route decides whether (and how) the
    // action is sent.
    let caps = driver.caps();
    let Some(route) = caps.route(action) else {
        let msg = format!("not supported on {:?}; paths tried: {}", profile.class, driver.paths_tried(action).join("; "));
        return ev(ip, action, ActionStatus::Unsupported, Some(msg));
    };
    let confirmed = caps.confirmed(action);
    if cfg.dry_run {
        let msg = (!confirmed).then(|| format!("would send via {}; route not yet confirmed on real hardware (live send refused)", route.label()));
        return ev(ip, action, ActionStatus::DryRun, msg).via(route);
    }
    if !confirmed && !cfg.allow_unconfirmed {
        let msg = format!(
            "{} via {} is not yet confirmed on real hardware; dry run only until a hardware check",
            action.name(),
            route.label()
        );
        return ev(ip, action, ActionStatus::Refused, Some(msg)).via(route);
    }
    // Baseline uptime so a reboot can be proven by a lower value afterwards
    // (reuses the identity read when it carried uptime).
    let before = if cfg.verify.is_some() && action == MinerAction::Reboot {
        match obs.and_then(|o| o.status).filter(|s| s.uptime_s.is_some()) {
            Some(s) => Some(s),
            None => tokio::time::timeout(cfg.timeout, driver.status()).await.ok().and_then(|r| r.ok()),
        }
    } else {
        None
    };
    trace.pre_state = before;
    let sent_ev = ev(ip, action, ActionStatus::Sent, None).via(route);
    audit(ctx, cfg, &sent_ev, trace, "attempt");
    sink(sent_ev);
    let reboot = action == MinerAction::Reboot;
    let sent = match tokio::time::timeout(cfg.timeout, driver.execute(action)).await {
        Ok(Ok(true)) if reboot => ev(ip, action, ActionStatus::Rebooting, None),
        Ok(Ok(true)) => ev(ip, action, ActionStatus::Ok, None),
        Ok(Ok(false)) => ev(ip, action, ActionStatus::Refused, None),
        // Reboot commonly kills the connection before replying.
        Ok(Err(e)) if reboot => ev(ip, action, ActionStatus::Rebooting, Some(format!("assumed rebooting: {e}"))),
        Ok(Err(e)) => ev(ip, action, ActionStatus::Failed, Some(e.to_string())),
        Err(_) if reboot => ev(ip, action, ActionStatus::Rebooting, Some("assumed rebooting: timeout".into())),
        Err(_) => ev(ip, action, ActionStatus::Failed, Some("timeout".into())),
    }
    .via(route);
    let Some(vcfg) = &cfg.verify else { return sent };
    if !matches!(sent.status, ActionStatus::Ok | ActionStatus::Rebooting) {
        return sent;
    }
    sink(ev(ip, action, ActionStatus::Verifying, None).via(route));
    // Same driver for every poll: no reconnect / re-identification.
    if let Some(want) = action.light_target() {
        return verify_light(ip, action, want, vcfg, cfg.timeout, driver.as_ref()).await.via(route);
    }
    verify(ip, action, before, vcfg, cfg.timeout, driver.as_ref()).await.via(route)
}

#[allow(unused_assignments)]
/// Poll the light state until it reads `want`. Firmware that cannot
/// report it ends as `unverified` with that reason.
async fn verify_light(
    ip: IpAddr,
    action: MinerAction,
    want: bool,
    v: &VerifyConfig,
    probe_timeout: Duration,
    driver: &dyn MinerDriver,
) -> ActionRowEvent {
    let deadline = tokio::time::Instant::now() + v.timeout;
    let mut last: Option<String> = None;
    loop {
        tokio::time::sleep(v.interval).await;
        match tokio::time::timeout(probe_timeout, driver.light_state()).await {
            Ok(Ok(Some(on))) if on == want => return ev(ip, action, ActionStatus::Verified, None),
            Ok(Ok(Some(on))) => last = Some(format!("light still reads {}", if on { "on" } else { "off" })),
            Ok(Ok(None)) => {
                return ev(ip, action, ActionStatus::Unverified, Some("firmware does not report the light state".into()))
            }
            Ok(Err(e)) => last = Some(e.to_string()),
            Err(_) => last = Some("light read timeout".into()),
        }
        if tokio::time::Instant::now() >= deadline {
            let msg = match last {
                Some(e) => format!("not confirmed before timeout: {e}"),
                None => "not confirmed before timeout".into(),
            };
            return ev(ip, action, ActionStatus::Unverified, Some(msg));
        }
    }
}

/// Outcome of comparing a fresh status against what the action should produce.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Done,
    Pending,
    Impossible(String),
}

pub fn check_status(action: MinerAction, before: Option<MinerStatus>, now: MinerStatus) -> Check {
    match action {
        MinerAction::Reboot => match (before.and_then(|b| b.uptime_s), now.uptime_s) {
            (Some(b), Some(n)) if n < b => Check::Done,
            (Some(_), Some(_)) => Check::Pending,
            (None, _) => Check::Impossible("no baseline uptime before reboot".into()),
            (_, None) => Check::Impossible("miner does not report uptime".into()),
        },
        MinerAction::Start | MinerAction::Stop => {
            let want_paused = action == MinerAction::Stop;
            match now.paused {
                Some(p) if p == want_paused => Check::Done,
                Some(_) => Check::Pending,
                None => Check::Impossible("miner does not report pause state".into()),
            }
        }
        // Verified through `light_state`, not the status read.
        MinerAction::LightOn | MinerAction::LightOff => Check::Impossible("light state is not part of status".into()),
    }
}

#[allow(unused_assignments)]
async fn verify(
    ip: IpAddr,
    action: MinerAction,
    before: Option<MinerStatus>,
    v: &VerifyConfig,
    probe_timeout: Duration,
    driver: &dyn MinerDriver,
) -> ActionRowEvent {
    let deadline = tokio::time::Instant::now() + v.timeout;
    let mut last_err: Option<String> = None;
    loop {
        tokio::time::sleep(v.interval).await;
        match tokio::time::timeout(probe_timeout, driver.status()).await {
            Ok(Ok(st)) => match check_status(action, before, st) {
                Check::Done => return ev(ip, action, ActionStatus::Verified, None),
                Check::Pending => last_err = None,
                Check::Impossible(m) => return ev(ip, action, ActionStatus::Unverified, Some(m)),
            },
            // Offline during reboot is expected; keep polling.
            Ok(Err(e)) => last_err = Some(e.to_string()),
            Err(_) => last_err = Some("status timeout".into()),
        }
        if tokio::time::Instant::now() >= deadline {
            let msg = match last_err {
                Some(e) => format!("not confirmed before timeout: {e}"),
                None => "not confirmed before timeout".into(),
            };
            return ev(ip, action, ActionStatus::Unverified, Some(msg));
        }
    }
}
