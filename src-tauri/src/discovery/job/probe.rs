use std::net::IpAddr;
use std::time::Duration;
use std::collections::VecDeque;

use anyhow::Result as AnyResult;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;
use rand::Rng;

use super::super::helpers::{row_id, sleep_with_cancel, wait_rate_limiter};
use super::super::types::{DiscoveryProgress, RowStatus};
use super::drain::drain_in_flight;
use super::enrich::{apply_enrich_outcome_for_progress, enrich_one};
use super::{AliveTarget, EnrichContext, EnrichTask, ProbeContext, ProbeOutcome, ProbeTask};

fn apply_outcome(
    outcome: ProbeOutcome,
    progress: &mut DiscoveryProgress,
    alive: &mut Vec<AliveTarget>,
) {
    progress.probed += 1;
    match outcome {
        ProbeOutcome::AliveMiner { index, ip, miner } => {
            progress.alive += 1;
            alive.push(AliveTarget { index, ip, miner });
        }
        ProbeOutcome::Miss => {
            progress.missed += 1;
        }
        ProbeOutcome::Cancelled => {
            progress.cancelled += 1;
        }
    }
}

async fn probe_one(ctx: ProbeContext, index: usize, ip: IpAddr) -> AnyResult<ProbeOutcome> {
    let id = row_id(index);

    ctx.ui
        .emit_row_ip_status(id.clone(), RowStatus::Probing, ip);

    if ctx.cancel.is_cancelled() {
        ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
        return Ok(ProbeOutcome::Cancelled);
    }

    if wait_rate_limiter(&ctx.cancel, ctx.rate_limiter.clone()).await {
        ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
        return Ok(ProbeOutcome::Cancelled);
    }

    let miner_task = tokio::time::timeout(ctx.timeout, ctx.factory.scan_miner(ip));

    let (mut miner_opt, timed_out) = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            ctx.ui.emit_row_ip_status(id.clone(), RowStatus::Cancelled, ip);
            return Ok(ProbeOutcome::Cancelled);
        }
        res = miner_task => {
            match res {
                Ok(Ok(opt)) => (opt, false),
                Ok(Err(e)) => {
                    log::debug!("probe error {ip}: {e}");
                    (None, false)
                }
                Err(_elapsed) => (None, true),
            }
        }
    };

    if timed_out && !ctx.cancel.is_cancelled() {
        let jitter_ms = match ctx.retry_jitter_ms {
            0 => 0,
            max => rand::rng().random_range(0..=max),
        };
        if jitter_ms > 0 {
            let jitter = Duration::from_millis(jitter_ms);
            if sleep_with_cancel(&ctx.cancel, jitter).await {
                ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
                return Ok(ProbeOutcome::Cancelled);
            }
        }

        let second = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                ctx.ui.emit_row_ip_status(id.clone(), RowStatus::Cancelled, ip);
                return Ok(ProbeOutcome::Cancelled);
            }
            res = tokio::time::timeout(ctx.timeout, ctx.factory.scan_miner(ip)) => res,
        };

        miner_opt = match second {
            Ok(Ok(opt)) => opt,
            _ => None,
        };
    }

    if ctx.cancel.is_cancelled() {
        ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
        return Ok(ProbeOutcome::Cancelled);
    }

    match miner_opt {
        Some(miner) => {
            ctx.ui.emit_row_ip_status(id.clone(), RowStatus::Alive, ip);
            Ok(ProbeOutcome::AliveMiner {
                index,
                ip,
                miner,
            })
        }
        None => {
            ctx.ui.emit_row_ip_status(id, RowStatus::Miss, ip);
            Ok(ProbeOutcome::Miss)
        }
    }
}

pub async fn run_probe_stage(
    ctx: ProbeContext,
    ips: Vec<IpAddr>,
    probe_concurrency: usize,
    enrich_ctx: EnrichContext,
    enrich_concurrency: usize,
    progress: &mut DiscoveryProgress,
) -> AnyResult<()> {
    enum StageEvent {
        Cancelled,
        Probe(AnyResult<ProbeOutcome>),
        Enrich(AnyResult<super::EnrichOutcome>),
    }

    let probe_concurrency = probe_concurrency.max(1);
    let enrich_concurrency = enrich_concurrency.max(1);
    let mut pending_enrich: VecDeque<AliveTarget> = VecDeque::new();
    let mut iter = ips.into_iter().enumerate().peekable();
    let mut probe_in_flight: FuturesUnordered<ProbeTask> = FuturesUnordered::new();
    let mut enrich_in_flight: FuturesUnordered<EnrichTask> = FuturesUnordered::new();
    let mut cancelled = false;

    while (!cancelled && iter.peek().is_some())
        || !probe_in_flight.is_empty()
        || !pending_enrich.is_empty()
        || !enrich_in_flight.is_empty()
    {
        if ctx.cancel.is_cancelled() {
            cancelled = true;
        }

        while !cancelled && probe_in_flight.len() < probe_concurrency {
            let Some((index, ip)) = iter.next() else {
                break;
            };
            probe_in_flight.push(probe_one(ctx.clone(), index, ip).boxed());
        }

        while !cancelled && enrich_in_flight.len() < enrich_concurrency {
            let Some(target) = pending_enrich.pop_front() else {
                break;
            };
            enrich_in_flight.push(
                enrich_one(enrich_ctx.clone(), target.index, target.ip, target.miner).boxed(),
            );
        }

        if probe_in_flight.is_empty() && enrich_in_flight.is_empty() {
            break;
        }

        let event = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                cancelled = true;
                StageEvent::Cancelled
            }
            probe = probe_in_flight.next(), if !probe_in_flight.is_empty() => {
                StageEvent::Probe(probe.expect("probe future ended unexpectedly"))
            }
            enrich = enrich_in_flight.next(), if !enrich_in_flight.is_empty() => {
                StageEvent::Enrich(enrich.expect("enrich future ended unexpectedly"))
            }
        };

        if cancelled {
            drain_in_flight(&mut probe_in_flight).await;
            drain_in_flight(&mut enrich_in_flight).await;
            return Ok(());
        }

        match event {
            StageEvent::Cancelled => {}
            StageEvent::Probe(result) => match result {
                Ok(outcome) => {
                    let mut alive_batch = Vec::new();
                    apply_outcome(outcome, progress, &mut alive_batch);
                    pending_enrich.extend(alive_batch);
                    ctx.ui.emit_progress(progress);
                }
                Err(e) => {
                    log::error!("probe stage: {e:?}");
                    drain_in_flight(&mut probe_in_flight).await;
                    drain_in_flight(&mut enrich_in_flight).await;
                    return Err(e);
                }
            },
            StageEvent::Enrich(result) => match result {
                Ok(outcome) => {
                    apply_enrich_outcome_for_progress(progress, outcome);
                    ctx.ui.emit_progress(progress);
                }
                Err(e) => {
                    log::error!("enrich stage: {e:?}");
                    drain_in_flight(&mut probe_in_flight).await;
                    drain_in_flight(&mut enrich_in_flight).await;
                    return Err(e);
                }
            },
        }
    }

    if cancelled {
        drain_in_flight(&mut probe_in_flight).await;
        drain_in_flight(&mut enrich_in_flight).await;
    }

    Ok(())
}
