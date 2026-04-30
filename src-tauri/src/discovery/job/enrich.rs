use std::net::IpAddr;

use anyhow::Result as AnyResult;
use asic_rs_core::traits::miner::Miner;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;
use serde_json::{json, Value};

use super::super::helpers::{row_id, wait_rate_limiter};
use super::super::sanitize::sanitize_miner_json;
use super::super::types::{DiscoveryProgress, DiscoveryRowEvent, RowStatus};
use super::drain::drain_in_flight;
use super::{EnrichContext, EnrichOutcome, EnrichTask};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichStageExit {
    Completed,
    Interrupted,
}

fn apply_enrich_outcome(progress: &mut DiscoveryProgress, outcome: EnrichOutcome) {
    match outcome {
        EnrichOutcome::Enriched => progress.enriched += 1,
        EnrichOutcome::Partial => progress.partial += 1,
        EnrichOutcome::Cancelled => progress.cancelled += 1,
    }
}

pub(super) fn apply_enrich_outcome_for_progress(
    progress: &mut DiscoveryProgress,
    outcome: EnrichOutcome,
) {
    apply_enrich_outcome(progress, outcome);
}

fn apply_enrich_task_result(progress: &mut DiscoveryProgress, r: AnyResult<EnrichOutcome>) {
    match r {
        Ok(outcome) => apply_enrich_outcome(progress, outcome),
        Err(e) => log::debug!("enrich task error: {e:?}"),
    }
}

fn merge_miner_discovery(v: Value, ip: IpAddr) -> Value {
    match v {
        Value::Object(mut m) => {
            m.insert("ip".to_string(), Value::String(ip.to_string()));
            m.insert(
                "discovery".to_string(),
                Value::String("miner".to_string()),
            );
            Value::Object(m)
        }
        other => json!({
            "ip": ip.to_string(),
            "discovery": "miner",
            "payload": other
        }),
    }
}

pub(super) async fn enrich_one(
    ctx: EnrichContext,
    index: usize,
    ip: IpAddr,
    miner: Box<dyn Miner + Send>,
) -> AnyResult<EnrichOutcome> {
    let id = row_id(index);

    if wait_rate_limiter(&ctx.cancel, ctx.rate_limiter.clone()).await {
        ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
        return Ok(EnrichOutcome::Cancelled);
    }

    ctx.ui
        .emit_row_ip_status(id.clone(), RowStatus::Enriching, ip);

    if ctx.cancel.is_cancelled() {
        ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
        return Ok(EnrichOutcome::Cancelled);
    }

    let res = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            ctx.ui.emit_row_ip_status(id, RowStatus::Cancelled, ip);
            return Ok(EnrichOutcome::Cancelled);
        }
        r = tokio::time::timeout(ctx.timeout, miner.get_data()) => r,
    };

    match res {
        Ok(data) => {
            let (v, status, outcome) = match serde_json::to_value(&data) {
                Ok(v) => (v, RowStatus::Enriched, EnrichOutcome::Enriched),
                Err(e) => {
                    log::debug!("serialize miner data {ip}: {e}");
                    (json!({}), RowStatus::Partial, EnrichOutcome::Partial)
                }
            };
            let v = sanitize_miner_json(merge_miner_discovery(v, ip));
            ctx.ui.emit_row(DiscoveryRowEvent {
                id,
                status,
                row: Some(v),
            });
            Ok(outcome)
        }
        Err(_elapsed) => {
            ctx.ui.emit_row_ip_status(id, RowStatus::Partial, ip);
            Ok(EnrichOutcome::Partial)
        }
    }
}

pub async fn run_enrich_stage(
    ctx: EnrichContext,
    alive: Vec<super::AliveTarget>,
    concurrency: usize,
    progress: &mut DiscoveryProgress,
) -> AnyResult<EnrichStageExit> {
    let concurrency = concurrency.max(1);
    let mut iter = alive.into_iter().peekable();
    let mut in_flight: FuturesUnordered<EnrichTask> = FuturesUnordered::new();
    let mut cancelled = false;
    let mut interrupted = false;

    while (!cancelled && iter.peek().is_some()) || !in_flight.is_empty() {
        if ctx.cancel.is_cancelled() {
            cancelled = true;
        }

        while !cancelled && in_flight.len() < concurrency {
            let Some(target) = iter.next() else {
                break;
            };
            in_flight.push(
                enrich_one(ctx.clone(), target.index, target.ip, target.miner).boxed(),
            );
        }

        if in_flight.is_empty() {
            break;
        }

        let next_res = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                cancelled = true;
                None
            }
            r = in_flight.next() => r,
        };

        if cancelled {
            interrupted = true;
            if let Some(r) = next_res {
                apply_enrich_task_result(progress, r);
                ctx.ui.emit_progress(progress);
            }
            break;
        }

        if let Some(r) = next_res {
            match r {
                Ok(outcome) => {
                    apply_enrich_outcome(progress, outcome);
                    ctx.ui.emit_progress(progress);
                }
                Err(e) => {
                    log::error!("enrich stage: {e:?}");
                    drain_in_flight(&mut in_flight).await;
                    ctx.ui.emit_progress(progress);
                    return Err(e);
                }
            }
        }
    }

    if cancelled && !in_flight.is_empty() {
        drain_in_flight(&mut in_flight).await;
    }

    Ok(if interrupted {
        EnrichStageExit::Interrupted
    } else {
        EnrichStageExit::Completed
    })
}
