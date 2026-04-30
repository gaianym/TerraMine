use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyResult;
use asic_rs::MinerFactory;
use asic_rs_core::traits::miner::Miner;
use futures::future::BoxFuture;
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use super::helpers::{ip_row, probe_queue_cap, row_id};
use super::rate_limiter::{RateLimiter, SharedRateLimiter};
use super::sanitize::sanitize_error_message;
use super::targets::parse_targets_to_ips;
use super::types::{
    seconds_to_duration, DiscoveryErrorCode, DiscoveryProgress, DiscoveryRowEvent,
    DiscoverySettings, RowStatus,
};
use super::ui::DiscoveryUi;

mod drain;
mod enrich;
mod probe;

use probe::run_probe_stage;

type ProbeTask = BoxFuture<'static, AnyResult<ProbeOutcome>>;
type EnrichTask = BoxFuture<'static, AnyResult<EnrichOutcome>>;

#[derive(Clone)]
pub(super) struct ProbeContext {
    pub(super) ui: DiscoveryUi,
    pub(super) cancel: CancellationToken,
    pub(super) factory: Arc<MinerFactory>,
    pub(super) timeout: Duration,
    pub(super) rate_limiter: Option<SharedRateLimiter>,
    pub(super) retry_jitter_ms: u64,
}

#[derive(Clone)]
pub(super) struct EnrichContext {
    pub(super) ui: DiscoveryUi,
    pub(super) cancel: CancellationToken,
    pub(super) timeout: Duration,
    pub(super) rate_limiter: Option<SharedRateLimiter>,
}

pub(super) struct AliveTarget {
    index: usize,
    ip: IpAddr,
    miner: Box<dyn Miner + Send>,
}

pub(super) enum ProbeOutcome {
    AliveMiner {
        index: usize,
        ip: IpAddr,
        miner: Box<dyn Miner + Send>,
    },
    Miss,
    Cancelled,
}

pub(super) enum EnrichOutcome {
    Enriched,
    Partial,
    Cancelled,
}

pub(crate) async fn run_discovery_job(
    app: AppHandle,
    run_id: String,
    targets: String,
    settings: DiscoverySettings,
    cancel: CancellationToken,
) -> AnyResult<()> {
    let ui = DiscoveryUi::new(app, run_id.as_str());

    let result: AnyResult<()> = {
        let ui = ui.clone();
        async move {
            let ips = match parse_targets_to_ips(targets.trim()) {
                Ok(ips) => ips,
                Err(e) => {
                    ui.emit_error(DiscoveryErrorCode::InvalidInput, e);
                    return Ok(());
                }
            };

            if ips.is_empty() {
                ui.emit_error(
                    DiscoveryErrorCode::InvalidInput,
                    "No valid targets to scan",
                );
                return Ok(());
            }

            let total = ips.len();
            let mut progress = DiscoveryProgress::new(total);
            ui.emit_progress(&progress);

            let cap = probe_queue_cap(total, settings.probe_queue_cap);
            for (i, ip) in ips.iter().copied().enumerate().take(cap) {
                ui.emit_row(DiscoveryRowEvent {
                    id: row_id(i),
                    status: RowStatus::Queued,
                    row: Some(ip_row(ip)),
                });
            }

            let probe_timeout = seconds_to_duration(settings.probe_timeout_sec);
            let factory = MinerFactory::new()
                .with_connectivity_timeout(probe_timeout)
                .with_identification_timeout(probe_timeout)
                .with_concurrent_limit(settings.probe_concurrency.max(1));
            let factory = Arc::new(factory);

            let rate_limiter = (settings.probe_rate_limit_per_sec > 0)
                .then(|| Arc::new(RateLimiter::new(settings.probe_rate_limit_per_sec)));
            let enrich_rate_limiter = (settings.enrich_rate_limit_per_sec > 0)
                .then(|| Arc::new(RateLimiter::new(settings.enrich_rate_limit_per_sec)));

            let probe_ctx = ProbeContext {
                ui: ui.clone(),
                cancel: cancel.clone(),
                factory,
                timeout: probe_timeout,
                rate_limiter,
                retry_jitter_ms: settings.retry_jitter_ms,
            };

            let enrich_timeout = seconds_to_duration(settings.enrich_timeout_sec);
            let enrich_ctx = EnrichContext {
                ui: ui.clone(),
                cancel: cancel.clone(),
                timeout: enrich_timeout,
                rate_limiter: enrich_rate_limiter,
            };

            let probe_conc = settings.probe_concurrency.max(1);
            let enrich_conc = settings.enrich_concurrency.max(1);
            run_probe_stage(
                probe_ctx,
                ips,
                probe_conc,
                enrich_ctx,
                enrich_conc,
                &mut progress,
            )
            .await?;

            if cancel.is_cancelled() {
                ui.emit_error(DiscoveryErrorCode::Cancelled, "Discovery cancelled");
            }

            ui.emit_progress(&progress);
            Ok(())
        }
        .await
    };

    if let Err(e) = &result {
        ui.emit_error(
            DiscoveryErrorCode::InternalError,
            sanitize_error_message(&e.to_string()),
        );
    }

    ui.emit_done();
    result
}
