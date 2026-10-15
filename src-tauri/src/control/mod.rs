//! Phase 1 miner control: start (resume), stop (pause), reboot.

pub mod driver;
pub mod job;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use driver::{AsicRsFactory, Capabilities, DriverFactory, MinerAction};
use job::{ActionRowEvent, JobConfig};

pub const EVENT_ROW: &str = "control:row";
pub const EVENT_DONE: &str = "control:done";
const MAX_CONCURRENCY: usize = 64;

#[derive(Clone, Default)]
pub struct ControlState {
    runs: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

#[derive(Debug, Deserialize)]
pub struct ActionRequest {
    pub ips: Vec<String>,
    pub action: MinerAction,
    #[serde(default = "default_stagger")]
    pub stagger_ms: u64,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_timeout")]
    pub timeout_sec: u64,
    #[serde(default)]
    pub dry_run: bool,
}
fn default_stagger() -> u64 { 500 }
fn default_concurrency() -> usize { 4 }
fn default_timeout() -> u64 { 10 }

#[derive(Debug, Clone, Serialize)]
pub struct DoneEvent {
    pub run_id: String,
    pub results: Vec<ActionRowEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapsRow {
    pub ip: String,
    pub found: bool,
    pub caps: Capabilities,
}

pub fn parse_ips(ips: &[String]) -> Result<Vec<IpAddr>, String> {
    let mut out: Vec<IpAddr> = Vec::with_capacity(ips.len());
    for s in ips {
        let ip: IpAddr = s.trim().parse().map_err(|_| format!("invalid IP: {s}"))?;
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    if out.is_empty() {
        return Err("no IPs given".into());
    }
    Ok(out)
}

pub fn job_config(req: &ActionRequest) -> JobConfig {
    JobConfig {
        action: req.action,
        concurrency: req.concurrency.clamp(1, MAX_CONCURRENCY),
        stagger: Duration::from_millis(req.stagger_ms),
        timeout: Duration::from_secs(req.timeout_sec.max(1)),
        dry_run: req.dry_run,
    }
}

#[tauri::command]
pub async fn miner_capabilities(ips: Vec<String>) -> Result<Vec<CapsRow>, String> {
    let ips = parse_ips(&ips)?;
    let f = AsicRsFactory::new(Duration::from_secs(5));
    let mut out = Vec::new();
    for ip in ips {
        let d = f.connect(ip).await.ok().flatten();
        out.push(CapsRow { ip: ip.to_string(), found: d.is_some(), caps: d.map(|d| d.caps()).unwrap_or_default() });
    }
    Ok(out)
}

#[tauri::command]
pub async fn run_miner_action(app: AppHandle, state: State<'_, ControlState>, req: ActionRequest) -> Result<String, String> {
    let ips = parse_ips(&req.ips)?;
    let cfg = job_config(&req);
    let run_id = uuid::Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    state.runs.lock().await.insert(run_id.clone(), cancel.clone());
    let runs = state.runs.clone();
    let rid = run_id.clone();
    tauri::async_runtime::spawn(async move {
        let factory: Arc<dyn DriverFactory> = Arc::new(AsicRsFactory::new(cfg.timeout));
        let app2 = app.clone();
        let sink: job::Sink = Arc::new(move |e| {
            let _ = app2.emit(EVENT_ROW, e);
        });
        let results = job::run_action(ips, cfg, factory, cancel, sink).await;
        runs.lock().await.remove(&rid);
        let _ = app.emit(EVENT_DONE, DoneEvent { run_id: rid, results });
    });
    Ok(run_id)
}

#[tauri::command]
pub async fn cancel_action_run(state: State<'_, ControlState>, run_id: String) -> Result<(), String> {
    match state.runs.lock().await.get(&run_id) {
        Some(t) => {
            t.cancel();
            Ok(())
        }
        None => Err("unknown run".into()),
    }
}
