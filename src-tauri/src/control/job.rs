//! Action runner: bounded concurrency, staggered dispatch, per-miner
//! timeout and cancellation. UI-agnostic; events go to a sink callback.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::driver::{DriverFactory, MinerAction};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRowEvent {
    pub ip: String,
    pub action: MinerAction,
    pub status: ActionStatus,
    pub msg: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobConfig {
    pub action: MinerAction,
    pub concurrency: usize,
    /// Minimum delay between successive dispatches (power-swing safety).
    pub stagger: Duration,
    pub timeout: Duration,
    pub dry_run: bool,
}

pub type Sink = Arc<dyn Fn(ActionRowEvent) + Send + Sync>;

fn ev(ip: IpAddr, action: MinerAction, status: ActionStatus, msg: Option<String>) -> ActionRowEvent {
    ActionRowEvent { ip: ip.to_string(), action, status, msg }
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
    if !driver.caps().supports(action) {
        return ev(ip, action, ActionStatus::Unsupported, None);
    }
    if cfg.dry_run {
        return ev(ip, action, ActionStatus::DryRun, None);
    }
    sink(ev(ip, action, ActionStatus::Sent, None));
    let reboot = action == MinerAction::Reboot;
    match tokio::time::timeout(cfg.timeout, driver.execute(action)).await {
        Ok(Ok(true)) if reboot => ev(ip, action, ActionStatus::Rebooting, None),
        Ok(Ok(true)) => ev(ip, action, ActionStatus::Ok, None),
        Ok(Ok(false)) => ev(ip, action, ActionStatus::Refused, None),
        // Reboot commonly kills the connection before replying.
        Ok(Err(e)) if reboot => ev(ip, action, ActionStatus::Rebooting, Some(format!("assumed rebooting: {e}"))),
        Ok(Err(e)) => ev(ip, action, ActionStatus::Failed, Some(e.to_string())),
        Err(_) if reboot => ev(ip, action, ActionStatus::Rebooting, Some("assumed rebooting: timeout".into())),
        Err(_) => ev(ip, action, ActionStatus::Failed, Some("timeout".into())),
    }
}
