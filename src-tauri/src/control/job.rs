//! Action runner: bounded concurrency, staggered dispatch, per-miner
//! timeout and cancellation. UI-agnostic; events go to a sink callback.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::driver::{DriverFactory, MinerAction, MinerStatus, Route};

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
}

#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub interval: Duration,
    /// Total time allowed for the miner to reach the expected state.
    pub timeout: Duration,
}

pub type Sink = Arc<dyn Fn(ActionRowEvent) + Send + Sync>;

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
    let action = cfg.action;
    for ip in &ips {
        sink(ev(*ip, action, ActionStatus::Queued, None));
    }
    let concurrency = cfg.concurrency.max(1);
    let stagger = cfg.stagger;
    let start = tokio::time::Instant::now();

    let results = stream::iter(ips.into_iter().enumerate())
        .map(|(i, ip)| {
            let factory = factory.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            let cfg = cfg.clone();
            async move {
                // Stagger: miner i is not dispatched before start + i * stagger.
                let slot = start + stagger * (i as u32);
                let fin = tokio::select! {
                    _ = cancel.cancelled() => ev(ip, action, ActionStatus::Cancelled, None),
                    r = async {
                        tokio::time::sleep_until(slot).await;
                        one(ip, &cfg, factory.as_ref(), &sink).await
                    } => r,
                };
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

async fn one(ip: IpAddr, cfg: &JobConfig, factory: &dyn DriverFactory, sink: &Sink) -> ActionRowEvent {
    let action = cfg.action;
    let driver = match tokio::time::timeout(cfg.timeout, factory.connect(ip)).await {
        Ok(Ok(Some(d))) => d,
        Ok(Ok(None)) => return ev(ip, action, ActionStatus::NotFound, None),
        Ok(Err(e)) => return ev(ip, action, ActionStatus::Failed, Some(e.to_string())),
        Err(_) => return ev(ip, action, ActionStatus::NotFound, Some("connect timeout".into())),
    };
    // Capability gate: the per-action route decides whether (and how) the
    // action is sent.
    let Some(route) = driver.caps().route(action) else {
        return ev(ip, action, ActionStatus::Unsupported, None);
    };
    if cfg.dry_run {
        return ev(ip, action, ActionStatus::DryRun, None).via(route);
    }
    // Baseline uptime so a reboot can be proven by a lower value afterwards.
    let before = if cfg.verify.is_some() && action == MinerAction::Reboot {
        tokio::time::timeout(cfg.timeout, driver.status()).await.ok().and_then(|r| r.ok())
    } else {
        None
    };
    sink(ev(ip, action, ActionStatus::Sent, None).via(route));
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
    drop(driver);
    let Some(vcfg) = &cfg.verify else { return sent };
    if !matches!(sent.status, ActionStatus::Ok | ActionStatus::Rebooting) {
        return sent;
    }
    sink(ev(ip, action, ActionStatus::Verifying, None).via(route));
    verify(ip, action, before, vcfg, cfg.timeout, factory).await.via(route)
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
    }
}

#[allow(unused_assignments)]
async fn verify(
    ip: IpAddr,
    action: MinerAction,
    before: Option<MinerStatus>,
    v: &VerifyConfig,
    probe_timeout: Duration,
    factory: &dyn DriverFactory,
) -> ActionRowEvent {
    let deadline = tokio::time::Instant::now() + v.timeout;
    let mut last_err: Option<String> = None;
    loop {
        tokio::time::sleep(v.interval).await;
        let probe = async {
            let d = factory.connect(ip).await?.ok_or_else(|| anyhow::anyhow!("offline"))?;
            d.status().await
        };
        match tokio::time::timeout(probe_timeout, probe).await {
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
