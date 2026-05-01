use std::net::IpAddr;

use anyhow::Result as AnyResult;
use asic_rs_core::traits::miner::Miner;
use serde_json::{json, Value};

use super::super::helpers::{row_id, wait_rate_limiter};
use super::super::sanitize::sanitize_miner_json;
use super::super::types::{DiscoveryProgress, DiscoveryRowEvent, RowStatus};
use super::{EnrichContext, EnrichOutcome};

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
