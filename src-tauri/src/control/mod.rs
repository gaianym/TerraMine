//! Phase 1 miner control: start (resume), stop (pause), reboot.

pub mod creds;
pub mod driver;
pub mod job;
pub mod overrides;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod http_mock;
#[cfg(test)]
mod override_tests;
#[cfg(test)]
mod e2e_tests;
#[cfg(any(test, feature = "mock-miner"))]
pub mod mock_miner;
#[cfg(test)]
mod verify_tests;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use driver::{Capabilities, DriverFactory, MinerAction, OverlayFactory};
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
    #[serde(default = "default_true")]
    pub verify: bool,
    #[serde(default = "default_verify_timeout")]
    pub verify_timeout_sec: u64,
}
fn default_true() -> bool { true }
fn default_verify_timeout() -> u64 { 180 }
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
        verify: req.verify.then(|| job::VerifyConfig {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(req.verify_timeout_sec.clamp(10, 900)),
        }),
    }
}

#[tauri::command]
pub async fn miner_capabilities(ips: Vec<String>) -> Result<Vec<CapsRow>, String> {
    let ips = parse_ips(&ips)?;
    let f = OverlayFactory::with_store(Duration::from_secs(5), &creds::KeyringStore);
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
        let factory: Arc<dyn DriverFactory> =
            Arc::new(OverlayFactory::with_store(cfg.timeout, &creds::KeyringStore));
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

#[tauri::command]
pub async fn list_miner_credentials() -> Result<Vec<creds::CredentialSummary>, String> {
    tauri::async_runtime::spawn_blocking(|| creds::summarize(&creds::KeyringStore))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_miner_credentials(
    firmware: creds::FirmwareKind,
    username: String,
    password: String,
) -> Result<(), String> {
    use creds::CredentialStore;
    if username.trim().is_empty() {
        return Err("username required".into());
    }
    let c = overrides::DigestCredentials { username, password };
    tauri::async_runtime::spawn_blocking(move || creds::KeyringStore.set(firmware, &c))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_miner_credentials(firmware: creds::FirmwareKind) -> Result<(), String> {
    use creds::CredentialStore;
    tauri::async_runtime::spawn_blocking(move || creds::KeyringStore.delete(firmware))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
